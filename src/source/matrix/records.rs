//! A Matrix event as a record: its pointer, its text, and what an edit or a redaction makes of
//! the message it names.
//!
//! A record's pointer is `matrix:<room id>/<event id>`, both ids as the homeserver writes them
//! (`matrix:!room:example.org/$event`). A room id never holds `/`, so the first `/` after the
//! scheme ends it and the event id is everything after.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::matrix::encode;
use crate::pointer::{is_matrix_event_byte, is_matrix_room_byte};
use crate::record::Record;

/// The longest room or event id the client-server API allows.
const MAX_ID_BYTES: usize = 255;

/// The most characters of a room or display name a record carries.
const MAX_NAME_CHARS: usize = 256;

/// The event types a fetch reads: a message, a sticker, a redaction, and an encrypted event,
/// which it counts but cannot read. Every other type — a state change such as a join or a new
/// room name, a reaction, a receipt — is not a message and is never asked for.
pub(super) const MESSAGE_TYPES: [&str; 4] = [
    "m.room.message",
    "m.sticker",
    "m.room.redaction",
    "m.room.encrypted",
];

/// The text of a message its sender or a moderator removed.
pub(super) const REMOVED: &str = "[message removed]";

/// What one timeline event is to the source.
#[derive(Debug, PartialEq)]
pub(super) enum Kind<'e> {
    /// A message or a sticker, recorded as its own text.
    Message,
    /// A message the homeserver serves redacted: its content is gone.
    Removed,
    /// An edit of `target`, whose new content is `content` (`m.new_content`).
    Edit { target: &'e str, content: &'e Value },
    /// A redaction of `target`.
    Redaction { target: &'e str },
    /// An encrypted event, which ctx holds no keys for.
    Encrypted,
    /// Anything else, such as a message with no body.
    Other,
}

/// What `event` is to the source.
#[must_use]
pub(super) fn kind(event: &Value) -> Kind<'_> {
    let content = event.get("content");
    match event.get("type").and_then(Value::as_str) {
        Some("m.room.redaction") => event
            .get("redacts")
            .or_else(|| content.and_then(|content| content.get("redacts")))
            .and_then(Value::as_str)
            .filter(|target| is_event_id(target))
            .map_or(Kind::Other, |target| Kind::Redaction { target }),
        Some("m.room.encrypted") => Kind::Encrypted,
        Some("m.room.message" | "m.sticker") => {
            if event.pointer("/unsigned/redacted_because").is_some()
                || content
                    .and_then(Value::as_object)
                    .is_none_or(serde_json::Map::is_empty)
            {
                return Kind::Removed;
            }
            let relation = content.and_then(|content| content.get("m.relates_to"));
            if relation
                .and_then(|relation| relation.get("rel_type"))
                .and_then(Value::as_str)
                == Some("m.replace")
            {
                let target = relation
                    .and_then(|relation| relation.get("event_id"))
                    .and_then(Value::as_str)
                    .filter(|target| is_event_id(target));
                let new = content.and_then(|content| content.get("m.new_content"));
                return match (target, new) {
                    (Some(target), Some(content)) if content.is_object() => {
                        Kind::Edit { target, content }
                    }
                    _ => Kind::Other,
                };
            }
            if content.and_then(|content| text(content, event)).is_some() {
                Kind::Message
            } else {
                Kind::Other
            }
        }
        _ => Kind::Other,
    }
}

/// The event's id.
#[must_use]
pub(super) fn event_id(event: &Value) -> Option<&str> {
    event.get("event_id").and_then(Value::as_str)
}

/// The event's sender, a Matrix user id.
#[must_use]
pub(super) fn sender(event: &Value) -> Option<&str> {
    event.get("sender").and_then(Value::as_str)
}

/// When the homeserver says the event was sent (`origin_server_ts`, in milliseconds). A bridge
/// sets it to the time the message was sent on the network it bridges.
#[must_use]
pub(super) fn sent_at(event: &Value) -> Option<DateTime<Utc>> {
    event
        .get("origin_server_ts")
        .and_then(Value::as_i64)
        .and_then(DateTime::from_timestamp_millis)
}

