//! Built-in Google Calendar source.
//!
//! Each configured calendar is one stream whose cursor is a Calendar sync token: a fetch asks
//! for what changed since the last run, and a first fetch lists every event from the backfill
//! window onwards. Either way it records no instance of a recurring series whose start and slot
//! in the series both lie [`HORIZON`] or more past the window end, reckoned to the second, unless
//! it recorded that instance before, and lists no event that far ahead by time. A record's
//! timestamp is the event's start, so an event that starts after the window ends is a context
//! record. A cancelled event it recorded before is recorded again as cancelled (`Emitted`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::ops::{Range, RangeBounds};
use std::path::Path;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{
    DateTime, Duration, NaiveDate, NaiveDateTime, SecondsFormat, SubsecRound as _, TimeZone as _,
    Utc,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client;
use crate::api::google::{self, Google};
use crate::atomic::write_replace;
use crate::exclude::Exclusion;
use crate::record::Record;

use super::{
    token_cursor, window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source,
    TokenCursor, PROTOCOL_VERSION,
};

/// How far past its window end a fetch records a calendar's events: 400 days, a year and a
/// month to spare.
///
/// A listing expands each recurring series into its instances, and a series with no end runs for
/// decades. Reading this far keeps a trip booked for next year and the next instance or two of a
/// yearly series, and stops every other series here. A listing by time asks for nothing later; a
/// sync token takes no time bounds, so from its listing only a series' instances whose start and
/// slot both lie past the horizon are dropped, unless the source recorded them before. A one-off
/// event it names, which has changed, is recorded wherever it now starts, and so is an occurrence
/// moved from a slot within reach or one recorded before. A fetch that follows a sync token also
/// makes the horizon listing: the events that came within this reach since its cursor's window,
/// which the token does not name unless they changed. So each series yields its next instance as
/// the horizon reaches it.
///
/// Where the reach of the fetch that wrote a cursor ended is reckoned from the cursor's time and
/// this value, so raising it leaves the events between the old reach and the new one unrecorded
/// until they change, unless each calendar's cursor is set to the time it is stored through.
const HORIZON: Duration = Duration::days(400);

/// How much later than the horizon, and earlier than where the horizon listing starts, a listing
/// by time asks Google to reach.
///
/// A record's start is an all-day event's date at midnight UTC, and Google places that date at
/// midnight in the calendar's time zone, from 12 hours behind UTC to 14 ahead. The margin lets
/// Google's bounds hold every event whose start lies inside the span this source keeps.
const LISTING_MARGIN: Duration = Duration::days(1);

/// Built-in source for Google Calendar events.
pub struct CalendarSource;

impl CalendarSource {
    /// Creates a Calendar source that calls the Calendar API.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for CalendarSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for CalendarSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                emit_fetch_hello(request, &config, sink)?;
                let api = GoogleCalendarApi::load()?;
                fetch_with_api(request, &config, &api, sink)
            }
            Request::Resolve(request) => {
                let _config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CalendarConfig {
    calendars: Vec<String>,
}

fn parse_config(value: &Value) -> Result<CalendarConfig> {
    let config: CalendarConfig =
        serde_json::from_value(value.clone()).context("parsing Calendar source configuration")?;
    if config.calendars.is_empty() {
        bail!("Calendar source calendars must not be empty");
    }
    let mut calendars = BTreeSet::new();
    for calendar in &config.calendars {
        if calendar.is_empty() {
            bail!("Calendar source calendar id must not be empty");
        }
        if !calendars.insert(calendar) {
            bail!("Calendar source calendar {calendar:?} appears more than once");
        }
    }
    Ok(config)
}

/// Every event one listing returned, and the token that follows it.
#[derive(Clone, Debug, Default)]
struct EventPage {
    items: Vec<Value>,
    next_sync_token: Option<String>,
    /// Google refused the sync token, so the listing has to start again.
    expired: bool,
}

trait CalendarApi {
    /// Events matching `query`, following every page.
    fn events(&self, calendar: &str, query: &[(&str, String)]) -> Result<EventPage>;
}

struct GoogleCalendarApi {
    google: Google,
}

impl GoogleCalendarApi {
    fn load() -> Result<Self> {
        Ok(Self {
            google: Google::load()?,
        })
    }
}

impl CalendarApi for GoogleCalendarApi {
    fn events(&self, calendar: &str, query: &[(&str, String)]) -> Result<EventPage> {
        let path = format!(
            "calendar/v3/calendars/{}/events",
            client::percent_encode(calendar)
        );
        let mut page = EventPage::default();
        let mut page_token: Option<String> = None;
        let mut seen_tokens = Vec::new();
        loop {
            let mut page_query = query.to_vec();
            page_query.push(("maxResults", google::page_size().to_string()));
            if let Some(token) = &page_token {
                page_query.push(("pageToken", token.clone()));
            }
            let (status, body) = self.google.reply(&path, &page_query)?;
            if status == 410 {
                return Ok(EventPage {
                    expired: true,
                    ..EventPage::default()
                });
            }
            if !(200..300).contains(&status) {
                bail!("Calendar {calendar} returned {status}: {body}");
            }
            if let Some(items) = body.get("items").and_then(Value::as_array) {
                page.items.extend(items.iter().cloned());
            }
            if let Some(token) = body
                .get("nextSyncToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
            {
                page.next_sync_token = Some(token.to_owned());
            }
            page_token = body
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned);
            match &page_token {
                Some(token) if seen_tokens.contains(token) => {
                    bail!("Calendar {calendar} repeated page token {token:?}");
                }
                Some(token) => seen_tokens.push(token.clone()),
                None => break,
            }
        }
        Ok(page)
    }
}

fn emit_fetch_hello(
    request: &FetchRequest,
    config: &CalendarConfig,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: false,
        streams: config.calendars.clone(),
    })
}

fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: false,
        streams: Vec::new(),
    })
}

/// The file a fetch lists the events it recorded in, under the source's state directory.
const EMITTED_FILE: &str = "emitted.json";

/// The text of the record that replaces a cancelled event's, as a removed chat message reads
/// `[message removed]`.
const CANCELLED: &str = "[event cancelled]";

/// The events a calendar's fetches recorded, each with the start its latest record has.
type Recorded = BTreeMap<String, DateTime<Utc>>;

/// What each calendar's fetches recorded, kept in `<state_dir>/emitted.json`.
///
/// A fetch has no view of the store, and Google names a cancelled event by its id, often with
/// no start. This list tells the cancellation of an event the tree holds, which is recorded,
/// from that of an event never stored, which is not; and the move of an event the tree holds,
/// which is recorded wherever it now starts, from an instance of a series past the horizon,
/// which is not. An entry is forgotten once its event started more than [`HORIZON`] before the
/// window end, so the list holds the events recorded within the horizon either side of the
/// window and the events moved past it, never a series' decades.
#[derive(Default, Deserialize, Serialize)]
struct Emitted {
    #[serde(default)]
    calendars: BTreeMap<String, CalendarList>,
}

/// One calendar's entries.
///
/// A fetch saves its entries before its window is stored, and a window may never be stored. So
/// they are kept beside those of the windows known to be stored, with the cursor the fetch
/// proposed. A fetch that starts from that cursor knows the window was stored and takes them as
/// they are. A fetch that starts from another cannot tell: the window may never have been stored,
/// or stored with its cursor left behind, as a fetch given an older window end leaves it. It
/// keeps every entry, each at the later of its two starts: a spurious `[event cancelled]` record
/// costs less than a cancelled meeting the tree shows as still on.
#[derive(Deserialize, Serialize)]
struct CalendarList {
    #[serde(default)]
    stored: Recorded,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposed: Option<Proposed>,
}

/// The entries of the last fetch that saved them, and the cursor it proposed.
#[derive(Deserialize, Serialize)]
struct Proposed {
    cursor: String,
    recorded: Recorded,
}

impl CalendarList {
    /// The entries of the windows that may have been stored, as a fetch from `cursor` finds them.
    fn stored_as_of(&self, cursor: Option<&str>) -> Recorded {
        let Some(proposed) = &self.proposed else {
            return self.stored.clone();
        };
        if Some(proposed.cursor.as_str()) == cursor {
            return proposed.recorded.clone();
        }
        let mut stored = self.stored.clone();
        for (id, start) in &proposed.recorded {
            stored
                .entry(id.clone())
                .and_modify(|held| *held = (*held).max(*start))
                .or_insert(*start);
        }
        stored
    }
}

impl Emitted {
    /// Reads the list. One this build cannot read is dropped with a warning rather than failing
    /// the fetch: each calendar then does without it, as one with a cursor and no list does.
    fn load(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join(EMITTED_FILE);
        let unread = |error: &dyn std::fmt::Display| {
            format!(
                "{} cannot be read ({error}); a cancellation of an event stored before this fetch is not recorded, and a move of one past the horizon is followed only from its slot",
                path.display()
            )
        };
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(list) => (list, None),
                Err(error) => (Self::default(), Some(unread(&error))),
            },
            Err(error) if error.kind() == ErrorKind::NotFound => (Self::default(), None),
            Err(error) => (Self::default(), Some(unread(&error))),
        }
    }

    /// Saves `calendar`'s entries, and every other calendar's as the file has them now.
    fn save(dir: &Path, calendar: &str, list: CalendarList) -> Result<()> {
        let (mut emitted, _) = Self::load(dir);
        emitted.calendars.insert(calendar.to_owned(), list);
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let bytes = serde_json::to_vec_pretty(&emitted)
            .context("serializing the recorded Calendar events")?;
        write_replace(&dir.join(EMITTED_FILE), &bytes)
    }
}

