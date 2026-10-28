//! Slack message-to-record rendering and timestamp parsing.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::slack::html_unescape;
use crate::record::Record;

/// Converts one Slack API message into a normalized source record.
///
/// # Errors
///
/// Returns an error when the message does not contain a valid Slack timestamp.
pub fn record_from_message(
    message: &Value,
    channel: &str,
    label: &str,
    names: &BTreeMap<String, String>,
    workspace: &str,
) -> Result<Record> {
    let timestamp = message_timestamp(message)?;
    let sent_at = slack_timestamp(timestamp)?;
    let author_id = message
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let author = author_id
        .as_deref()
        .map(|id| names.get(id).map_or_else(|| id.to_owned(), Clone::clone));
    let thread_ts = message.get("thread_ts").and_then(Value::as_str);
    let thread = thread_ts
        .filter(|parent| *parent != timestamp)
        .map(|parent| format!("slack:{channel}/{parent}"));
    Ok(Record {
        pointer: format!("slack:{channel}/{timestamp}"),
        stream: channel.to_owned(),
        ts: sent_at,
        label: Some(label.to_owned()),
        author,
        author_id,
        thread,
        title: None,
        url: Some(permalink(workspace, channel, timestamp, thread_ts)),
        text: render_slack_text(
            message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            names,
        ),
        aliases: vec![timestamp.to_owned()],
        context: false,
        raw: message.clone(),
    })
}

pub(super) fn apply_scheme(record: &mut Record, scheme: &str) {
    if scheme == "slack" {
        return;
    }
    record.pointer = rescheme(&record.pointer, scheme);
    if let Some(thread) = record.thread.as_deref() {
        record.thread = Some(rescheme(thread, scheme));
    }
}

fn rescheme(pointer: &str, scheme: &str) -> String {
    pointer.strip_prefix("slack:").map_or_else(
        || pointer.to_owned(),
        |locator| format!("{scheme}:{locator}"),
    )
}

fn render_slack_text(text: &str, names: &BTreeMap<String, String>) -> String {
    let decoded = html_unescape(text)
        .replace("<!channel>", "@channel")
        .replace("<!here>", "@here");
    let mut rendered = String::with_capacity(decoded.len());
    let mut remaining = decoded.as_str();
    while let Some((prefix, after_marker)) = remaining.split_once("<@") {
        rendered.push_str(prefix);
        let Some((id, rest)) = after_marker.split_once('>') else {
            rendered.push_str("<@");
            rendered.push_str(after_marker);
            return rendered;
        };
        if id.is_empty() {
            rendered.push_str("<@>");
        } else {
            rendered.push('@');
            rendered.push_str(names.get(id).map_or(id, String::as_str));
        }
        remaining = rest;
    }
    rendered.push_str(remaining);
    rendered
}

fn permalink(workspace: &str, channel: &str, timestamp: &str, thread_ts: Option<&str>) -> String {
    let mut url = format!(
        "https://{workspace}.slack.com/archives/{channel}/p{}",
        timestamp.replace('.', "")
    );
    if let Some(parent) = thread_ts.filter(|parent| *parent != timestamp) {
        url.push_str("?thread_ts=");
        url.push_str(parent);
        url.push_str("&cci=1");
    }
    url
}

pub(super) fn message_timestamp(message: &Value) -> Result<&str> {
    let timestamp = message
        .get("ts")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Slack message is missing ts"))?;
    is_timestamp(timestamp)
        .then_some(timestamp)
        .ok_or_else(|| anyhow!("Slack message timestamp {timestamp:?} is invalid"))
}

pub(super) fn is_timestamp(timestamp: &str) -> bool {
    timestamp.len() == 17
        && timestamp.bytes().enumerate().all(|(index, byte)| {
            (index == 10 && byte == b'.') || (index != 10 && byte.is_ascii_digit())
        })
}

pub(super) fn slack_timestamp(timestamp: &str) -> Result<DateTime<Utc>> {
    if !is_timestamp(timestamp) {
        bail!("Slack timestamp {timestamp:?} is invalid");
    }
    let seconds = timestamp
        .get(..10)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?
        .parse::<i64>()
        .with_context(|| format!("parsing Slack timestamp {timestamp:?}"))?;
    let micros = timestamp
        .get(11..)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?
        .parse::<u32>()
        .with_context(|| format!("parsing Slack timestamp {timestamp:?}"))?;
    let nanos = micros
        .checked_mul(1_000)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?;
    DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))
}
