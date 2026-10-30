//! Slack message-to-record rendering and timestamp parsing.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::slack::html_unescape;
use crate::record::Record;

/// Converts one Slack API message into a normalized source record.
///
/// `names` maps Slack ids to the names Slack gives them: a person's display name, for the
/// message's author and each person its text mentions, and a channel's name, for each channel
/// it mentions without one. A mention whose id `names` does not hold keeps the label Slack
/// wrote, or else the id; `raw` keeps the message as Slack sent it, ids and all.
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

/// Renders a message's text for a reader: each mention as the name of the person, group or
/// channel it names, typed text unescaped, and every other sequence, such as a link, as Slack
/// wrote it.
fn render_slack_text(text: &str, names: &BTreeMap<String, String>) -> String {
    let mut rendered = String::with_capacity(text.len());
    for part in parts(text) {
        match part {
            Part::Typed(typed) => rendered.push_str(&html_unescape(typed)),
            Part::Sequence(sequence) => {
                if let Some(mention) = Mention::parse(sequence) {
                    mention.render(names, &mut rendered);
                } else {
                    rendered.push('<');
                    rendered.push_str(&html_unescape(sequence));
                    rendered.push('>');
                }
            }
        }
    }
    rendered
}

/// One part of a message's text as Slack sends it.
enum Part<'a> {
    /// Text the writer typed, still escaped.
    Typed(&'a str),
    /// What lies between the `<` and `>` of a sequence Slack wrote: a mention or a link.
    Sequence(&'a str),
}

/// Splits a message's text into what its writer typed and the `<…>` sequences Slack writes
/// for mentions and links. Slack escapes a typed `<`, `>` and `&`, so each `<` opens a
/// sequence; one that never closes is typed text.
fn parts(text: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut remaining = text;
    while let Some((typed, after_open)) = remaining.split_once('<') {
        let Some((sequence, after_close)) = after_open.split_once('>') else {
            break;
        };
        if !typed.is_empty() {
            parts.push(Part::Typed(typed));
        }
        parts.push(Part::Sequence(sequence));
        remaining = after_close;
    }
    if !remaining.is_empty() {
        parts.push(Part::Typed(remaining));
    }
    parts
}

/// The mentions in a message's text, in order.
pub(super) fn mentions(text: &str) -> impl Iterator<Item = Mention<'_>> {
    parts(text).into_iter().filter_map(|part| match part {
        Part::Sequence(sequence) => Mention::parse(sequence),
        Part::Typed(_) => None,
    })
}

/// A mention Slack writes into a message's text. A label is still escaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mention<'a> {
    /// `<@U…>` or `<@U…|label>`: a person.
    User { id: &'a str, label: Option<&'a str> },
    /// `<#C…>` or `<#C…|name>`: a channel. Slack may leave the name empty.
    Channel { id: &'a str, label: Option<&'a str> },
    /// `<!subteam^S…>` or `<!subteam^S…|@handle>`: a user group.
    Group { id: &'a str, label: Option<&'a str> },
    /// `<!here>`, `<!channel>` or `<!everyone>`, with or without a label.
    Everyone(&'a str),
}

impl<'a> Mention<'a> {
    fn parse(sequence: &'a str) -> Option<Self> {
        let (target, label) = sequence
            .split_once('|')
            .map_or((sequence, None), |(target, label)| {
                (target, Some(label).filter(|label| !label.is_empty()))
            });
        let id = |prefix: &str| target.strip_prefix(prefix).filter(|id| !id.is_empty());
        if let Some(id) = id("@") {
            return Some(Self::User { id, label });
        }
        if let Some(id) = id("#") {
            return Some(Self::Channel { id, label });
        }
        if let Some(id) = id("!subteam^") {
            return Some(Self::Group { id, label });
        }
        match target {
            "!here" | "!channel" | "!everyone" => target.strip_prefix('!').map(Self::Everyone),
            _ => None,
        }
    }

    /// Writes the mention as a reader sees it: `@` and a person's name, as the author line
    /// gives it, or a group's handle, and `#` and a channel's name. A person is named by
    /// `names` first, since a label is the name Slack had when the message was sent; a channel
    /// by its label first, since Slack is asked only about a channel it left unnamed. A
    /// mention neither names keeps its id.
    fn render(self, names: &BTreeMap<String, String>, rendered: &mut String) {
        match self {
            Self::User { id, label } => {
                rendered.push('@');
                match names.get(id) {
                    Some(name) => rendered.push_str(name),
                    None => push_label(rendered, '@', label, id),
                }
            }
            Self::Channel { id, label } => {
                rendered.push('#');
                match (label, names.get(id)) {
                    (None, Some(name)) => rendered.push_str(name),
                    _ => push_label(rendered, '#', label, id),
                }
            }
            Self::Group { id, label } => {
                rendered.push('@');
                push_label(rendered, '@', label, id);
            }
            Self::Everyone(name) => {
                rendered.push('@');
                rendered.push_str(name);
            }
        }
    }
}

/// Writes the label Slack wrote, unescaped and less the `sigil` already written before it
/// (a group's handle carries its own `@`), or else the id.
fn push_label(rendered: &mut String, sigil: char, label: Option<&str>, id: &str) {
    match label {
        Some(label) => {
            rendered.push_str(&html_unescape(label.strip_prefix(sigil).unwrap_or(label)));
        }
        None => rendered.push_str(id),
    }
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
