//! Source protocol types and validation shared by built-in and external sources.

mod calendar;
mod drive;
mod external;
pub mod github;
pub mod gmail;
pub mod slack;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{default_scheme, SourceConfig};
use crate::pointer::Schemes;
use crate::record::Record;
use crate::tree::Tree;

pub use calendar::CalendarSource;
pub use drive::DriveSource;
pub use external::External;
pub use github::GithubSource;
pub use gmail::GmailSource;
pub use slack::SlackSource;

/// The version of the JSONL source protocol understood by this crate.
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest pointer, or thread pointer, a source may name a record with.
pub const MAX_POINTER_BYTES: usize = 2048;

/// A request sent to a configured source.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Fetch a bounded window of records.
    Fetch(FetchRequest),
    /// Resolve specific pointers that were absent from the local store.
    Resolve(ResolveRequest),
}

impl Request {
    /// Returns the configured source named by this request.
    #[must_use]
    pub fn source(&self) -> &SourceRef {
        match self {
            Self::Fetch(request) => &request.source,
            Self::Resolve(request) => &request.source,
        }
    }

    /// Returns the operation requested from the source.
    #[must_use]
    pub const fn op(&self) -> Op {
        match self {
            Self::Fetch(_) => Op::Fetch,
            Self::Resolve(_) => Op::Resolve,
        }
    }
}

/// Fetch operation input supplied to a source.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub protocol: u32,
    pub source: SourceRef,
    pub config: serde_json::Value,
    pub window_end: DateTime<Utc>,
    pub backfill_hours: u32,
    pub cursors: BTreeMap<String, String>,
    pub discovered: Vec<DiscoveredStream>,
    /// `<data_dir>/sources/<id>`: what the source keeps between fetches, shared by every
    /// machine that fetches this tree, so a fetch elsewhere still reaches it.
    pub state_dir: PathBuf,
    /// The deployment's exclusion patterns ([`crate::exclude`]). The fetch drops every record
    /// one names, so a source may skip reading them; a source that ignores this loses nothing
    /// but the requests. Omitted when there are none, so a source written before it existed
    /// reads the same request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// Live pointer resolution input supplied to a source.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub protocol: u32,
    pub source: SourceRef,
    pub config: serde_json::Value,
    pub pointers: Vec<String>,
    /// `<state_dir>/sources/<id>`: scratch space on this machine. A resolve is a read, and a
    /// read never writes the shared data directory.
    pub state_dir: PathBuf,
}

/// Identifies one configured source and its pointer scheme.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub id: String,
    pub kind: String,
    pub scheme: String,
}

/// A stream discovered from a tree record's configured frontmatter field.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredStream {
    pub stream: String,
    pub owner: String,
}

/// One message emitted by a source over the protocol stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// Declares a source's protocol version, scheme, capabilities, and attempted streams.
    Hello {
        protocol: u32,
        scheme: String,
        ops: Vec<Op>,
        verbatim: bool,
        #[serde(default)]
        streams: Vec<String>,
    },
    /// Delivers one normalized record.
    Record(Box<Record>),
    /// Reports a non-fatal source warning.
    Warning {
        #[serde(default)]
        stream: Option<String>,
        message: String,
    },
    /// Marks one fetch stream as complete or failed.
    StreamEnd {
        stream: String,
        status: StreamStatus,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
}

/// A source operation advertised in its hello message.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Fetch,
    Resolve,
}

/// The terminal state of a fetch stream.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StreamStatus {
    Ok,
    Error,
}

/// Runs one source request and forwards its messages to `sink`.
pub trait Source {
    /// Emits `Hello` first, then messages. Returning an error fails every unfinished stream.
    ///
    /// # Errors
    ///
    /// Returns an error when source execution or the message sink fails.
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()>;
}

