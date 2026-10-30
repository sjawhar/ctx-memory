//! Slack Web API client using sequential authenticated JSON requests.

use std::collections::HashSet;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client;
use crate::credentials::{self, Secret};

const SLACK_API: &str = "https://slack.com/api";

/// Where the browser authorizes `ctx slack login`: Slack's OAuth v2 page, which installs the
/// ctx-memory app for the signed-in user (`docs/sources.md`, "Slack source").
pub const SLACK_AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";
/// Where `ctx slack login` exchanges the authorization code for the user token.
pub const SLACK_ACCESS_URL: &str = "https://slack.com/api/oauth.v2.access";
/// The redirect URI the ctx-memory app registers (`slack/app-manifest.yaml`).
pub const SLACK_REDIRECT_URI: &str = "http://localhost:3118/callback";
/// The user scopes `ctx` needs: public and private channel listing and history, and user
/// names. They are the manifest's `oauth_config.scopes.user`, in the same order.
pub const SLACK_SCOPES: [&str; 5] = [
    "channels:history",
    "channels:read",
    "groups:history",
    "groups:read",
    "users:read",
];
/// Longest permitted `ctx fetch` run: the deployment's fetch job timeout, which must not be
/// longer than this, so a token renewed at fetch start cannot expire during the fetch.
const FETCH_JOB_MAX_RUNTIME_SECONDS: i64 = 3_600;
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

/// Why Slack will not take the stored token, for the two answers that mean the sign-in itself
/// is gone, so that no retry helps and only `ctx slack login` does. The causes are Slack's own
/// descriptions of the errors (`docs/sources.md`, "Slack source").
fn lost_sign_in(error: &str) -> Option<&'static str> {
    match error {
        "token_revoked" => Some(
            "Slack revoked the stored token: its user or workspace was deleted, the app was removed from the workspace, or the token itself was revoked",
        ),
        "invalid_auth" => Some(
            "Slack does not accept the stored token: the token is not valid, or this machine's address is outside the app's allowed IP ranges",
        ),
        _ => None,
    }
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

    /// Call a Slack Web API method; `ok: false` is an error naming Slack's error code. A
    /// `token_revoked` or `invalid_auth` answer also names its cause and the fix, `ctx slack
    /// login`; no error carries the token.
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
        // The fix comes before the cause, so a log line cut to its first few hundred characters
        // still says what to run.
        if let Some(cause) = lost_sign_in(error) {
            bail!("slack {method}: {error:?}: run `ctx slack login` to sign in again. {cause}");
        }
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

/// The Slack credential the fetch job and a live resolve read (contract C6), in either of the
/// two shapes `ctx slack login` has written. Both are rotating user tokens. A credential with
/// `kind: "app"` is the ctx-memory app's; one with no `kind` is the Claude Code app's, which an
/// earlier build wrote.
#[derive(Clone)]
pub enum SlackCredential {
    /// The ctx-memory app's token, refreshed with the app's client id and secret.
    App(AppCredential),
    /// The Claude Code app's token, refreshed as that app's public client, with no secret.
    ClaudeCode(ClaudeCodeCredential),
}

impl SlackCredential {
    /// Reads the credential from its stored JSON, in either shape.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is neither shape. The error never carries a token.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        let value: Value = secret.parse().context("parsing the Slack credential")?;
        let credential = if value.get("kind").is_some() {
            serde_json::from_value(value).map(Self::App)
        } else {
            serde_json::from_value(value).map(Self::ClaudeCode)
        };
        credential.map_err(|error| {
            anyhow!("parsing the Slack credential: it is not the expected JSON ({error})")
        })
    }

    /// Serializes the credential for storage, in its own shape.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential cannot be serialized.
    pub fn to_secret(&self) -> Result<Secret> {
        let json = match self {
            Self::App(credential) => serde_json::to_string(credential),
            Self::ClaudeCode(credential) => serde_json::to_string(credential),
        };
        Ok(Secret::new(
            json.context("serializing the Slack credential")?,
        ))
    }

    /// The bearer token every Web API call presents.
    #[must_use]
    pub fn access(&self) -> &str {
        match self {
            Self::App(credential) => &credential.access,
            Self::ClaudeCode(credential) => &credential.access,
        }
    }

    /// When the access token expires.
    #[must_use]
    pub const fn expires(&self) -> DateTime<Utc> {
        match self {
            Self::App(credential) => credential.expires,
            Self::ClaudeCode(credential) => credential.expires,
        }
    }

    /// Whether the access token expires before a `ctx-fetch` run can finish.
    #[must_use]
    pub fn near_expiry(&self, now: DateTime<Utc>) -> bool {
        self.expires() - now <= chrono::Duration::seconds(FETCH_JOB_MAX_RUNTIME_SECONDS)
    }
}

