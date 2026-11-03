//! A Matrix fetch: which rooms changed since the last run, and each changed room read forward
//! from its cursor.
//!
//! One room is one stream, and its cursor is the last event the stream holds with the time it
//! was sent, `<event id>@<rfc3339>`, or a bare time while the room has given it no event. A run
//! reads a room from just after that event (`/context`, then `/messages` oldest first), so an
//! event a bridge delivers late with an old timestamp is read too, and a room further behind
//! than one run's budget moves its cursor as far as the run read. A room the stream has no
//! event of yet is read from the first event dated at or after the backfill start
//! (`/timestamp_to_event`), and from as far before it in the timeline as events dated after
//! that time reach: a homeserver finds that event by date, and a late delivery sits after
//! events dated later than it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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

use super::homeserver::{members, Direction, Found, Reader};
use super::records::{
    author_line, event_id, is_event_id, is_room_id, kind, sender, sent_at, Kind, Room,
};
use super::rooms::{Decision, Facts, Selection};
use super::state::State;
use super::{MatrixConfig, SyncMode};

/// How many pages a read goes back to find where it starts: from a room's end when the
/// homeserver does not serve `/timestamp_to_event`, else from the first event dated at or after
/// the start, for events placed before it and dated after that time.
const BACKWARD_PAGES: u32 = 20;
/// How many pages of a room one run reads forward. A room further behind goes on from there in
/// the next run, so one busy room neither holds a whole room's backlog in memory nor spends the
/// budget every other room needs.
const ROOM_PAGES: u32 = 50;
/// How many events and member names one run looks up for a room: the message an edit or a
/// redaction names, and a sender no page named. Any member can send edits of made-up events, so
/// a room that asks for more goes on from there in the next run.
const ROOM_LOOKUPS: u32 = 100;
/// How long the source remembers an event it read that made no record of its own.
pub(super) const UNRECORDED_DAYS: i64 = 90;

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
    let (state, mut warnings) = State::load(&request.state_dir)?;
    let prefix = format!("{scheme}:");
    let unnamable = joined.iter().filter(|room| !is_room_id(room)).count();
    if unnamable > 0 {
        warnings.push(format!(
            "{unnamable} joined room(s) have an id that is not a Matrix room id a pointer can name; they are not read"
        ));
    }
    for id in selection.allowed_ids() {
        if !joined.contains(id) {
            warnings.push(format!(
                "configured room {id} is not a room this account has joined; skipping"
            ));
        }
    }
    // Only what needs no room state leaves a room out of the `/sync`: an id entry, or a
    // pattern that covers the whole room. A name or an alias decides on the state the `/sync`
    // gives, or that the room's read reads first, so a renamed room is decided again.
    let excluded: Vec<&String> = joined
        .iter()
        .filter(|room| left_out(room, None, selection, &exclusion, &prefix))
        .collect();
    let prepass = Prepass::run(&mut reader, config.sync, &state, &excluded, &mut warnings);
    let planned: Vec<String> = joined
        .iter()
        .filter(|room| {
            is_room_id(room)
                && !left_out(
                    room,
                    prepass.facts.get(room.as_str()),
                    selection,
                    &exclusion,
                    &prefix,
                )
        })
        .cloned()
        .collect();
    hello(scheme, &planned, warnings, sink)?;

    let mut pass = Pass {
        reader,
        state,
        window: Window::new(request),
        prepass,
        selection,
        scheme,
        cursors: &request.cursors,
        first_stopped: None,
        unread: 0,
    };
    // The run starts at the room the last run's budget or the homeserver's rate limit stopped
    // at and goes round, so every room is read in turn.
    let start = pass
        .state
        .first
        .take()
        .and_then(|first| planned.iter().position(|room| *room == first))
        .unwrap_or(0);
    for room in planned.iter().cycle().skip(start).take(planned.len()) {
        pass.room(room, sink)?;
    }
    pass.finish(&planned, config.sync, &request.state_dir, sink)
}