/// One calendar's records, warnings, and the cursor that follows them, and the events it has
/// recorded since.
#[derive(Default)]
struct Collected {
    records: Vec<Record>,
    warnings: Vec<String>,
    cursor: String,
    recorded: Recorded,
}

/// The start before which a recorded event is forgotten: [`HORIZON`] before the window end, so
/// the list does not grow for decades. The cancellation of an event that old is not recorded.
fn forget_before(request: &FetchRequest) -> DateTime<Utc> {
    request.window_end.trunc_subsecs(0) - HORIZON
}

fn fetch_with_api<C: CalendarApi + ?Sized>(
    request: &FetchRequest,
    config: &CalendarConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let exclusion = Exclusion::new(&request.exclude)?;
    let (emitted, warning) = Emitted::load(&request.state_dir);
    if let Some(message) = warning {
        sink(Message::Warning {
            stream: None,
            message,
        })?;
    }
    for calendar in &config.calendars {
        let cursor = request.cursors.get(calendar).map(String::as_str);
        let held = emitted.calendars.get(calendar.as_str()).map(|list| {
            let mut stored = list.stored_as_of(cursor);
            stored.retain(|_, start| *start >= forget_before(request));
            stored
        });
        let missing = (held.is_none() && cursor.is_some()).then(|| {
            format!(
                "Calendar {calendar} has no list of the events it stored ({EMITTED_FILE}), as an earlier build kept none; a cancellation of an event stored before this fetch is not recorded, and a move of one past the horizon is followed only from its slot"
            )
        });
        // The list is saved before any record is sent: a record stored without its entry would
        // leave its cancellation unrecorded, so a save that fails stores nothing and leaves the
        // cursor, and the next fetch reads the same changes again.
        let collected = collect_calendar(request, calendar, api, held.as_ref()).and_then(|c| {
            let mut recorded = c.recorded;
            recorded.retain(|_, start| *start >= forget_before(request));
            // The fetch drops the record of an excluded event, so the tree never holds it.
            if !exclusion.is_empty() {
                let scheme = &request.source.scheme;
                recorded.retain(|id, _| !exclusion.matches(&format!("{scheme}:{calendar}/{id}")));
            }
            let list = CalendarList {
                stored: held.unwrap_or_default(),
                proposed: Some(Proposed {
                    cursor: c.cursor.clone(),
                    recorded,
                }),
            };
            Emitted::save(&request.state_dir, calendar, list)?;
            let warnings: Vec<String> = missing.into_iter().chain(c.warnings).collect();
            Ok((warnings, c.records, c.cursor))
        });
        super::emit_stream(calendar, collected, sink)?;
    }
    Ok(())
}

/// Reads one calendar: incrementally when the cursor names a sync token, else from the time it
/// names or the backfill window. Either way it records no instance of a recurring series whose
/// start and slot both lie at or after the horizon, [`HORIZON`] past the window end to the second,
/// unless the tree holds a record of it (`held`, the events recorded in windows that were stored);
/// a listing by time records no event that starts past it at all.
fn collect_calendar<C: CalendarApi + ?Sized>(
    request: &FetchRequest,
    calendar: &str,
    api: &C,
    held: Option<&Recorded>,
) -> Result<Collected> {
    let cursor = request
        .cursors
        .get(calendar)
        .map(String::as_str)
        .map(TokenCursor::parse);
    let backfill = window_start(None, request.window_end, request.backfill_hours)?;
    // The cursor keeps its time to the second, and the next fetch's horizon listing starts at
    // that time plus the horizon. Reckoning this horizon to the second as well keeps an event that
    // starts on it from being recorded by both fetches.
    let horizon = request.window_end.trunc_subsecs(0) + HORIZON;
    let stored_through = match cursor {
        Some(TokenCursor::Token { stored_through, .. }) => stored_through,
        _ => None,
    };
    // A cursor's time lies past the horizon only in a fetch given a much older window end. Such
    // a listing starts at the horizon, since Google refuses one whose start is not before its end.
    let list_from = |start: DateTime<Utc>| {
        list_by_time(api, calendar, start.min(horizon), horizon + LISTING_MARGIN)
    };
    let mut collected = Collected {
        recorded: held.cloned().unwrap_or_default(),
        ..Collected::default()
    };
    let mut page = match cursor {
        Some(TokenCursor::Token { token, .. }) => api.events(calendar, &sync_query(token))?,
        Some(TokenCursor::Time(start)) => list_from(start)?,
        None => list_from(backfill)?,
    };
    let named_by_token = matches!(cursor, Some(TokenCursor::Token { .. })) && !page.expired;
    let mut horizon_listing = None;
    if page.expired {
        // Google refused the sync token, so the calendar is listed again. Only the time the
        // cursor carries says how far back that listing reaches; without it a listing of the
        // backfill window would step over every event older than it and still write a token.
        let Some(stored_through) = stored_through else {
            bail!(
                "Calendar {calendar} refused its sync token, and the cursor names no time through which its events are stored; set the cursor to the RFC 3339 time to list from"
            );
        };
        collected.warnings.push(format!(
            "Calendar {calendar} refused its sync token; listing from {} again",
            stored_through
                .min(horizon)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
        page = list_from(stored_through)?;
    } else if let Some(stored_through) = stored_through {
        // A sync token names only what changed, so the horizon listing lists by time the events
        // that came within the horizon since the cursor's window. A window that has not moved on
        // has brought nothing new within reach.
        let reached = stored_through + HORIZON;
        if reached < horizon {
            let listing = list_by_time(
                api,
                calendar,
                reached - LISTING_MARGIN,
                horizon + LISTING_MARGIN,
            )?;
            horizon_listing = Some((reached..horizon, listing));
        }
    }
    collected.cursor = if let Some(token) = page.next_sync_token {
        token_cursor(&token, request.window_end)
    } else {
        collected.warnings.push(format!(
            "Calendar {calendar} returned no sync token; the next fetch lists the window again"
        ));
        request
            .window_end
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    };
    let record = |event: &Value, start: DateTime<Utc>| {
        record_from_event(
            event,
            start,
            calendar,
            &request.source.scheme,
            request.window_end,
        )
    };
    for event in &page.items {
        let stored = event_id(event).and_then(|id| collected.recorded.get(id).copied());
        let start = match stored {
            // The tree holds a record of an event the sync token names, so the change reaches
            // it wherever the event now starts. A cancelled one is recorded as cancelled, at the
            // start its record has, since Google may name it by its id alone.
            Some(stored) if named_by_token => {
                if is_cancelled(event) {
                    let mut cancelled = record(event, stored)?;
                    CANCELLED.clone_into(&mut cancelled.text);
                    collected.records.push(cancelled);
                    continue;
                }
                recorded_start(event, &..)?
            }
            _ if named_by_token => token_named_start(event, horizon)?,
            _ => recorded_start(event, &(..horizon))?,
        };
        if let Some(start) = start {
            collected.keep(record(event, start)?, event, start);
        }
    }
    if let Some((kept, listing)) = &horizon_listing {
        collect_horizon_listing(&mut collected, &page.items, kept, listing, &record)?;
    }
    Ok(collected)
}

impl Collected {
    /// Adds `record`, of `event` at `start`, and lists the event as recorded there.
    fn keep(&mut self, record: Record, event: &Value, start: DateTime<Utc>) {
        self.records.push(record);
        if let Some(id) = event_id(event) {
            self.recorded.insert(id.to_owned(), start);
        }
    }
}

/// Records the events of the horizon listing that start in `kept`. An event the sync token named
/// as well is recorded once, as that listing has it.
fn collect_horizon_listing(
    collected: &mut Collected,
    named: &[Value],
    kept: &Range<DateTime<Utc>>,
    listing: &EventPage,
    record: &impl Fn(&Value, DateTime<Utc>) -> Result<Record>,
) -> Result<()> {
    let named = named.iter().filter_map(event_id).collect::<BTreeSet<_>>();
    let unnamed = listing
        .items
        .iter()
        .filter(|event| event_id(event).is_none_or(|id| !named.contains(id)));
    for event in unnamed {
        if let Some(start) = recorded_start(event, kept)? {
            collected.keep(record(event, start)?, event, start);
        }
    }
    Ok(())
}

fn is_cancelled(event: &Value) -> bool {
    event.get("status").and_then(Value::as_str) == Some("cancelled")
}

/// The start of `event` when it is recorded: it is not cancelled, and its start lies in `kept`.
fn recorded_start(
    event: &Value,
    kept: &impl RangeBounds<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>> {
    if is_cancelled(event) {
        return Ok(None);
    }
    let start = event_start(event)?;
    Ok(kept.contains(&start).then_some(start))
}

/// The start of `event`, which a sync token names, when it is recorded: it is not cancelled, and
/// it starts before `horizon`, or it is a one-off event, or it is an instance of a series whose
/// slot lies before `horizon`.
///
/// Only a recurring series runs for decades, and a change to a series names every instance it
/// expands. An instance's slot, its start by the series' recurrence (`originalStartTime`), does
/// not move when the instance does, so keeping the slots before the horizon still stops a series
/// there, while an occurrence moved from one of them is recorded wherever it now starts, as a
/// changed one-off event is: the record the tree holds for it follows the move.
fn token_named_start(event: &Value, horizon: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let Some(start) = recorded_start(event, &..)? else {
        return Ok(None);
    };
    if start < horizon || event.get("recurringEventId").is_none() {
        return Ok(Some(start));
    }
    let Some(slot) = event.get("originalStartTime") else {
        return Ok(None);
    };
    Ok((earliest_slot(slot)? < horizon).then_some(start))
}

/// The earliest instant an instance's slot (`originalStartTime`) can name.
///
/// Google may give its time without an offset and name the time zone beside it, and time zones
/// run from 12 hours behind UTC to 14 ahead. Read in the zone furthest ahead, such a slot lies
/// before the horizon whenever it does in its own zone: an occurrence the tree holds is never
/// dropped for it, and a series yields at most 26 hours' more slots.
fn earliest_slot(slot: &Value) -> Result<DateTime<Utc>> {
    let Some(value) = slot.get("dateTime").and_then(Value::as_str) else {
        return event_time(slot, "originalStartTime");
    };
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let local = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .with_context(|| format!("parsing Calendar event originalStartTime {value:?}"))?;
    Ok(Utc.from_utc_datetime(&local) - Duration::hours(14))
}

fn event_id(event: &Value) -> Option<&str> {
    event.get("id").and_then(Value::as_str)
}

/// Lists every event that ends after `after` and starts before `before`, as Google bounds
/// `timeMin` and `timeMax`, with each recurring series expanded into its instances.
///
/// 410 Gone names a sync token Google no longer holds. A listing by time sends none, so a 410
/// here stops the stream: read as an empty listing, it would move the cursor over events the run
/// never recorded.
fn list_by_time<C: CalendarApi + ?Sized>(
    api: &C,
    calendar: &str,
    after: DateTime<Utc>,
    before: DateTime<Utc>,
) -> Result<EventPage> {
    let page = api.events(
        calendar,
        &[
            ("timeMin", after.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ("timeMax", before.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ("singleEvents", "true".to_owned()),
        ],
    )?;
    if page.expired {
        bail!("Calendar {calendar} answered 410 Gone to a listing by time");
    }
    Ok(page)
}

/// The query of an incremental listing.
fn sync_query(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("syncToken", token.to_owned()),
        ("singleEvents", "true".to_owned()),
    ]
}

/// Builds one record whose timestamp is the event's `start`.
///
/// An event that starts after the window ends is a context record: the window must not carry a
/// record after its end, and an upcoming meeting is still worth citing.
fn record_from_event(
    event: &Value,
    start: DateTime<Utc>,
    calendar: &str,
    scheme: &str,
    window_end: DateTime<Utc>,
) -> Result<Record> {
    let id = required_event_string(event, "id")?;
    let title = event
        .get("summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.is_empty())
        .map(str::to_owned);
    let organizer = event.get("organizer");
    let author = organizer.and_then(attendee_name);
    let author_id = organizer
        .and_then(|organizer| organizer.get("email"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned);
    Ok(Record {
        pointer: format!("{scheme}:{calendar}/{id}"),
        stream: calendar.to_owned(),
        ts: start,
        label: Some(calendar.to_owned()),
        author,
        author_id,
        thread: None,
        title,
        url: event
            .get("htmlLink")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text: render_event(event, start),
        aliases: vec![id.to_owned()],
        context: start > window_end,
        raw: event.clone(),
    })
}

/// Renders an event as its times, place, attendees, and description.
fn render_event(event: &Value, start: DateTime<Utc>) -> String {
    let mut lines = vec![match event_end(event) {
        Some(end) => format!(
            "When: {} to {end}",
            start.to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
        None => format!("When: {}", start.to_rfc3339_opts(SecondsFormat::Secs, true)),
    }];
    if let Some(location) = event
        .get("location")
        .and_then(Value::as_str)
        .filter(|location| !location.is_empty())
    {
        lines.push(format!("Where: {location}"));
    }
    let attendees = event
        .get("attendees")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |attendees| {
            attendees.iter().map(render_attendee).collect()
        });
    if !attendees.is_empty() {
        lines.push(format!("Attendees: {}", attendees.join(", ")));
    }
    if let Some(description) = event
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
    {
        lines.push(String::new());
        lines.push(description.to_owned());
    }
    lines.join("\n")
}

/// One attendee as `Name <email> (response)`, with whatever parts the event carries.
fn render_attendee(attendee: &Value) -> String {
    let email = attendee.get("email").and_then(Value::as_str);
    let name = attendee
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let response = attendee
        .get("responseStatus")
        .and_then(Value::as_str)
        .filter(|response| !response.is_empty());
    let mut rendered = match (name, email) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (Some(name), None) => name.to_owned(),
        (None, Some(email)) => email.to_owned(),
        (None, None) => "(unnamed)".to_owned(),
    };
    if let Some(response) = response {
        let _ = write!(rendered, " ({response})");
    }
    rendered
}

/// A person's display name, or their address when they have no name.
fn attendee_name(person: &Value) -> Option<String> {
    person
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            person
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
        })
        .map(str::to_owned)
}

fn required_event_string<'a>(event: &'a Value, field: &str) -> Result<&'a str> {
    event
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Calendar event is missing {field}"))
}

/// The start of an event: its time, or midnight UTC for an all-day event.
fn event_start(event: &Value) -> Result<DateTime<Utc>> {
    let start = event
        .get("start")
        .ok_or_else(|| anyhow!("Calendar event is missing start"))?;
    event_time(start, "start")
}

/// The time `field` of an event gives, such as its `start`: its time, or midnight UTC for a date.
fn event_time(time: &Value, field: &str) -> Result<DateTime<Utc>> {
    if let Some(value) = time.get("dateTime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(value)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .with_context(|| format!("parsing Calendar event {field} {value:?}"));
    }
    let value = time
        .get("date")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Calendar event {field} has neither dateTime nor date"))?;
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("parsing Calendar event {field} date {value:?}"))?;
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("Calendar event {field} date {value:?} has no midnight"))?;
    Ok(Utc.from_utc_datetime(&midnight))
}