/// The root event of the thread a message is in, when its relation is `m.thread` and names an
/// event id.
#[must_use]
pub(super) fn thread_root(event: &Value) -> Option<&str> {
    let relation = event.pointer("/content/m.relates_to")?;
    (relation.get("rel_type").and_then(Value::as_str) == Some("m.thread"))
        .then(|| relation.get("event_id").and_then(Value::as_str))
        .flatten()
        .filter(|root| is_event_id(root))
}

/// Whether `id` is an event id a record may name: a `$`, then at most 254 bytes the pointer
/// parser reads as an event id's, not ending in the `.` or `:` it leaves to the prose after a
/// pointer. A relation names its target in the sender's own content, which no homeserver checks
/// for an event a federated server sent, so a target that is not one is no relation at all.
#[must_use]
pub(super) fn is_event_id(id: &str) -> bool {
    id.len() <= MAX_ID_BYTES
        && id.strip_prefix('$').is_some_and(|rest| {
            !rest.is_empty()
                && rest.bytes().all(is_matrix_event_byte)
                && !rest.ends_with(['.', ':'])
        })
}

/// Whether `id` is a room id a pointer can name: a `!`, then at most 254 bytes the pointer
/// parser reads as a room id's.
#[must_use]
pub(super) fn is_room_id(id: &str) -> bool {
    id.len() <= MAX_ID_BYTES
        && id
            .strip_prefix('!')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(is_matrix_room_byte))
}

/// `text` as one line of at most `max_chars` characters: every control character and run of
/// white space becomes one space, and a longer text is cut, ending in `…`. Anyone can set a
/// room's name or their own display name, and both reach the lines the ingest cycle reads.
#[must_use]
pub(super) fn one_line(text: &str, max_chars: usize) -> String {
    let spaced: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let joined = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max_chars {
        return joined;
    }
    let mut cut: String = joined.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

/// How a record names a message's sender: their display name with their Matrix id beside it,
/// `Ada (@ada:example.org)`, so a name borrowed from someone else still shows whose account
/// sent the message; the Matrix id alone when the room holds no name for them.
#[must_use]
pub(super) fn author_line(name: Option<&str>, user: &str) -> String {
    let user = one_line(user, MAX_ID_BYTES);
    match name.map(|name| one_line(name, MAX_NAME_CHARS)) {
        Some(name) if !name.is_empty() && name != user => format!("{name} ({user})"),
        _ => user,
    }
}

/// `matrix:<room>/<event>`, with the configured scheme.
#[must_use]
pub(super) fn pointer(scheme: &str, room: &str, event: &str) -> String {
    format!("{scheme}:{room}/{event}")
}

/// The room and event ids a pointer names: the room id runs from the `!` after the scheme to
/// the first `/`, and the event id, which starts with `$`, is the rest.
#[must_use]
pub(super) fn parse_pointer<'p>(pointer: &'p str, scheme: &str) -> Option<(&'p str, &'p str)> {
    let (room, event) = pointer
        .trim()
        .strip_prefix(scheme)?
        .strip_prefix(':')?
        .split_once('/')?;
    (room.len() > 1 && room.starts_with('!') && event.len() > 1 && event.starts_with('$'))
        .then_some((room, event))
}

/// A link a person can open the message at, in a Matrix client.
#[must_use]
pub(super) fn matrix_to(room: &str, event: &str) -> String {
    format!("https://matrix.to/#/{}/{}", encode(room), encode(event))
}