/// Whether a run leaves `room` out: an exclusion pattern covers the whole room, or `rooms` and
/// `exclude_rooms` leave it out by its id, or by `facts`, its name and alias, when known.
fn left_out(
    room: &str,
    facts: Option<&Facts>,
    selection: &Selection,
    exclusion: &Exclusion,
    prefix: &str,
) -> bool {
    exclusion.covers(&format!("{prefix}{room}/")) || selection.decide(room, facts) == Decision::Skip
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

/// One run over the planned rooms.
struct Pass<'p, 'c> {
    reader: Reader<'c>,
    state: State,
    window: Window,
    prepass: Prepass,
    selection: &'p Selection,
    scheme: &'p str,
    cursors: &'p BTreeMap<String, String>,
    /// The first room the run stopped at, which the next run reads first.
    first_stopped: Option<String>,
    /// How many rooms the run did not read because it stopped.
    unread: usize,
}

impl Pass<'_, '_> {
    /// Ends `room`'s stream: unread when the run has stopped, at its cursor when the `/sync`
    /// shows nothing new, else after reading it.
    fn room(&mut self, room: &str, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        let cursor = self.cursors.get(room).map(String::as_str);
        if self.reader.stopped().is_some() {
            self.unread += 1;
            self.first_stopped.get_or_insert_with(|| room.to_owned());
            self.state.unsettle(room);
            let unmoved = self.window.unmoved(cursor);
            return emit_stream(room, Ok((Vec::new(), Vec::new(), unmoved)), sink);
        }
        if let Some(cursor) = self.prepass.settled(room, cursor, &self.state) {
            self.state.settle(room, &cursor, self.prepass.newest(room));
            return emit_stream(room, Ok((Vec::new(), Vec::new(), cursor)), sink);
        }
        match self.read(room, cursor) {
            Ok(read) => {
                if read.complete {
                    self.state
                        .settle(room, &read.cursor, self.prepass.newest(room));
                } else {
                    self.state.unsettle(room);
                }
                if read.stopped {
                    self.first_stopped.get_or_insert_with(|| room.to_owned());
                }
                emit_stream(room, Ok((read.warnings, read.records, read.cursor)), sink)
            }
            Err(error) => {
                self.state.unsettle(room);
                if self.reader.stopped().is_some() {
                    self.first_stopped.get_or_insert_with(|| room.to_owned());
                }
                emit_stream(room, Err(error), sink)
            }
        }
    }

    /// Reads one room: its name first — from this run's full `/sync`, else from the room's
    /// state — so a room `exclude_rooms` names by name or alias has no message read, then its
    /// events from the cursor on.
    fn read(&mut self, room: &str, cursor: Option<&str>) -> Result<RoomRead> {
        let unread = |window: &Window, stopped| RoomRead {
            records: Vec::new(),
            warnings: Vec::new(),
            cursor: window.unmoved(cursor),
            complete: false,
            stopped,
        };
        let facts = if let Some(facts) = self.prepass.facts.get(room) {
            facts.clone()
        } else {
            let Ok(facts) = self.reader.facts(room, self.selection.names_aliases())? else {
                return Ok(unread(&self.window, true));
            };
            facts
        };
        if self.selection.decide(room, Some(&facts)) == Decision::Skip {
            return Ok(unread(&self.window, false));
        }
        let label = facts.label(room).to_owned();
        let mut walk = Walk {
            reader: &mut self.reader,
            room: Room {
                scheme: self.scheme,
                id: room,
                label: &label,
            },
            window: &self.window,
            unrecorded: &mut self.state.unrecorded,
            members: BTreeMap::new(),
            records: Vec::new(),
            index: BTreeMap::new(),
            originals: BTreeMap::new(),
            removed: BTreeSet::new(),
            warnings: Vec::new(),
            encrypted: 0,
            unserved: 0,
            unnamable: 0,
            lookups: 0,
            position: Position::Cursor(cursor.map(str::to_owned)),
            met: BTreeSet::new(),
            halt: None,
            reached_end: false,
            believable: latest_believable(self.window.end),
        };
        walk.start(cursor)?;
        Ok(walk.finish())
    }

    /// Warns of the rooms a stop left unread and keeps what the next run needs.
    fn finish(
        mut self,
        planned: &[String],
        sync: SyncMode,
        dir: &Path,
        sink: &mut dyn FnMut(Message) -> Result<()>,
    ) -> Result<()> {
        if let Some(stop) = self.reader.stopped() {
            sink(Message::Warning {
                stream: None,
                message: format!(
                    "{}: {} room(s) were not read this run and keep their cursors; the next fetch reads them first",
                    stop.why(),
                    self.unread
                ),
            })?;
        }
        self.state.first = self.first_stopped;
        self.state
            .retain(planned, self.window.end - Duration::days(UNRECORDED_DAYS));
        self.state.keep_sync(sync, self.prepass.next_batch);
        self.state.save(dir)
    }
}

/// What the run's `/sync` said before it read any room.
struct Prepass {
    seen: Seen,
    /// The token the next incremental `/sync` starts from.
    next_batch: Option<String>,
    /// The name and alias of each room a full `/sync` listed, as they stand now.
    facts: BTreeMap<String, Facts>,
}

enum Seen {
    /// No `/sync` ran, or it failed: every room is read.
    Nothing,
    /// An incremental `/sync`: the rooms with any event since the stored token.
    Changed(BTreeSet<String>),
    /// A full `/sync`: the id of each room's newest event of any type, or `None` for a room
    /// whose timeline came back empty.
    Newest(BTreeMap<String, Option<String>>),
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
            Seen::Newest(newest) => newest.get(room).and_then(Option::as_deref),
            Seen::Nothing | Seen::Changed(_) => None,
        }
    }

    /// The cursor a room ends at without being read: the run that last read it to its end,
    /// holding nothing back, proposed the cursor it still has, and the `/sync` shows nothing
    /// since. A room no run has settled is always read.
    ///
    /// Both kinds of `/sync` ask for each room's newest event of any type, not its newest
    /// message: a homeserver may filter a timeline by type after it has cut it to its newest
    /// events, so a room whose newest event is a reaction can come back with no message at all.
    fn settled(&self, room: &str, cursor: Option<&str>, state: &State) -> Option<String> {
        let cursor = cursor?;
        if state.settled.get(room).map(String::as_str) != Some(cursor) {
            return None;
        }
        let unchanged = match &self.seen {
            Seen::Nothing => false,
            Seen::Changed(changed) => !changed.contains(room),
            Seen::Newest(newest) => newest
                .get(room)
                .and_then(Option::as_deref)
                .is_some_and(|id| state.newest.get(room).map(String::as_str) == Some(id)),
        };
        unchanged.then(|| cursor.to_owned())
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

/// The events of one array of a `/sync` room entry.
fn entry_events<'e>(entry: &'e Value, pointer: &str) -> &'e [Value] {
    entry
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
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
        let timeline = entry_events(entry, "/timeline/events");
        newest.insert(
            room.clone(),
            timeline.last().and_then(event_id).map(str::to_owned),
        );
        // The state comes as it stood before the timeline's events, so a rename that is the
        // room's newest event comes in the timeline.
        let mut known = Facts::default();
        for event in entry_events(entry, "/state/events").iter().chain(timeline) {
            if event.get("state_key").and_then(Value::as_str) != Some("") {
                continue;
            }
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

/// Where a room's read stands.
enum Position {
    /// Where the cursor it started from says.
    Cursor(Option<String>),
    /// After this event, stored through this time.
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
    /// The room has had its [`ROOM_LOOKUPS`] this run: the event and everything after it wait
    /// for the next fetch.
    Pause,
}

/// Why a room's read ended before the room's end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Halt {
    /// An event was sent after the window end.
    Held,
    /// The run stopped: its budget, the homeserver's rate limit, or no answer.
    Stopped,
    /// The room had its [`ROOM_LOOKUPS`] this run.
    Paused,
}

