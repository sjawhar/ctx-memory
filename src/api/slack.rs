//! Slack Web API client using sequential authenticated JSON requests.

use std::collections::HashSet;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::http::{get_json, percent_encode};

const SLACK_API: &str = "https://slack.com/api";

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
        let query = params
            .iter()
            .map(|(key, value)| format!("{key}={}", percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            format!("{}/{method}", self.api)
        } else {
            format!("{}/{method}?{query}", self.api)
        };
        let mut body = get_json(&url, self.token).with_context(|| format!("slack {method}"))?;
        for _ in 0..2 {
            if slack_error(&body) != Some("ratelimited") {
                break;
            }
            thread::sleep(Duration::from_secs(5));
            body = get_json(&url, self.token).with_context(|| format!("slack {method}"))?;
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
                ("oldest", format!("{}.000000", oldest.timestamp())),
                ("latest", format!("{}.000000", latest.timestamp())),
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

    /// Fetch replies to a message, excluding the parent message.
    ///
    /// # Errors
    ///
    /// Returns an error when a request fails, Slack reports an error, or a cursor repeats.
    pub fn replies(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
        let mut replies = Vec::new();
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
                replies.extend(found.iter().cloned());
            }
            cursor = checked_next_cursor(&body, &mut seen_cursors, "conversations.replies")?;
            if cursor.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        Ok(replies
            .into_iter()
            .filter(|message| message.get("ts").and_then(Value::as_str) != Some(thread_ts))
            .collect())
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

    /// Member user IDs of a conversation, following the cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when a request fails, Slack reports an error, or a cursor repeats.
    pub fn members(&self, channel: &str) -> Result<Vec<String>> {
        let mut members = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut params = vec![
                ("channel", channel.to_owned()),
                ("limit", "1000".to_owned()),
            ];
            if let Some(value) = &cursor {
                params.push(("cursor", value.clone()));
            }
            let body = self.call("conversations.members", &params)?;
            if let Some(Value::Array(found)) = body.get("members") {
                members.extend(found.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            cursor = checked_next_cursor(&body, &mut seen_cursors, "conversations.members")?;
            if cursor.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        Ok(members)
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    use anyhow::Result;

    use super::*;
    use serde_json::json;

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
}
