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
pub(super) fn render_slack_text(text: &str, names: &BTreeMap<String, String>) -> String {
    let mut rendered = String::with_capacity(text.len());
    for piece in pieces(text) {
        match piece {
            Piece::Fixed { current, .. } => rendered.push_str(&current),
            Piece::Named { mention, .. } => mention.render(names, &mut rendered),
        }
    }
    rendered
}

/// The text to serve for a message Slack sent as `text` and a fetch stored as `stored`, when
/// it is not the stored text: each person the stored text still shows as their bare id named
/// by `name`, which gives the name the store holds for a Slack id, and the rest as
/// [`render_slack_text`] renders it.
///
/// A served text never names anyone the stored text does not point at, and never shows as an
/// id a mention the stored text names:
///
/// - `stored` must read as `text` mention by mention, as this renderer or an earlier one wrote
///   it ([`align`]). When it does not, the stored text is served.
/// - Only a person the stored text shows as their id, `@U…`, or as an earlier renderer wrote a
///   labelled one, `@U…|label`, changes: to the name `name` gives that id, else the label, as a
///   fetch writes it now. The id the stored text shows is the one it is named for.
/// - Every other mention keeps what the stored text shows. A record that shows each person by
///   a name or a label is served as stored: its `raw` is the message as first fetched, which a
///   later edit that rendered alike, such as to another person of the same display name, did
///   not replace.
///
/// # Errors
///
/// Returns an error when `name` does.
pub(super) fn served_text(
    text: &str,
    stored: &str,
    name: &mut dyn FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<String>> {
    let pieces = pieces(text);
    let Some(shown) = align(&pieces, stored) else {
        return Ok(None);
    };
    let unnamed = BTreeMap::new();
    let mut served = String::with_capacity(stored.len());
    let mut renamed = false;
    for (piece, shown) in pieces.iter().zip(shown) {
        match piece {
            Piece::Fixed { current, .. } => served.push_str(current),
            Piece::Named {
                mention: mention @ Mention::User { id, .. },
                ..
            } if shows_id(*mention, shown) => {
                let id: &str = id;
                let names = name(id)?
                    .map(|known| BTreeMap::from([(id.to_owned(), known)]))
                    .unwrap_or_default();
                let start = served.len();
                mention.render(&names, &mut served);
                renamed |= served.get(start..) != Some(shown);
            }
            Piece::Named {
                mention,
                legacy: Some(legacy),
            } if shown == legacy => mention.render(&unnamed, &mut served),
            Piece::Named { .. } => served.push_str(shown),
        }
    }
    Ok((renamed && served != stored).then_some(served))
}

/// Whether `shown`, what a stored text shows for `mention`, is a person's bare id: `@U…`, or
/// `@U…|label` as an earlier renderer wrote a labelled mention.
fn shows_id(mention: Mention<'_>, shown: &str) -> bool {
    let Mention::User { id, label } = mention else {
        return false;
    };
    let Some(after_id) = shown
        .strip_prefix('@')
        .and_then(|name| name.strip_prefix(id))
    else {
        return false;
    };
    after_id.is_empty()
        || label.is_some_and(|label| after_id.strip_prefix('|') == Some(&html_unescape(label)))
}

/// One part of a Slack message's text as a quote may have read it.
pub(super) enum Spelling<'a> {
    /// Text every rendering of the message writes alike.
    Text(String),
    /// A mention, and each way a reader may have seen it written: as Slack sends it, as a
    /// fetch renders it now or rendered it before, and as the stored text shows it. `person`
    /// is the Slack id of the person it mentions, whose other names may stand for it too.
    Mention {
        person: Option<&'a str>,
        spellings: Vec<String>,
    },
}