/// The one `kind` an [`AppCredential`] carries, which tells it from a [`ClaudeCodeCredential`].
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    App,
}

/// The credential `ctx slack login` writes: the ctx-memory app's user token. The app has token
/// rotation on, so the token expires 12 hours after Slack issues it and comes with a refresh
/// token, which the fetch job redeems with the app's client id and secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCredential {
    /// Always [`AppKind::App`].
    pub kind: AppKind,
    /// The bearer token every Web API call presents.
    pub access: String,
    /// The refresh token, redeemed for a new pair; Slack retires it once it is redeemed.
    pub refresh: String,
    /// When `access` expires.
    pub expires: DateTime<Utc>,
    /// The client id of the app the token belongs to, which a refresh presents.
    pub client_id: String,
    /// The token endpoint a refresh posts to.
    pub token_url: String,
    /// The workspace the token reads.
    pub team_id: String,
    /// The person the token reads as.
    pub user_id: String,
}

impl AppCredential {
    /// Serializes the credential for storage, `kind` included.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential cannot be serialized.
    pub fn to_secret(&self) -> Result<Secret> {
        Ok(Secret::new(
            serde_json::to_string(self).context("serializing the Slack credential")?,
        ))
    }
}

/// The credential an earlier `ctx slack login` wrote with the Claude Code Slack app: everything
/// needed to call the Web API and to refresh the token, as that app's public client, without a
/// browser.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaudeCodeCredential {
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

/// The ctx-memory app's OAuth client: its id, and the secret that proves a code exchange or a
/// refresh is the app's. It is the `slack_app` credential, the JSON `{client_id,
/// client_secret}`: `CTX_SECRET_SLACK_APP` names its Secret Manager secret, or
/// `CTX_CREDENTIAL_SLACK_APP_FILE` and `CTX_CREDENTIAL_SLACK_APP` hold it. The secret is held as
/// a [`Secret`], whose `Debug` output never carries it.
pub struct SlackApp {
    pub client_id: String,
    client_secret: Secret,
}

impl SlackApp {
    /// Reads the app from the `slack_app` credential.
    ///
    /// # Errors
    ///
    /// Returns an error when no variable names the credential, it cannot be read, or it is not
    /// the expected JSON. No error carries the secret.
    pub fn load() -> Result<Self> {
        let secret = credentials::load("slack_app")
            .context("reading the ctx-memory Slack app's client id and secret")?;
        Self::from_secret(&secret)
    }

    /// Parses the JSON `{client_id, client_secret}`.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON has another shape or an empty field. No error carries
    /// the secret.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Stored {
            client_id: String,
            client_secret: String,
        }
        let stored: Stored = secret
            .parse()
            .context("parsing the Slack app credential, the JSON {client_id, client_secret}")?;
        if stored.client_id.is_empty() || stored.client_secret.is_empty() {
            bail!("the Slack app credential needs a non-empty client_id and client_secret");
        }
        Ok(Self {
            client_id: stored.client_id,
            client_secret: Secret::new(stored.client_secret),
        })
    }

    /// A form POST to Slack's token endpoint as this app: the client id and secret travel in
    /// HTTP Basic, as `oauth.v2.access` recommends, never in the form or the URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the POST fails, or Slack answers a non-success status whose body
    /// names no Slack error.
    pub fn post(&self, url: &str, form: &[(&str, String)]) -> Result<Value> {
        let (status, _headers, body) =
            client::post_form_basic(url, &self.client_id, self.client_secret.expose(), form)?;
        if !status.is_success() && slack_error(&body).is_none() {
            bail!("the Slack token endpoint returned {status}");
        }
        Ok(body)
    }
}

