//! Built-in Google Calendar source.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::gws;
use crate::record::Record;

use super::{
    window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source, StreamStatus,
    PROTOCOL_VERSION,
};

/// Built-in source for Google Calendar events.
pub struct CalendarSource;

impl CalendarSource {
    /// Creates a Calendar source using the `gws` command line client.
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
                fetch_with_api(request, &config, &GwsCalendarApi, sink)
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

trait CalendarApi {
    fn events(&self, params: &Value) -> Result<Vec<Value>>;
}

struct GwsCalendarApi;

impl CalendarApi for GwsCalendarApi {
    fn events(&self, params: &Value) -> Result<Vec<Value>> {
        gws::gws_paginate(&["calendar", "events", "list"], params, "items")
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

fn fetch_with_api<C: CalendarApi + ?Sized>(
    request: &FetchRequest,
    config: &CalendarConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    for calendar in &config.calendars {
        let result = window_start(
            request.cursors.get(calendar).map(String::as_str),
            request.window_end,
            request.backfill_hours,
        )
        .and_then(|start| {
            fetch_events(api, calendar, start).and_then(|events| {
                records_from_events(
                    events,
                    calendar,
                    &request.source.scheme,
                    start,
                    request.window_end,
                )
            })
        });
        match result {
            Ok(records) => {
                for record in records {
                    sink(Message::Record(Box::new(record)))?;
                }
                sink(Message::StreamEnd {
                    stream: calendar.clone(),
                    status: StreamStatus::Ok,
                    cursor: Some(
                        request
                            .window_end
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                    ),
                    error: None,
                })?;
            }
            Err(error) => sink(Message::StreamEnd {
                stream: calendar.clone(),
                status: StreamStatus::Error,
                cursor: None,
                error: Some(format!("{error:#}")),
            })?,
        }
    }
    Ok(())
}

fn fetch_events<C: CalendarApi + ?Sized>(
    api: &C,
    calendar: &str,
    start: DateTime<Utc>,
) -> Result<Vec<Value>> {
    api.events(&json!({
        "calendarId": calendar,
        "updatedMin": start.to_rfc3339_opts(SecondsFormat::Secs, true),
        "singleEvents": true,
    }))
}

fn records_from_events(
    events: Vec<Value>,
    calendar: &str,
    scheme: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for event in events {
        let timestamp = event_timestamp(&event)?;
        if timestamp > start && timestamp <= end {
            records.push(record_from_event(&event, calendar, scheme, timestamp)?);
        }
    }
    Ok(records)
}

fn record_from_event(
    event: &Value,
    calendar: &str,
    scheme: &str,
    timestamp: DateTime<Utc>,
) -> Result<Record> {
    let id = required_event_string(event, "id")?;
    let title = event
        .get("summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.is_empty())
        .map(str::to_owned);
    let organizer = event.get("organizer");
    let author = organizer
        .and_then(|organizer| {
            organizer
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    organizer
                        .get("email")
                        .and_then(Value::as_str)
                        .filter(|email| !email.is_empty())
                })
        })
        .map(str::to_owned);
    let author_id = organizer
        .and_then(|organizer| organizer.get("email"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned);
    let text = event
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
        .map_or_else(String::new, str::to_owned);
    Ok(Record {
        pointer: format!("{scheme}:{calendar}/{id}"),
        stream: calendar.to_owned(),
        ts: timestamp,
        label: Some(calendar.to_owned()),
        author,
        author_id,
        thread: None,
        title,
        url: event
            .get("htmlLink")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text,
        aliases: vec![id.to_owned()],
        context: false,
        raw: event.clone(),
    })
}

fn required_event_string<'a>(event: &'a Value, field: &str) -> Result<&'a str> {
    event
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Calendar event is missing {field}"))
}

fn event_timestamp(event: &Value) -> Result<DateTime<Utc>> {
    let value = required_event_string(event, "updated")?;
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .with_context(|| format!("parsing Calendar event updated {value:?}"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct FakeCalendarApi {
        calls: RefCell<Vec<Value>>,
        events: Vec<Value>,
    }

    impl CalendarApi for FakeCalendarApi {
        fn events(&self, params: &Value) -> Result<Vec<Value>> {
            self.calls.borrow_mut().push(params.clone());
            Ok(self.events.clone())
        }
    }

    #[test]
    fn calendar_fetches_events_updated_in_the_window() -> Result<()> {
        let config = parse_config(&json!({"calendars": ["team@example.com"]}))?;
        let window_end = DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc);
        let request = FetchRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "calendar".to_owned(),
                kind: "calendar".to_owned(),
                scheme: "gcal".to_owned(),
            },
            config: json!({"calendars": ["team@example.com"]}),
            window_end,
            backfill_hours: 24,
            cursors: std::collections::BTreeMap::from([(
                "team@example.com".to_owned(),
                "2026-10-25T16:00:00Z".to_owned(),
            )]),
            discovered: Vec::new(),
            state_dir: std::env::temp_dir(),
        };
        let api = FakeCalendarApi {
            events: vec![
                json!({
                    "id": "past-edited",
                    "updated": "2026-10-25T17:00:00Z",
                    "start": {"dateTime": "2020-01-01T09:00:00Z"},
                    "end": {"dateTime": "2020-01-01T10:00:00Z"},
                }),
                json!({
                    "id": "future-created",
                    "updated": "2026-10-25T17:30:00Z",
                    "start": {"dateTime": "2030-01-01T09:00:00Z"},
                    "end": {"dateTime": "2030-01-01T10:00:00Z"},
                }),
                json!({
                    "id": "updated-later",
                    "updated": "2026-10-25T18:01:00Z",
                    "start": {"dateTime": "2026-10-25T09:00:00Z"},
                    "end": {"dateTime": "2026-10-25T10:00:00Z"},
                }),
            ],
            ..FakeCalendarApi::default()
        };
        let mut messages = Vec::new();
        fetch_with_api(&request, &config, &api, &mut |message| {
            messages.push(message);
            Ok(())
        })?;

        let records = messages
            .iter()
            .filter_map(|message| match message {
                Message::Record(record) => Some((
                    record.pointer.as_str(),
                    record.ts.to_rfc3339_opts(SecondsFormat::Secs, true),
                )),
                Message::Hello { .. } | Message::Warning { .. } | Message::StreamEnd { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            records,
            vec![
                (
                    "gcal:team@example.com/past-edited",
                    "2026-10-25T17:00:00Z".to_owned()
                ),
                (
                    "gcal:team@example.com/future-created",
                    "2026-10-25T17:30:00Z".to_owned()
                ),
            ]
        );
        let calls = api.calls.borrow();
        let params = calls.first().context("Calendar request has parameters")?;
        assert_eq!(
            params.get("calendarId").and_then(Value::as_str),
            Some("team@example.com")
        );
        assert_eq!(
            params.get("updatedMin").and_then(Value::as_str),
            Some("2026-10-25T16:00:00Z")
        );
        assert_eq!(params.get("timeMin"), None);
        assert_eq!(params.get("timeMax"), None);
        assert_eq!(
            params.get("singleEvents").and_then(Value::as_bool),
            Some(true)
        );
        Ok(())
    }
}