/// A message Slack sent as `text`, and a fetch stored as `stored` when that is known, in the
/// parts a quote of it is checked against: each mention spelled each way a reader may have
/// seen it, and the rest as a fetch renders it.
pub(super) fn spellings<'t>(text: &'t str, stored: Option<&str>) -> Vec<Spelling<'t>> {
    let pieces = pieces(text);
    let shown = stored.and_then(|stored| align(&pieces, stored));
    let unnamed = BTreeMap::new();
    pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let shown = shown
                .as_ref()
                .and_then(|shown| shown.get(index))
                .map(|shown| (*shown).to_owned());
            match piece {
                Piece::Fixed {
                    current,
                    legacy: None,
                } => Spelling::Text(current.clone()),
                Piece::Fixed {
                    current,
                    legacy: Some(legacy),
                } => Spelling::Mention {
                    person: None,
                    spellings: vec![current.clone(), legacy.clone()],
                },
                Piece::Named { mention, legacy } => {
                    // As a fetch renders it when it learns no name: the label, else the id.
                    let mut unnamed_text = String::new();
                    mention.render(&unnamed, &mut unnamed_text);
                    let mut spellings = vec![unnamed_text];
                    spellings.extend(legacy.clone());
                    let person = match *mention {
                        Mention::User { id, label } => {
                            spellings.push(format!("<@{id}>"));
                            if let Some(label) = label {
                                spellings.push(format!("@{id}|{}", html_unescape(label)));
                                spellings.push(format!("<@{id}|{label}>"));
                                spellings.push(format!("@{id}"));
                            }
                            Some(id)
                        }
                        Mention::Channel { .. } | Mention::Group { .. } | Mention::Everyone(_) => {
                            None
                        }
                    };
                    spellings.extend(shown);
                    Spelling::Mention { person, spellings }
                }
            }
        })
        .collect()
}

/// One piece of a message's text as a reader sees it.
enum Piece<'a> {
    /// What a rendering writes whatever it could learn: typed text, a link, a group or
    /// broadcast mention, and a channel mention Slack wrote the name into. `legacy` is how
    /// an earlier renderer wrote it, when that differs: such a mention as Slack sent it.
    Fixed {
        current: String,
        legacy: Option<String>,
    },
    /// A mention whose name depends on what the renderer could learn: a person, or a channel
    /// Slack left unnamed. `legacy` is how an earlier renderer wrote it, when that is not a
    /// sigil and a name: an unnamed channel as Slack sent it.
    Named {
        mention: Mention<'a>,
        legacy: Option<String>,
    },
}

/// A message's text, as Slack sends it, in the pieces a reader sees.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let unnamed = BTreeMap::new();
    parts(text)
        .into_iter()
        .map(|part| match part {
            Part::Typed(typed) => Piece::Fixed {
                current: html_unescape(typed),
                legacy: None,
            },
            Part::Sequence(sequence) => {
                // An earlier renderer unescaped the whole text and left every sequence but a
                // person's as Slack wrote it.
                let as_sent = format!("<{}>", html_unescape(sequence));
                match Mention::parse(sequence) {
                    Some(mention @ Mention::User { .. }) => Piece::Named {
                        mention,
                        legacy: None,
                    },
                    Some(mention) if mention.named().is_some() => Piece::Named {
                        mention,
                        legacy: Some(as_sent),
                    },
                    Some(mention) => {
                        let mut current = String::new();
                        mention.render(&unnamed, &mut current);
                        let legacy = (as_sent != current).then_some(as_sent);
                        Piece::Fixed { current, legacy }
                    }
                    None => Piece::Fixed {
                        current: as_sent,
                        legacy: None,
                    },
                }
            }
        })
        .collect()
}

/// How many ways of reading a stored text [`align`] tries before it gives up, so a message
/// written to make the search slow costs no more than this.
const ALIGNMENT_STEPS: usize = 10_000;

/// What `stored` shows for each of `pieces`, in order, when it reads as them piece by piece:
/// each fixed piece as a renderer writes it, and each named mention as its sigil and a name
/// that is never empty and never runs past its line, or as an earlier renderer wrote it.
/// `None` when it does not, or when finding out would take more than [`ALIGNMENT_STEPS`].
fn align<'s>(pieces: &[Piece<'_>], stored: &'s str) -> Option<Vec<&'s str>> {
    let mut shown = Vec::with_capacity(pieces.len());
    let mut steps = ALIGNMENT_STEPS;
    align_from(pieces, stored, &mut shown, &mut steps).then_some(shown)
}