/// An event a read looked up, with the time its record carries, or `None` when the homeserver
/// does not serve it.
type Looked = Option<(Value, DateTime<Utc>)>;

/// One room's read, event by event.
struct Walk<'w, 'c> {
    reader: &'w mut Reader<'c>,
    room: Room<'w>,
    window: &'w Window,
    /// The message events read, in this run or an earlier one, that made no record of their
    /// own, with the end of the window that read each.
    unrecorded: &'w mut BTreeMap<String, DateTime<Utc>>,
    /// Each sender's display name, or `None` when the room holds none.
    members: BTreeMap<String, Option<String>>,
    records: Vec<Record>,
    /// Where in `records` the record of each pointer is.
    index: BTreeMap<String, usize>,
    /// Each message event the read has met or looked up.
    originals: BTreeMap<String, Looked>,
    /// The messages a redaction this read met removed.
    removed: BTreeSet<String>,
    warnings: Vec<String>,
    encrypted: usize,
    /// Edits and redactions of an event the homeserver does not serve.
    unserved: usize,
    /// Events whose own id is not one a pointer can name.
    unnamable: usize,
    lookups: u32,
    position: Position,
    /// The message events this read met in the room's timeline.
    met: BTreeSet<String>,
    /// Why the read ended before the room's end, when it did.
    halt: Option<Halt>,
    reached_end: bool,
    /// The latest time an event the last page held can carry and still have been sent, taken
    /// just after that page was read: an event dated later is taken for a wrong clock and
    /// stored at the window end.
    believable: DateTime<Utc>,
}

