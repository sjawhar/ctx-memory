//! An emulated Matrix homeserver: joined rooms with timelines a test appends to, served the way
//! the client-server API serves them, so the Matrix source runs its real client against it.
//!
//! It answers `/joined_rooms`, `/sync` (full and since a token), room state, `/context`,
//! `/messages` in both directions with tokens that name a gap in a room's timeline,
//! `/timestamp_to_event`, `/event` and `/relations`, and it can refuse requests for its rate
//! limit, fail one room's reads, not serve `/timestamp_to_event` at all, hang up on a route, or
//! refuse one with a status. Where Synapse's behaviour is what a source has to survive, it
//! follows Synapse: `/timestamp_to_event` picks by `origin_server_ts`, not by timeline order;
//! `/messages` takes a page of events before it leaves out those of users the account ignores,
//! so a page can come back empty and still carry `end`; a request line longer than Twisted's
//! 16 KiB is answered `400` with no body; a rename is a state event in the timeline, and a full
//! `/sync` gives the state as it stood before the events its timeline returns. Every room and
//! event in it is invented.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::fake_http::{self, Recorded, Response};

/// The access token this homeserver accepts.
pub const TOKEN: &str = "syt_fixture_access_token";

/// The longest request line Twisted, which Synapse runs on, reads before it answers `400`.
const MAX_REQUEST_LINE: usize = 16 * 1024;

/// One room the account has joined.
#[derive(Default)]
struct Room {
    /// The name the room had before any event of its timeline.
    name: Option<String>,
    alias: Option<String>,
    /// Each member's display name.
    members: BTreeMap<String, String>,
    /// The timeline in order, each event with the stream position it was sent at.
    timeline: Vec<(u64, Value)>,
}

impl Room {
    /// The room's name as it stood before timeline index `before`: the newest rename before
    /// it, else the name it started with.
    fn name_before(&self, before: usize) -> Option<String> {
        self.timeline
            .iter()
            .take(before)
            .rev()
            .find(|(_, event)| {
                event.get("type").and_then(Value::as_str) == Some("m.room.name")
                    && event.get("state_key").and_then(Value::as_str) == Some("")
            })
            .map_or_else(
                || self.name.clone(),
                |(_, event)| {
                    event
                        .pointer("/content/name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                },
            )
    }

    fn current_name(&self) -> Option<String> {
        self.name_before(self.timeline.len())
    }
}

#[derive(Default)]
struct World {
    rooms: BTreeMap<String, Room>,
    /// The position of the last event sent, which a `/sync` token names.
    stream: u64,
    next_event: u64,
    requests: Vec<Recorded>,
    /// Paths that answer `429` this many more times.
    throttled: BTreeMap<String, (u32, u64)>,
    /// Rooms whose `/messages` fail.
    failing: BTreeSet<String>,
    /// Whether `/timestamp_to_event` is served.
    no_timestamp_to_event: bool,
    /// Users the account ignores, whose events no answer carries.
    ignored: BTreeSet<String>,
    /// Paths whose requests get no answer at all: the connection closes.
    hung: BTreeSet<String>,
    /// Path fragments answered with a refusal: the status and the Matrix error code.
    refused: Vec<(String, u16, String)>,
}

/// A loopback homeserver.
#[derive(Clone)]
pub struct FakeMatrix {
    address: SocketAddr,
    world: Arc<Mutex<World>>,
}

impl FakeMatrix {
    /// Starts a homeserver with no rooms.
    ///
    /// # Errors
    ///
    /// Returns an error when the port cannot be bound.
    pub fn start() -> Result<Self> {
        let world = Arc::new(Mutex::new(World::default()));
        let serving = Arc::clone(&world);
        let address = fake_http::serve("the fake homeserver", move |request, _| {
            let mut world = serving
                .lock()
                .map_err(|_| anyhow!("the fake homeserver's state was poisoned"))?;
            world.requests.push(request.clone());
            route(&mut world, request)
        })?;
        Ok(Self { address, world })
    }

    /// The homeserver's base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn lock(&self) -> Result<MutexGuard<'_, World>> {
        self.world
            .lock()
            .map_err(|_| anyhow!("the fake homeserver's state was poisoned"))
    }

    /// Joins a room with a name, or none, that it has had since before any of its events.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn room(&self, id: &str, name: Option<&str>) -> Result<()> {
        let mut world = self.lock()?;
        let room = world.rooms.entry(id.to_owned()).or_default();
        room.name = name.map(str::to_owned);
        Ok(())
    }

