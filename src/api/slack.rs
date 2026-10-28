//! Slack Web API client using sequential authenticated JSON requests.

use std::collections::HashSet;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client;
use crate::credentials::Secret;

const SLACK_API: &str = "https://slack.com/api";

/// The public OAuth client id of the Claude Code Slack app, which `ctx` reuses, the same app
/// omp signs in with (`docs/sources.md`, "Slack source").
pub const SLACK_CLIENT_ID: &str = "1601185624273.8899143856786";
/// Where the browser authorizes the sign-in.
pub const SLACK_AUTHORIZE_URL: &str = "https://slack.com/oauth/v2_user/authorize";
/// Where an authorization code or a refresh token is exchanged for an access token.
pub const SLACK_TOKEN_URL: &str = "https://slack.com/api/oauth.v2.user.access";
/// The resource this app's tokens are scoped to.
pub const SLACK_RESOURCE: &str = "https://mcp.slack.com";
/// The only redirect URI this public client accepts.
pub const SLACK_REDIRECT_URI: &str = "http://localhost:3118/callback";
/// The read scopes `ctx` needs: public and private channel listing and history, and user
/// names. Scopes are additive per user, so the issued token may carry more.
pub const SLACK_SCOPES: [&str; 5] = [
    "channels:history",
    "channels:read",
    "groups:history",
    "groups:read",
    "users:read",
];
/// A token is refreshed once it is within this long of expiring (omp's skew).
const REFRESH_SKEW_SECONDS: i64 = 300;
/// Slack's token lifetime, used when a refresh response omits `expires_in`.
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 43_200;

pub struct SlackClient<'a> {
    token: &'a str,
    api: &'a str,
}

fn slack_error(body: &Value) -> Option<&str> {
    body.get("error").and_then(Value::as_str)
}

fn is_ok(body: &Value) -> bool {
    body.get("ok").and_then(Value::as_bool) == Some(true)
}

impl<'a> SlackClient<'a> {
    #[must_use]
    pub const fn new(token: &'a str) -> Self {
        Self {
            token,
            api: SLACK_API,
        }
    }

    /// Build a client that uses `api` as the Slack API base URL.
    #[must_use]
    pub const fn with_api(token: &'a str, api: &'a str) -> Self {
        Self { token, api }
    }

    /// The response body whether or not Slack said `ok`, retrying up to twice on
    /// `ratelimited` five seconds apart.
    fn call_raw(&self, method: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/{method}", self.api);
        let (_, _, mut body) = client::get_json(&url, self.token, params)
            .with_context(|| format!("slack {method}"))?;
        for _ in 0..2 {
            if slack_error(&body) != Some("ratelimited") {
                break;
            }
            thread::sleep(Duration::from_secs(5));
            let (_, _, retried) = client::get_json(&url, self.token, params)
                .with_context(|| format!("slack {method}"))?;
            body = retried;
        }
        Ok(body)
    }

    /// Call a Slack Web API method; `ok: false` is an error naming Slack's error code.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or Slack reports an error.
    pub fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value> {
        let body = self.call_raw(method, params)?;
        if is_ok(&body) {
            return Ok(body);
        }
        let error = slack_error(&body).map_or("unknown", |value| value);
        bail!("slack {method}: {error:?}")
    }