/// Opens the source implementation configured by `spec`.
///
/// # Errors
///
/// Returns an error when an external executable is not available on `PATH`.
pub fn open(spec: &SourceConfig, scheme: &str) -> Result<Box<dyn Source>> {
    match spec.kind.as_str() {
        "slack" => Ok::<Box<dyn Source>, anyhow::Error>(Box::new(SlackSource::new())),
        "gmail" => Ok::<Box<dyn Source>, anyhow::Error>(Box::new(GmailSource::new())),
        "drive" => Ok::<Box<dyn Source>, anyhow::Error>(Box::new(DriveSource::new())),
        "calendar" => Ok::<Box<dyn Source>, anyhow::Error>(Box::new(CalendarSource::new())),
        "github" => Ok::<Box<dyn Source>, anyhow::Error>(Box::new(GithubSource::new())),
        _ => {
            let external = External::find(&spec.kind)?.with_timeout_secs(spec.timeout_secs);
            Ok::<Box<dyn Source>, anyhow::Error>(Box::new(external))
        }
    }
    .with_context(|| format!("opening source {} for scheme {scheme}", spec.id))
}

/// Resolves the pointer scheme assigned to a configured source.
///
/// # Errors
///
/// Returns an error if neither the source configuration nor its source kind supplies a scheme.
pub fn scheme(spec: &SourceConfig) -> Result<String> {
    spec.scheme
        .clone()
        .or_else(|| default_scheme(&spec.kind).map(str::to_owned))
        .ok_or_else(|| anyhow!("source {} has no pointer scheme", spec.id))
}

/// Builds the pointer parser registry for every configured source.
///
/// # Errors
///
/// Returns an error when a configured source has no assigned scheme.
pub fn configured_schemes(sources: &[SourceConfig]) -> Result<Schemes> {
    let schemes = sources.iter().map(scheme).collect::<Result<Vec<_>>>()?;
    Ok(Schemes::new(schemes))
}

/// Serializes source-specific settings for the wire request.
///
/// # Errors
///
/// Returns an error when the settings table cannot be represented as JSON.
pub fn config_json(spec: &SourceConfig) -> Result<serde_json::Value> {
    serde_json::to_value(&spec.settings)
        .with_context(|| format!("serializing source {} configuration", spec.id))
}

/// Returns every tree record whose `field` starts with a stream identifier.
#[must_use]
pub fn discover_streams(tree: &Tree, field: &str) -> Vec<DiscoveredStream> {
    tree.docs
        .iter()
        .filter_map(|document| {
            document
                .front
                .extra_str(field)
                .and_then(|value| value.split_whitespace().next())
                .filter(|stream| !stream.is_empty())
                .map(|stream| DiscoveredStream {
                    stream: stream.to_owned(),
                    owner: document.rel.to_string_lossy().into_owned(),
                })
        })
        .collect()
}

/// Returns the start of a time-based source window.
///
/// # Errors
///
/// Returns an error when a present cursor is not an RFC 3339 timestamp.
pub fn window_start(
    cursor: Option<&str>,
    window_end: DateTime<Utc>,
    backfill_hours: u32,
) -> Result<DateTime<Utc>> {
    match cursor {
        Some(cursor) => DateTime::parse_from_rfc3339(cursor)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .with_context(|| format!("parsing source cursor {cursor}")),
        None => Ok(window_end - Duration::hours(i64::from(backfill_hours))),
    }
}

/// The longest stretch of time one run lists for a stream that lists by time.
///
/// A stream whose position is far behind its window end — after an outage, or on a first fetch
/// with a long backfill — reads the gap a slice at a time, oldest first, and names the slice's
/// end as its cursor, so one run's requests stay bounded and every run moves the stream on.
/// Seven days is about what the Gmail history path reads in one run, since Gmail keeps a
/// mailbox's history for about a week.
pub const MAX_SLICE_DAYS: i64 = 7;

/// Where the slice a run lists from `start` ends: `MAX_SLICE_DAYS` after `start`, or the window
/// end when that comes first.
#[must_use]
pub fn slice_end(start: DateTime<Utc>, window_end: DateTime<Utc>) -> DateTime<Utc> {
    (start + Duration::days(MAX_SLICE_DAYS)).min(window_end)
}

/// What separates a token from the time beside it in a cursor.
const STORED_THROUGH: char = '@';

/// What a cursor names when a stream's position is a token its API issued.
///
/// A token — a Gmail history ID, a Calendar sync token — says nothing about time, so a run
/// whose token the API no longer holds cannot tell what the stream already stored, and listing
/// the backfill window instead would step over everything older than it. Writing the time
/// beside the token, as `<token>@<rfc3339>`, lets that run list from what is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenCursor<'a> {
    /// A token, with the time through which the stream is stored when the cursor carries one.
    Token {
        token: &'a str,
        stored_through: Option<DateTime<Utc>>,
    },
    /// A time, which a stream names when it holds no token to follow.
    Time(DateTime<Utc>),
}