/// A refresh answer's new access token, refresh token and lifetime in seconds. A user token's
/// refresh answers at the top level, as Slack's own token rotator reads it. An answer without a
/// new refresh token keeps `old_refresh`.
///
/// # Errors
///
/// Returns an error when Slack refused the refresh, or the answer has no access token.
fn renewed<'a>(body: &'a Value, old_refresh: &'a str) -> Result<(&'a str, &'a str, i64)> {
    if body.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = slack_error(body).unwrap_or("unknown");
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
        .unwrap_or(old_refresh);
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECONDS);
    Ok((access, refresh, expires_in))
}

/// Exchanges the ctx-memory app's refresh token for a renewed credential, as `app`, keeping the
/// fields the response does not carry.
///
/// Slack answers `200` even for a failure, with `{"ok":false,"error":...}`, so this inspects
/// the body rather than the status.
///
/// # Errors
///
/// Returns an error when `app` is not the app the token belongs to, the POST fails, Slack
/// reports an error, or the response has no access token. A Slack error is fatal for the
/// refresh: the caller must sign in again rather than loop. No error carries a secret.
pub fn refresh_app_credential(
    credential: &AppCredential,
    app: &SlackApp,
    now: DateTime<Utc>,
) -> Result<AppCredential> {
    if app.client_id != credential.client_id {
        bail!(
            "the Slack app credential is for client {}, but the stored sign-in is for client {}; store the client id and secret of the app the sign-in used, or run `ctx slack login` with this app",
            app.client_id,
            credential.client_id
        );
    }
    let form = [
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", credential.refresh.clone()),
    ];
    let body = app.post(&credential.token_url, &form)?;
    let (access, refresh, expires_in) = renewed(&body, &credential.refresh)?;
    Ok(AppCredential {
        kind: AppKind::App,
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now + chrono::Duration::seconds(expires_in),
        client_id: credential.client_id.clone(),
        token_url: credential.token_url.clone(),
        team_id: credential.team_id.clone(),
        user_id: credential.user_id.clone(),
    })
}

/// Exchanges the Claude Code app's refresh token for a renewed credential, keeping the fields
/// the response does not carry. `post` performs the form POST to `token_url` and returns the
/// parsed body.
///
/// # Errors
///
/// Returns an error when the POST fails, Slack reports an error, or the response has no access
/// token. A Slack error is fatal for the refresh: the caller must sign in again rather than
/// loop.
pub fn refresh_credential(
    credential: &ClaudeCodeCredential,
    now: DateTime<Utc>,
    post: impl Fn(&str, &[(&str, String)]) -> Result<Value>,
) -> Result<ClaudeCodeCredential> {
    let form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", credential.refresh.clone()),
        ("client_id", credential.client_id.clone()),
        ("resource", credential.resource.clone()),
    ];
    let body = post(&credential.token_url, &form)?;
    let (access, refresh, expires_in) = renewed(&body, &credential.refresh)?;
    Ok(ClaudeCodeCredential {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now + chrono::Duration::seconds(expires_in),
        client_id: credential.client_id.clone(),
        token_url: credential.token_url.clone(),
        resource: credential.resource.clone(),
    })
}

/// A stub of Slack's token endpoint, for the tests of the code exchange and the refresh.
#[cfg(test)]
pub(crate) mod stub {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread::JoinHandle;

    use anyhow::{anyhow, Result};

