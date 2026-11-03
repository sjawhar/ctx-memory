//! A Matrix fetch: which rooms changed since the last run, and each changed room read forward
//! from its cursor.
//!
//! One room is one stream, and its cursor is the last event the stream holds with the time it
//! was sent, `<event id>@<rfc3339>`, or a bare time while the room has given it no event. A run
//! reads a room from just after that event (`/context`, then `/messages` oldest first), so an
//! event a bridge delivers late with an old timestamp is read too, and a room further behind
//! than one run's budget moves its cursor as far as the run read. A room the stream has no
//! event of yet is read from the first event at or after the backfill start
//! (`/timestamp_to_event`).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::api::matrix::MatrixClient;
use crate::exclude::Exclusion;
use crate::record::Record;
use crate::source::{
    emit_stream, latest_believable, token_cursor, FetchRequest, Message, Op, TokenCursor,
    CLOCK_SKEW_MARGIN, PROTOCOL_VERSION,
};

use super::homeserver::{members, Direction, Found, Reader, Step};
use super::records::{event_id, kind, sender, sent_at, Kind, Room};
use super::rooms::{Decision, Facts, Selection};
use super::state::State;
use super::{MatrixConfig, SyncMode};

/// How many pages a room is read backward from its end when the homeserver does not serve
/// `/timestamp_to_event`. A first fetch of a room busier than this many pages in its backfill
/// window keeps only its newest events, with a warning.
const BACKWARD_PAGES: u32 = 20;
/// How many pages of a room one run reads forward. A room further behind goes on from there in
/// the next run, so one busy room neither holds a whole room's backlog in memory nor spends the
/// budget every other room needs.
const ROOM_PAGES: u32 = 50;

/// The times a run reads between.
pub(super) struct Window {
    /// The window end: an event sent later waits for the next fetch.
    end: DateTime<Utc>,
    /// Where a room the stream has no position in yet is read from.
    start: DateTime<Utc>,
}

impl Window {
    fn new(request: &FetchRequest) -> Self {
        Self {
            end: request.window_end,
            start: request.window_end - Duration::hours(i64::from(request.backfill_hours)),
        }
    }