    /// Gives a room a canonical alias.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn alias(&self, id: &str, alias: &str) -> Result<()> {
        let mut world = self.lock()?;
        world.rooms.entry(id.to_owned()).or_default().alias = Some(alias.to_owned());
        Ok(())
    }

    /// Adds a member with a display name.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn member(&self, room: &str, user: &str, name: &str) -> Result<()> {
        let mut world = self.lock()?;
        world
            .rooms
            .entry(room.to_owned())
            .or_default()
            .members
            .insert(user.to_owned(), name.to_owned());
        Ok(())
    }

    /// Renames a room, as a bridge does when a contact's name syncs: an `m.room.name` state
    /// event in the timeline.
    ///
    /// # Errors
    ///
    /// Returns an error when the room is unknown.
    pub fn rename(&self, room: &str, sender: &str, ts: i64, name: &str) -> Result<String> {
        self.append(
            room,
            "m.room.name",
            Some(""),
            sender,
            ts,
            json!({"name": name}),
        )
    }

    /// Ignores a user: no `/messages` page and no `/sync` timeline carries their events, though
    /// a page still counts them against its limit, as Synapse does.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn ignore(&self, user: &str) -> Result<()> {
        self.lock()?.ignored.insert(user.to_owned());
        Ok(())
    }

    /// Closes every connection that asks for a path ending in `path` without an answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn hang_up(&self, path: &str) -> Result<()> {
        self.lock()?.hung.insert(path.to_owned());
        Ok(())
    }

    /// Answers every request whose decoded path holds `fragment` with `status` and `errcode`,
    /// as a homeserver that will not show the user an event does.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn refuse(&self, fragment: &str, status: u16, errcode: &str) -> Result<()> {
        self.lock()?
            .refused
            .push((fragment.to_owned(), status, errcode.to_owned()));
        Ok(())
    }

    /// Appends an event of `kind` with `content`, sent by `sender` at `ts` (milliseconds), and
    /// returns its id. A redaction strips the event it names, as a homeserver does.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned or the room is unknown.
    pub fn send(
        &self,
        room: &str,
        kind: &str,
        sender: &str,
        ts: i64,
        content: Value,
    ) -> Result<String> {
        self.append(room, kind, None, sender, ts, content)
    }

    fn append(
        &self,
        room: &str,
        kind: &str,
        state_key: Option<&str>,
        sender: &str,
        ts: i64,
        content: Value,
    ) -> Result<String> {
        let mut world = self.lock()?;
        world.next_event += 1;
        world.stream += 1;
        let id = format!("$ev{:04}", world.next_event);
        let stream = world.stream;
        let redacts = (kind == "m.room.redaction")
            .then(|| {
                content
                    .get("redacts")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        let mut event = json!({
            "type": kind,
            "event_id": id,
            "room_id": room,
            "sender": sender,
            "origin_server_ts": ts,
        });
        set(&mut event, "content", content);
        if let Some(state_key) = state_key {
            set(&mut event, "state_key", json!(state_key));
        }
        let timeline = &mut world
            .rooms
            .get_mut(room)
            .ok_or_else(|| anyhow!("no room {room}"))?
            .timeline;
        if let Some(target) = redacts {
            for (_, earlier) in timeline.iter_mut() {
                if earlier.get("event_id").and_then(Value::as_str) == Some(target.as_str()) {
                    set(earlier, "content", json!({}));
                    set(
                        earlier,
                        "unsigned",
                        json!({"redacted_because": event.clone()}),
                    );
                }
            }
        }
        timeline.push((stream, event));
        Ok(id)
    }

    /// Appends a text message.
    ///
    /// # Errors
    ///
    /// Returns an error when the room is unknown.
    pub fn text(&self, room: &str, sender: &str, ts: i64, body: &str) -> Result<String> {
        self.send(
            room,
            "m.room.message",
            sender,
            ts,
            json!({"msgtype": "m.text", "body": body}),
        )
    }

    /// Answers the next `times` requests whose path contains `path` with `429`, asking for a
    /// wait of `wait_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn throttle(&self, path: &str, times: u32, wait_ms: u64) -> Result<()> {
        self.lock()?
            .throttled
            .insert(path.to_owned(), (times, wait_ms));
        Ok(())
    }

    /// Fails every `/messages` and `/context` request of `room` with `500`.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn fail(&self, room: &str) -> Result<()> {
        self.lock()?.failing.insert(room.to_owned());
        Ok(())
    }

    /// Stops serving `/timestamp_to_event`, as a homeserver without it answers.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn without_timestamp_to_event(&self) -> Result<()> {
        self.lock()?.no_timestamp_to_event = true;
        Ok(())
    }

    /// Every request received, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn requests(&self) -> Result<Vec<Recorded>> {
        Ok(self.lock()?.requests.clone())
    }

    /// Forgets the requests received so far.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn clear_requests(&self) -> Result<()> {
        self.lock()?.requests.clear();
        Ok(())
    }
}