    /// Answers one request with `status` and `body`, and returns the request it received,
    /// headers and body, or nothing for a connection that carried none. Also returns its URL,
    /// which ends in `/api/oauth.v2.access`, and its address, for [`finish`].
    pub(crate) fn token_endpoint(
        status: &str,
        body: &str,
    ) -> Result<(String, SocketAddr, JoinHandle<Result<String>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || -> Result<String> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut request = String::new();
            let mut length = 0_usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 {
                    if request.is_empty() {
                        return Ok(request);
                    }
                    break;
                }
                if line == "\r\n" {
                    request.push_str(&line);
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse()?;
                    }
                }
                request.push_str(&line);
            }
            let mut content = vec![0_u8; length];
            reader.read_exact(&mut content)?;
            request.push_str(&String::from_utf8(content)?);
            stream.write_all(response.as_bytes())?;
            Ok(request)
        });
        Ok((
            format!("http://{address}/api/oauth.v2.access"),
            address,
            server,
        ))
    }

    /// The request the stub received. It connects once first, with no request, so a caller
    /// that failed before it posted cannot leave the stub waiting; once the stub has answered,
    /// that connection does nothing.
    pub(crate) fn finish(
        address: SocketAddr,
        server: JoinHandle<Result<String>>,
    ) -> Result<String> {
        drop(TcpStream::connect(address));
        server
            .join()
            .map_err(|_| anyhow!("the stub token endpoint panicked"))?
    }

    /// A request header's value, by case-insensitive name.
    pub(crate) fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};

    use base64::Engine as _;
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

    /// What the Claude Code app's credential carried, as `ctx slack login` once stored it.
    const CLAUDE_CODE_CLIENT_ID: &str = "1601185624273.8899143856786";
    const CLAUDE_CODE_TOKEN_URL: &str = "https://slack.com/api/oauth.v2.user.access";
    const CLAUDE_CODE_RESOURCE: &str = "https://mcp.slack.com";

    fn credential(expires: DateTime<Utc>) -> ClaudeCodeCredential {
        ClaudeCodeCredential {
            access: "old-access".to_owned(),
            refresh: "old-refresh".to_owned(),
            expires,
            client_id: CLAUDE_CODE_CLIENT_ID.to_owned(),
            token_url: CLAUDE_CODE_TOKEN_URL.to_owned(),
            resource: CLAUDE_CODE_RESOURCE.to_owned(),
        }
    }

    const APP_CLIENT_ID: &str = "1234567890.0987654321";
    /// A client secret no output, error or stored credential may carry.
    const APP_CLIENT_SECRET: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    fn app() -> Result<SlackApp> {
        SlackApp::from_secret(&Secret::new(format!(
            r#"{{"client_id":"{APP_CLIENT_ID}","client_secret":"{APP_CLIENT_SECRET}"}}"#
        )))
    }

    fn app_credential(token_url: &str, expires: DateTime<Utc>) -> AppCredential {
        AppCredential {
            kind: AppKind::App,
            access: "xoxe.xoxp-1-old".to_owned(),
            refresh: "xoxe-1-old".to_owned(),
            expires,
            client_id: APP_CLIENT_ID.to_owned(),
            token_url: token_url.to_owned(),
            team_id: "T0EXAMPLE1".to_owned(),
            user_id: "U0EXAMPLE9".to_owned(),
        }
    }

    #[test]
    fn the_ctx_memory_apps_credential_is_read_as_a_rotating_app_token() -> Result<()> {
        let secret = Secret::new(
            r#"{"kind":"app","access":"xoxe.xoxp-1-a","refresh":"xoxe-1-r","expires":"2026-10-29T16:00:00Z","client_id":"123.456","token_url":"https://slack.com/api/oauth.v2.access","team_id":"T0EXAMPLE1","user_id":"U0EXAMPLE9"}"#
                .to_owned(),
        );
        let credential = SlackCredential::from_secret(&secret)?;
        assert_eq!(credential.access(), "xoxe.xoxp-1-a");
        assert_eq!(
            credential.expires(),
            DateTime::parse_from_rfc3339("2026-10-29T16:00:00Z")?
        );
        let SlackCredential::App(app) = credential else {
            anyhow::bail!("a credential with kind \"app\" must read as the ctx-memory app's");
        };
        assert_eq!(app.refresh, "xoxe-1-r");
        assert_eq!(app.client_id, "123.456");
        assert_eq!(app.token_url, SLACK_ACCESS_URL);
        assert_eq!(app.team_id, "T0EXAMPLE1");
        assert_eq!(app.user_id, "U0EXAMPLE9");
        Ok(())
    }

    /// The shape of a credential an earlier build stored from the Claude Code app's sign-in: it
    /// stays readable, and stays the kind the fetch job refreshes.
    #[test]
    fn the_claude_code_apps_credential_is_still_read() -> Result<()> {
        let secret = Secret::new(format!(
            r#"{{"access":"xoxe.xoxp-old","refresh":"xoxe-1-old","expires":"2026-10-29T07:59:25.065552121Z","client_id":"{CLAUDE_CODE_CLIENT_ID}","token_url":"{CLAUDE_CODE_TOKEN_URL}","resource":"{CLAUDE_CODE_RESOURCE}"}}"#
        ));
        let SlackCredential::ClaudeCode(rotating) = SlackCredential::from_secret(&secret)? else {
            anyhow::bail!("a credential with no kind must read as the Claude Code app's");
        };
        assert_eq!(rotating.access, "xoxe.xoxp-old");
        assert_eq!(rotating.refresh, "xoxe-1-old");
        assert_eq!(rotating.client_id, CLAUDE_CODE_CLIENT_ID);
        assert_eq!(rotating.token_url, CLAUDE_CODE_TOKEN_URL);
        assert_eq!(rotating.resource, CLAUDE_CODE_RESOURCE);
        assert_eq!(
            rotating.expires,
            DateTime::parse_from_rfc3339("2026-10-29T07:59:25.065552121Z")?
        );
        Ok(())
    }

    #[test]
    fn an_app_credential_is_stored_with_its_kind_and_nothing_else() -> Result<()> {
        let expires = DateTime::parse_from_rfc3339("2026-10-29T16:00:00Z")?.with_timezone(&Utc);
        let stored = app_credential(SLACK_ACCESS_URL, expires).to_secret()?;
        let value: Value = stored.parse()?;
        assert_eq!(
            value,
            json!({
                "kind": "app",
                "access": "xoxe.xoxp-1-old",
                "refresh": "xoxe-1-old",
                "expires": "2026-10-29T16:00:00Z",
                "client_id": APP_CLIENT_ID,
                "token_url": SLACK_ACCESS_URL,
                "team_id": "T0EXAMPLE1",
                "user_id": "U0EXAMPLE9",
            })
        );
        let SlackCredential::App(read) = SlackCredential::from_secret(&stored)? else {
            anyhow::bail!("a stored app credential must read back as one");
        };
        assert_eq!(read.refresh, "xoxe-1-old");
        Ok(())
    }

    /// An app credential with a field ctx does not know or without one it needs, or naming a
    /// kind ctx does not know, is refused rather than read as something it is not, and the
    /// refusal names no token.
    #[test]
    fn a_credential_of_neither_shape_is_refused_without_its_token() {
        let complete = r#""access":"xoxp-secret-value","refresh":"xoxe-1-secret","expires":"2026-10-29T16:00:00Z","client_id":"1","token_url":"u","team_id":"T","user_id":"U""#;
        for (json, why) in [
            (format!(r#"{{"kind":"app",{complete},"resource":"r"}}"#), "unknown field"),
            (format!(r#"{{"kind":"user",{complete}}}"#), "unknown variant"),
            (
                r#"{"kind":"app","access":"xoxp-secret-value","refresh":"xoxe-1-secret","client_id":"1"}"#
                    .to_owned(),
                "missing field",
            ),
        ] {
            let error = SlackCredential::from_secret(&Secret::new(json.clone()))
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_default();
            assert!(error.contains(why), "{json}: unexpected error {error:?}");
            assert!(
                !error.contains("secret-value") && !error.contains("xoxe-1-secret"),
                "{json}: the error carries a token: {error}"
            );
        }
    }

    #[test]
    fn the_app_credential_is_its_client_id_and_secret() -> Result<()> {
        let app = app()?;
        assert_eq!(app.client_id, APP_CLIENT_ID);
        assert_eq!(app.client_secret.expose(), APP_CLIENT_SECRET);
        assert_eq!(format!("{:?}", app.client_secret), "Secret(redacted)");
        Ok(())
    }

    #[test]
    fn a_malformed_app_credential_is_refused_without_its_secret() {
        for json in [
            format!(
                r#"{{"client_id":"{APP_CLIENT_ID}","client_secret":"{APP_CLIENT_SECRET}","extra":1}}"#
            ),
            format!(r#"{{"client_id":"","client_secret":"{APP_CLIENT_SECRET}"}}"#),
            format!(r#"{{"client_secret":"{APP_CLIENT_SECRET}"}}"#),
            format!(r#"{{"client_id":"{APP_CLIENT_ID}","client_secret":"{APP_CLIENT_SECRET}""#),
        ] {
            let error = SlackApp::from_secret(&Secret::new(json.clone()))
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_default();
            assert!(!error.is_empty(), "{json} was accepted");
            assert!(
                !error.contains(APP_CLIENT_SECRET),
                "{json}: the error carries the secret: {error}"
            );
        }
    }

    #[test]
    fn a_missing_app_credential_names_its_variable() {
        for name in [
            "CTX_SECRET_SLACK_APP",
            "CTX_CREDENTIAL_SLACK_APP",
            "CTX_CREDENTIAL_SLACK_APP_FILE",
        ] {
            std::env::remove_var(name);
        }
        let error = SlackApp::load()
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            error.contains("CTX_SECRET_SLACK_APP") && error.contains("ctx-memory Slack app"),
            "unexpected error: {error}"
        );
    }

    /// The app's refresh over HTTP: the refresh token goes to the stored token endpoint with the
    /// client id and secret in HTTP Basic, the answer's new pair replaces the old, and the client
    /// secret is in neither the form nor the renewed credential.
    #[test]
    fn an_app_refresh_presents_the_client_secret_in_basic_and_stores_the_new_pair() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let (url, address, server) = stub::token_endpoint(
            "200 OK",
            r#"{"ok":true,"access_token":"xoxe.xoxp-1-new","refresh_token":"xoxe-1-new","expires_in":43200,"token_type":"user"}"#,
        )?;
        let renewed = refresh_app_credential(&app_credential(&url, now), &app()?, now);
        let request = stub::finish(address, server)?;
        let renewed = renewed?;

        assert!(
            request.starts_with("POST /api/oauth.v2.access HTTP/1.1\r\n"),
            "{request}"
        );
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{APP_CLIENT_ID}:{APP_CLIENT_SECRET}"));
        assert!(
            stub::header(&request, "authorization") == Some(format!("Basic {basic}").as_str()),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert_eq!(body, "grant_type=refresh_token&refresh_token=xoxe-1-old");
        assert_eq!(renewed.access, "xoxe.xoxp-1-new");
        assert_eq!(renewed.refresh, "xoxe-1-new");
        assert_eq!(renewed.expires, now + chrono::Duration::seconds(43_200));
        assert_eq!(renewed.token_url, url);
        assert_eq!(
            (renewed.team_id.as_str(), renewed.user_id.as_str()),
            ("T0EXAMPLE1", "U0EXAMPLE9")
        );
        assert!(!renewed.to_secret()?.expose().contains(APP_CLIENT_SECRET));
        Ok(())
    }

    #[test]
    fn a_refused_app_refresh_says_to_sign_in_and_never_shows_the_secret() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let (url, address, server) =
            stub::token_endpoint("200 OK", r#"{"ok":false,"error":"invalid_refresh_token"}"#)?;
        let result = refresh_app_credential(&app_credential(&url, now), &app()?, now);
        stub::finish(address, server)?;
        let error = result
            .err()
            .map(|error| format!("{error:#}"))
            .ok_or_else(|| anyhow::anyhow!("a refused refresh must fail"))?;
        assert!(error.contains("invalid_refresh_token"), "{error}");
        assert!(error.contains("run `ctx slack login`"), "{error}");
        assert!(!error.contains(APP_CLIENT_SECRET), "{error}");
        Ok(())
    }

    /// A client secret of another app would be refused by Slack only after the refresh token
    /// was sent; the mismatch is caught first and nothing is sent.
    #[test]
    fn an_app_refresh_with_another_apps_secret_sends_nothing() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let mut credential = app_credential("http://127.0.0.1:1/unused", now);
        credential.client_id = "999.999".to_owned();
        let error = refresh_app_credential(&credential, &app()?, now)
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            error.contains("is for client 1234567890.0987654321"),
            "{error}"
        );
        assert!(
            error.contains("stored sign-in is for client 999.999"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn a_revoked_or_invalid_token_names_the_cause_and_the_fix_without_the_token() -> Result<()> {
        for (answer, cause) in [
            ("token_revoked", "Slack revoked the stored token"),
            ("invalid_auth", "Slack does not accept the stored token"),
        ] {
            let (api, server) = fake_api(vec![format!(r#"{{"ok":false,"error":"{answer}"}}"#)])?;
            let client = SlackClient::with_api("xoxp-secret-value", &api);
            let result = client.call("conversations.list", &[]);
            server
                .join()
                .map_err(|_| anyhow::anyhow!("fake API server failed"))??;
            let error = result
                .err()
                .map(|error| format!("{error:#}"))
                .ok_or_else(|| anyhow::anyhow!("{answer} must fail the call"))?;
            assert!(error.contains(&format!("\"{answer}\"")), "{error}");
            assert!(error.contains(cause), "{error}");
            assert!(error.contains("run `ctx slack login`"), "{error}");
            assert!(!error.contains("xoxp-secret-value"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn another_slack_error_does_not_send_the_reader_to_sign_in() -> Result<()> {
        let (api, server) = fake_api(vec![
            r#"{"ok":false,"error":"channel_not_found"}"#.to_owned()
        ])?;
        let client = SlackClient::with_api("test-token", &api);
        let result = client.call("conversations.info", &[]);
        server
            .join()
            .map_err(|_| anyhow::anyhow!("fake API server failed"))??;
        let error = result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert_eq!(error, r#"slack conversations.info: "channel_not_found""#);
        Ok(())
    }

    #[test]
    fn near_expiry_covers_the_full_fetch_timeout() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow::anyhow!("constructing test time"))?;
        let near = |expires| SlackCredential::ClaudeCode(credential(expires)).near_expiry(now);
        assert!(near(now + chrono::Duration::minutes(30)));
        assert!(near(now + chrono::Duration::hours(1)));
        assert!(!near(
            now + chrono::Duration::hours(1) + chrono::Duration::seconds(1)
        ));
        let app_near = |expires| {
            SlackCredential::App(app_credential(SLACK_ACCESS_URL, expires)).near_expiry(now)
        };
        assert!(app_near(now + chrono::Duration::minutes(59)));
        assert!(!app_near(now + chrono::Duration::hours(2)));
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
        assert_eq!(url, CLAUDE_CODE_TOKEN_URL);
        assert!(form.contains(&("grant_type".to_owned(), "refresh_token".to_owned())));
        assert!(form.contains(&("refresh_token".to_owned(), "old-refresh".to_owned())));
        assert!(form.contains(&("client_id".to_owned(), CLAUDE_CODE_CLIENT_ID.to_owned())));
        assert!(form.contains(&("resource".to_owned(), CLAUDE_CODE_RESOURCE.to_owned())));
        assert!(!form.iter().any(|(key, _)| key == "client_secret"));
        assert_eq!(renewed.access, "new-access");
        assert_eq!(renewed.refresh, "new-refresh");
        assert_eq!(renewed.expires, now + chrono::Duration::seconds(43_200));
        assert_eq!(renewed.client_id, CLAUDE_CODE_CLIENT_ID);
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