fn is_message_type(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("m.room.message" | "m.sticker")
    )
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
                Err(_) => self.halt = Some(Halt::Stopped),
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

    /// Reads from `time`: from the first event dated at or after it, and from the events placed
    /// before that one that are dated after it, which a bridge's late delivery puts there.
    fn read_from_time(&mut self, time: DateTime<Utc>) -> Result<()> {
        let room = self.room.id;
        match self.reader.timestamp_to_event(room, time)? {
            Err(_) => self.halt = Some(Halt::Stopped),
            Ok(Found::Nothing) => {
                self.position = Position::Time(time);
                self.reached_end = true;
            }
            Ok(Found::Unsupported) => self.read_back_from_end(time)?,
            Ok(Found::Event(first)) => {
                match self.reader.context(room, &first)? {
                    Err(_) => self.halt = Some(Halt::Stopped),
                    Ok(None) => {
                        self.warnings.push(format!(
                        "the homeserver named {first} as the first event at {} and then served no context for it; reading back from the room's end",
                        rfc3339(time)
                    ));
                        self.read_back_from_end(time)?;
                    }
                    Ok(Some(context)) => {
                        self.learn(&context.state);
                        let earlier = match context.start {
                            Some(start) => match self.back(Some(start), time)? {
                                Some(events) => events,
                                None => return Ok(()),
                            },
                            None => Vec::new(),
                        };
                        if self.replay(&earlier, time)?
                            && matches!(self.step(&context.event)?, Flow::Go)
                        {
                            let end = context.end.ok_or_else(|| {
                            anyhow!("the homeserver's context of {first} in {room} has no end token")
                        })?;
                            self.forward(end)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads back from the room's end past `time`, then on from there, for a homeserver that
    /// cannot say where `time` falls in the room.
    fn read_back_from_end(&mut self, time: DateTime<Utc>) -> Result<()> {
        if let Some(events) = self.back(None, time)? {
            if self.replay(&events, time)? {
                self.reached_end = true;
            }
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
                self.halt = Some(Halt::Stopped);
                return Ok(());
            };
            self.learn(&page.state);
            for event in &page.chunk {
                if !matches!(self.step(event)?, Flow::Go) {
                    return Ok(());
                }
            }
            // A homeserver leaves out what the user may not see after it has taken a page, so
            // an empty page can have more after it: only a page with no `end` is the room's end.
            from = page.end.filter(|end| *end != token);
        }
        self.reached_end = true;
        Ok(())
    }

    /// Pages back from `from`, or from the room's end, until a page holds only events dated
    /// before `time` or the room's start, for at most [`BACKWARD_PAGES`] pages. The events it
    /// read come back oldest first; `None` when the run stopped.
    fn back(&mut self, from: Option<String>, time: DateTime<Utc>) -> Result<Option<Vec<Value>>> {
        let mut events = Vec::new();
        let mut from = from;
        for _ in 0..BACKWARD_PAGES {
            let Ok(page) =
                self.reader
                    .messages(self.room.id, Direction::Backward, from.as_deref())?
            else {
                self.halt = Some(Halt::Stopped);
                return Ok(None);
            };
            self.learn(&page.state);
            let older = !page.chunk.is_empty()
                && page
                    .chunk
                    .iter()
                    .all(|event| sent_at(event).is_some_and(|sent| sent < time));
            events.extend(page.chunk);
            let next = page.end.filter(|end| Some(end) != from.as_ref());
            if older || next.is_none() {
                events.reverse();
                return Ok(Some(events));
            }
            from = next;
        }
        self.warnings.push(format!(
            "{BACKWARD_PAGES} pages back from where this room's read starts reach only to {}: earlier events dated since {} are not read",
            events
                .last()
                .and_then(sent_at)
                .map_or_else(|| "its start".to_owned(), rfc3339),
            rfc3339(time)
        ));
        events.reverse();
        Ok(Some(events))
    }

    /// Reads `events`, oldest first, leaving out those dated before `time`, which are older
    /// than where the read starts though a bridge delivered them after it. `false` when the
    /// read must stop.
    fn replay(&mut self, events: &[Value], time: DateTime<Utc>) -> Result<bool> {
        for event in events {
            match (event_id(event), sent_at(event)) {
                (Some(id), Some(sent)) if sent < time && is_event_id(id) => {
                    self.position = Position::Event {
                        id: id.to_owned(),
                        ts: time,
                    };
                }
                _ => {
                    if !matches!(self.step(event)?, Flow::Go) {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// Learns what a page or context just read says: its senders' names, and the latest date
    /// one of its events can believably carry.
    fn learn(&mut self, state: &[Value]) {
        self.members.extend(members(state));
        self.believable = latest_believable(self.window.end);
    }

    /// Reads one event, and moves the position past it unless it must wait or the read stops.
    fn step(&mut self, event: &Value) -> Result<Flow> {
        let (Some(id), Some(sent)) = (event_id(event), sent_at(event)) else {
            return Ok(Flow::Go);
        };
        if !is_event_id(id) {
            // No pointer can name it, and no cursor either: the read goes past it in place.
            self.unnamable += 1;
            return Ok(Flow::Go);
        }
        let ts = if sent <= self.window.end {
            sent
        } else if sent <= self.believable {
            self.halt = Some(Halt::Held);
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
        let message = is_message_type(event);
        if message {
            self.originals
                .insert(id.to_owned(), Some((event.clone(), ts)));
            self.met.insert(id.to_owned());
        }
        let flow = match kind(event) {
            Kind::Message => self.message(event, ts)?,
            // Served already redacted: the stream never read its text, so no record of it
            // exists to change.
            Kind::Removed => Flow::Go,
            Kind::Edit { target, content } => {
                self.unrecorded.insert(id.to_owned(), self.window.end);
                self.edit(event, target, content)?
            }
            Kind::Redaction { target } => self.redaction(event, target)?,
            Kind::Encrypted => {
                self.encrypted += 1;
                Flow::Go
            }
            Kind::Other => {
                if message {
                    self.unrecorded.insert(id.to_owned(), self.window.end);
                }
                Flow::Go
            }
        };
        match flow {
            Flow::Go => {
                self.position = Position::Event {
                    id: id.to_owned(),
                    ts,
                };
            }
            Flow::Stop => self.halt = Some(Halt::Stopped),
            Flow::Pause => self.halt = Some(Halt::Paused),
            Flow::Hold => self.halt = Some(Halt::Held),
        }
        Ok(flow)
    }

    fn message(&mut self, event: &Value, ts: DateTime<Utc>) -> Result<Flow> {
        let author = match self.author(event)? {
            Ok(author) => author,
            Err(flow) => return Ok(flow),
        };
        let content = event.get("content").unwrap_or(&Value::Null);
        if let Some(record) = self.room.record(event, content, event, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// The event `target` names, as this read met or looked it up when `cached`, else as the
    /// homeserver serves it now, with the time its record carries. `Err` carries the flow when
    /// the read cannot look it up: the run stopped, or the room has had its lookups.
    fn lookup(&mut self, target: &str, cached: bool) -> Result<Result<Looked, Flow>> {
        if cached {
            if let Some(found) = self.originals.get(target) {
                return Ok(Ok(found.clone()));
            }
        }
        if self.lookups == ROOM_LOOKUPS {
            return Ok(Err(Flow::Pause));
        }
        self.lookups += 1;
        let Ok(fetched) = self.reader.related_event(self.room.id, target)? else {
            return Ok(Err(Flow::Stop));
        };
        let found = fetched.and_then(|event| {
            let ts = sent_at(&event)?.min(self.window.end);
            Some((event, ts))
        });
        self.originals.insert(target.to_owned(), found.clone());
        Ok(Ok(found))
    }

    /// Applies an edit: the message it names, with the new text, as Matrix's rules for a
    /// replacement allow it — the sender of both is the same, and the message is neither
    /// removed nor itself an edit.
    fn edit(&mut self, edit: &Value, target: &str, content: &Value) -> Result<Flow> {
        let found = match self.lookup(target, true)? {
            Ok(found) => found,
            Err(flow) => return Ok(flow),
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
        let author = match self.author(&original)? {
            Ok(author) => author,
            Err(flow) => return Ok(flow),
        };
        if let Some(record) = self.room.record(&original, content, edit, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// Applies a redaction: the message it names reads as removed, when the homeserver serves
    /// that message redacted now. A redaction the homeserver did not apply — its sender may not
    /// redact that message — changes nothing, and neither does one of an event that made no
    /// record of its own (an edit, a reaction) or of a message this read met already redacted.
    fn redaction(&mut self, redaction: &Value, target: &str) -> Result<Flow> {
        if self.unrecorded.contains_key(target) {
            return Ok(Flow::Go);
        }
        // This read met the message in the timeline: its record is this read's own, or none at
        // all when the homeserver served it already redacted.
        let met = if self.met.contains(target) {
            self.originals.get(target).cloned().flatten()
        } else {
            None
        };
        if met
            .as_ref()
            .is_some_and(|(event, _)| kind(event) == Kind::Removed)
        {
            return Ok(Flow::Go);
        }
        let current = match self.lookup(target, false)? {
            Ok(current) => current,
            Err(flow) => return Ok(flow),
        };
        let Some((current, ts)) = current else {
            self.unserved += 1;
            return Ok(Flow::Go);
        };
        if !is_message_type(&current) || kind(&current) != Kind::Removed {
            return Ok(Flow::Go);
        }
        let (message, ts) = met.unwrap_or((current, ts));
        self.removed.insert(target.to_owned());
        let author = match self.author(&message)? {
            Ok(author) => author,
            Err(flow) => return Ok(flow),
        };
        if let Some(record) = self.room.removed(&message, redaction, &author, ts) {
            self.put(record);
        }
        Ok(Flow::Go)
    }

    /// How a record names `event`'s sender ([`author_line`]), with the display name the room
    /// holds for them.
    fn author(&mut self, event: &Value) -> Result<Result<String, Flow>> {
        let Some(user) = sender(event) else {
            return Ok(Ok(String::new()));
        };
        if let Some(name) = self.members.get(user) {
            return Ok(Ok(author_line(name.as_deref(), user)));
        }
        if self.lookups == ROOM_LOOKUPS {
            return Ok(Err(Flow::Pause));
        }
        self.lookups += 1;
        let Ok(name) = self.reader.member_name(self.room.id, user)? else {
            return Ok(Err(Flow::Stop));
        };
        let line = author_line(name.as_deref(), user);
        self.members.insert(user.to_owned(), name);
        Ok(Ok(line))
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
                "{} edit(s) or redaction(s) skipped: each names an event the homeserver does not serve to this account",
                self.unserved
            ));
        }
        if self.unnamable > 0 {
            self.warnings.push(format!(
                "{} event(s) skipped: their id is not a Matrix event id a pointer can name",
                self.unnamable
            ));
        }
        if self.halt == Some(Halt::Paused) {
            self.warnings.push(format!(
                "this room's read stopped after {ROOM_LOOKUPS} lookups of the events its edits and redactions name and of senders no page named, the most one run makes for a room; the next fetch reads on from where this one stopped"
            ));
        }
        let cursor = match self.position {
            Position::Cursor(cursor) => self.window.unmoved(cursor.as_deref()),
            Position::Event { id, ts } => token_cursor(&id, ts),
            Position::Time(time) => rfc3339(time),
        };
        RoomRead {
            complete: self.reached_end && self.halt.is_none(),
            records: self.records,
            warnings: self.warnings,
            cursor,
            stopped: self.halt == Some(Halt::Stopped),
        }
    }
}
