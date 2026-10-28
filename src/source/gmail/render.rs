//! Gmail MIME decoding and message-to-record rendering.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

use crate::api::slack::html_unescape;
use crate::record::Record;

use super::STREAM;

/// Converts one Gmail API message into a normalized source record.
///
/// # Errors
///
/// Returns an error when the message is missing a required identifier or timestamp.
pub fn record_from_message(message: &Value) -> Result<Record> {
    record_from_message_with_scheme(message, "gmail")
}

pub(super) fn record_from_message_with_scheme(message: &Value, scheme: &str) -> Result<Record> {
    let id = required_string(message, "id", "Gmail message")?;
    let thread_id = required_string(message, "threadId", "Gmail message")?;
    let headers = gmail_headers(message);
    let author = headers.get("From").cloned();
    let title = headers.get("Subject").cloned();
    Ok(Record {
        pointer: format!("{scheme}:{id}"),
        stream: STREAM.to_owned(),
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

pub(super) fn required_string<'a>(message: &'a Value, field: &str, label: &str) -> Result<&'a str> {
    message
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{label} is missing {field}"))
}

pub(super) fn message_timestamp(message: &Value) -> Result<DateTime<Utc>> {
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
    part_headers(message.get("payload"))
}

fn part_headers(part: Option<&Value>) -> BTreeMap<String, String> {
    part.and_then(|part| part.get("headers"))
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

/// The character set a part declares, from its own `Content-Type` header.
fn part_charset(part: &Value) -> Option<String> {
    let headers = part_headers(Some(part));
    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())?;
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Decodes part bytes with the declared character set, falling back to UTF-8.
fn decode_part(bytes: &[u8], charset: Option<&str>) -> String {
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

fn gmail_body(message: &Value) -> String {
    fn find_part<'a>(part: &'a Value, wanted: &str, found: &mut Option<&'a Value>) {
        if found.is_some() {
            return;
        }
        if part.get("mimeType").and_then(Value::as_str) == Some(wanted)
            && part.pointer("/body/data").and_then(Value::as_str).is_some()
        {
            *found = Some(part);
            return;
        }
        if let Some(parts) = part.get("parts").and_then(Value::as_array) {
            for child in parts {
                find_part(child, wanted, found);
            }
        }
    }

    fn decoded(part: &Value) -> String {
        let data = part
            .pointer("/body/data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        decode_part(&base64url_decode(data), part_charset(part).as_deref())
    }

    let Some(payload) = message.get("payload") else {
        return String::new();
    };
    let mut plain = None;
    find_part(payload, "text/plain", &mut plain);
    if let Some(part) = plain {
        return decoded(part);
    }
    let mut html = None;
    find_part(payload, "text/html", &mut html);
    html.map_or_else(String::new, |part| strip_tags(&decoded(part)))
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