/// The end of an event, in UTC when it is a time and as written when it is a date.
fn event_end(event: &Value) -> Option<String> {
    let end = event.get("end")?;
    if let Some(value) = end.get("dateTime").and_then(Value::as_str) {
        return Some(DateTime::parse_from_rfc3339(value).map_or_else(
            |_| value.to_owned(),
            |timestamp| {
                timestamp
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
            },
        ));
    }
    end.get("date").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::super::StreamStatus;
    use super::*;

    #[derive(Default)]
    struct FakeCalendarApi {
        calls: RefCell<Vec<Vec<(String, String)>>>,
        pages: RefCell<Vec<EventPage>>,
    }

    impl CalendarApi for FakeCalendarApi {
        fn events(&self, _calendar: &str, query: &[(&str, String)]) -> Result<EventPage> {
            self.calls.borrow_mut().push(
                query
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
            );
            let mut pages = self.pages.borrow_mut();
            if pages.is_empty() {
                return Ok(EventPage::default());
            }
            Ok(pages.remove(0))
        }
    }

    fn api(pages: Vec<EventPage>) -> FakeCalendarApi {
        FakeCalendarApi {
            pages: RefCell::new(pages),
            ..FakeCalendarApi::default()
        }
    }

    fn meeting() -> Value {
        event(
            "EVENT-EXAMPLE",
            "2026-10-25T09:00:00Z",
            "2026-10-25T10:00:00Z",
            None,
        )
    }

    /// One event, with the times and status a test needs.
    fn event(id: &str, start: &str, end: &str, status: Option<&str>) -> Value {
        let mut event = json!({
            "id": id,
            "summary": "Weekly sync",
            "updated": "2026-10-25T17:00:00Z",
            "start": {"dateTime": start},
            "end": {"dateTime": end},
            "location": "Meet",
            "description": "Agenda in the doc.",
            "organizer": {"displayName": "Ada Lovelace", "email": "ada@example.com"},
            "attendees": [
                {"email": "ada@example.com", "displayName": "Ada Lovelace", "responseStatus": "accepted"},
                {"email": "alice@example.com", "responseStatus": "needsAction"}
            ],
            "htmlLink": "https://calendar.google.com/event?eid=EXAMPLE"
        });
        if let (Some(status), Some(fields)) = (status, event.as_object_mut()) {
            fields.insert("status".to_owned(), json!(status));
        }
        event
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    /// A state directory of its own, as each tree's source has.
    fn scratch() -> Result<PathBuf> {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ctx-calendar-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    fn request(cursor: Option<&str>) -> Result<FetchRequest> {
        let mut cursors = BTreeMap::new();
        if let Some(cursor) = cursor {
            cursors.insert("team@example.com".to_owned(), cursor.to_owned());
        }
        Ok(FetchRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "calendar".to_owned(),
                kind: "calendar".to_owned(),
                scheme: "gcal".to_owned(),
            },
            config: json!({"calendars": ["team@example.com"]}),
            window_end: DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc),
            backfill_hours: 24,
            cursors,
            discovered: Vec::new(),
            state_dir: scratch()?,
            exclude: Vec::new(),
        })
    }

    /// A request for the fetch after the one whose state is in `state_dir`, as the fetches of one
    /// tree share it.
    fn request_after(cursor: &str, state_dir: &Path) -> Result<FetchRequest> {
        let mut request = request(Some(cursor))?;
        request.state_dir = state_dir.to_owned();
        Ok(request)
    }

    fn warnings(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn ended_with_error(messages: &[Message]) -> bool {
        messages.iter().any(|message| {
            matches!(
                message,
                Message::StreamEnd {
                    status: StreamStatus::Error,
                    ..
                }
            )
        })
    }

    fn run_fetch(api: &FakeCalendarApi, request: &FetchRequest) -> Result<Vec<Message>> {
        let config = parse_config(&request.config)?;
        let mut messages = Vec::new();
        fetch_with_api(request, &config, api, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    }

    fn records(messages: &[Message]) -> Vec<Record> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Record(record) => Some((**record).clone()),
                _ => None,
            })
            .collect()
    }

    fn cursor(messages: &[Message]) -> Option<String> {
        messages.iter().find_map(|message| match message {
            Message::StreamEnd { cursor, .. } => cursor.clone(),
            _ => None,
        })
    }

    fn query_value(call: &[(String, String)], name: &str) -> Option<String> {
        call.iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
    }

    /// One instance of a yearly all-day series, as a listing that expands recurring events
    /// into their instances returns it.
    fn birthday(year: i32) -> Value {
        json!({
            "id": format!("BIRTHDAY-EXAMPLE_{year}1103"),
            "summary": "Ada Lovelace's birthday",
            "eventType": "birthday",
            "recurringEventId": "BIRTHDAY-EXAMPLE",
            "start": {"date": format!("{year}-11-03")},
            "end": {"date": format!("{year}-11-04")}
        })
    }

    fn starts(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .map(|record| record.ts.to_rfc3339_opts(SecondsFormat::Secs, true))
            .collect()
    }

    fn at(time: &str) -> Result<DateTime<Utc>> {
        Ok(DateTime::parse_from_rfc3339(time)?.with_timezone(&Utc))
    }

    /// A listing that ends with `token`, as Google answers a request it accepts.
    fn listing(token: &str, items: Vec<Value>) -> EventPage {
        EventPage {
            items,
            next_sync_token: Some(token.to_owned()),
            expired: false,
        }
    }

    /// The answer Google gives with 410 Gone.
    fn gone() -> EventPage {
        EventPage {
            expired: true,
            ..EventPage::default()
        }
    }

    /// An event that lasts the whole of `date`.
    fn all_day(id: &str, date: &str, next_date: &str) -> Value {
        json!({
            "id": id,
            "summary": "Holiday",
            "start": {"date": date},
            "end": {"date": next_date}
        })
    }

    /// `event` as one instance of a recurring series, as a listing that expands series has it.
    fn instance(mut event: Value) -> Value {
        if let Some(fields) = event.as_object_mut() {
            fields.insert("recurringEventId".to_owned(), json!("SERIES-EXAMPLE"));
        }
        event
    }

    fn pointers(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .map(|record| record.pointer.clone())
            .collect()
    }

    #[test]
    fn calendar_records_attendees_and_start_time() -> Result<()> {
        let api = api(vec![EventPage {
            items: vec![meeting()],
            next_sync_token: Some("sync-2".to_owned()),
            expired: false,
        }]);
        let request = request(None)?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        let record = records.first().context("the listing produced a record")?;
        assert_eq!(
            record.ts.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-10-25T09:00:00Z"
        );
        assert_eq!(record.pointer, "gcal:team@example.com/EVENT-EXAMPLE");
        assert_eq!(record.title.as_deref(), Some("Weekly sync"));
        assert!(record.text.contains("alice@example.com"), "{}", record.text);
        assert!(record.text.contains("ada@example.com"), "{}", record.text);
        assert!(
            record.text.contains("2026-10-25T09:00:00Z"),
            "{}",
            record.text
        );
        assert!(
            record.text.contains("2026-10-25T10:00:00Z"),
            "{}",
            record.text
        );
        assert!(
            record.text.contains("Agenda in the doc."),
            "{}",
            record.text
        );
        assert!(!record.context);
        Ok(())
    }

    #[test]
    fn calendar_renders_both_times_in_utc() -> Result<()> {
        let api = api(vec![EventPage {
            items: vec![event(
                "EVENT-OFFSET",
                "2026-10-25T09:00:00Z",
                "2026-10-25T03:30:00-07:00",
                None,
            )],
            next_sync_token: Some("sync-2".to_owned()),
            expired: false,
        }]);
        let request = request(None)?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        let record = records.first().context("the listing produced a record")?;
        assert!(
            record
                .text
                .starts_with("When: 2026-10-25T09:00:00Z to 2026-10-25T10:30:00Z"),
            "{}",
            record.text
        );
        Ok(())
    }

    #[test]
    fn calendar_without_a_cursor_lists_from_the_backfill_window() -> Result<()> {
        let api = api(vec![EventPage {
            items: Vec::new(),
            next_sync_token: Some("sync-2".to_owned()),
            expired: false,
        }]);
        let request = request(None)?;
        let messages = run_fetch(&api, &request)?;

        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 1);
        let call = calls.first().context("the fetch made a request")?;
        assert_eq!(
            query_value(call, "timeMin"),
            Some("2026-10-24T18:00:00Z".to_owned())
        );
        // 400 days past the window end, and a day more, since Google places an all-day event
        // by the calendar's time zone.
        assert_eq!(
            query_value(call, "timeMax"),
            Some("2027-11-30T18:00:00Z".to_owned())
        );
        assert_eq!(query_value(call, "singleEvents"), Some("true".to_owned()));
        assert_eq!(query_value(call, "syncToken"), None);
        assert_eq!(
            cursor(&messages),
            Some("sync-2@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn calendar_uses_a_sync_token_cursor() -> Result<()> {
        let api = api(vec![EventPage {
            items: vec![meeting()],
            next_sync_token: Some("sync-3".to_owned()),
            expired: false,
        }]);
        let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let calls = api.calls.borrow();
        let call = calls.first().context("the fetch made a request")?;
        assert_eq!(query_value(call, "syncToken"), Some("sync-2".to_owned()));
        assert_eq!(query_value(call, "timeMin"), None);
        // The cursor carries the time through which the calendar is stored beside the token,
        // so a run whose token Google refuses knows where to list from.
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn calendar_resyncs_when_the_sync_token_expired() -> Result<()> {
        let api = api(vec![
            EventPage {
                expired: true,
                ..EventPage::default()
            },
            EventPage {
                items: vec![meeting()],
                next_sync_token: Some("sync-4".to_owned()),
                expired: false,
            },
        ]);
        let request = request(Some("sync-2@2026-10-20T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        let second = calls.get(1).context("the resync made a second request")?;
        // The listing reaches back to what the cursor says is stored, not to the backfill
        // window, so the days between the two are not stepped over.
        assert_eq!(
            query_value(second, "timeMin"),
            Some("2026-10-20T00:00:00Z".to_owned())
        );
        assert_eq!(
            query_value(second, "timeMax"),
            Some("2027-11-30T18:00:00Z".to_owned())
        );
        assert_eq!(records(&messages).len(), 1);
        assert_eq!(
            cursor(&messages),
            Some("sync-4@2026-10-25T18:00:00Z".to_owned())
        );
        assert!(messages
            .iter()
            .any(|message| matches!(message, Message::Warning { .. })));
        Ok(())
    }

    #[test]
    fn calendar_errors_when_an_expired_token_names_no_stored_time() -> Result<()> {
        let api = api(vec![EventPage {
            expired: true,
            ..EventPage::default()
        }]);
        // A cursor an earlier build wrote names no time, so nothing says how far back the
        // listing must reach. The stream stops rather than write a cursor over that gap.
        let request = request(Some("sync-2"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(api.calls.borrow().len(), 1);
        assert!(records(&messages).is_empty());
        assert_eq!(cursor(&messages), None);
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::StreamEnd {
                    status: StreamStatus::Error,
                    ..
                }
            )),
            "{messages:?}"
        );
        Ok(())
    }

    #[test]
    fn a_future_event_is_a_context_record() -> Result<()> {
        let upcoming = event(
            "EVENT-LATER",
            "2026-11-02T09:00:00Z",
            "2026-11-02T10:00:00Z",
            None,
        );
        let api = api(vec![EventPage {
            items: vec![upcoming],
            next_sync_token: Some("sync-2".to_owned()),
            expired: false,
        }]);
        let request = request(None)?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        let record = records.first().context("the listing produced a record")?;
        assert!(record.context);
        assert_eq!(
            record.ts.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-11-02T09:00:00Z"
        );
        Ok(())
    }

    #[test]
    fn a_yearly_series_yields_only_its_instances_inside_the_horizon() -> Result<()> {
        // A listing by sync token takes no time bounds, so when a series changes Google names
        // every instance it expands, decades of them.
        let api = api(vec![listing(
            "sync-3",
            (2026..=2056).map(birthday).collect(),
        )]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(
            starts(&records(&messages)),
            ["2026-11-03T00:00:00Z", "2027-11-03T00:00:00Z"]
        );
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_sync_listing_keeps_only_the_series_instances_that_start_before_the_horizon() -> Result<()>
    {
        // The horizon is 2027-11-29T18:00:00Z, and an all-day event starts at midnight UTC.
        let api = api(vec![listing(
            "sync-3",
            vec![
                instance(all_day("EVENT-HORIZON-DAY", "2027-11-29", "2027-11-30")),
                instance(all_day("EVENT-DAY-AFTER", "2027-11-30", "2027-12-01")),
                instance(event(
                    "EVENT-AT-HORIZON",
                    "2027-11-29T18:00:00Z",
                    "2027-11-29T19:00:00Z",
                    None,
                )),
                instance(event(
                    "EVENT-JUST-BEFORE",
                    "2027-11-29T17:59:59Z",
                    "2027-11-29T19:00:00Z",
                    None,
                )),
            ],
        )]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(
            pointers(&records(&messages)),
            [
                "gcal:team@example.com/EVENT-HORIZON-DAY",
                "gcal:team@example.com/EVENT-JUST-BEFORE"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_one_off_event_the_sync_token_names_is_recorded_wherever_it_now_starts() -> Result<()> {
        // The trip moved from next summer to a date past the horizon, 2027-11-29T18:00:00Z. A
        // one-off event the token names has changed, and only a series runs for decades, so the
        // record follows the move at once; the series instance on the same day does not.
        let api = api(vec![listing(
            "sync-3",
            vec![
                event(
                    "EVENT-TRIP",
                    "2029-01-22T09:00:00Z",
                    "2029-01-26T18:00:00Z",
                    None,
                ),
                instance(event(
                    "EVENT-SERIES_20290122",
                    "2029-01-22T09:00:00Z",
                    "2029-01-22T10:00:00Z",
                    None,
                )),
            ],
        )]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        assert_eq!(pointers(&records), ["gcal:team@example.com/EVENT-TRIP"]);
        assert_eq!(starts(&records), ["2029-01-22T09:00:00Z"]);
        assert!(records.iter().all(|record| record.context));
        Ok(())
    }

    /// `event` as the instance of a series whose slot, by the series' recurrence, starts at
    /// `slot`, as Google gives it in `originalStartTime`; a moved instance keeps its slot.
    fn occurrence(mut event: Value, slot: &str) -> Value {
        if let Some(fields) = event.as_object_mut() {
            fields.insert("recurringEventId".to_owned(), json!("SERIES-EXAMPLE"));
            fields.insert("originalStartTime".to_owned(), json!({"dateTime": slot}));
        }
        event
    }

    #[test]
    fn an_occurrence_whose_slot_names_its_time_zone_apart_follows_its_move() -> Result<()> {
        // Google may give a time without its offset and name the time zone beside it. The slot
        // next June is inside the horizon, 2027-11-29T18:00:00Z, in every time zone; the one in
        // December is past it in every time zone, and is not kept.
        let named_zone = |id: &str, start: &str, slot: &str| {
            let mut moved = instance(event(id, start, "2029-01-22T10:00:00Z", None));
            if let Some(fields) = moved.as_object_mut() {
                fields.insert(
                    "originalStartTime".to_owned(),
                    json!({"dateTime": slot, "timeZone": "Europe/London"}),
                );
            }
            moved
        };
        let api = api(vec![listing(
            "sync-3",
            vec![
                named_zone(
                    "EVENT-SERIES_20270614",
                    "2029-01-22T09:00:00Z",
                    "2027-06-14T09:00:00",
                ),
                named_zone(
                    "EVENT-SERIES_20271201",
                    "2029-01-23T09:00:00Z",
                    "2027-12-01T09:00:00",
                ),
            ],
        )]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(
            pointers(&records(&messages)),
            ["gcal:team@example.com/EVENT-SERIES_20270614"]
        );
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn an_occurrence_the_sync_token_names_moved_past_the_horizon_is_recorded_at_its_new_start(
    ) -> Result<()> {
        // The first fetch stores the occurrence in its slot next June, inside the horizon.
        let slot = "2027-06-14T08:00:00Z";
        let stored = occurrence(
            event("EVENT-SERIES_20270614", slot, "2027-06-14T09:00:00Z", None),
            slot,
        );
        let first_api = api(vec![listing("sync-3", vec![stored])]);
        let first_messages = run_fetch(&first_api, &request(None)?)?;
        assert_eq!(starts(&records(&first_messages)), [slot]);
        let first_cursor = cursor(&first_messages).context("the first fetch moved on")?;

        // That one occurrence then moves to 500 days past the window end, well past the
        // horizon, 2027-11-29T18:00:00Z, and the sync token names it.
        let moved = occurrence(
            event(
                "EVENT-SERIES_20270614",
                "2028-03-08T18:00:00Z",
                "2028-03-08T19:00:00Z",
                None,
            ),
            slot,
        );
        let second_api = api(vec![listing("sync-4", vec![moved])]);
        let messages = run_fetch(&second_api, &request(Some(&first_cursor))?)?;

        let records = records(&messages);
        assert_eq!(
            pointers(&records),
            ["gcal:team@example.com/EVENT-SERIES_20270614"]
        );
        assert_eq!(starts(&records), ["2028-03-08T18:00:00Z"]);
        assert!(
            records
                .iter()
                .all(|record| record.context && record.text.contains("2028-03-08T18:00:00Z")),
            "{records:?}"
        );
        // The cursor moves past the change only after the record that carries it.
        let recorded_at = messages
            .iter()
            .position(|message| matches!(message, Message::Record(_)))
            .context("the fetch sent the record")?;
        let ended_at = messages
            .iter()
            .position(|message| matches!(message, Message::StreamEnd { .. }))
            .context("the fetch ended the stream")?;
        assert!(recorded_at < ended_at, "{messages:?}");
        assert_eq!(
            cursor(&messages),
            Some("sync-4@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_sync_listing_keeps_a_series_instance_whose_start_or_slot_is_inside_the_horizon(
    ) -> Result<()> {
        // The series changed, so the token names every instance, each with its slot. An instance
        // is kept when its start or its slot is inside the horizon, 2027-11-29T18:00:00Z, so a
        // series with no end still stops at the horizon.
        let mut items = (2026..=2056)
            .map(|year| {
                let start = format!("{year}-11-03T09:00:00Z");
                occurrence(
                    event(
                        &format!("EVENT-SERIES_{year}1103"),
                        &start,
                        &format!("{year}-11-03T10:00:00Z"),
                        None,
                    ),
                    &start,
                )
            })
            .collect::<Vec<_>>();
        items.extend([
            // Its slot is inside the horizon, and it moved past it.
            occurrence(
                event(
                    "EVENT-SERIES_20270614",
                    "2029-01-22T09:00:00Z",
                    "2029-01-22T10:00:00Z",
                    None,
                ),
                "2027-06-14T08:00:00Z",
            ),
            // Its slot is past the horizon, and it moved inside it.
            occurrence(
                event(
                    "EVENT-SERIES_20300614",
                    "2027-07-01T08:00:00Z",
                    "2027-07-01T09:00:00Z",
                    None,
                ),
                "2030-06-14T08:00:00Z",
            ),
            // Its slot is past the horizon, and so is its new start.
            occurrence(
                event(
                    "EVENT-SERIES_20310614",
                    "2031-07-01T08:00:00Z",
                    "2031-07-01T09:00:00Z",
                    None,
                ),
                "2031-06-14T08:00:00Z",
            ),
        ]);
        let api = api(vec![listing("sync-3", items)]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(
            starts(&records(&messages)),
            [
                "2026-11-03T09:00:00Z",
                "2027-11-03T09:00:00Z",
                "2029-01-22T09:00:00Z",
                "2027-07-01T08:00:00Z"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_cancelled_occurrence_the_sync_token_names_past_the_horizon_is_not_recorded() -> Result<()>
    {
        // Google sends a cancelled instance with its id, status, series and slot, and no start.
        let cancelled = json!({
            "id": "EVENT-SERIES_20270614",
            "status": "cancelled",
            "recurringEventId": "SERIES-EXAMPLE",
            "originalStartTime": {"dateTime": "2027-06-14T08:00:00Z"}
        });
        let api = api(vec![listing("sync-4", vec![cancelled])]);
        let request = request(Some("sync-3@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert!(records(&messages).is_empty());
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                Message::StreamEnd {
                    status: StreamStatus::Error,
                    ..
                }
            )),
            "{messages:?}"
        );
        assert_eq!(
            cursor(&messages),
            Some("sync-4@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_listing_by_time_keeps_only_what_starts_before_the_horizon() -> Result<()> {
        // Google answers to a day past the horizon, so a one-off event there comes back too; the
        // horizon listing records it once the horizon reaches it.
        let page = || {
            listing(
                "sync-3",
                vec![
                    event(
                        "EVENT-BEFORE",
                        "2027-11-29T12:00:00Z",
                        "2027-11-29T13:00:00Z",
                        None,
                    ),
                    event(
                        "EVENT-IN-MARGIN",
                        "2027-11-30T12:00:00Z",
                        "2027-11-30T13:00:00Z",
                        None,
                    ),
                ],
            )
        };
        for (case, cursor, pages) in [
            ("first listing", None, vec![page()]),
            ("time cursor", Some("2026-10-24T00:00:00Z"), vec![page()]),
            (
                "resync after 410",
                Some("sync-2@2026-10-24T00:00:00Z"),
                vec![gone(), page()],
            ),
        ] {
            let api = api(pages);
            let messages = run_fetch(&api, &request(cursor)?)?;

            assert_eq!(
                pointers(&records(&messages)),
                ["gcal:team@example.com/EVENT-BEFORE"],
                "{case}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_time_cursor_lists_no_further_than_the_horizon() -> Result<()> {
        // Even a reply that ignores `timeMax` yields nothing past the horizon.
        let api = api(vec![listing(
            "sync-3",
            (2026..=2056).map(birthday).collect(),
        )]);
        let request = request(Some("2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 1);
        let call = calls.first().context("the fetch made a request")?;
        assert_eq!(
            query_value(call, "timeMin"),
            Some("2026-10-24T00:00:00Z".to_owned())
        );
        assert_eq!(
            query_value(call, "timeMax"),
            Some("2027-11-30T18:00:00Z".to_owned())
        );
        assert_eq!(
            starts(&records(&messages)),
            ["2026-11-03T00:00:00Z", "2027-11-03T00:00:00Z"]
        );
        Ok(())
    }

    #[test]
    fn a_cursor_an_earlier_build_wrote_lists_only_the_changes() -> Result<()> {
        // The cursor names no time, so nothing says where the earlier fetch's reach ended; that
        // build recorded with no horizon, so nothing within reach is missing.
        let api = api(vec![listing(
            "sync-3",
            (2026..=2056).map(birthday).collect(),
        )]);
        let request = request(Some("sync-2"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(api.calls.borrow().len(), 1);
        assert_eq!(
            starts(&records(&messages)),
            ["2026-11-03T00:00:00Z", "2027-11-03T00:00:00Z"]
        );
        Ok(())
    }

    #[test]
    fn a_trip_booked_for_next_year_is_recorded() -> Result<()> {
        let trip = event(
            "EVENT-TRIP",
            "2027-06-14T08:00:00Z",
            "2027-06-21T18:00:00Z",
            None,
        );
        let api = api(vec![listing("sync-3", vec![trip])]);
        let request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        let record = records.first().context("the listing produced a record")?;
        assert_eq!(record.pointer, "gcal:team@example.com/EVENT-TRIP");
        assert!(record.context);
        Ok(())
    }

    #[test]
    fn a_series_instance_past_the_horizon_is_recorded_once_it_comes_within_it() -> Result<()> {
        let far = instance(event(
            "EVENT-FAR",
            "2027-11-28T12:00:00Z",
            "2027-11-28T13:00:00Z",
            None,
        ));
        // The series changed, so the token names every instance it expands.
        let first_api = api(vec![listing("sync-3", vec![far.clone()])]);
        let mut first = request(Some("sync-2@2026-10-22T18:00:00Z"))?;
        first.window_end = at("2026-10-23T18:00:00Z")?;
        let first_messages = run_fetch(&first_api, &first)?;
        // The instance starts after this fetch's horizon, 2027-11-27T18:00:00Z.
        assert!(records(&first_messages).is_empty());
        let first_cursor = cursor(&first_messages).context("the first fetch moved on")?;
        assert_eq!(first_cursor, "sync-3@2026-10-23T18:00:00Z");

        // Nothing about the series changed since, so the sync token names it no more; the
        // horizon listing, of what came within the horizon since the cursor's window, does.
        let second_api = api(vec![
            listing("sync-4", Vec::new()),
            listing("listing-token", vec![far]),
        ]);
        let second = request(Some(&first_cursor))?;
        let messages = run_fetch(&second_api, &second)?;

        let calls = second_api.calls.borrow();
        let horizon_listing = calls.get(1).context("the fetch made the horizon listing")?;
        assert_eq!(query_value(horizon_listing, "syncToken"), None);
        assert_eq!(
            query_value(horizon_listing, "singleEvents"),
            Some("true".to_owned())
        );
        assert_eq!(
            query_value(horizon_listing, "timeMin"),
            Some("2027-11-26T18:00:00Z".to_owned())
        );
        assert_eq!(
            query_value(horizon_listing, "timeMax"),
            Some("2027-11-30T18:00:00Z".to_owned())
        );
        assert_eq!(
            pointers(&records(&messages)),
            ["gcal:team@example.com/EVENT-FAR"]
        );
        // The cursor is the sync token, never the one that ends the horizon listing.
        assert_eq!(
            cursor(&messages),
            Some("sync-4@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn an_instance_at_the_horizon_of_a_window_ending_within_a_second_is_recorded_once() -> Result<()>
    {
        // `ctx fetch` ends its window at the current time, to the nanosecond, while the cursor
        // keeps the time to the second.
        let boundary = instance(event(
            "EVENT-BOUNDARY",
            "2027-11-27T18:00:00Z",
            "2027-11-27T19:00:00Z",
            None,
        ));
        let first_api = api(vec![listing("sync-3", vec![boundary.clone()])]);
        let mut first = request(Some("sync-2@2026-10-23T18:00:00Z"))?;
        first.window_end = at("2026-10-23T18:00:00.400Z")?;
        let first_messages = run_fetch(&first_api, &first)?;
        assert!(records(&first_messages).is_empty());
        let first_cursor = cursor(&first_messages).context("the first fetch moved on")?;
        assert_eq!(first_cursor, "sync-3@2026-10-23T18:00:00Z");

        let second_api = api(vec![
            listing("sync-4", Vec::new()),
            listing("listing-token", vec![boundary]),
        ]);
        let second = request(Some(&first_cursor))?;
        let messages = run_fetch(&second_api, &second)?;

        assert_eq!(
            pointers(&records(&messages)),
            ["gcal:team@example.com/EVENT-BOUNDARY"]
        );
        Ok(())
    }

    #[test]
    fn the_horizon_listing_keeps_only_what_came_within_reach() -> Result<()> {
        let api = api(vec![
            listing("sync-3", Vec::new()),
            listing(
                "listing-token",
                vec![
                    // Within the reach of the cursor's window, 2027-11-28T00:00:00Z, so
                    // recorded then.
                    event(
                        "EVENT-REACHED",
                        "2027-11-27T12:00:00Z",
                        "2027-11-28T12:00:00Z",
                        None,
                    ),
                    event(
                        "EVENT-AT-REACH",
                        "2027-11-28T00:00:00Z",
                        "2027-11-28T01:00:00Z",
                        None,
                    ),
                    event(
                        "EVENT-NEW",
                        "2027-11-28T12:00:00Z",
                        "2027-11-28T13:00:00Z",
                        None,
                    ),
                    event(
                        "EVENT-CANCELLED",
                        "2027-11-28T14:00:00Z",
                        "2027-11-28T15:00:00Z",
                        Some("cancelled"),
                    ),
                    // Past this fetch's horizon, 2027-11-29T18:00:00Z.
                    event(
                        "EVENT-LATER",
                        "2027-11-30T12:00:00Z",
                        "2027-11-30T13:00:00Z",
                        None,
                    ),
                ],
            ),
        ]);
        let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert_eq!(
            pointers(&records(&messages)),
            [
                "gcal:team@example.com/EVENT-AT-REACH",
                "gcal:team@example.com/EVENT-NEW"
            ]
        );
        Ok(())
    }

    #[test]
    fn an_event_both_listings_name_is_recorded_once_as_the_sync_token_names_it() -> Result<()> {
        let stale = event(
            "EVENT-NEW",
            "2027-11-28T12:00:00Z",
            "2027-11-28T13:00:00Z",
            None,
        );
        let mut changed = stale.clone();
        if let Some(fields) = changed.as_object_mut() {
            fields.insert("summary".to_owned(), json!("Weekly sync, new room"));
        }
        let api = api(vec![
            listing("sync-3", vec![changed]),
            listing("listing-token", vec![stale]),
        ]);
        let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        let records = records(&messages);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records.first().and_then(|record| record.title.as_deref()),
            Some("Weekly sync, new room")
        );
        Ok(())
    }

    #[test]
    fn an_event_the_sync_token_names_cancelled_is_not_recorded_from_the_horizon_listing(
    ) -> Result<()> {
        let live = event(
            "EVENT-NEW",
            "2027-11-28T12:00:00Z",
            "2027-11-28T13:00:00Z",
            None,
        );
        let cancelled = event(
            "EVENT-NEW",
            "2027-11-28T12:00:00Z",
            "2027-11-28T13:00:00Z",
            Some("cancelled"),
        );
        let api = api(vec![
            listing("sync-3", vec![cancelled]),
            listing("listing-token", vec![live]),
        ]);
        let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        assert!(records(&messages).is_empty());
        Ok(())
    }

    #[test]
    fn a_listing_by_time_google_refuses_leaves_the_cursor_alone() -> Result<()> {
        // 410 names a sync token Google no longer holds. A listing by time sends none, so the
        // answer is no empty listing, and moving the cursor would step over the events it would
        // have named: those that came within the horizon, or those a resync lists again.
        for (case, pages) in [
            (
                "horizon listing",
                vec![listing("sync-3", Vec::new()), gone()],
            ),
            ("resync", vec![gone(), gone()]),
        ] {
            let api = api(pages);
            let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
            let messages = run_fetch(&api, &request)?;

            assert_eq!(cursor(&messages), None, "{case}");
            assert!(
                messages.iter().any(|message| matches!(
                    message,
                    Message::StreamEnd {
                        status: StreamStatus::Error,
                        ..
                    }
                )),
                "{case}: {messages:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_fetch_whose_window_has_not_moved_on_lists_only_the_changes() -> Result<()> {
        // Nothing new has come within the horizon when the window ends where the cursor's did,
        // within the same second, or before it, as a fetch given an older window end does.
        for (stored_through, window_end) in [
            ("2026-10-25T18:00:00Z", "2026-10-25T18:00:00Z"),
            ("2026-10-25T18:00:00Z", "2026-10-25T18:00:00.400Z"),
            ("2026-10-26T00:00:00Z", "2026-10-25T18:00:00Z"),
        ] {
            let api = api(vec![listing("sync-3", Vec::new())]);
            let mut request = request(Some(&format!("sync-2@{stored_through}")))?;
            request.window_end = at(window_end)?;
            run_fetch(&api, &request)?;

            assert_eq!(
                api.calls.borrow().len(),
                1,
                "stored through {stored_through}, window end {window_end}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_listing_from_past_the_horizon_starts_at_the_horizon() -> Result<()> {
        // A fetch given a window end long before its cursor's time; Google refuses a listing
        // whose `timeMin` is not before its `timeMax`.
        let by_time = api(vec![listing("sync-3", Vec::new())]);
        run_fetch(&by_time, &request(Some("2028-01-01T00:00:00Z"))?)?;
        let resync = api(vec![gone(), listing("sync-3", Vec::new())]);
        run_fetch(&resync, &request(Some("sync-2@2028-01-01T00:00:00Z"))?)?;

        let by_time_calls = by_time.calls.borrow();
        let resync_calls = resync.calls.borrow();
        for call in [by_time_calls.first(), resync_calls.get(1)] {
            let call = call.context("the fetch listed by time")?;
            assert_eq!(
                query_value(call, "timeMin"),
                Some("2027-11-29T18:00:00Z".to_owned())
            );
            assert_eq!(
                query_value(call, "timeMax"),
                Some("2027-11-30T18:00:00Z".to_owned())
            );
        }
        Ok(())
    }

    #[test]
    fn a_cancelled_event_is_not_recorded() -> Result<()> {
        let cancelled = event(
            "EVENT-EXAMPLE",
            "2026-10-25T09:00:00Z",
            "2026-10-25T10:00:00Z",
            Some("cancelled"),
        );
        let api = api(vec![EventPage {
            items: vec![cancelled],
            next_sync_token: Some("sync-2".to_owned()),
            expired: false,
        }]);
        let request = request(Some("sync-1"))?;
        let messages = run_fetch(&api, &request)?;

        assert!(records(&messages).is_empty());
        assert_eq!(
            cursor(&messages),
            Some("sync-2@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_failure_part_way_through_a_listing_leaves_the_cursor_alone() -> Result<()> {
        let mut malformed = meeting();
        if let Some(fields) = malformed.as_object_mut() {
            // Google answered without the field the record's timestamp comes from.
            fields.remove("start");
        }
        let api = api(vec![EventPage {
            items: vec![meeting(), malformed],
            next_sync_token: Some("sync-3".to_owned()),
            expired: false,
        }]);
        let request = request(Some("sync-2@2026-10-24T00:00:00Z"))?;
        let messages = run_fetch(&api, &request)?;

        // The run stopped inside the listing, so it proposes no cursor and the stored sync
        // token stands: the next fetch asks Google for the same changes again.
        assert!(records(&messages).is_empty());
        assert_eq!(cursor(&messages), None);
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::StreamEnd {
                    status: StreamStatus::Error,
                    ..
                }
            )),
            "{messages:?}"
        );
        Ok(())
    }

    /// Runs a first fetch over `items`, with no cursor, and returns its state directory and the
    /// cursor it proposed.
    fn first_fetch(items: Vec<Value>) -> Result<(PathBuf, String, Vec<Record>)> {
        let api = api(vec![listing("sync-2", items)]);
        let request = request(None)?;
        let messages = run_fetch(&api, &request)?;
        let cursor = cursor(&messages).context("the first fetch moved on")?;
        Ok((request.state_dir, cursor, records(&messages)))
    }

    #[test]
    fn an_event_recorded_and_then_named_cancelled_is_recorded_cancelled() -> Result<()> {
        let (state_dir, first_cursor, first) = first_fetch(vec![meeting()])?;
        assert_eq!(starts(&first), ["2026-10-25T09:00:00Z"]);

        // Google names a deleted event by its id and status alone.
        let cancelled = json!({"id": "EVENT-EXAMPLE", "status": "cancelled"});
        let api = api(vec![listing("sync-3", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &state_dir)?)?;

        let records = records(&messages);
        assert_eq!(pointers(&records), ["gcal:team@example.com/EVENT-EXAMPLE"]);
        // The record replaces the one the store holds, at the start that one has.
        assert_eq!(starts(&records), ["2026-10-25T09:00:00Z"]);
        assert!(
            records
                .iter()
                .all(|record| record.text == "[event cancelled]" && !record.context),
            "{records:?}"
        );
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn an_occurrence_recorded_and_then_named_cancelled_is_recorded_cancelled() -> Result<()> {
        let slot = "2027-06-14T08:00:00Z";
        let (state_dir, first_cursor, first) = first_fetch(vec![occurrence(
            event("EVENT-SERIES_20270614", slot, "2027-06-14T09:00:00Z", None),
            slot,
        )])?;
        assert_eq!(starts(&first), [slot]);

        // Google sends a cancelled instance with its id, status, series and slot, and no start.
        let cancelled = json!({
            "id": "EVENT-SERIES_20270614",
            "status": "cancelled",
            "recurringEventId": "SERIES-EXAMPLE",
            "originalStartTime": {"dateTime": slot}
        });
        let api = api(vec![listing("sync-3", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &state_dir)?)?;

        let records = records(&messages);
        assert_eq!(
            pointers(&records),
            ["gcal:team@example.com/EVENT-SERIES_20270614"]
        );
        assert_eq!(starts(&records), [slot]);
        assert!(
            records
                .iter()
                .all(|record| record.text == "[event cancelled]" && record.context),
            "{records:?}"
        );
        Ok(())
    }

    #[test]
    fn an_event_never_recorded_and_cancelled_produces_nothing() -> Result<()> {
        let (state_dir, first_cursor, _) = first_fetch(vec![meeting()])?;

        let cancelled = json!({"id": "EVENT-NEVER-STORED", "status": "cancelled"});
        let api = api(vec![listing("sync-3", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &state_dir)?)?;

        assert!(records(&messages).is_empty(), "{messages:?}");
        assert!(warnings(&messages).is_empty(), "{messages:?}");
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        Ok(())
    }

    #[test]
    fn an_occurrence_moved_within_the_horizon_from_a_slot_past_it_follows_its_next_move(
    ) -> Result<()> {
        // Its slot, in 2030, is past the horizon, 2027-11-29T18:00:00Z; it moved to next July.
        let slot = "2030-06-14T08:00:00Z";
        let (state_dir, first_cursor, first) = first_fetch(vec![occurrence(
            event(
                "EVENT-SERIES_20300614",
                "2027-07-01T08:00:00Z",
                "2027-07-01T09:00:00Z",
                None,
            ),
            slot,
        )])?;
        assert_eq!(starts(&first), ["2027-07-01T08:00:00Z"]);

        // Then it moves past the horizon: neither its start nor its slot is within it.
        let moved = occurrence(
            event(
                "EVENT-SERIES_20300614",
                "2029-01-22T09:00:00Z",
                "2029-01-22T10:00:00Z",
                None,
            ),
            slot,
        );
        let api = api(vec![listing("sync-3", vec![moved])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &state_dir)?)?;

        assert_eq!(starts(&records(&messages)), ["2029-01-22T09:00:00Z"]);
        Ok(())
    }

    #[test]
    fn a_stored_occurrence_moved_past_the_nearer_horizon_of_an_older_window_end_follows_it(
    ) -> Result<()> {
        // Inside this fetch's horizon, 2027-11-29T18:00:00Z.
        let slot = "2027-11-28T12:00:00Z";
        let (state_dir, first_cursor, first) = first_fetch(vec![occurrence(
            event("EVENT-SERIES_20271128", slot, "2027-11-28T13:00:00Z", None),
            slot,
        )])?;
        assert_eq!(starts(&first), [slot]);

        // A fetch given an older window end has a nearer horizon, 2027-11-27T18:00:00Z, past
        // which the slot lies.
        let moved = occurrence(
            event(
                "EVENT-SERIES_20271128",
                "2029-01-22T09:00:00Z",
                "2029-01-22T10:00:00Z",
                None,
            ),
            slot,
        );
        let api = api(vec![listing("sync-3", vec![moved])]);
        let mut older = request_after(&first_cursor, &state_dir)?;
        older.window_end = at("2026-10-23T18:00:00Z")?;
        let messages = run_fetch(&api, &older)?;

        assert_eq!(api.calls.borrow().len(), 1);
        assert_eq!(starts(&records(&messages)), ["2029-01-22T09:00:00Z"]);
        Ok(())
    }

    #[test]
    fn an_event_that_started_more_than_the_horizon_ago_is_forgotten() -> Result<()> {
        let old = event(
            "EVENT-OLD",
            "2025-10-01T09:00:00Z",
            "2025-10-01T10:00:00Z",
            None,
        );
        let (state_dir, first_cursor, first) = first_fetch(vec![old, meeting()])?;
        assert_eq!(first.len(), 2);
        let list = std::fs::read_to_string(state_dir.join("emitted.json"))?;
        assert!(list.contains("EVENT-OLD"), "{list}");

        // 400 days before this window's end is 2025-10-26T18:00:00Z, after the old event started.
        let cancelled = json!({"id": "EVENT-OLD", "status": "cancelled"});
        let api = api(vec![
            listing("sync-3", vec![cancelled]),
            listing("listing-token", Vec::new()),
        ]);
        let mut later = request_after(&first_cursor, &state_dir)?;
        later.window_end = at("2026-11-30T18:00:00Z")?;
        let messages = run_fetch(&api, &later)?;

        assert!(records(&messages).is_empty(), "{messages:?}");
        let list = std::fs::read_to_string(state_dir.join("emitted.json"))?;
        assert!(!list.contains("EVENT-OLD"), "{list}");
        assert!(list.contains("EVENT-EXAMPLE"), "{list}");
        Ok(())
    }

    #[test]
    fn the_list_of_emitted_events_holds_only_the_series_instances_recorded() -> Result<()> {
        let api = api(vec![listing(
            "sync-2",
            (2026..=2056).map(birthday).collect(),
        )]);
        let request = request(None)?;
        run_fetch(&api, &request)?;

        let list = std::fs::read_to_string(request.state_dir.join("emitted.json"))?;
        assert!(list.contains("BIRTHDAY-EXAMPLE_20271103"), "{list}");
        assert!(!list.contains("BIRTHDAY-EXAMPLE_20281103"), "{list}");
        Ok(())
    }

    #[test]
    fn a_corrupt_list_of_emitted_events_falls_back_to_the_slot_with_a_warning() -> Result<()> {
        let (state_dir, first_cursor, _) = first_fetch(vec![meeting()])?;
        std::fs::write(state_dir.join("emitted.json"), b"{not json")?;

        let slot = "2027-06-14T08:00:00Z";
        let moved = occurrence(
            event(
                "EVENT-SERIES_20270614",
                "2029-01-22T09:00:00Z",
                "2029-01-22T10:00:00Z",
                None,
            ),
            slot,
        );
        let cancelled = json!({"id": "EVENT-EXAMPLE", "status": "cancelled"});
        let api = api(vec![listing("sync-3", vec![cancelled, moved])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &state_dir)?)?;

        // The list is lost, so the cancellation of the stored meeting is not recorded, and the
        // move is followed by its slot.
        assert_eq!(
            pointers(&records(&messages)),
            ["gcal:team@example.com/EVENT-SERIES_20270614"]
        );
        assert!(
            warnings(&messages)
                .iter()
                .any(|warning| warning.contains("emitted.json")),
            "{messages:?}"
        );
        assert!(!ended_with_error(&messages), "{messages:?}");
        assert_eq!(
            cursor(&messages),
            Some("sync-3@2026-10-25T18:00:00Z".to_owned())
        );
        let list = std::fs::read_to_string(state_dir.join("emitted.json"))?;
        assert!(list.contains("EVENT-SERIES_20270614"), "{list}");
        Ok(())
    }

    #[test]
    fn a_calendar_with_a_cursor_and_no_list_of_emitted_events_warns_once() -> Result<()> {
        // An earlier build kept no list, so a calendar with a cursor has records it does not know.
        let first_api = api(vec![listing("sync-3", Vec::new())]);
        let first = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        let first_messages = run_fetch(&first_api, &first)?;
        assert!(
            warnings(&first_messages)
                .iter()
                .any(|warning| warning.contains("emitted.json")),
            "{first_messages:?}"
        );
        assert!(!ended_with_error(&first_messages), "{first_messages:?}");
        let first_cursor = cursor(&first_messages).context("the first fetch moved on")?;

        let second_api = api(vec![listing("sync-4", Vec::new())]);
        let messages = run_fetch(
            &second_api,
            &request_after(&first_cursor, &first.state_dir)?,
        )?;
        assert!(warnings(&messages).is_empty(), "{messages:?}");
        Ok(())
    }

    #[test]
    fn a_cancellation_after_a_window_that_was_not_stored_reaches_the_record_the_tree_holds(
    ) -> Result<()> {
        let slot = "2027-06-14T08:00:00Z";
        let (state_dir, stored_cursor, first) = first_fetch(vec![occurrence(
            event("EVENT-SERIES_20270614", slot, "2027-06-14T09:00:00Z", None),
            slot,
        )])?;
        assert_eq!(starts(&first), [slot]);

        // A fetch reads the occurrence moved to more than 400 days ago, and its window is never
        // stored: the store keeps June 2027, and the cursor stays where it was.
        let moved = occurrence(
            event(
                "EVENT-SERIES_20270614",
                "2025-01-01T08:00:00Z",
                "2025-01-01T09:00:00Z",
                None,
            ),
            slot,
        );
        let lost_api = api(vec![listing("sync-3", vec![moved])]);
        let lost = run_fetch(&lost_api, &request_after(&stored_cursor, &state_dir)?)?;
        assert_eq!(starts(&records(&lost)), ["2025-01-01T08:00:00Z"]);

        // Before the fetch that reads the same changes again, the occurrence is cancelled.
        let cancelled = json!({
            "id": "EVENT-SERIES_20270614",
            "status": "cancelled",
            "recurringEventId": "SERIES-EXAMPLE",
            "originalStartTime": {"dateTime": slot}
        });
        let api = api(vec![listing("sync-4", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&stored_cursor, &state_dir)?)?;

        let records = records(&messages);
        assert_eq!(
            pointers(&records),
            ["gcal:team@example.com/EVENT-SERIES_20270614"]
        );
        assert_eq!(starts(&records), [slot]);
        assert!(
            records
                .iter()
                .all(|record| record.text == "[event cancelled]"),
            "{records:?}"
        );
        Ok(())
    }

    #[test]
    fn a_cancellation_after_a_window_stored_without_moving_its_cursor_reaches_the_record(
    ) -> Result<()> {
        let (state_dir, stored_cursor, _) = first_fetch(vec![meeting()])?;

        // A fetch given an older window end records a new meeting. Its window is stored, but the
        // cursor stays where it was, since a newer window moved it.
        let new = event(
            "EVENT-NEW",
            "2026-11-02T09:00:00Z",
            "2026-11-02T10:00:00Z",
            None,
        );
        let older_api = api(vec![listing("sync-3", vec![new])]);
        let mut older = request_after(&stored_cursor, &state_dir)?;
        older.window_end = at("2026-10-23T18:00:00Z")?;
        let stored = run_fetch(&older_api, &older)?;
        assert_eq!(records(&stored).len(), 1);

        // The next fetch, from the cursor that did not move, finds it cancelled. Whether that
        // window was stored cannot be told, so the cancellation is recorded.
        let cancelled = json!({"id": "EVENT-NEW", "status": "cancelled"});
        let api = api(vec![listing("sync-4", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&stored_cursor, &state_dir)?)?;

        let records = records(&messages);
        assert_eq!(pointers(&records), ["gcal:team@example.com/EVENT-NEW"]);
        assert_eq!(starts(&records), ["2026-11-02T09:00:00Z"]);
        assert!(
            records
                .iter()
                .all(|record| record.text == "[event cancelled]"),
            "{records:?}"
        );
        Ok(())
    }

    #[test]
    fn an_excluded_event_is_not_listed_as_recorded() -> Result<()> {
        // The fetch drops the record of an event the deployment excludes, so the tree never
        // holds it.
        let first_api = api(vec![listing("sync-2", vec![meeting()])]);
        let mut first = request(None)?;
        first.exclude = vec!["gcal:team@example.com/EVENT-EXAMPLE".to_owned()];
        let first_messages = run_fetch(&first_api, &first)?;
        let first_cursor = cursor(&first_messages).context("the first fetch moved on")?;
        let list = std::fs::read_to_string(first.state_dir.join("emitted.json"))?;
        assert!(!list.contains("EVENT-EXAMPLE"), "{list}");

        let cancelled = json!({"id": "EVENT-EXAMPLE", "status": "cancelled"});
        let api = api(vec![listing("sync-3", vec![cancelled])]);
        let messages = run_fetch(&api, &request_after(&first_cursor, &first.state_dir)?)?;

        assert!(records(&messages).is_empty(), "{messages:?}");
        Ok(())
    }

    #[test]
    fn a_list_of_emitted_events_that_cannot_be_saved_leaves_the_cursor_alone() -> Result<()> {
        let mut request = request(Some("sync-2@2026-10-25T18:00:00Z"))?;
        // The state directory is a file, so nothing can be saved under it.
        let blocked = request.state_dir.join("blocked");
        std::fs::write(&blocked, b"")?;
        request.state_dir = blocked;
        let api = api(vec![listing("sync-3", vec![meeting()])]);
        let messages = run_fetch(&api, &request)?;

        // Stored without its entry in the list, the meeting's cancellation would later be lost,
        // so the stream stores nothing and the next fetch reads the same changes again.
        assert!(records(&messages).is_empty(), "{messages:?}");
        assert_eq!(cursor(&messages), None);
        assert!(ended_with_error(&messages), "{messages:?}");
        Ok(())
    }
}
