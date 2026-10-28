//! Built-in Google Calendar source.
//!
//! Each configured calendar is one stream whose cursor is a Calendar sync token: a fetch asks
//! for what changed since the last run, and a first fetch lists every event from the backfill
//! window onwards. A record's timestamp is the event's start, so an event that starts after the
//! window ends is a context record.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeZone as _, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::api::client;
use crate::api::google::{self, Google};
use crate::record::Record;

use super::{
    token_cursor, window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source,
    TokenCursor, PROTOCOL_VERSION,
};

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

/// One calendar's records, warnings, and the cursor that follows them.
#[derive(Default)]
struct Collected {
    records: Vec<Record>,
    warnings: Vec<String>,
    cursor: String,
}

fn fetch_with_api<C: CalendarApi + ?Sized>(
    request: &FetchRequest,
    config: &CalendarConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    for calendar in &config.calendars {
        super::emit_stream(
            calendar,
            collect_calendar(request, calendar, api).map(|c| (c.warnings, c.records, c.cursor)),
            sink,
        )?;
    }
    Ok(())
}

/// Reads one calendar: incrementally when the cursor names a sync token, else from the time it
/// names or the backfill window.
fn collect_calendar<C: CalendarApi + ?Sized>(
    request: &FetchRequest,
    calendar: &str,
    api: &C,
) -> Result<Collected> {
    let cursor = request
        .cursors
        .get(calendar)
        .map(String::as_str)
        .map(TokenCursor::parse);
    let backfill = window_start(None, request.window_end, request.backfill_hours)?;
    let mut collected = Collected::default();
    let mut page = match cursor {
        Some(TokenCursor::Token { token, .. }) => api.events(calendar, &sync_query(token))?,
        Some(TokenCursor::Time(start)) => api.events(calendar, &initial_query(start))?,
        None => api.events(calendar, &initial_query(backfill))?,
    };
    if page.expired {
        // Google refused the sync token, so the calendar is listed again. Only the time the
        // cursor carries says how far back that listing reaches; without it a listing of the
        // backfill window would step over every event older than it and still write a token.
        let Some(TokenCursor::Token {
            stored_through: Some(stored_through),
            ..
        }) = cursor
        else {
            bail!(
                "Calendar {calendar} refused its sync token, and the cursor names no time through which its events are stored; set the cursor to the RFC 3339 time to list from"
            );
        };
        collected.warnings.push(format!(
            "Calendar {calendar} refused its sync token; listing from {} again",
            stored_through.to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
        page = api.events(calendar, &initial_query(stored_through))?;
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
    for event in &page.items {
        if event.get("status").and_then(Value::as_str) == Some("cancelled") {
            continue;
        }
        collected.records.push(record_from_event(
            event,
            calendar,
            &request.source.scheme,
            request.window_end,
        )?);
    }
    Ok(collected)
}

/// The query of a first listing: everything from the backfill window onwards.
fn initial_query(start: DateTime<Utc>) -> Vec<(&'static str, String)> {
    vec![
        ("timeMin", start.to_rfc3339_opts(SecondsFormat::Secs, true)),
        ("singleEvents", "true".to_owned()),
    ]
}

/// The query of an incremental listing.
fn sync_query(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("syncToken", token.to_owned()),
        ("singleEvents", "true".to_owned()),
    ]
}

/// Builds one record whose timestamp is the event's start.
///
/// An event that starts after the window ends is a context record: the window must not carry a
/// record after its end, and an upcoming meeting is still worth citing.
fn record_from_event(
    event: &Value,
    calendar: &str,
    scheme: &str,
    window_end: DateTime<Utc>,
) -> Result<Record> {
    let id = required_event_string(event, "id")?;
    let start = event_start(event)?;
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
    if let Some(value) = start.get("dateTime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(value)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .with_context(|| format!("parsing Calendar event start {value:?}"));
    }
    let value = start
        .get("date")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Calendar event start has neither dateTime nor date"))?;
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("parsing Calendar event date {value:?}"))?;
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("Calendar event date {value:?} has no midnight"))?;
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
            state_dir: std::env::temp_dir(),
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
        let call = calls.first().context("the fetch made a request")?;
        assert_eq!(
            query_value(call, "timeMin"),
            Some("2026-10-24T18:00:00Z".to_owned())
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
}