/// [`align`] from where `shown` has got to, keeping what each piece read as in `shown`.
fn align_from<'s>(
    pieces: &[Piece<'_>],
    stored: &'s str,
    shown: &mut Vec<&'s str>,
    steps: &mut usize,
) -> bool {
    let depth = shown.len();
    let mut remaining = stored;
    for (index, piece) in pieces.iter().enumerate() {
        let rest = pieces.get(index + 1..).unwrap_or_default();
        let held = match piece {
            Piece::Fixed { current, legacy } => {
                let mut fits = [Some(current), legacy.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter(|written| remaining.starts_with(written.as_str()))
                    .map(String::len);
                match (fits.next(), fits.next()) {
                    (None, _) => false,
                    (Some(end), None) => match remaining.split_at_checked(end) {
                        Some((taken, after)) => {
                            shown.push(taken);
                            remaining = after;
                            continue;
                        }
                        None => false,
                    },
                    (Some(first), Some(second)) => {
                        try_ends([first, second].into_iter(), rest, remaining, shown, steps)
                    }
                }
            }
            Piece::Named { mention, legacy } => {
                let as_sent = legacy
                    .as_deref()
                    .filter(|legacy| remaining.starts_with(legacy))
                    .map(str::len);
                let named = mention
                    .named()
                    .and_then(|(sigil, _)| {
                        remaining
                            .strip_prefix(sigil)
                            .map(|name| (sigil.len_utf8(), name))
                    })
                    .into_iter()
                    .flat_map(|(sigil, name)| {
                        let line_end = name.find('\n').unwrap_or(name.len());
                        name.char_indices()
                            .map(|(offset, _)| offset)
                            .skip(1)
                            .chain(std::iter::once(name.len()))
                            .take_while(move |end| *end <= line_end)
                            .map(move |end| sigil + end)
                    });
                try_ends(
                    as_sent.into_iter().chain(named),
                    rest,
                    remaining,
                    shown,
                    steps,
                )
            }
        };
        if !held {
            shown.truncate(depth);
        }
        return held;
    }
    let held = remaining.is_empty();
    if !held {
        shown.truncate(depth);
    }
    held
}

/// Tries each place `ends` names for the current piece to end in `remaining`, shortest first,
/// and aligns `rest` after it. Each place tried spends one of `steps`.
fn try_ends<'s>(
    ends: impl Iterator<Item = usize>,
    rest: &[Piece<'_>],
    remaining: &'s str,
    shown: &mut Vec<&'s str>,
    steps: &mut usize,
) -> bool {
    for end in ends {
        let Some(left) = steps.checked_sub(1) else {
            return false;
        };
        *steps = left;
        let Some((taken, after)) = remaining.split_at_checked(end) else {
            continue;
        };
        if !can_start(rest, after) {
            continue;
        }
        shown.push(taken);
        if align_from(rest, after, shown, steps) {
            return true;
        }
        shown.pop();
    }
    false
}

/// Whether `text` can begin what `pieces` read as: their first piece, or nothing at all.
fn can_start(pieces: &[Piece<'_>], text: &str) -> bool {
    match pieces.first() {
        None => text.is_empty(),
        Some(Piece::Fixed { current, legacy }) => {
            text.starts_with(current.as_str())
                || legacy
                    .as_deref()
                    .is_some_and(|legacy| text.starts_with(legacy))
        }
        Some(Piece::Named { mention, legacy }) => {
            mention
                .named()
                .is_some_and(|(sigil, _)| text.starts_with(sigil))
                || legacy
                    .as_deref()
                    .is_some_and(|legacy| text.starts_with(legacy))
        }
    }
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

    /// The sigil and id of a mention whose name depends on what a renderer could learn: a
    /// person, or a channel Slack left unnamed. `None` for every other mention, which each
    /// rendering writes alike.
    fn named(self) -> Option<(char, &'a str)> {
        match self {
            Self::User { id, .. } => Some(('@', id)),
            Self::Channel { id, label: None } => Some(('#', id)),
            Self::Channel { .. } | Self::Group { .. } | Self::Everyone(_) => None,
        }
    }

    /// Writes the mention as a reader sees it: `@` and the name `names` gives a person, or a
    /// group's handle, and `#` and a channel's name. A person is named by
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