    /// The cursor of a room this run did not move: the one it has, or, for a room with none,
    /// the backfill start, so the next run reads the same backfill window.
    fn unmoved(&self, cursor: Option<&str>) -> String {
        cursor.map_or_else(|| rfc3339(self.start), str::to_owned)
    }
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Lists the joined rooms, asks `/sync` which changed, and reads the changed ones.
pub(super) fn run(
    request: &FetchRequest,
    config: &MatrixConfig,
    selection: &Selection,
    client: &MatrixClient,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let scheme = request.source.scheme.as_str();
    let exclusion = Exclusion::new(&request.exclude)?;
    let mut reader = Reader::new(client, Some(config.request_budget));
    let joined: BTreeSet<String> = match reader.joined_rooms()? {
        Ok(rooms) => rooms.into_iter().collect(),
        Err(stop) => bail!("listing the joined rooms: {}", stop.why()),
    };
    let (mut state, mut warnings) = State::load(&request.state_dir)?;
    let window = Window::new(request);
    let prefix = format!("{scheme}:");
    let excluded: Vec<&String> = joined
        .iter()
        .filter(|room| left_out(room, &state.rooms, selection, &exclusion, &prefix))
        .collect();
    let prepass = Prepass::run(&mut reader, config.sync, &state, &excluded, &mut warnings);
    let planned = plan(
        &joined,
        &prepass,
        &mut state,
        selection,
        &prefix,
        &exclusion,
        &mut warnings,
    );
    hello(scheme, &planned, warnings, sink)?;

    // The run starts at the room the last run's budget or the homeserver's rate limit stopped
    // at and goes round, so every room is read in turn.
    let start = state
        .first
        .take()
        .and_then(|first| planned.iter().position(|room| *room == first))
        .unwrap_or(0);
    let mut first_stopped: Option<String> = None;
    let mut unread = 0_usize;
    for room in planned.iter().cycle().skip(start).take(planned.len()) {
        let cursor = request.cursors.get(room).map(String::as_str);
        if reader.stopped().is_some() {
            unread += 1;
            first_stopped.get_or_insert_with(|| room.clone());
            state.unsettle(room);
            emit_stream(
                room,
                Ok((Vec::new(), Vec::new(), window.unmoved(cursor))),
                sink,
            )?;
            continue;
        }
        if let Some(cursor) = prepass.settled(room, cursor, &state, &window) {
            state.settle(room, &cursor, prepass.newest(room));
            emit_stream(room, Ok((Vec::new(), Vec::new(), cursor)), sink)?;
            continue;
        }
        match read_room(
            &mut reader,
            scheme,
            room,
            cursor,
            &window,
            selection,
            &mut state.rooms,
        ) {
            Ok(read) => {
                if read.complete {
                    state.settle(room, &read.cursor, prepass.newest(room));
                } else {
                    state.unsettle(room);
                }
                if read.stopped {
                    first_stopped.get_or_insert_with(|| room.clone());
                }
                emit_stream(room, Ok((read.warnings, read.records, read.cursor)), sink)?;
            }
            Err(error) => {
                state.unsettle(room);
                if reader.stopped().is_some() {
                    first_stopped.get_or_insert_with(|| room.clone());
                }
                emit_stream(room, Err(error), sink)?;
            }
        }
    }
    if let Some(stop) = reader.stopped() {
        sink(Message::Warning {
            stream: None,
            message: format!(
                "{}: {unread} room(s) were not read this run and keep their cursors; the next fetch reads them first",
                stop.why()
            ),
        })?;
    }
    state.first = first_stopped;
    state.retain(&joined, &planned);
    state.keep_sync(config.sync, prepass.next_batch);
    state.save(&request.state_dir)
}

/// The rooms a run reads: every joined room no exclusion leaves out, by what the run knows of
/// each room's name and alias, the names a full `/sync` gave replacing any kept from earlier
/// runs. An allowlisted room the account has not joined is a warning.
fn plan(
    joined: &BTreeSet<String>,
    prepass: &Prepass,
    state: &mut State,
    selection: &Selection,
    prefix: &str,
    exclusion: &Exclusion,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    for (room, facts) in &prepass.facts {
        state.rooms.insert(room.clone(), facts.clone());
    }
    for id in selection.allowed_ids() {
        if !joined.contains(id) {
            warnings.push(format!(
                "configured room {id} is not a room this account has joined; skipping"
            ));
        }
    }
    joined
        .iter()
        .filter(|room| !left_out(room, &state.rooms, selection, exclusion, prefix))
        .cloned()
        .collect()
}

/// Whether a run leaves `room` out: an exclusion pattern covers the whole room, or `rooms` and
/// `exclude_rooms` leave it out by what the run knows of its name and alias.
fn left_out(
    room: &str,
    known: &BTreeMap<String, Facts>,
    selection: &Selection,
    exclusion: &Exclusion,
    prefix: &str,
) -> bool {
    exclusion.covers(&format!("{prefix}{room}/"))
        || selection.decide(room, known.get(room)) == Decision::Skip
}

/// The fetch's hello, planning `planned`, and the warnings found before any room was read.
fn hello(
    scheme: &str,
    planned: &[String],
    warnings: Vec<String>,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: scheme.to_owned(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: planned.to_vec(),
    })?;
    for message in warnings {
        sink(Message::Warning {
            stream: None,
            message,
        })?;
    }
    Ok(())
}

/// What the run's `/sync` said before it read any room.
struct Prepass {
    seen: Seen,
    /// The token the next incremental `/sync` starts from.
    next_batch: Option<String>,
    /// The name and alias of each room a full `/sync` listed.
    facts: BTreeMap<String, Facts>,
}

enum Seen {
    /// No `/sync` ran, or it failed: every room is read.
    Nothing,
    /// An incremental `/sync`: the rooms with any event since the stored token.
    Changed(BTreeSet<String>),
    /// A full `/sync`: the id and time of each room's newest event of any type, or `None` for a
    /// room whose timeline came back empty.
    Newest(BTreeMap<String, Option<(String, DateTime<Utc>)>>),
}

impl Prepass {
    const fn nothing() -> Self {
        Self {
            seen: Seen::Nothing,
            next_batch: None,
            facts: BTreeMap::new(),
        }
    }

