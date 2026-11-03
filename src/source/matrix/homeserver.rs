//! The requests a Matrix source sends, each through the run's request budget and the
//! homeserver's rate limit.
//!
//! Every request is a `GET`. A `/sync` passes `set_presence=offline`, so the account never
//! shows as online because a fetch ran, and no route that sends a read receipt or moves a read
//! marker is ever called: the same account is often someone's live chat client, whose unread
//! list a fetch must leave alone.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::api::client::NoAnswer;
use crate::api::matrix::{encode, MatrixClient, Reply};

use super::records::MESSAGE_TYPES;
use super::rooms::Facts;

/// How many events one page of `/messages` or `/relations` asks for.
pub(super) const PAGE_SIZE: u32 = 100;
/// How many times one request is sent while the homeserver answers it with a rate limit.
const RATE_LIMIT_TRIES: u32 = 5;
/// The longest wait a rate limit may ask for before the run stops instead of waiting.
const LONGEST_WAIT: Duration = Duration::from_secs(60);
/// The wait after a rate limit that names none.
const DEFAULT_WAIT: Duration = Duration::from_secs(1);

/// Why a run stops reading rooms before it has read them all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stop {
    /// The run's request budget is spent.
    Budget,
    /// The homeserver kept refusing requests for its rate limit, or asked for a wait longer than
    /// [`LONGEST_WAIT`].
    Throttled,
    /// A request got no answer at all, so the homeserver is taken to be unreachable.
    NoAnswer,
}

impl Stop {
    /// What stopped the run, as a warning says it.
    pub(super) const fn why(self) -> &'static str {
        match self {
            Self::Budget => "the run's request budget ran out",
            Self::Throttled => "the homeserver's rate limit held",
            Self::NoAnswer => "the homeserver stopped answering",
        }
    }
}

/// A request's outcome: an error that fails the room it was for, or `Err(Stop)` inside, which
/// stops the run.
pub(super) type Step<T> = Result<Result<T, Stop>>;

/// A room's place in its timeline, as `/context` serves it for one event.
pub(super) struct Context {
    /// The event itself.
    pub(super) event: Value,
    /// The token that reads on from just after the event.
    pub(super) end: Option<String>,
    /// The state that comes with it, which names the members whose events it returns.
    pub(super) state: Vec<Value>,
}

/// One page of `/messages`.
pub(super) struct Page {
    pub(super) chunk: Vec<Value>,
    /// The token that reads on, absent when the page reached the end of the timeline.
    pub(super) end: Option<String>,
    pub(super) state: Vec<Value>,
}

/// What `/timestamp_to_event` found.
pub(super) enum Found {
    /// The first event sent at or after the time.
    Event(String),
    /// No event was sent at or after the time.
    Nothing,
    /// The homeserver does not serve the endpoint.
    Unsupported,
}

/// The direction a `/messages` page reads in.
#[derive(Clone, Copy)]
pub(super) enum Direction {
    /// Oldest first, from a token.
    Forward,
    /// Newest first, from a token or from the end of the timeline.
    Backward,
}

/// Sends a run's requests: it charges each try of a room's read to the budget, waits out a rate
/// limit the homeserver names, and once the run must stop answers every later request with that
/// stop.
pub(super) struct Reader<'c> {
    client: &'c MatrixClient,
    /// Requests left; `None` for a resolve, which reads a few events and counts none.
    budget: Option<u32>,
    stopped: Option<Stop>,
}

impl<'c> Reader<'c> {
    pub(super) const fn new(client: &'c MatrixClient, budget: Option<u32>) -> Self {
        Self {
            client,
            budget,
            stopped: None,
        }
    }

    /// Why the run stopped, once it has.
    pub(super) const fn stopped(&self) -> Option<Stop> {
        self.stopped
    }

    /// Sends `GET path` until the homeserver answers it with something other than a rate
    /// limit. `charged` requests count against the budget, each try before it is sent.
    fn send(&mut self, path: &str, query: &[(&str, String)], charged: bool) -> Step<Reply> {
        let mut tries = 0;
        loop {
            if let Some(stop) = self.stopped {
                return Ok(Err(stop));
            }
            if charged {
                match self.budget {
                    Some(0) => return Ok(Err(self.stop(Stop::Budget))),
                    Some(left) => self.budget = Some(left - 1),
                    None => {}
                }
            }
            tries += 1;
            let reply = match self.client.get(path, query) {
                Ok(reply) => reply,
                Err(error) => {
                    if error.downcast_ref::<NoAnswer>().is_some() {
                        self.stopped = Some(Stop::NoAnswer);
                    }
                    return Err(error);
                }
            };
            if !reply.is_rate_limited() {
                return Ok(Ok(reply));
            }
            let wait = reply.retry_after.unwrap_or(DEFAULT_WAIT);
            if tries >= RATE_LIMIT_TRIES || wait > LONGEST_WAIT {
                return Ok(Err(self.stop(Stop::Throttled)));
            }
            std::thread::sleep(wait);
        }
    }