    /// Fetch messages in a channel's inclusive timestamp range.
    ///
    /// # Errors
    ///
    /// Returns an error when a request fails, Slack reports an error, or a cursor repeats.
    pub fn history(
        &self,
        channel: &str,
        oldest: DateTime<Utc>,
        latest: DateTime<Utc>,
    ) -> Result<Vec<Value>> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut params = vec![
                ("channel", channel.to_owned()),
                ("oldest", slack_timestamp_param(oldest)),
                ("latest", slack_timestamp_param(latest)),
                ("inclusive", "true".to_owned()),
                ("limit", "200".to_owned()),
            ];
            if let Some(value) = &cursor {
                params.push(("cursor", value.clone()));
            }
            let body = self.call("conversations.history", &params)?;
            if let Some(Value::Array(found)) = body.get("messages") {
                messages.extend(found.iter().cloned());
            }
            cursor = checked_next_cursor(&body, &mut seen_cursors, "conversations.history")?;
            let Some(_) = &cursor else { break };
            thread::sleep(Duration::from_millis(300));
        }
        Ok(messages)
    }

    /// Fetch every message in a thread, including its parent message.
    ///
    /// # Errors
    ///
    /// Returns an error when a request fails, Slack reports an error, or a cursor repeats.
    pub fn thread_messages(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut params = vec![
                ("channel", channel.to_owned()),
                ("ts", thread_ts.to_owned()),
                ("limit", "200".to_owned()),
            ];
            if let Some(value) = &cursor {
                params.push(("cursor", value.clone()));
            }
            let body = self.call("conversations.replies", &params)?;
            if let Some(Value::Array(found)) = body.get("messages") {
                messages.extend(found.iter().cloned());
            }
            cursor = checked_next_cursor(&body, &mut seen_cursors, "conversations.replies")?;
            if cursor.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        Ok(messages)
    }

    /// Fetch metadata for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails, Slack reports an error, or the response lacks
    /// conversation metadata.
    pub fn channel_info(&self, channel: &str) -> Result<Value> {
        let body = self.call("conversations.info", &[("channel", channel.to_owned())])?;
        body.get("channel")
            .cloned()
            .ok_or_else(|| anyhow!("slack conversations.info: missing 'channel' in response"))
    }

    /// Every conversation of the given `types`, following the cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when a request fails, Slack reports an error, or a cursor repeats.
    pub fn list_conversations(&self, types: &str, exclude_archived: bool) -> Result<Vec<Value>> {
        let mut channels = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut params = vec![("types", types.to_owned()), ("limit", "1000".to_owned())];
            if exclude_archived {
                params.push(("exclude_archived", "true".to_owned()));
            }
            if let Some(value) = &cursor {
                params.push(("cursor", value.clone()));
            }
            let body = self.call("conversations.list", &params)?;
            if let Some(Value::Array(found)) = body.get("channels") {
                channels.extend(found.iter().cloned());
            }
            cursor = checked_next_cursor(&body, &mut seen_cursors, "conversations.list")?;
            if cursor.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        Ok(channels)
    }
}

fn slack_timestamp_param(timestamp: DateTime<Utc>) -> String {
    format!(
        "{}.{:06}",
        timestamp.timestamp(),
        timestamp.timestamp_subsec_micros()
    )
}

