//! Built-in Gmail source.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, SecondsFormat, TimeZone as _, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::gws;
use crate::api::slack::html_unescape;
use crate::record::Record;

use super::{
    window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source, StreamStatus,
    PROTOCOL_VERSION,
};

/// Built-in source for Gmail messages.
pub struct GmailSource;

impl GmailSource {
    /// Creates a Gmail source using the `gws` command line client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for GmailSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for GmailSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        let api = GwsGmailApi;
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                emit_fetch_hello(request, sink)?;
                fetch_with_api(request, &config, &api, sink)
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)?;
                resolve_with_api(request, &config, &api, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GmailConfig {
    #[serde(default)]
    query: String,
}

fn parse_config(value: &Value) -> Result<GmailConfig> {
    serde_json::from_value(value.clone()).context("parsing Gmail source configuration")
}

trait GmailApi {
    fn list_messages(&self, params: &Value) -> Result<Vec<Value>>;
    fn message(&self, id: &str) -> Result<Value>;
    fn thread(&self, id: &str) -> Result<Vec<Value>>;
}

struct GwsGmailApi;

impl GmailApi for GwsGmailApi {
    fn list_messages(&self, params: &Value) -> Result<Vec<Value>> {
        gws::gws_paginate(&["gmail", "users", "messages", "list"], params, "messages")
    }

    fn message(&self, id: &str) -> Result<Value> {
        call_gws(
            &["gmail", "users", "messages", "get"],
            &json!({ "userId": "me", "id": id, "format": "full" }),
        )
    }

    fn thread(&self, id: &str) -> Result<Vec<Value>> {
        let response = call_gws(
            &["gmail", "users", "threads", "get"],
            &json!({ "userId": "me", "id": id, "format": "full" }),
        )?;
        Ok(response
            .get("messages")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, Clone::clone))
    }
}

fn call_gws(call: &[&str], params: &Value) -> Result<Value> {
    let params = serde_json::to_string(params).context("serializing Gmail request parameters")?;
    let mut args = Vec::with_capacity(call.len() + 2);
    args.extend_from_slice(call);
    args.extend(["--params", &params]);
    gws::gws(&args)
}

fn emit_fetch_hello(
    request: &FetchRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: vec!["me".to_owned()],
    })
}

fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: Vec::new(),
    })
}