/// A request's query parameters, decoded.
#[must_use]
pub fn query(target: &str) -> BTreeMap<String, String> {
    target
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// One percent-encoded component, with `+` as a space.
#[must_use]
pub fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                if let Some(decoded) = hex {
                    out.push(decoded);
                    index += 2;
                } else {
                    out.push(byte);
                }
            }
            _ => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn error(status: u16, code: &str) -> Response {
    Response::json(status, json!({"errcode": code, "error": code}).to_string())
}

fn ok(body: &Value) -> Response {
    Response::json(200, body.to_string())
}

fn route(world: &mut World, request: &Recorded) -> Result<Response> {
    if request.target.len() > MAX_REQUEST_LINE {
        // Twisted answers a request line it will not read with a bare 400 and no body.
        return Ok(Response::json(400, ""));
    }
    if world
        .hung
        .iter()
        .any(|path| request.path.ends_with(path.as_str()))
    {
        bail!("hanging up on {}", request.path);
    }
    if request.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}"))
    {
        return Ok(error(401, "M_UNKNOWN_TOKEN"));
    }
    if request.method != "GET" {
        return Ok(error(405, "M_UNRECOGNIZED"));
    }
    let decoded = decode(&request.path);
    if let Some((_, status, code)) = world
        .refused
        .iter()
        .find(|(fragment, _, _)| decoded.contains(fragment.as_str()))
    {
        return Ok(error(*status, code));
    }
    let throttled = world
        .throttled
        .iter_mut()
        .find(|(path, (times, _))| *times > 0 && request.path.contains(path.as_str()));
    if let Some((_, (times, wait))) = throttled {
        *times -= 1;
        return Ok(Response::json(
            429,
            json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Too many requests", "retry_after_ms": wait})
                .to_string(),
        ));
    }
    let params = query(&request.target);
    let segments: Vec<String> = request.path.split('/').map(decode).collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    Ok(match parts.as_slice() {
        ["", "_matrix", "client", "v3", "joined_rooms"] => {
            ok(&json!({"joined_rooms": world.rooms.keys().collect::<Vec<_>>()}))
        }
        ["", "_matrix", "client", "v3", "sync"] => sync(world, &params),
        ["", "_matrix", "client", "v3", "rooms", room, rest @ ..] => {
            let room = (*room).to_owned();
            if world.failing.contains(&room)
                && matches!(rest.first(), Some(&"messages" | &"context"))
            {
                return Ok(error(500, "M_UNKNOWN"));
            }
            let Some(data) = world.rooms.get(&room) else {
                return Ok(error(403, "M_FORBIDDEN"));
            };
            room_route(data, &world.ignored, rest, &params)
        }
        ["", "_matrix", "client", "v1", "rooms", room, "timestamp_to_event"] => {
            if world.no_timestamp_to_event {
                return Ok(error(404, "M_UNRECOGNIZED"));
            }
            world
                .rooms
                .get(*room)
                .map_or_else(|| error(403, "M_FORBIDDEN"), |data| first_at(data, &params))
        }
        ["", "_matrix", "client", "v1", "rooms", room, "relations", event, relation] => {
            world.rooms.get(*room).map_or_else(
                || error(403, "M_FORBIDDEN"),
                |data| relations(data, event, relation),
            )
        }
        _ => error(404, "M_UNRECOGNIZED"),
    })
}