/// What a message's content reads as, or `None` when it has no body to show.
///
/// A text, a notice or an emote reads as its body, less the quote of the message it replies to
/// that a client writes in front of a reply (the rich reply fallback). An image, a video, an
/// audio clip, a file or a sticker reads as a placeholder naming the file, `[image: photo.jpg]`,
/// followed on the next line by its caption when it has one; the file is never downloaded.
/// `event` is the event the content belongs to, which says whether it is a sticker.
#[must_use]
pub(super) fn text(content: &Value, event: &Value) -> Option<String> {
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let media = if event.get("type").and_then(Value::as_str) == Some("m.sticker") {
        Some("sticker")
    } else {
        match content.get("msgtype").and_then(Value::as_str) {
            Some("m.image") => Some("image"),
            Some("m.video") => Some("video"),
            Some("m.audio") => Some("audio"),
            Some("m.file") => Some("file"),
            _ => None,
        }
    };
    if let Some(media) = media {
        let filename = content
            .get("filename")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty());
        let name = filename.unwrap_or(body).trim();
        let caption = filename
            .is_some_and(|filename| !body.trim().is_empty() && body != filename)
            .then_some(body);
        let mut placeholder = if name.is_empty() {
            format!("[{media}]")
        } else {
            format!("[{media}: {name}]")
        };
        if let Some(caption) = caption {
            placeholder.push('\n');
            placeholder.push_str(strip_reply_fallback(content, caption));
        }
        return Some(placeholder);
    }
    if !content.get("body").is_some_and(Value::is_string) {
        return None;
    }
    Some(strip_reply_fallback(content, body).to_owned())
}

/// `body` without the rich reply fallback: the lines starting with `> ` that a client puts in
/// front of a reply's own text, quoting the message it replies to, and the empty line after
/// them. Only a reply (`m.relates_to.m.in_reply_to`) has one, its first line names the quoted
/// sender (`> <@alice:example.org> …`, or `> * <@alice:example.org> …` for an emote), and a
/// client following Matrix 1.13 or later sends none, so any other body is kept whole.
fn strip_reply_fallback<'b>(content: &Value, body: &'b str) -> &'b str {
    if content.pointer("/m.relates_to/m.in_reply_to").is_none()
        || !(body.starts_with("> <") || body.starts_with("> * <"))
    {
        return body;
    }
    let mut rest = body;
    while rest.starts_with("> ") || rest == ">" || rest.starts_with(">\n") {
        rest = rest.split_once('\n').map_or("", |(_, after)| after);
    }
    rest.strip_prefix('\n').unwrap_or(rest)
}

/// Where a record lives: the scheme, the room, and the room's label.
pub(super) struct Room<'a> {
    pub(super) scheme: &'a str,
    pub(super) id: &'a str,
    /// The room's name, else its alias, else its id; a record carries it as [`one_line`].
    pub(super) label: &'a str,
}

impl Room<'_> {
    /// The record of `message` (an event whose [`kind`] is a message), whose text is `content`
    /// read as [`text`], dated `ts` and written by `author` ([`author_line`]). `raw` is the event
    /// its text comes from: the message itself, or the edit that last changed it.
    pub(super) fn record(
        &self,
        message: &Value,
        content: &Value,
        raw: &Value,
        author: &str,
        ts: DateTime<Utc>,
    ) -> Option<Record> {
        let text = text(content, message)?;
        Some(self.build(message, text, raw, author, ts))
    }

    /// The record of a removed message: its text says it was removed, and `raw` is the event
    /// that says so (the redaction, or the message as the homeserver serves it redacted).
    pub(super) fn removed(
        &self,
        message: &Value,
        raw: &Value,
        author: &str,
        ts: DateTime<Utc>,
    ) -> Option<Record> {
        event_id(message)?;
        Some(self.build(message, REMOVED.to_owned(), raw, author, ts))
    }

    fn build(
        &self,
        message: &Value,
        text: String,
        raw: &Value,
        author: &str,
        ts: DateTime<Utc>,
    ) -> Record {
        let event = event_id(message).unwrap_or_default();
        Record {
            pointer: pointer(self.scheme, self.id, event),
            stream: self.id.to_owned(),
            ts,
            label: Some(one_line(self.label, MAX_NAME_CHARS)),
            author: Some(one_line(author, MAX_NAME_CHARS + MAX_ID_BYTES + 3)),
            author_id: sender(message).map(str::to_owned),
            thread: thread_root(message).map(|root| pointer(self.scheme, self.id, root)),
            title: None,
            url: Some(matrix_to(self.id, event)),
            text,
            aliases: Vec::new(),
            context: false,
            raw: raw.clone(),
        }
    }
}