impl<'a> TokenCursor<'a> {
    /// Reads a stored cursor. A cursor an earlier build wrote names no time.
    #[must_use]
    pub fn parse(cursor: &'a str) -> Self {
        if let Some((token, time)) = cursor.rsplit_once(STORED_THROUGH) {
            if let Some(stored_through) = timestamp(time) {
                return Self::Token {
                    token,
                    stored_through: Some(stored_through),
                };
            }
        }
        timestamp(cursor).map_or(
            Self::Token {
                token: cursor,
                stored_through: None,
            },
            Self::Time,
        )
    }
}

/// Writes the cursor of a stream that holds `token` and is stored through `stored_through`.
#[must_use]
pub fn token_cursor(token: &str, stored_through: DateTime<Utc>) -> String {
    format!(
        "{token}{STORED_THROUGH}{}",
        stored_through.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// What separates the times a sliced listing covers from the token taken before its first slice.
const TAKEN_FIRST: char = '#';
/// What separates the time a catch-up began from the time its slices reach, as an ISO 8601
/// interval does.
const BEGAN: char = '/';

/// Where a stream that lists a long gap a slice at a time stands, once the token that follows
/// the listing has been taken: the time its slices reach, that token, and, for a stream whose
/// token can expire, the time the catch-up began.
///
/// The token is taken before the first slice and adopted after the last. A token taken only
/// when the last slice is listed opens a feed that never names what changed during the
/// catch-up with a time behind the slices — a file uploaded with an old modified time, mail
/// imported with an old date — and no later slice lists it either. A stream whose token expires
/// before the last slice lists again from where the catch-up began, under a new token.
///
/// Written `<rfc3339>#<token>`, or `<began>/<rfc3339>#<token>` when it carries the start. A
/// bare time, which an earlier build wrote, names no token, and the run that reads it takes one
/// before it lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SliceCursor<'a> {
    /// The time the catch-up began, when the stream keeps it.
    pub began: Option<DateTime<Utc>>,
    /// The time the slices listed so far reach.
    pub through: DateTime<Utc>,
    /// The token taken before the first slice.
    pub token: &'a str,
}

impl<'a> SliceCursor<'a> {
    /// Reads a cursor written by [`slice_cursor`], or `None` for any other cursor.
    #[must_use]
    pub fn parse(cursor: &'a str) -> Option<Self> {
        let (times, token) = cursor.split_once(TAKEN_FIRST)?;
        if token.is_empty() {
            return None;
        }
        let (began, through) = match times.split_once(BEGAN) {
            Some((began, through)) => (Some(timestamp(began)?), timestamp(through)?),
            None => (None, timestamp(times)?),
        };
        Some(Self {
            began,
            through,
            token,
        })
    }
}

/// Writes the cursor of a listing whose slices reach `through`, holding `token` for the run
/// whose slice reaches the window end, and the time the catch-up `began` when the stream keeps
/// it.
#[must_use]
pub fn slice_cursor(began: Option<DateTime<Utc>>, through: DateTime<Utc>, token: &str) -> String {
    let through = through.to_rfc3339_opts(SecondsFormat::Secs, true);
    began.map_or_else(
        || format!("{through}{TAKEN_FIRST}{token}"),
        |began| {
            format!(
                "{}{BEGAN}{through}{TAKEN_FIRST}{token}",
                began.to_rfc3339_opts(SecondsFormat::Secs, true)
            )
        },
    )
}

fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// Emits a fetch stream's warnings, then its records, then its terminal `StreamEnd`.
///
/// On success the stream ends `Ok` at `cursor`; on failure it ends `Error` with the cause and
/// no cursor, so a failing stream does not advance its position.
///
/// # Errors
///
/// Returns an error when the sink cannot accept a message.
pub fn emit_stream(
    stream: &str,
    collected: Result<(Vec<String>, Vec<Record>, String)>,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    match collected {
        Ok((warnings, records, cursor)) => {
            for warning in warnings {
                sink(Message::Warning {
                    stream: Some(stream.to_owned()),
                    message: warning,
                })?;
            }
            for record in records {
                sink(Message::Record(Box::new(record)))?;
            }
            sink(Message::StreamEnd {
                stream: stream.to_owned(),
                status: StreamStatus::Ok,
                cursor: Some(cursor),
                error: None,
            })
        }
        Err(error) => sink(Message::StreamEnd {
            stream: stream.to_owned(),
            status: StreamStatus::Error,
            cursor: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

/// An accepted message kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Accepted {
    Hello,
    Record,
    Warning,
    StreamEnd,
}

/// A warning retained from a source protocol stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceWarning {
    pub stream: Option<String>,
    pub message: String,
}

/// A source's terminal result for one fetch stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StreamResult {
    pub status: StreamStatus,
    pub cursor: Option<String>,
    pub error: Option<String>,
}

/// The validated records, stream results, and warnings from one source execution.
#[derive(Clone, Debug, Serialize)]
pub struct SourceOutcome {
    pub records: Vec<Record>,
    pub streams: BTreeMap<String, StreamResult>,
    pub warnings: Vec<SourceWarning>,
    pub verbatim: Option<bool>,
    pub error: Option<String>,
}

impl SourceOutcome {
    /// Returns whether the source process or any planned stream failed.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.error.is_some()
            || self
                .streams
                .values()
                .any(|result| result.status == StreamStatus::Error)
    }
}

/// Validates a source protocol stream against its request.
pub struct Session<'a> {
    request: &'a Request,
    schemes: &'a Schemes,
    seen_end: BTreeSet<String>,
    planned: BTreeSet<String>,
    hello: bool,
    verbatim: Option<bool>,
    records: Vec<Record>,
    streams: BTreeMap<String, StreamResult>,
    warnings: Vec<SourceWarning>,
}