/// `/timestamp_to_event` forward, as Synapse answers it:
/// `WHERE origin_server_ts >= ? ORDER BY origin_server_ts ASC LIMIT 1`, so the earliest date,
/// wherever in the timeline it sits.
fn first_at(room: &Room, params: &BTreeMap<String, String>) -> Response {
    let at: i64 = params
        .get("ts")
        .and_then(|ts| ts.parse().ok())
        .unwrap_or_default();
    room.timeline
        .iter()
        .enumerate()
        .filter(|(_, (_, event))| ts(event) >= at)
        .min_by_key(|(index, (_, event))| (ts(event), *index))
        .map_or_else(
            || error(404, "M_NOT_FOUND"),
            |(_, (_, event))| {
                ok(&json!({"event_id": event_id(event), "origin_server_ts": ts(event)}))
            },
        )
}

/// The events that relate to `event` by `relation`, newest first.
fn relations(room: &Room, event: &str, relation: &str) -> Response {
    let mut related: Vec<&Value> = room
        .timeline
        .iter()
        .map(|(_, candidate)| candidate)
        .filter(|candidate| {
            candidate
                .pointer("/content/m.relates_to/rel_type")
                .and_then(Value::as_str)
                == Some(relation)
                && candidate
                    .pointer("/content/m.relates_to/event_id")
                    .and_then(Value::as_str)
                    == Some(event)
        })
        .collect();
    related.reverse();
    ok(&json!({"chunk": related}))
}