fn fetch_with_api<C: GmailApi + ?Sized>(
    request: &FetchRequest,
    config: &GmailConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let result = window_start(
        request.cursors.get("me").map(String::as_str),
        request.window_end,
        request.backfill_hours,
    )
    .and_then(|start| fetch_messages(api, start, request.window_end, &config.query))
    .and_then(|messages| {
        messages
            .into_iter()
            .map(|message| record_from_message_with_scheme(&message, &request.source.scheme))
            .collect::<Result<Vec<_>>>()
    });

    match result {
        Ok(records) => {
            for record in records {
                sink(Message::Record(Box::new(record)))?;
            }
            sink(Message::StreamEnd {
                stream: "me".to_owned(),
                status: StreamStatus::Ok,
                cursor: Some(
                    request
                        .window_end
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
                error: None,
            })
        }
        Err(error) => sink(Message::StreamEnd {
            stream: "me".to_owned(),
            status: StreamStatus::Error,
            cursor: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

fn fetch_messages<C: GmailApi + ?Sized>(
    api: &C,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    extra_query: &str,
) -> Result<Vec<Value>> {
    let day_floor = (start - Duration::days(1)).format("%Y/%m/%d");
    let day_ceiling = (end + Duration::days(1)).format("%Y/%m/%d");
    let mut query = format!("after:{day_floor} before:{day_ceiling}");
    if !extra_query.trim().is_empty() {
        query.push(' ');
        query.push_str(extra_query.trim());
    }
    let candidates = api.list_messages(&json!({
        "userId": "me",
        "q": query,
        "maxResults": 500,
    }))?;
    let start_millis = start.timestamp_millis();
    let end_millis = end.timestamp_millis();
    let mut messages = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let id = candidate
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Gmail candidate is missing id"))?;
        let message = api.message(id)?;
        let millis = message_millis(&message)?;
        if millis > start_millis && millis <= end_millis {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn resolve_with_api<C: GmailApi + ?Sized>(
    request: &ResolveRequest,
    _config: &GmailConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut records = Vec::new();
    for pointer in &request.pointers {
        let Some(locator) = pointer
            .strip_prefix(&request.source.scheme)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let messages = if let Some(id) = locator.strip_prefix("thread/") {
            if id.is_empty() {
                continue;
            }
            api.thread(id)?
        } else {
            if locator.is_empty() {
                continue;
            }
            vec![api.message(locator)?]
        };
        for message in messages {
            records.push(record_from_message_with_scheme(
                &message,
                &request.source.scheme,
            )?);
        }
    }
    records.sort_by(|left, right| {
        left.ts
            .cmp(&right.ts)
            .then_with(|| left.pointer.cmp(&right.pointer))
    });
    for record in records {
        sink(Message::Record(Box::new(record)))?;
    }
    Ok(())
}

/// Converts one Gmail API message into a normalized source record.
///
/// # Errors
///
/// Returns an error when the message is missing a required identifier or timestamp.
pub fn record_from_message(message: &Value) -> Result<Record> {
    record_from_message_with_scheme(message, "gmail")
}

fn record_from_message_with_scheme(message: &Value, scheme: &str) -> Result<Record> {
    let id = required_string(message, "id", "Gmail message")?;
    let thread_id = required_string(message, "threadId", "Gmail message")?;
    let headers = gmail_headers(message);
    let author = headers.get("From").cloned();
    let title = headers.get("Subject").cloned();
    Ok(Record {
        pointer: format!("{scheme}:{id}"),
        stream: "me".to_owned(),
        ts: message_timestamp(message)?,
        label: Some("gmail".to_owned()),
        author,
        author_id: None,
        thread: Some(format!("{scheme}:thread/{thread_id}")),
        title,
        url: None,
        text: render_gmail(message),
        aliases: vec![id.to_owned(), thread_id.to_owned()],
        context: false,
        raw: message.clone(),
    })
}

fn required_string<'a>(message: &'a Value, field: &str, label: &str) -> Result<&'a str> {
    message
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{label} is missing {field}"))
}

fn message_timestamp(message: &Value) -> Result<DateTime<Utc>> {
    let millis = message_millis(message)?;
    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| anyhow!("Gmail message internalDate {millis:?} is invalid"))
}

fn message_millis(message: &Value) -> Result<i64> {
    let value = message
        .get("internalDate")
        .ok_or_else(|| anyhow!("Gmail message is missing internalDate"))?;
    match value {
        Value::String(value) => value
            .parse::<i64>()
            .with_context(|| format!("parsing Gmail internalDate {value:?}")),
        Value::Number(value) => value
            .as_i64()
            .ok_or_else(|| anyhow!("Gmail message internalDate is not an integer")),
        _ => bail!("Gmail message internalDate is not an integer"),
    }
}

fn gmail_headers(message: &Value) -> BTreeMap<String, String> {
    message
        .get("payload")
        .and_then(|payload| payload.get("headers"))
        .and_then(Value::as_array)
        .map_or_else(BTreeMap::new, |headers| {
            headers
                .iter()
                .filter_map(|header| {
                    let name = header.get("name").and_then(Value::as_str)?;
                    let value = header.get("value").and_then(Value::as_str)?;
                    Some((name.to_owned(), value.to_owned()))
                })
                .collect()
        })
}

fn base64url_decode(data: &str) -> Vec<u8> {
    fn value(character: u8) -> Option<u32> {
        match character {
            b'A'..=b'Z' => Some(u32::from(character - b'A')),
            b'a'..=b'z' => Some(u32::from(character - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(character - b'0') + 52),
            b'-' | b'+' => Some(62),
            b'_' | b'/' => Some(63),
            _ => None,
        }
    }

    let mut output = Vec::with_capacity(data.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for character in data.bytes() {
        if matches!(character, b'=' | b'\n' | b'\r') {
            continue;
        }
        let Some(value) = value(character) else {
            return Vec::new();
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the value is masked to one byte"
            )]
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    output
}

fn gmail_body(message: &Value) -> String {
    fn find_part<'a>(part: &'a Value, wanted: &str, found: &mut Option<&'a str>) {
        if found.is_some() {
            return;
        }
        if part.get("mimeType").and_then(Value::as_str) == Some(wanted) {
            if let Some(data) = part
                .get("body")
                .and_then(|body| body.get("data"))
                .and_then(Value::as_str)
            {
                *found = Some(data);
                return;
            }
        }
        if let Some(parts) = part.get("parts").and_then(Value::as_array) {
            for child in parts {
                find_part(child, wanted, found);
            }
        }
    }

    let Some(payload) = message.get("payload") else {
        return String::new();
    };
    let mut plain = None;
    find_part(payload, "text/plain", &mut plain);
    if let Some(data) = plain {
        return String::from_utf8_lossy(&base64url_decode(data)).into_owned();
    }
    let mut html = None;
    find_part(payload, "text/html", &mut html);
    html.map_or_else(String::new, |data| {
        let bytes = base64url_decode(data);
        let decoded = String::from_utf8_lossy(&bytes);
        strip_tags(&decoded)
    })
}

fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    html_unescape(&text)
}

fn render_gmail(message: &Value) -> String {
    let headers = gmail_headers(message);
    let mut lines = vec![
        format!(
            "Message-Id: {}",
            message
                .get("id")
                .and_then(Value::as_str)
                .map_or("?", |id| id)
        ),
        format!(
            "Thread-Id: {}",
            message
                .get("threadId")
                .and_then(Value::as_str)
                .map_or("?", |thread_id| thread_id)
        ),
    ];
    for name in ["From", "To", "Cc", "Subject", "Date"] {
        if let Some(value) = headers.get(name) {
            lines.push(format!("{name}: {value}"));
        }
    }
    lines.push(String::new());
    lines.push(gmail_body(message));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct FakeGmailApi {
        calls: RefCell<Vec<String>>,
        candidates: Vec<Value>,
        messages: BTreeMap<String, Value>,
        threads: BTreeMap<String, Vec<Value>>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(&self, _params: &Value) -> Result<Vec<Value>> {
            Ok(self.candidates.clone())
        }

        fn message(&self, id: &str) -> Result<Value> {
            self.calls.borrow_mut().push(format!("messages get {id}"));
            self.messages
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("missing fake Gmail message {id}"))
        }

        fn thread(&self, id: &str) -> Result<Vec<Value>> {
            self.calls.borrow_mut().push(format!("threads get {id}"));
            Ok(self.threads.get(id).cloned().unwrap_or_default())
        }
    }

    fn message(id: &str, internal_date: &str, body: &str) -> Value {
        json!({
            "id": id,
            "threadId": "cccccccccccccccc",
            "internalDate": internal_date,
            "payload": {
                "headers": [{"name": "Subject", "value": id}],
                "mimeType": "text/plain",
                "body": {"data": body}
            }
        })
    }

    #[test]
    fn resolving_a_thread_uses_the_threads_endpoint_and_sorts_messages() -> Result<()> {
        let api = FakeGmailApi {
            threads: BTreeMap::from([(
                "cccccccccccccccc".to_owned(),
                vec![
                    message("bbbbbbbbbbbbbbbb", "1792947600000", "c2Vjb25k"),
                    message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
                ],
            )]),
            ..FakeGmailApi::default()
        };
        let request = ResolveRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "gmail".to_owned(),
                kind: "gmail".to_owned(),
                scheme: "gmail".to_owned(),
            },
            config: json!({}),
            pointers: vec!["gmail:thread/cccccccccccccccc".to_owned()],
            state_dir: std::env::temp_dir(),
        };
        let config = parse_config(&request.config)?;
        let mut output = Vec::new();
        resolve_with_api(&request, &config, &api, &mut |message| {
            output.push(message);
            Ok(())
        })?;

        let pointers = output
            .iter()
            .filter_map(|message| match message {
                Message::Record(record) => Some(record.pointer.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(api.calls.into_inner(), vec!["threads get cccccccccccccccc"]);
        assert_eq!(
            pointers,
            vec!["gmail:aaaaaaaaaaaaaaaa", "gmail:bbbbbbbbbbbbbbbb"]
        );
        Ok(())
    }

    #[test]
    fn fetch_rejects_a_message_without_a_timestamp() -> Result<()> {
        let api = FakeGmailApi {
            candidates: vec![json!({"id": "aaaaaaaaaaaaaaaa"})],
            messages: BTreeMap::from([(
                "aaaaaaaaaaaaaaaa".to_owned(),
                json!({"id": "aaaaaaaaaaaaaaaa", "threadId": "cccccccccccccccc"}),
            )]),
            ..FakeGmailApi::default()
        };
        let start = DateTime::parse_from_rfc3339("2026-10-25T17:00:00Z")?.with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc);
        let error = fetch_messages(&api, start, end, "")
            .err()
            .context("Gmail fetch accepted a message without internalDate")?;

        assert!(format!("{error:#}").contains("internalDate"));
        Ok(())
    }

    #[test]
    fn html_body_is_used_when_plain_text_is_absent() -> Result<()> {
        let record = record_from_message(&json!({
            "id": "aaaaaaaaaaaaaaaa",
            "threadId": "cccccccccccccccc",
            "internalDate": "1792944000000",
            "payload": {
                "mimeType": "text/html",
                "body": {"data": "PHA-SGVsbG8gJmFtcDsgZ29vZGJ5ZTwvcD4"}
            }
        }))?;

        assert!(record.text.contains("Hello & goodbye"));
        Ok(())
    }
}