impl<'a> Session<'a> {
    /// Starts validation for `request`.
    #[must_use]
    pub fn new(request: &'a Request, schemes: &'a Schemes) -> Self {
        Self {
            request,
            schemes,
            seen_end: BTreeSet::new(),
            planned: candidate_streams(request),
            hello: false,
            verbatim: None,
            records: Vec::new(),
            streams: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Validates and retains one source message.
    ///
    /// # Errors
    ///
    /// Returns an error when the message violates the source protocol for this request.
    pub fn accept(&mut self, message: Message) -> Result<Accepted> {
        if !self.hello {
            return self.accept_hello(message);
        }
        match message {
            Message::Hello { .. } => bail!("source emitted hello more than once"),
            Message::Record(record) => self.accept_record(*record),
            Message::Warning { stream, message } => {
                self.warnings.push(SourceWarning { stream, message });
                Ok(Accepted::Warning)
            }
            Message::StreamEnd {
                stream,
                status,
                cursor,
                error,
            } => self.accept_stream_end(stream, status, cursor, error),
        }
    }

    /// Produces source results after a source returns or fails.
    #[must_use]
    pub fn finish(mut self, run: Result<()>) -> SourceOutcome {
        let run_error = run.err().map(|error| format!("{error:#}"));
        if !self.hello {
            let error = run_error.unwrap_or_else(|| "source did not emit hello".to_owned());
            self.mark_unfinished(&error);
            return SourceOutcome {
                records: Vec::new(),
                streams: self.streams,
                warnings: self.warnings,
                verbatim: None,
                error: Some(error),
            };
        }
        if let Some(error) = run_error.as_deref() {
            self.mark_unfinished(error);
        } else {
            self.mark_unfinished("source ended without stream_end");
        }
        SourceOutcome {
            records: self.records,
            streams: self.streams,
            warnings: self.warnings,
            verbatim: self.verbatim,
            error: run_error,
        }
    }

    fn accept_hello(&mut self, message: Message) -> Result<Accepted> {
        let Message::Hello {
            protocol,
            scheme,
            ops,
            verbatim,
            streams,
        } = message
        else {
            bail!("source must emit hello before other messages");
        };
        if protocol != PROTOCOL_VERSION {
            bail!("source protocol version must be {PROTOCOL_VERSION}, got {protocol}");
        }
        if scheme != self.request.source().scheme {
            bail!(
                "source declared scheme {scheme}, expected {}",
                self.request.source().scheme
            );
        }
        if !ops.contains(&self.request.op()) {
            bail!("source does not support {:?}", self.request.op());
        }
        if matches!(self.request, Request::Fetch(_)) {
            self.planned = streams.into_iter().collect();
        }
        self.hello = true;
        self.verbatim = Some(verbatim);
        Ok(Accepted::Hello)
    }

    fn accept_record(&mut self, record: Record) -> Result<Accepted> {
        // A pointer is a message's identity: every answer quotes it whole, so it is bounded
        // here, where a source hands it over, rather than cut where an answer is sized.
        for pointer in std::iter::once(&record.pointer).chain(record.thread.as_ref()) {
            if pointer.len() > MAX_POINTER_BYTES {
                bail!(
                    "record pointer {}… is longer than {MAX_POINTER_BYTES} bytes",
                    pointer.chars().take(64).collect::<String>()
                );
            }
        }
        let pointer = self
            .schemes
            .parse(&record.pointer)
            .ok_or_else(|| anyhow!("record pointer {} is invalid", record.pointer))?;
        if pointer.scheme != self.request.source().scheme {
            bail!(
                "record pointer {} does not use source scheme {}",
                record.pointer,
                self.request.source().scheme
            );
        }
        if let Request::Fetch(request) = self.request {
            if !self.planned.contains(&record.stream) {
                bail!("record stream {} was not planned", record.stream);
            }
            if self.seen_end.contains(&record.stream) {
                self.streams.insert(
                    record.stream.clone(),
                    StreamResult {
                        status: StreamStatus::Error,
                        cursor: None,
                        error: Some("record emitted after stream_end".to_owned()),
                    },
                );
                bail!(
                    "record stream {} was emitted after stream_end",
                    record.stream
                );
            }
            if !record.context && record.ts > request.window_end {
                bail!(
                    "record {} occurs after window end {}",
                    record.pointer,
                    request.window_end
                );
            }
        }
        self.records.push(record);
        Ok(Accepted::Record)
    }

    fn accept_stream_end(
        &mut self,
        stream: String,
        status: StreamStatus,
        cursor: Option<String>,
        error: Option<String>,
    ) -> Result<Accepted> {
        if !matches!(self.request, Request::Fetch(_)) {
            bail!("resolve sources must not emit stream_end");
        }
        if !self.planned.contains(&stream) {
            bail!("stream_end for unplanned stream {stream}");
        }
        if self.seen_end.contains(&stream) {
            let error = format!("stream {stream} emitted stream_end more than once");
            self.streams.insert(
                stream,
                StreamResult {
                    status: StreamStatus::Error,
                    cursor: None,
                    error: Some(error.clone()),
                },
            );
            bail!("{error}");
        }
        if status == StreamStatus::Ok && cursor.is_none() {
            bail!("successful stream {stream} did not provide a cursor");
        }
        if status == StreamStatus::Error && cursor.is_some() {
            bail!("errored stream {stream} must not provide a cursor");
        }
        self.seen_end.insert(stream.clone());
        self.streams.insert(
            stream,
            StreamResult {
                status,
                cursor,
                error,
            },
        );
        Ok(Accepted::StreamEnd)
    }

    fn mark_unfinished(&mut self, error: &str) {
        if !matches!(self.request, Request::Fetch(_)) {
            return;
        }
        for stream in self.planned.clone() {
            if !self.seen_end.contains(&stream) {
                self.streams.insert(
                    stream,
                    StreamResult {
                        status: StreamStatus::Error,
                        cursor: None,
                        error: Some(error.to_owned()),
                    },
                );
            }
        }
    }
}

fn candidate_streams(request: &Request) -> BTreeSet<String> {
    let Request::Fetch(request) = request else {
        return BTreeSet::new();
    };
    let mut streams = request
        .config
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    streams.extend(
        request
            .discovered
            .iter()
            .map(|stream| stream.stream.clone()),
    );
    streams
}

#[cfg(test)]
mod tests {
    use anyhow::{Context as _, Result};
    use chrono::{TimeZone as _, Utc};

    use super::{
        FetchRequest, Message, Op, Request, Session, SourceRef, StreamStatus, PROTOCOL_VERSION,
    };
    use crate::pointer::Schemes;
    use crate::record::Record;

    fn request() -> Result<Request> {
        let window_end = Utc
            .timestamp_opt(1_700_000_000, 0)
            .single()
            .context("constructing test timestamp")?;
        Ok(Request::Fetch(FetchRequest {
            protocol: PROTOCOL_VERSION,
            source: SourceRef {
                id: "echo".to_owned(),
                kind: "echo".to_owned(),
                scheme: "echo".to_owned(),
            },
            config: serde_json::json!({ "streams": ["C0EXAMPLE01"] }),
            window_end,
            backfill_hours: 24,
            cursors: std::collections::BTreeMap::default(),
            discovered: Vec::new(),
            state_dir: std::env::temp_dir(),
            exclude: Vec::new(),
        }))
    }

    fn hello() -> Message {
        Message::Hello {
            protocol: PROTOCOL_VERSION,
            scheme: "echo".to_owned(),
            ops: vec![Op::Fetch],
            verbatim: true,
            streams: vec!["C0EXAMPLE01".to_owned()],
        }
    }

    fn record() -> Result<Record> {
        let ts = Utc
            .timestamp_opt(1_699_999_999, 0)
            .single()
            .context("constructing record timestamp")?;
        Ok(Record {
            pointer: "echo:C0EXAMPLE01/record".to_owned(),
            stream: "C0EXAMPLE01".to_owned(),
            ts,
            label: None,
            author: None,
            author_id: None,
            thread: None,
            title: None,
            url: None,
            text: "record".to_owned(),
            aliases: Vec::new(),
            context: false,
            raw: serde_json::Value::Null,
        })
    }

    #[test]
    fn errored_stream_end_cannot_propose_a_cursor() -> Result<()> {
        let request = request()?;
        let schemes = Schemes::new(["echo".to_owned()]);
        let mut session = Session::new(&request, &schemes);
        session.accept(hello())?;
        let rejected = session.accept(Message::StreamEnd {
            stream: "C0EXAMPLE01".to_owned(),
            status: StreamStatus::Error,
            cursor: Some("unexpected".to_owned()),
            error: Some("failed".to_owned()),
        });
        let error = rejected.err().context("errored stream accepted a cursor")?;
        let outcome = session.finish(Err(error));

        let stream = outcome
            .streams
            .get("C0EXAMPLE01")
            .context("failed stream is recorded")?;
        assert_eq!(stream.status, StreamStatus::Error);
        assert_eq!(stream.cursor, None);
        Ok(())
    }

    #[test]
    fn records_after_stream_end_fail_the_stream() -> Result<()> {
        let request = request()?;
        let schemes = Schemes::new(["echo".to_owned()]);
        let mut session = Session::new(&request, &schemes);
        session.accept(hello())?;
        session.accept(Message::StreamEnd {
            stream: "C0EXAMPLE01".to_owned(),
            status: StreamStatus::Ok,
            cursor: Some("cursor".to_owned()),
            error: None,
        })?;
        let rejected = session.accept(Message::Record(Box::new(record()?)));
        let error = rejected
            .err()
            .context("record after stream end was accepted")?;
        let outcome = session.finish(Err(error));

        let stream = outcome
            .streams
            .get("C0EXAMPLE01")
            .context("failed stream is recorded")?;
        assert_eq!(stream.status, StreamStatus::Error);
        assert_eq!(stream.cursor, None);
        Ok(())
    }

    /// Every answer quotes a record's pointer whole, so a source may not name one, or a
    /// thread, longer than `MAX_POINTER_BYTES`.
    #[test]
    fn a_pointer_longer_than_the_bound_is_refused() -> Result<()> {
        let request = request()?;
        let schemes = Schemes::new(["echo".to_owned()]);
        let mut session = Session::new(&request, &schemes);
        session.accept(hello())?;
        let mut long = record()?;
        long.pointer = format!("echo:C0EXAMPLE01/{}", "x".repeat(super::MAX_POINTER_BYTES));
        let mut threaded = record()?;
        threaded.thread = Some(long.pointer.clone());
        assert!(session.accept(Message::Record(Box::new(long))).is_err());
        assert!(session.accept(Message::Record(Box::new(threaded))).is_err());
        assert!(session.accept(Message::Record(Box::new(record()?))).is_ok());
        Ok(())
    }
}