fn ts(event: &Value) -> i64 {
    event
        .get("origin_server_ts")
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

fn event_id(event: &Value) -> &str {
    event
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn sender(event: &Value) -> &str {
    event
        .get("sender")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The event types a filter asks for, or `None` for every type.
fn filter_types(params: &BTreeMap<String, String>, path: &[&str]) -> Option<Vec<String>> {
    let filter: Value = serde_json::from_str(params.get("filter")?).ok()?;
    let mut node = &filter;
    for key in path {
        node = node.get(*key)?;
    }
    node.get("types")?.as_array().map(|types| {
        types
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

/// The rooms a `/sync` filter leaves out.
fn filter_not_rooms(params: &BTreeMap<String, String>) -> BTreeSet<String> {
    params
        .get("filter")
        .and_then(|filter| serde_json::from_str::<Value>(filter).ok())
        .and_then(|filter| filter.pointer("/room/not_rooms").cloned())
        .and_then(|rooms| rooms.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn wanted(types: Option<&Vec<String>>, event: &Value) -> bool {
    types.is_none_or(|types| {
        event
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| types.iter().any(|wanted| wanted == kind))
    })
}

fn sync(world: &World, params: &BTreeMap<String, String>) -> Response {
    let timeline_types = filter_types(params, &["room", "timeline"]);
    let state_types = filter_types(params, &["room", "state"]).unwrap_or_default();
    let not_rooms = filter_not_rooms(params);
    let since = params
        .get("since")
        .and_then(|since| since.strip_prefix('s'))
        .and_then(|since| since.parse::<u64>().ok());
    let mut join = serde_json::Map::new();
    for (id, room) in &world.rooms {
        if not_rooms.contains(id) {
            continue;
        }
        // A timeline of one event: the newest of the types asked for. Synapse takes it, then
        // leaves it out when it is an ignored user's, so the timeline can come back empty.
        let newest = room
            .timeline
            .iter()
            .enumerate()
            .rfind(|(_, (_, event))| wanted(timeline_types.as_ref(), event));
        let changed = match since {
            Some(since) => room
                .timeline
                .iter()
                .any(|(stream, event)| *stream > since && wanted(timeline_types.as_ref(), event)),
            None => true,
        };
        if !changed {
            continue;
        }
        let shown: Vec<&Value> = newest
            .map(|(_, (_, event))| event)
            .filter(|event| !world.ignored.contains(sender(event)))
            .into_iter()
            .collect();
        // The state as it stood before the events the timeline returns.
        let before = newest.map_or(room.timeline.len(), |(index, _)| index);
        let mut state = Vec::new();
        if since.is_none() {
            if let (Some(name), true) = (
                room.name_before(before),
                state_types.iter().any(|kind| kind == "m.room.name"),
            ) {
                state.push(
                    json!({"type": "m.room.name", "state_key": "", "content": {"name": name}}),
                );
            }
            if let (Some(alias), true) = (
                &room.alias,
                state_types
                    .iter()
                    .any(|kind| kind == "m.room.canonical_alias"),
            ) {
                state.push(json!({"type": "m.room.canonical_alias", "state_key": "", "content": {"alias": alias}}));
            }
        }
        join.insert(
            id.clone(),
            json!({
                "timeline": {"events": shown, "limited": true},
                "state": {"events": state},
            }),
        );
    }
    ok(&json!({
        "next_batch": format!("s{}", world.stream),
        "rooms": {"join": join},
        "to_device": {"events": []},
    }))
}

/// The member events of the senders of `events`.
fn member_state(room: &Room, events: &[&Value]) -> Vec<Value> {
    let senders: BTreeSet<&str> = events.iter().map(|event| sender(event)).collect();
    senders
        .into_iter()
        .filter_map(|user| {
            let name = room.members.get(user)?;
            Some(json!({"type": "m.room.member", "state_key": user, "content": {"membership": "join", "displayname": name}}))
        })
        .collect()
}

/// A token for the gap before timeline index `index`.
fn token(index: usize) -> String {
    format!("t{index}")
}

fn index_of(token: &str) -> Option<usize> {
    token.strip_prefix('t')?.parse().ok()
}

fn room_route(
    room: &Room,
    ignored: &BTreeSet<String>,
    rest: &[&str],
    params: &BTreeMap<String, String>,
) -> Response {
    match rest {
        ["state", "m.room.name"] => room.current_name().map_or_else(
            || error(404, "M_NOT_FOUND"),
            |name| ok(&json!({"name": name})),
        ),
        ["state", "m.room.canonical_alias"] => room.alias.as_ref().map_or_else(
            || error(404, "M_NOT_FOUND"),
            |alias| ok(&json!({"alias": alias})),
        ),
        ["state", "m.room.member", user] => room.members.get(*user).map_or_else(
            || error(404, "M_NOT_FOUND"),
            |name| ok(&json!({"membership": "join", "displayname": name})),
        ),
        ["event", event] => room
            .timeline
            .iter()
            .find(|(_, candidate)| event_id(candidate) == *event)
            .map_or_else(|| error(404, "M_NOT_FOUND"), |(_, found)| ok(found)),
        ["context", event] => {
            let Some(at) = room
                .timeline
                .iter()
                .position(|(_, candidate)| event_id(candidate) == *event)
            else {
                return error(404, "M_NOT_FOUND");
            };
            let found = room
                .timeline
                .get(at)
                .map(|(_, event)| event.clone())
                .unwrap_or_default();
            ok(&json!({
                "event": found,
                "events_before": [],
                "start": token(at),
                "end": token(at + 1),
                "state": member_state(room, &[&found]),
            }))
        }
        ["messages"] => messages(room, ignored, params),
        _ => error(404, "M_UNRECOGNIZED"),
    }
}

/// One page of `/messages`, as Synapse makes it: `limit` events of the types asked for, taken
/// from the database, then those of ignored users left out. `end` comes whenever the database
/// gave events, even when none of them is left.
fn messages(
    room: &Room,
    ignored: &BTreeSet<String>,
    params: &BTreeMap<String, String>,
) -> Response {
    let types = filter_types(params, &[]);
    let limit: usize = params
        .get("limit")
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(10);
    let forward = params.get("dir").map(String::as_str) == Some("f");
    let from = params.get("from").and_then(|from| index_of(from));
    let mut taken: Vec<&Value> = Vec::new();
    let mut end = None;
    if forward {
        let Some(mut index) = from else {
            return error(400, "M_INVALID_PARAM");
        };
        while let Some((_, event)) = room.timeline.get(index) {
            index += 1;
            if wanted(types.as_ref(), event) {
                taken.push(event);
                end = Some(index);
                if taken.len() == limit {
                    break;
                }
            }
        }
    } else {
        let mut index = from.unwrap_or(room.timeline.len());
        while index > 0 {
            index -= 1;
            if let Some((_, event)) = room.timeline.get(index) {
                if wanted(types.as_ref(), event) {
                    taken.push(event);
                    end = Some(index);
                    if taken.len() == limit {
                        break;
                    }
                }
            }
        }
    }
    let chunk: Vec<&Value> = taken
        .into_iter()
        .filter(|event| !ignored.contains(sender(event)))
        .collect();
    let state = member_state(room, &chunk);
    let mut body = json!({"chunk": chunk, "start": params.get("from").cloned().unwrap_or_else(|| token(room.timeline.len())), "state": state});
    if let Some(end) = end {
        set(&mut body, "end", json!(token(end)));
    }
    ok(&body)
}

/// Sets one field of a JSON object.
fn set(object: &mut Value, key: &str, value: Value) {
    if let Some(object) = object.as_object_mut() {
        object.insert(key.to_owned(), value);
    }
}
