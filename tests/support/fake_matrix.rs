//! An emulated Matrix homeserver: joined rooms with timelines a test appends to, served the way
//! the client-server API serves them, so the Matrix source runs its real client against it.
//!
//! It answers `/joined_rooms`, `/sync` (full and since a token), room state, `/context`,
//! `/messages` in both directions with tokens that name a gap in a room's timeline,
//! `/timestamp_to_event`, `/event` and `/relations`, and it can refuse requests for its rate
//! limit, fail one room's reads, or not serve `/timestamp_to_event` at all. Every room and
//! event in it is invented.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::fake_http::{self, Recorded, Response};

/// The access token this homeserver accepts.
pub const TOKEN: &str = "syt_fixture_access_token";

/// One room the account has joined.
#[derive(Default)]
struct Room {
    name: Option<String>,
    alias: Option<String>,
    /// Each member's display name.
    members: BTreeMap<String, String>,
    /// The timeline in order, each event with the stream position it was sent at.
    timeline: Vec<(u64, Value)>,
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
            Ok(route(&mut world, request))
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

    /// Joins a room with a name, or none.
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

fn route(world: &mut World, request: &Recorded) -> Response {
    if request.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}"))
    {
        return error(401, "M_UNKNOWN_TOKEN");
    }
    if request.method != "GET" {
        return error(405, "M_UNRECOGNIZED");
    }
    let throttled = world
        .throttled
        .iter_mut()
        .find(|(path, (times, _))| *times > 0 && request.path.contains(path.as_str()));
    if let Some((_, (times, wait))) = throttled {
        *times -= 1;
        return Response::json(
            429,
            json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Too many requests", "retry_after_ms": wait})
                .to_string(),
        );
    }
    let params = query(&request.target);
    let segments: Vec<String> = request.path.split('/').map(decode).collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    match parts.as_slice() {
        ["", "_matrix", "client", "v3", "joined_rooms"] => {
            ok(&json!({"joined_rooms": world.rooms.keys().collect::<Vec<_>>()}))
        }
        ["", "_matrix", "client", "v3", "sync"] => sync(world, &params),
        ["", "_matrix", "client", "v3", "rooms", room, rest @ ..] => {
            let room = (*room).to_owned();
            if world.failing.contains(&room)
                && matches!(rest.first(), Some(&"messages" | &"context"))
            {
                return error(500, "M_UNKNOWN");
            }
            let Some(data) = world.rooms.get(&room) else {
                return error(403, "M_FORBIDDEN");
            };
            room_route(data, rest, &params)
        }
        ["", "_matrix", "client", "v1", "rooms", room, "timestamp_to_event"] => {
            if world.no_timestamp_to_event {
                return error(404, "M_UNRECOGNIZED");
            }
            let Some(data) = world.rooms.get(*room) else {
                return error(403, "M_FORBIDDEN");
            };
            let at: i64 = params
                .get("ts")
                .and_then(|ts| ts.parse().ok())
                .unwrap_or_default();
            data.timeline
                .iter()
                .map(|(_, event)| event)
                .find(|event| ts(event) >= at)
                .map_or_else(
                    || error(404, "M_NOT_FOUND"),
                    |event| ok(&json!({"event_id": event["event_id"], "origin_server_ts": event["origin_server_ts"]})),
                )
        }
        ["", "_matrix", "client", "v1", "rooms", room, "relations", event, relation] => {
            let Some(data) = world.rooms.get(*room) else {
                return error(403, "M_FORBIDDEN");
            };
            let mut related: Vec<&Value> = data
                .timeline
                .iter()
                .map(|(_, event)| event)
                .filter(|candidate| {
                    candidate
                        .pointer("/content/m.relates_to/rel_type")
                        .and_then(Value::as_str)
                        == Some(*relation)
                        && candidate
                            .pointer("/content/m.relates_to/event_id")
                            .and_then(Value::as_str)
                            == Some(*event)
                })
                .collect();
            related.reverse();
            ok(&json!({"chunk": related}))
        }
        _ => error(404, "M_UNRECOGNIZED"),
    }
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
    let since = params
        .get("since")
        .and_then(|since| since.strip_prefix('s'))
        .and_then(|since| since.parse::<u64>().ok());
    let mut join = serde_json::Map::new();
    for (id, room) in &world.rooms {
        let newest = room
            .timeline
            .iter()
            .rfind(|(_, event)| wanted(timeline_types.as_ref(), event));
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
        let mut state = Vec::new();
        if since.is_none() {
            if let (Some(name), true) = (
                &room.name,
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
                "timeline": {"events": newest.map(|(_, event)| vec![event]).unwrap_or_default(), "limited": true},
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
    let senders: BTreeSet<&str> = events
        .iter()
        .filter_map(|event| event.get("sender").and_then(Value::as_str))
        .collect();
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

fn room_route(room: &Room, rest: &[&str], params: &BTreeMap<String, String>) -> Response {
    match rest {
        ["state", "m.room.name"] => room.name.as_ref().map_or_else(
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
        ["messages"] => messages(room, params),
        _ => error(404, "M_UNRECOGNIZED"),
    }
}

fn messages(room: &Room, params: &BTreeMap<String, String>) -> Response {
    let types = filter_types(params, &[]);
    let limit: usize = params
        .get("limit")
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(10);
    let forward = params.get("dir").map(String::as_str) == Some("f");
    let from = params.get("from").and_then(|from| index_of(from));
    let mut chunk: Vec<&Value> = Vec::new();
    let mut end = None;
    if forward {
        let Some(mut index) = from else {
            return error(400, "M_INVALID_PARAM");
        };
        while let Some((_, event)) = room.timeline.get(index) {
            index += 1;
            if wanted(types.as_ref(), event) {
                chunk.push(event);
                end = Some(index);
                if chunk.len() == limit {
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
                    chunk.push(event);
                    end = Some(index);
                    if chunk.len() == limit {
                        break;
                    }
                }
            }
        }
    }
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