    fn stop(&mut self, stop: Stop) -> Stop {
        self.stopped = Some(stop);
        stop
    }

    /// Every room the account has joined. Not charged to the budget.
    pub(super) fn joined_rooms(&mut self) -> Step<Vec<String>> {
        let reply = match self.send("/_matrix/client/v3/joined_rooms", &[], false)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if !reply.is_success() {
            return Err(reply.refusal("GET /joined_rooms"));
        }
        let rooms = reply
            .body
            .get("joined_rooms")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("the homeserver's /joined_rooms answer has no joined_rooms"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok(Ok(rooms))
    }

    /// One `/sync` that returns at once, from `since` when given, with `filter`, and with the
    /// account's presence left offline. Not charged to the budget.
    pub(super) fn sync(&mut self, since: Option<&str>, filter: &Value) -> Step<Value> {
        let mut query = vec![
            ("timeout", "0".to_owned()),
            ("set_presence", "offline".to_owned()),
            ("filter", filter.to_string()),
        ];
        if let Some(since) = since {
            query.push(("since", since.to_owned()));
        }
        let reply = match self.send("/_matrix/client/v3/sync", &query, false)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if !reply.is_success() {
            return Err(reply.refusal("GET /sync"));
        }
        Ok(Ok(reply.body))
    }

    /// The room's name and canonical alias, as its state holds them now. The alias is read
    /// when the room has no name, which a label then falls back to, or when `alias` says the
    /// configuration names a room by its alias.
    pub(super) fn facts(&mut self, room: &str, alias: bool) -> Step<Facts> {
        let name = match self.state_field(room, "m.room.name", "name")? {
            Ok(name) => name,
            Err(stop) => return Ok(Err(stop)),
        };
        if name.is_some() && !alias {
            return Ok(Ok(Facts { name, alias: None }));
        }
        let alias = match self.state_field(room, "m.room.canonical_alias", "alias")? {
            Ok(alias) => alias,
            Err(stop) => return Ok(Err(stop)),
        };
        Ok(Ok(Facts { name, alias }))
    }

    /// One string field of a room state event with an empty state key, or `None` when the room
    /// has no such event or it holds no such string.
    fn state_field(&mut self, room: &str, kind: &str, field: &str) -> Step<Option<String>> {
        let path = format!("/_matrix/client/v3/rooms/{}/state/{kind}", encode(room));
        let reply = match self.send(&path, &[], true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if reply.is_not_found() {
            return Ok(Ok(None));
        }
        if !reply.is_success() {
            return Err(reply.refusal(&format!("GET /rooms/{room}/state/{kind}")));
        }
        Ok(Ok(reply
            .body
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)))
    }

    /// The display name a member has in the room, or `None` when the room holds no name for
    /// them.
    pub(super) fn member_name(&mut self, room: &str, user: &str) -> Step<Option<String>> {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/state/m.room.member/{}",
            encode(room),
            encode(user)
        );
        let reply = match self.send(&path, &[], true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if reply.is_not_found() || reply.errcode() == Some("M_FORBIDDEN") {
            return Ok(Ok(None));
        }
        if !reply.is_success() {
            return Err(reply.refusal(&format!("GET /rooms/{room}/state/m.room.member")));
        }
        Ok(Ok(display_name(&reply.body)))
    }

    /// The event and the token that reads on after it, or `None` when the homeserver no longer
    /// serves the event.
    pub(super) fn context(&mut self, room: &str, event: &str) -> Step<Option<Context>> {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/context/{}",
            encode(room),
            encode(event)
        );
        let query = [("limit", "0".to_owned()), ("filter", event_filter())];
        let reply = match self.send(&path, &query, true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if reply.is_not_found() {
            return Ok(Ok(None));
        }
        if !reply.is_success() {
            return Err(reply.refusal(&format!("GET /rooms/{room}/context/{event}")));
        }
        let mut body = reply.body;
        let Some(found) = body.get_mut("event").map(Value::take) else {
            bail!("the homeserver's context of {event} in {room} has no event");
        };
        Ok(Ok(Some(Context {
            event: found,
            end: token(&body, "end"),
            state: events(&mut body, "state"),
        })))
    }

    /// One page of the room's message events, in `direction`, from `from` (or, backward, from
    /// the end of the timeline).
    pub(super) fn messages(
        &mut self,
        room: &str,
        direction: Direction,
        from: Option<&str>,
    ) -> Step<Page> {
        let path = format!("/_matrix/client/v3/rooms/{}/messages", encode(room));
        let mut query = vec![
            (
                "dir",
                match direction {
                    Direction::Forward => "f",
                    Direction::Backward => "b",
                }
                .to_owned(),
            ),
            ("limit", PAGE_SIZE.to_string()),
            ("filter", event_filter()),
        ];
        if let Some(from) = from {
            query.push(("from", from.to_owned()));
        }
        let reply = match self.send(&path, &query, true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if !reply.is_success() {
            return Err(reply.refusal(&format!("GET /rooms/{room}/messages")));
        }
        let mut body = reply.body;
        Ok(Ok(Page {
            chunk: events(&mut body, "chunk"),
            end: token(&body, "end"),
            state: events(&mut body, "state"),
        }))
    }

    /// The first event sent in the room at or after `at`.
    pub(super) fn timestamp_to_event(&mut self, room: &str, at: DateTime<Utc>) -> Step<Found> {
        let path = format!(
            "/_matrix/client/v1/rooms/{}/timestamp_to_event",
            encode(room)
        );
        let query = [
            ("ts", at.timestamp_millis().to_string()),
            ("dir", "f".to_owned()),
        ];
        let reply = match self.send(&path, &query, true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if reply.is_success() {
            let event = reply
                .body
                .get("event_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!("the homeserver's timestamp_to_event answer in {room} has no event_id")
                })?;
            return Ok(Ok(Found::Event(event.to_owned())));
        }
        if reply.is_not_found() {
            return Ok(Ok(Found::Nothing));
        }
        if reply.is_unrecognized()
            || (matches!(reply.status, 404 | 405) && reply.errcode().is_none())
        {
            return Ok(Ok(Found::Unsupported));
        }
        Err(reply.refusal(&format!("GET /rooms/{room}/timestamp_to_event")))
    }

    /// One event, or `None` when the homeserver does not serve it.
    pub(super) fn event(&mut self, room: &str, event: &str) -> Step<Option<Value>> {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/event/{}",
            encode(room),
            encode(event)
        );
        let reply = match self.send(&path, &[], true)? {
            Ok(reply) => reply,
            Err(stop) => return Ok(Err(stop)),
        };
        if reply.is_not_found() {
            return Ok(Ok(None));
        }
        if !reply.is_success() {
            return Err(reply.refusal(&format!("GET /rooms/{room}/event/{event}")));
        }
        Ok(Ok(Some(reply.body)))
    }

    /// The events that relate to `event` by `relation`, newest first, at most `pages` pages of
    /// them.
    pub(super) fn relations(
        &mut self,
        room: &str,
        event: &str,
        relation: &str,
        pages: u32,
    ) -> Step<Vec<Value>> {
        let path = format!(
            "/_matrix/client/v1/rooms/{}/relations/{}/{relation}",
            encode(room),
            encode(event)
        );
        let mut found = Vec::new();
        let mut from: Option<String> = None;
        for _ in 0..pages {
            let mut query = vec![("dir", "b".to_owned()), ("limit", PAGE_SIZE.to_string())];
            if let Some(from) = &from {
                query.push(("from", from.clone()));
            }
            let reply = match self.send(&path, &query, true)? {
                Ok(reply) => reply,
                Err(stop) => return Ok(Err(stop)),
            };
            if reply.is_not_found() {
                break;
            }
            if !reply.is_success() {
                return Err(reply.refusal(&format!("GET /rooms/{room}/relations/{event}")));
            }
            let mut body = reply.body;
            found.extend(events(&mut body, "chunk"));
            from = token(&body, "next_batch");
            if from.is_none() {
                break;
            }
        }
        Ok(Ok(found))
    }
}

/// The filter of every room event request: the message types alone, and the members of the
/// events returned, so each page names its senders.
fn event_filter() -> String {
    json!({"types": MESSAGE_TYPES, "lazy_load_members": true}).to_string()
}

/// A non-empty string field of `body`.
fn token(body: &Value, field: &str) -> Option<String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The events an array field of `body` holds, taken out of it.
fn events(body: &mut Value, field: &str) -> Vec<Value> {
    match body.get_mut(field).map(Value::take) {
        Some(Value::Array(events)) => events,
        _ => Vec::new(),
    }
}

/// The display name a member event's content holds.
pub(super) fn display_name(content: &Value) -> Option<String> {
    content
        .get("displayname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// What `state` (member events, as `/messages` and `/context` return them) says each member is
/// called: a name, or `None` when the room holds none for them.
pub(super) fn members(state: &[Value]) -> BTreeMap<String, Option<String>> {
    state
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("m.room.member"))
        .filter_map(|event| {
            let user = event.get("state_key").and_then(Value::as_str)?;
            let name = event.get("content").and_then(display_name);
            Some((user.to_owned(), name))
        })
        .collect()
}