    /// Runs the `/sync` the configuration names, leaving out the `excluded` rooms. A `/sync`
    /// that fails costs this run a read of every room and is a warning, never an error.
    fn run(
        reader: &mut Reader<'_>,
        mode: SyncMode,
        state: &State,
        excluded: &[&String],
        warnings: &mut Vec<String>,
    ) -> Self {
        match mode {
            SyncMode::Off => return Self::nothing(),
            SyncMode::Incremental => {
                if let Some(since) = state.since.as_deref() {
                    match incremental(reader, since, excluded) {
                        Ok(Some(prepass)) => return prepass,
                        Ok(None) => return Self::nothing(),
                        Err(error) => warnings.push(format!(
                            "the /sync since the stored token failed ({error:#}); this run makes a full /sync instead"
                        )),
                    }
                }
            }
            SyncMode::Full => {}
        }
        match full(reader, excluded) {
            Ok(Some(prepass)) => prepass,
            Ok(None) => Self::nothing(),
            Err(error) => {
                warnings.push(format!(
                    "the full /sync failed ({error:#}); every room is read this run"
                ));
                Self::nothing()
            }
        }
    }

    /// The id of the newest event a full `/sync` showed in `room`, which a room settled this run
    /// keeps beside its cursor.
    fn newest(&self, room: &str) -> Option<&str> {
        match &self.seen {
            Seen::Newest(newest) => newest
                .get(room)
                .and_then(Option::as_ref)
                .map(|(id, _)| id.as_str()),
            Seen::Nothing | Seen::Changed(_) => None,
        }
    }

    /// The cursor a room ends at without being read, when the `/sync` shows it has nothing a
    /// read would find.
    ///
    /// Both kinds of `/sync` ask for each room's newest event of any type, not its newest
    /// message: a homeserver may filter a timeline by type after it has cut it to its newest
    /// events, so a room whose last event is a reaction can come back with no message at all.
    fn settled(
        &self,
        room: &str,
        cursor: Option<&str>,
        state: &State,
        window: &Window,
    ) -> Option<String> {
        let settled_here = state.settled.get(room).map(String::as_str) == cursor;
        match &self.seen {
            Seen::Nothing => None,
            // Unchanged since the token, and settled at the cursor it still has: the window that
            // settled it was stored, and nothing arrived since.
            Seen::Changed(changed) => cursor
                .filter(|_| settled_here && !changed.contains(room))
                .map(str::to_owned),
            Seen::Newest(newest) => {
                let (id, sent) = newest.get(room)?.as_ref()?;
                // The run that settled the room read it to its end after a `/sync` that showed
                // this same newest event, and the window it wrote was stored: nothing arrived
                // since.
                if cursor.is_some()
                    && settled_here
                    && state.newest.get(room).map(String::as_str) == Some(id.as_str())
                {
                    return cursor.map(str::to_owned);
                }
                // A room the stream has no event of, whose newest event is older than where its
                // read would start: nothing in it is new.
                let from = match cursor.map(TokenCursor::parse) {
                    None => window.start,
                    Some(TokenCursor::Time(time)) => time,
                    Some(TokenCursor::Token { .. }) => return None,
                };
                (*sent < from).then(|| window.unmoved(cursor))
            }
        }
    }
}

/// The `/sync` filter: no presence, no account data, no receipts or typing, none of the
/// `excluded` rooms, and of each other room only its newest event, with its name and alias when
/// `facts`.
fn sync_filter(facts: bool, excluded: &[&String]) -> Value {
    let state: &[&str] = if facts {
        &["m.room.name", "m.room.canonical_alias"]
    } else {
        &[]
    };
    json!({
        "presence": {"types": []},
        "account_data": {"types": []},
        "room": {
            "not_rooms": excluded,
            "timeline": {"limit": 1},
            "state": {"types": state},
            "ephemeral": {"types": []},
            "account_data": {"types": []},
        },
    })
}

fn next_batch(body: &Value) -> Result<String> {
    body.get("next_batch")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the homeserver's /sync answer has no next_batch"))
}

