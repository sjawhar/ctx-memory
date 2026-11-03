//! A live resolve: the messages some pointers name, as the homeserver serves them now.
//!
//! Each pointer's event comes from `/event`, with its newest edit applied, and when it is in a
//! thread, the thread's root and replies come with it through `/relations`, each with its own
//! newest edit: a quote check refreshes a thread reply by resolving the thread's root
//! (`ctx cite verify --live`), so the thread is what keeps such a citation resolving.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::api::matrix::MatrixClient;
use crate::source::{Message, ResolveRequest};

use super::homeserver::{Reader, Stop};
use super::records::{
    author_line, event_id, is_event_id, kind, parse_pointer, sender, sent_at, thread_root, Kind,
    Room,
};
use super::rooms::{Decision, Facts, Selection};

/// How many pages of a thread's replies a resolve reads.
const THREAD_PAGES: u32 = 5;
/// How many pages of a message's edits a resolve reads to find the newest valid one.
const EDIT_PAGES: u32 = 1;

/// Resolves every pointer of `request`.
pub(super) fn run(
    request: &ResolveRequest,
    selection: &Selection,
    client: &MatrixClient,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let scheme = request.source.scheme.as_str();
    let mut reader = Reader::new(client, None);
    let mut known: BTreeMap<String, Facts> = BTreeMap::new();
    let mut names: Names = BTreeMap::new();
    for pointer in &request.pointers {
        let (room, id) = parse_pointer(pointer, scheme)
            .ok_or_else(|| anyhow!("{pointer} is not a Matrix message pointer"))?;
        refuse_unread(selection, room, None)?;
        // The room's name and alias come first, so a room excluded by either has no message
        // read at all.
        let facts = if let Some(facts) = known.get(room) {
            facts.clone()
        } else {
            let facts = need(reader.facts(room, selection.names_aliases())?)?;
            known.insert(room.to_owned(), facts.clone());
            facts
        };
        refuse_unread(selection, room, Some(&facts))?;
        let Some(event) = need(reader.event(room, id)?)? else {
            continue;
        };
        if !is_message(&event) {
            continue;
        }
        let label = facts.label(room).to_owned();
        let place = Room {
            scheme,
            id: room,
            label: &label,
        };
        for message in thread(&mut reader, room, id, event)? {
            if let Some(record) = record(&mut reader, &mut names, &place, &message)? {
                sink(Message::Record(Box::new(record)))?;
            }
        }
    }
    Ok(())
}

/// Refuses a room the configuration does not read: a resolve reads only what a fetch reads.
fn refuse_unread(selection: &Selection, room: &str, facts: Option<&Facts>) -> Result<()> {
    if selection.decide(room, facts) == Decision::Skip {
        bail!("{room} is not a room this source reads: its rooms or exclude_rooms leave it out");
    }
    Ok(())
}

/// A request's answer, or the error that the homeserver stopped answering.
fn need<T>(step: Result<T, Stop>) -> Result<T> {
    step.map_err(|stop| anyhow!("resolving stopped: {}", stop.why()))
}

fn is_message(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("m.room.message" | "m.sticker")
    )
}

/// `event` and, when it is in a thread, the thread's root and replies, oldest first.
fn thread(reader: &mut Reader<'_>, room: &str, wanted: &str, event: Value) -> Result<Vec<Value>> {
    let origin = thread_root(&event).unwrap_or(wanted).to_owned();
    let mut messages = Vec::new();
    if origin != wanted {
        if let Some(found) = need(reader.related_event(room, &origin)?)? {
            messages.push(found);
        }
    }
    messages.extend(need(reader.relations(
        room,
        &origin,
        "m.thread",
        THREAD_PAGES,
    )?)?);
    messages.push(event);
    let mut seen = BTreeSet::new();
    messages.retain(|message| event_id(message).is_some_and(|id| seen.insert(id.to_owned())));
    messages.sort_by(|left, right| {
        sent_at(left)
            .cmp(&sent_at(right))
            .then_with(|| event_id(left).cmp(&event_id(right)))
    });
    Ok(messages)
}

/// Each member's name, by room and Matrix id, as the run learned it.
type Names = BTreeMap<(String, String), String>;

/// The record of one message as it reads now: removed, or with its newest edit applied.
fn record(
    reader: &mut Reader<'_>,
    names: &mut Names,
    place: &Room<'_>,
    message: &Value,
) -> Result<Option<crate::record::Record>> {
    let Some(ts) = sent_at(message) else {
        return Ok(None);
    };
    let author = author(reader, names, place.id, message)?;
    match kind(message) {
        Kind::Removed => Ok(place.removed(message, message, &author, ts)),
        Kind::Message => {
            let edit = newest_edit(reader, place.id, message)?;
            let (content, raw) = match &edit {
                Some(edit) => (
                    edit.pointer("/content/m.new_content")
                        .unwrap_or(&Value::Null),
                    edit,
                ),
                None => (message.get("content").unwrap_or(&Value::Null), message),
            };
            Ok(place.record(message, content, raw, &author, ts))
        }
        Kind::Edit { .. } | Kind::Redaction { .. } | Kind::Encrypted | Kind::Other => Ok(None),
    }
}

/// The newest edit of `message` that Matrix's rules allow: by the message's own sender, with
/// new content. The homeserver may bundle it with the message; otherwise `/relations` lists
/// the message's edits, newest first.
fn newest_edit(reader: &mut Reader<'_>, room: &str, message: &Value) -> Result<Option<Value>> {
    let valid = |edit: &Value| {
        sender(edit) == sender(message)
            && edit
                .pointer("/content/m.new_content")
                .is_some_and(Value::is_object)
    };
    if let Some(bundled) = message.pointer("/unsigned/m.relations/m.replace") {
        let full = if bundled.get("content").is_some() {
            Some(bundled.clone())
        } else if let Some(id) = event_id(bundled).filter(|id| is_event_id(id)) {
            need(reader.related_event(room, id)?)?
        } else {
            None
        };
        if let Some(edit) = full.filter(valid) {
            return Ok(Some(edit));
        }
    }
    let Some(id) = event_id(message) else {
        return Ok(None);
    };
    let edits = need(reader.relations(room, id, "m.replace", EDIT_PAGES)?)?;
    Ok(edits
        .into_iter()
        .filter(|edit| valid(edit))
        .max_by(|left, right| sent_at(left).cmp(&sent_at(right))))
}

/// How a record names `message`'s sender ([`author_line`]), with the display name the room
/// holds for them.
fn author(
    reader: &mut Reader<'_>,
    names: &mut Names,
    room: &str,
    message: &Value,
) -> Result<String> {
    let Some(user) = sender(message) else {
        return Ok(String::new());
    };
    let key = (room.to_owned(), user.to_owned());
    if let Some(name) = names.get(&key) {
        return Ok(name.clone());
    }
    let line = author_line(need(reader.member_name(room, user)?)?.as_deref(), user);
    names.insert(key, line.clone());
    Ok(line)
}