fn next_cursor(body: &Value) -> Option<String> {
    body.get("response_metadata")
        .and_then(|metadata| metadata.get("next_cursor"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn checked_next_cursor(
    body: &Value,
    seen_cursors: &mut HashSet<String>,
    method: &str,
) -> Result<Option<String>> {
    let cursor = next_cursor(body);
    if let Some(value) = &cursor {
        if !seen_cursors.insert(value.clone()) {
            bail!("slack {method}: repeated cursor {value:?}");
        }
    }
    Ok(cursor)
}

fn decode_html_entity(token: &str) -> Option<char> {
    if let Some(hex) = token.strip_prefix('x').or_else(|| token.strip_prefix('X')) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if let Some(hex) = token
        .strip_prefix("#x")
        .or_else(|| token.strip_prefix("#X"))
    {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if let Some(decimal) = token.strip_prefix('#') {
        return decimal.parse::<u32>().ok().and_then(char::from_u32);
    }
    match token {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => None,
    }
}

/// Decode supported HTML entities once, preserving unrecognized entities unchanged.
#[must_use]
pub fn html_unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = text.get(cursor..).and_then(|remaining| remaining.find('&')) {
        let start = cursor + offset;
        let after_start = start + 1;
        let Some(remaining) = text.get(after_start..) else {
            break;
        };
        let Some(semicolon_offset) = remaining.find(';') else {
            break;
        };
        let end = after_start + semicolon_offset;
        let Some(prefix) = text.get(cursor..start) else {
            break;
        };
        let Some(token) = text.get(after_start..end) else {
            break;
        };
        let Some(entity) = text.get(start..=end) else {
            break;
        };
        output.push_str(prefix);
        if let Some(decoded) = decode_html_entity(token) {
            output.push(decoded);
        } else {
            output.push_str(entity);
        }
        cursor = end + 1;
    }
    if let Some(tail) = text.get(cursor..) {
        output.push_str(tail);
    }
    output
}

/// The Slack user OAuth credential (contract C6): everything needed to call the Web API and
/// to refresh the token without a browser.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlackCredential {
    /// The bearer token every Web API call presents.
    pub access: String,
    /// The refresh token, redeemed for a new pair; it stops working once redeemed.
    pub refresh: String,
    /// When `access` expires.
    pub expires: DateTime<Utc>,
    /// The public client id a refresh presents.
    pub client_id: String,
    /// The token endpoint a refresh posts to.
    pub token_url: String,
    /// The resource the token is scoped to.
    pub resource: String,
}

impl SlackCredential {
    /// Reads the credential from its stored JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not a Slack credential.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        secret.parse().context("parsing the Slack credential")
    }

    /// Serializes the credential for storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential cannot be serialized.
    pub fn to_secret(&self) -> Result<Secret> {
        Ok(Secret::new(
            serde_json::to_string(self).context("serializing the Slack credential")?,
        ))
    }

    /// Whether `access` is within the refresh skew of expiring at `now`.
    #[must_use]
    pub fn near_expiry(&self, now: DateTime<Utc>) -> bool {
        self.expires - now <= chrono::Duration::seconds(REFRESH_SKEW_SECONDS)
    }
}

/// Exchanges the refresh token for a renewed credential, keeping the fields the response does
/// not carry. `post` performs the form POST to `token_url` and returns the parsed body.
///
/// Slack answers `200` even for a failure, with `{"ok":false,"error":...}`, so this inspects
/// the body rather than the status.
///
/// # Errors
///
/// Returns an error when the POST fails, Slack reports an error, or the response has no access
/// token. A Slack error is fatal for the refresh: the caller must sign in again rather than
/// loop.
pub fn refresh_credential(
    credential: &SlackCredential,
    now: DateTime<Utc>,
    post: impl Fn(&str, &[(&str, String)]) -> Result<Value>,
) -> Result<SlackCredential> {
    let form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", credential.refresh.clone()),
        ("client_id", credential.client_id.clone()),
        ("resource", credential.resource.clone()),
    ];
    let body = post(&credential.token_url, &form)?;
    if body.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = slack_error(&body).unwrap_or("unknown");
        bail!("Slack refused the token refresh: {error:?}; run `ctx slack login` to sign in again");
    }
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("the Slack refresh response has no access_token"))?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .unwrap_or(&credential.refresh);
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECONDS);
    Ok(SlackCredential {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now + chrono::Duration::seconds(expires_in),
        client_id: credential.client_id.clone(),
        token_url: credential.token_url.clone(),
        resource: credential.resource.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use anyhow::Result;

    use super::*;
    use serde_json::json;

    type RecordingApi = (String, Arc<Mutex<Vec<String>>>, JoinHandle<Result<()>>);

    fn fake_api(responses: Vec<String>) -> Result<(String, JoinHandle<Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> Result<()> {
            for body in responses {
                let (mut stream, _) = listener.accept()?;
                let mut request = BufReader::new(stream.try_clone()?);
                loop {
                    let mut line = String::new();
                    let bytes_read = request.read_line(&mut line)?;
                    if bytes_read == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes())?;
            }
            Ok(())
        });
        Ok((format!("http://{address}"), server))
    }

    fn fake_api_recording_requests(responses: Vec<String>) -> Result<RecordingApi> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = std::thread::spawn(move || -> Result<()> {
            for body in responses {
                let (mut stream, _) = listener.accept()?;
                let mut request = BufReader::new(stream.try_clone()?);
                let mut first_line = String::new();
                request.read_line(&mut first_line)?;
                captured
                    .lock()
                    .map_err(|error| anyhow::anyhow!("locking requests: {error}"))?
                    .push(first_line);
                loop {
                    let mut line = String::new();
                    let bytes_read = request.read_line(&mut line)?;
                    if bytes_read == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes())?;
            }
            Ok(())
        });
        Ok((format!("http://{address}"), requests, server))
    }

    #[test]
    fn html_unescape_decodes_amp_lt_gt_without_double_unescaping() {
        assert_eq!(html_unescape("a &amp;lt; b"), "a &lt; b");
        assert_eq!(html_unescape("&lt;tag&gt; &amp; text"), "<tag> & text");
    }

    #[test]
    fn next_cursor_treats_empty_string_as_end() {
        assert_eq!(
            next_cursor(&json!({"response_metadata": {"next_cursor": "CURSOR"}})),
            Some("CURSOR".to_owned())
        );
        assert_eq!(
            next_cursor(&json!({"response_metadata": {"next_cursor": ""}})),
            None
        );
        assert_eq!(next_cursor(&json!({"ok": true})), None);
    }

    #[test]
    fn ok_and_error_are_read_from_the_body() {
        assert!(is_ok(&json!({"ok": true})));
        assert!(!is_ok(&json!({"ok": false, "error": "users_not_found"})));
        assert_eq!(
            slack_error(&json!({"ok": false, "error": "ratelimited"})),
            Some("ratelimited")
        );
    }

    #[test]
    fn a_repeated_cursor_stops_pagination_with_an_error() -> Result<()> {
        let repeated_page =
            "{\"ok\":true,\"messages\":[],\"response_metadata\":{\"next_cursor\":\"abc\"}}"
                .to_owned();
        let (api, server) = fake_api(vec![repeated_page.clone(), repeated_page])?;
        let client = SlackClient::with_api("test-token", &api);
        let timestamp: DateTime<Utc> = DateTime::from_timestamp(0, 0)
            .ok_or_else(|| anyhow::anyhow!("Unix epoch is unavailable"))?;

        let history = client.history("C0EXAMPLE01", timestamp, timestamp);
        let server_result = server
            .join()
            .map_err(|_| anyhow::anyhow!("fake API server failed"))?;
        server_result?;
        let error = history
            .err()
            .ok_or_else(|| anyhow::anyhow!("pagination unexpectedly succeeded"))?;
        assert!(format!("{error:#}").contains("repeated cursor \"abc\""));
        Ok(())
    }

    #[test]
    fn history_preserves_microsecond_bounds_and_requests_inclusive_results() -> Result<()> {
        let (api, requests, server) =
            fake_api_recording_requests(vec![r#"{"ok":true,"messages":[]}"#.to_owned()])?;
        let client = SlackClient::with_api("test-token", &api);
        let timestamp = DateTime::from_timestamp(1_792_951_800, 123_456_000)
            .ok_or_else(|| anyhow::anyhow!("constructing test timestamp"))?;

        let messages = client.history("C0EXAMPLE01", timestamp, timestamp);
        server
            .join()
            .map_err(|_| anyhow::anyhow!("fake API server failed"))??;
        messages?;
        let request = requests
            .lock()
            .map_err(|error| anyhow::anyhow!("locking requests: {error}"))?
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fake API received no request"))?;

        assert!(request.contains("oldest=1792951800.123456"));
        assert!(request.contains("latest=1792951800.123456"));
        assert!(request.contains("inclusive=true"));
        Ok(())
    }

    fn credential(expires: DateTime<Utc>) -> SlackCredential {
        SlackCredential {
            access: "old-access".to_owned(),
            refresh: "old-refresh".to_owned(),
            expires,
            client_id: SLACK_CLIENT_ID.to_owned(),
            token_url: SLACK_TOKEN_URL.to_owned(),
            resource: SLACK_RESOURCE.to_owned(),
        }
    }

    #[test]
    fn near_expiry_uses_the_skew() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        assert!(credential(now + chrono::Duration::seconds(60)).near_expiry(now));
        assert!(credential(now + chrono::Duration::seconds(299)).near_expiry(now));
        assert!(!credential(now + chrono::Duration::seconds(600)).near_expiry(now));
        Ok(())
    }

    #[test]
    fn refresh_posts_the_rotation_form_and_keeps_unreturned_fields() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let seen = std::cell::RefCell::new(Vec::new());
        let renewed = refresh_credential(&credential(now), now, |url, form| {
            let owned: Vec<(String, String)> = form
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect();
            seen.borrow_mut().push((url.to_owned(), owned));
            Ok(json!({
                "ok": true,
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 43_200,
            }))
        })?;
        let calls = seen.into_inner();
        assert_eq!(calls.len(), 1);
        let (url, form) = calls.first().ok_or_else(|| anyhow::anyhow!("no call"))?;
        assert_eq!(url, SLACK_TOKEN_URL);
        assert!(form.contains(&("grant_type".to_owned(), "refresh_token".to_owned())));
        assert!(form.contains(&("refresh_token".to_owned(), "old-refresh".to_owned())));
        assert!(form.contains(&("client_id".to_owned(), SLACK_CLIENT_ID.to_owned())));
        assert!(form.contains(&("resource".to_owned(), SLACK_RESOURCE.to_owned())));
        assert!(!form.iter().any(|(key, _)| key == "client_secret"));
        assert_eq!(renewed.access, "new-access");
        assert_eq!(renewed.refresh, "new-refresh");
        assert_eq!(renewed.expires, now + chrono::Duration::seconds(43_200));
        assert_eq!(renewed.client_id, SLACK_CLIENT_ID);
        Ok(())
    }

    #[test]
    fn refresh_keeps_the_old_refresh_token_when_none_is_returned() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let renewed = refresh_credential(&credential(now), now, |_url, _form| {
            Ok(json!({ "ok": true, "access_token": "new-access", "expires_in": 43_200 }))
        })?;
        assert_eq!(renewed.refresh, "old-refresh");
        Ok(())
    }

    #[test]
    fn refresh_rejects_a_slack_error_without_looping() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let calls = std::cell::Cell::new(0_u32);
        let error = refresh_credential(&credential(now), now, |_url, _form| {
            calls.set(calls.get() + 1);
            Ok(json!({ "ok": false, "error": "invalid_refresh_token" }))
        })
        .err()
        .ok_or_else(|| anyhow::anyhow!("a Slack error must fail the refresh"))?;
        assert_eq!(calls.get(), 1);
        let rendered = format!("{error:#}");
        assert!(rendered.contains("invalid_refresh_token"));
        assert!(rendered.contains("ctx slack login"));
        Ok(())
    }
}