fn joined_entries(body: &Value) -> impl Iterator<Item = (&String, &Value)> {
    body.pointer("/rooms/join")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
}

/// The rooms with any event since `since`. `None` when the run must stop.
fn incremental(
    reader: &mut Reader<'_>,
    since: &str,
    excluded: &[&String],
) -> Result<Option<Prepass>> {
    let Ok(body) = reader.sync(Some(since), &sync_filter(false, excluded))? else {
        return Ok(None);
    };
    Ok(Some(Prepass {
        next_batch: Some(next_batch(&body)?),
        seen: Seen::Changed(
            joined_entries(&body)
                .map(|(room, _)| room.clone())
                .collect(),
        ),
        facts: BTreeMap::new(),
    }))
}

/// Each room's newest event, name and alias. `None` when the run must stop.
fn full(reader: &mut Reader<'_>, excluded: &[&String]) -> Result<Option<Prepass>> {
    let Ok(body) = reader.sync(None, &sync_filter(true, excluded))? else {
        return Ok(None);
    };
    let mut newest = BTreeMap::new();
    let mut facts = BTreeMap::new();
    for (room, entry) in joined_entries(&body) {
        let last = entry
            .pointer("/timeline/events")
            .and_then(Value::as_array)
            .and_then(|events| events.last())
            .and_then(|event| Some((event_id(event)?.to_owned(), sent_at(event)?)));
        newest.insert(room.clone(), last);
        let mut known = Facts::default();
        for event in entry
            .pointer("/state/events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = |name: &str| {
                event
                    .pointer(&format!("/content/{name}"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            match event.get("type").and_then(Value::as_str) {
                Some("m.room.name") => known.name = field("name"),
                Some("m.room.canonical_alias") => known.alias = field("alias"),
                _ => {}
            }
        }
        facts.insert(room.clone(), known);
    }
    Ok(Some(Prepass {
        next_batch: Some(next_batch(&body)?),
        seen: Seen::Newest(newest),
        facts,
    }))
}

/// What reading one room gave.
struct RoomRead {
    records: Vec<Record>,
    warnings: Vec<String>,
    cursor: String,
    /// Read to the end of the room with nothing held back.
    complete: bool,
    /// The run stopped while reading it.
    stopped: bool,
}

/// Reads one room: its name first, so a room `exclude_rooms` names by name is never read, then
/// its events from the cursor on.
fn read_room(
    reader: &mut Reader<'_>,
    scheme: &str,
    room: &str,
    cursor: Option<&str>,
    window: &Window,
    selection: &Selection,
    known: &mut BTreeMap<String, Facts>,
) -> Result<RoomRead> {
    let unread = |stopped| RoomRead {
        records: Vec::new(),
        warnings: Vec::new(),
        cursor: window.unmoved(cursor),
        complete: false,
        stopped,
    };
    let Ok(facts) = reader.facts(room, selection.names_aliases())? else {
        return Ok(unread(true));
    };
    known.insert(room.to_owned(), facts.clone());
    if selection.decide(room, Some(&facts)) == Decision::Skip {
        return Ok(unread(false));
    }
    let label = facts.label(room).to_owned();
    let mut walk = Walk {
        reader,
        room: Room {
            scheme,
            id: room,
            label: &label,
        },
        window,
        members: BTreeMap::new(),
        records: Vec::new(),
        index: BTreeMap::new(),
        originals: BTreeMap::new(),
        removed: BTreeSet::new(),
        warnings: Vec::new(),
        encrypted: 0,
        unserved: 0,
        position: Position::Cursor(cursor.map(str::to_owned)),
        held: false,
        stopped: false,
        reached_end: false,
        believable: latest_believable(window.end),
    };
    walk.start(cursor)?;
    Ok(walk.finish())
}

/// Where a room's read stands.
enum Position {
    /// Where the cursor it started from says.
    Cursor(Option<String>),
    /// After this event, sent at this time.
    Event { id: String, ts: DateTime<Utc> },
    /// Through this time, with no event to name.
    Time(DateTime<Utc>),
}

/// What a read does after one event.
enum Flow {
    Go,
    /// The event was sent after the window end: it and everything after it wait.
    Hold,
    /// The run stopped before the event could be read.
    Stop,
}

/// One room's read, event by event.
struct Walk<'w, 'c> {
    reader: &'w mut Reader<'c>,
    room: Room<'w>,
    window: &'w Window,
    /// Each sender's display name, or `None` when the room holds none.
    members: BTreeMap<String, Option<String>>,
    records: Vec<Record>,
    /// Where in `records` the record of each pointer is.
    index: BTreeMap<String, usize>,
    /// Each message event the read has met, and the time its record carries.
    originals: BTreeMap<String, (Value, DateTime<Utc>)>,
    /// The messages a redaction this read met removed.
    removed: BTreeSet<String>,
    warnings: Vec<String>,
    encrypted: usize,
    /// Edits and redactions of an event the homeserver no longer serves.
    unserved: usize,
    position: Position,
    held: bool,
    stopped: bool,
    reached_end: bool,
    /// The latest time an event the last page held can carry and still have been sent, taken
    /// just after that page was read: an event dated later is taken for a wrong clock and
    /// stored at the window end.
    believable: DateTime<Utc>,
}

impl Walk<'_, '_> {
    /// Reads on from the cursor: after its event, from its time, or from the backfill start.
    fn start(&mut self, cursor: Option<&str>) -> Result<()> {
        let room = self.room.id;
        match cursor.map(TokenCursor::parse) {
            Some(TokenCursor::Token {
                token,
                stored_through,
            }) => match self.reader.context(room, token)? {
                Err(_) => self.stopped = true,
                Ok(Some(context)) => {
                    self.learn(&context.state);
                    let end = context.end.ok_or_else(|| {
                        anyhow!("the homeserver's context of {token} in {room} has no end token")
                    })?;
                    self.forward(end)?;
                }
                Ok(None) => {
                    let from = stored_through.unwrap_or(self.window.start);
                    self.warnings.push(format!(
                        "the homeserver no longer serves {token}, the event this room's cursor names; reading from {}",
                        rfc3339(from)
                    ));
                    self.read_from_time(from)?;
                }
            },
            Some(TokenCursor::Time(time)) => self.read_from_time(time)?,
            None => self.read_from_time(self.window.start)?,
        }
        Ok(())
    }

    /// Reads from the first event sent at or after `time`.
    fn read_from_time(&mut self, time: DateTime<Utc>) -> Result<()> {
        let room = self.room.id;
        match self.reader.timestamp_to_event(room, time)? {
            Err(_) => self.stopped = true,
            Ok(Found::Nothing) => {
                self.position = Position::Time(time);
                self.reached_end = true;
            }
            Ok(Found::Unsupported) => self.backward(time)?,
            Ok(Found::Event(first)) => match self.reader.context(room, &first)? {
                Err(_) => self.stopped = true,
                Ok(None) => bail!(
                    "the homeserver named {first} as the first event in {room} at {} and then served no context for it",
                    rfc3339(time)
                ),
                Ok(Some(context)) => {
                    self.learn(&context.state);
                    if matches!(self.step(&context.event)?, Flow::Go) {
                        let end = context.end.ok_or_else(|| {
                            anyhow!("the homeserver's context of {first} in {room} has no end token")
                        })?;
                        self.forward(end)?;
                    }
                }
            },
        }
        Ok(())
    }

    /// Reads oldest first from `from` to the end of the timeline, or until an event must wait,
    /// the run stops, or the room has had its [`ROOM_PAGES`] pages this run.
    fn forward(&mut self, from: String) -> Result<()> {
        let mut from = Some(from);
        let mut pages = 0;
        while let Some(token) = from.take() {
            if pages == ROOM_PAGES {
                self.warnings.push(format!(
                    "this room was read for {ROOM_PAGES} pages, the most one run reads of a room; the next fetch reads on from where this one stopped"
                ));
                return Ok(());
            }
            pages += 1;
            let Ok(page) = self
                .reader
                .messages(self.room.id, Direction::Forward, Some(&token))?
            else {
                self.stopped = true;
                return Ok(());
            };
            self.learn(&page.state);
            for event in &page.chunk {
                if !matches!(self.step(event)?, Flow::Go) {
                    return Ok(());
                }
            }
            if !page.chunk.is_empty() {
                from = page.end;
            }
        }
        self.reached_end = true;
        Ok(())
    }

    /// Reads the room newest first from its end back past `time`, then oldest first, for a
    /// homeserver that does not serve `/timestamp_to_event`.
    fn backward(&mut self, time: DateTime<Utc>) -> Result<()> {
        let mut events = Vec::new();
        let mut from: Option<String> = None;
        let mut reached = false;
        for _ in 0..BACKWARD_PAGES {
            let Ok(page) =
                self.reader
                    .messages(self.room.id, Direction::Backward, from.as_deref())?
            else {
                self.stopped = true;
                return Ok(());
            };
            self.learn(&page.state);
            let last_page = page.chunk.is_empty() || page.end.is_none();
            for event in page.chunk {
                reached |= sent_at(&event).is_some_and(|sent| sent < time);
                events.push(event);
            }
            if reached || last_page {
                reached = true;
                break;
            }
            from = page.end;
        }
        if !reached {
            self.warnings.push(format!(
                "the homeserver does not serve /timestamp_to_event, and the newest {} pages of this room reach back only to {}: its older events since {} are not read",
                BACKWARD_PAGES,
                events
                    .last()
                    .and_then(sent_at)
                    .map_or_else(|| "its start".to_owned(), rfc3339),
                rfc3339(time)
            ));
        }
        events.reverse();
        for event in &events {
            match (event_id(event), sent_at(event)) {
                (Some(id), Some(sent)) if sent < time => {
                    self.position = Position::Event {
                        id: id.to_owned(),
                        ts: sent,
                    };
                }
                _ => {
                    if !matches!(self.step(event)?, Flow::Go) {
                        return Ok(());
                    }
                }
            }
        }
        self.reached_end = true;
        Ok(())
    }

    /// Learns what a page or context just read says: its senders' names, and the latest date
    /// one of its events can believably carry.
    fn learn(&mut self, state: &[Value]) {
        self.members.extend(members(state));
        self.believable = latest_believable(self.window.end);
    }

    /// Reads one event, and moves the position past it unless it must wait or the run stopped.
    fn step(&mut self, event: &Value) -> Result<Flow> {
        let (Some(id), Some(sent)) = (event_id(event), sent_at(event)) else {
            return Ok(Flow::Go);
        };
        let ts = if sent <= self.window.end {
            sent
        } else if sent <= self.believable {
            self.held = true;
            return Ok(Flow::Hold);
        } else {
            self.warnings.push(format!(
                "event {id} is dated {}, more than {} minutes after both the window end and the time this fetch read it; the date is taken for a wrong clock, so this window stores the event at its end ({}) and its raw data keeps the date the homeserver gave",
                rfc3339(sent),
                CLOCK_SKEW_MARGIN.num_minutes(),
                rfc3339(self.window.end)
            ));
            self.window.end
        };
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("m.room.message" | "m.sticker")
        ) {
            self.originals.insert(id.to_owned(), (event.clone(), ts));
        }
        let flow = match kind(event) {
            Kind::Message => self.message(event, ts)?,
            Kind::Removed => self.removal(event, event, ts)?,
            Kind::Edit { target, content } => self.edit(event, target, content)?,
            Kind::Redaction { target } => self.redaction(event, target)?,
            Kind::Encrypted => {
                self.encrypted += 1;
                Flow::Go
            }
            Kind::Other => Flow::Go,
        };
        if matches!(flow, Flow::Go) {
            self.position = Position::Event {
                id: id.to_owned(),
                ts,
            };
        } else {
            self.stopped = true;
        }
        Ok(flow)
    }

    fn message(&mut self, event: &Value, ts: DateTime<Utc>) -> Result<Flow> {
        let Ok(author) = self.author(event)? else {
            return Ok(Flow::Stop);
        };
        let content = event.get("content").unwrap_or(&Value::Null);
        if let Some(record) = self.room.record(event, content, event, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// Records `message` as removed, by `cause`.
    fn removal(&mut self, message: &Value, cause: &Value, ts: DateTime<Utc>) -> Result<Flow> {
        let Ok(author) = self.author(message)? else {
            return Ok(Flow::Stop);
        };
        if let Some(record) = self.room.removed(message, cause, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// The message `target` names, met in this read or fetched, and the time its record
    /// carries; `None` when the homeserver does not serve it.
    fn original(&mut self, target: &str) -> Step<Option<(Value, DateTime<Utc>)>> {
        if let Some(original) = self.originals.get(target) {
            return Ok(Ok(Some(original.clone())));
        }
        let fetched = match self.reader.event(self.room.id, target)? {
            Ok(fetched) => fetched,
            Err(stop) => return Ok(Err(stop)),
        };
        Ok(Ok(fetched.and_then(|event| {
            let ts = sent_at(&event)?.min(self.window.end);
            Some((event, ts))
        })))
    }

    /// Applies an edit: the message it names, with the new text, as Matrix's rules for a
    /// replacement allow it — the sender of both is the same, and the message is neither
    /// removed nor itself an edit.
    fn edit(&mut self, edit: &Value, target: &str, content: &Value) -> Result<Flow> {
        let Ok(found) = self.original(target)? else {
            return Ok(Flow::Stop);
        };
        let Some((original, ts)) = found else {
            self.unserved += 1;
            return Ok(Flow::Go);
        };
        if kind(&original) != Kind::Message
            || sender(&original) != sender(edit)
            || self.removed.contains(target)
        {
            return Ok(Flow::Go);
        }
        let Ok(author) = self.author(&original)? else {
            return Ok(Flow::Stop);
        };
        if let Some(record) = self.room.record(&original, content, edit, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// Applies a redaction: the message it names reads as removed. A redaction of anything
    /// but a message — a reaction, an edit — changes no record.
    fn redaction(&mut self, redaction: &Value, target: &str) -> Result<Flow> {
        let Ok(found) = self.original(target)? else {
            return Ok(Flow::Stop);
        };
        let Some((original, ts)) = found else {
            self.unserved += 1;
            return Ok(Flow::Go);
        };
        let is_message = matches!(
            original.get("type").and_then(Value::as_str),
            Some("m.room.message" | "m.sticker")
        );
        if !is_message || matches!(kind(&original), Kind::Edit { .. }) {
            return Ok(Flow::Go);
        }
        self.removed.insert(target.to_owned());
        self.removal(&original, redaction, ts)
    }

    /// The name of `event`'s sender: the display name the room holds for them, else their
    /// Matrix id.
    fn author(&mut self, event: &Value) -> Step<String> {
        let Some(user) = sender(event) else {
            return Ok(Ok(String::new()));
        };
        if let Some(name) = self.members.get(user) {
            return Ok(Ok(name.clone().unwrap_or_else(|| user.to_owned())));
        }
        let name = match self.reader.member_name(self.room.id, user)? {
            Ok(name) => name,
            Err(stop) => return Ok(Err(stop)),
        };
        self.members.insert(user.to_owned(), name.clone());
        Ok(Ok(name.unwrap_or_else(|| user.to_owned())))
    }

    /// Adds a record, or replaces the one for the same message, which keeps its place.
    fn put(&mut self, record: Record) {
        if let Some(slot) = self
            .index
            .get(&record.pointer)
            .and_then(|&at| self.records.get_mut(at))
        {
            *slot = record;
        } else {
            self.index
                .insert(record.pointer.clone(), self.records.len());
            self.records.push(record);
        }
    }

    fn finish(mut self) -> RoomRead {
        if self.encrypted > 0 {
            self.warnings.push(format!(
                "{} encrypted event(s) skipped: ctx holds no keys to read them",
                self.encrypted
            ));
        }
        if self.unserved > 0 {
            self.warnings.push(format!(
                "{} edit(s) or redaction(s) skipped: each names an event the homeserver no longer serves",
                self.unserved
            ));
        }
        let cursor = match self.position {
            Position::Cursor(cursor) => self.window.unmoved(cursor.as_deref()),
            Position::Event { id, ts } => token_cursor(&id, ts),
            Position::Time(time) => rfc3339(time),
        };
        RoomRead {
            complete: self.reached_end && !self.held && !self.stopped,
            records: self.records,
            warnings: self.warnings,
            cursor,
            stopped: self.stopped,
        }
    }
}
