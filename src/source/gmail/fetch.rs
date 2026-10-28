//! Gmail history and listing progression, and the Gmail API adapter.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;

use crate::api::client;
use crate::api::google::{self, Google};
use crate::source::{
    slice_cursor, slice_end, token_cursor, window_start, FetchRequest, Message, Op, ResolveRequest,
    SliceCursor, StreamStatus, TokenCursor, MAX_SLICE_DAYS, PROTOCOL_VERSION,
};

use super::render::{message_timestamp, record_from_message_with_scheme, required_string};
use super::{GmailConfig, STREAM};

/// Labels never fetched, because a listing never returns them either.
const ALWAYS_EXCLUDED: [&str; 2] = ["SPAM", "TRASH"];
/// A listing asks for this many messages per page.
const LIST_PAGE_SIZE: &str = "500";
/// The most messages one run fetches by history. A gap Gmail still holds the history of is
/// walked this many messages at a time, so one run's requests stay bounded.
pub(super) const MAX_HISTORY_MESSAGES: usize = 2000;

/// One mailbox change and the messages it added.
#[derive(Clone, Debug)]
pub(super) struct HistoryEntry {
    pub(super) id: String,
    pub(super) added: Vec<String>,
}

/// What `users.history.list` returned for one cursor.
#[derive(Clone, Debug)]
pub(super) struct History {
    pub(super) entries: Vec<HistoryEntry>,
    pub(super) latest: Option<String>,
}

pub(super) trait GmailApi {
    /// Message stubs matching a Gmail search query.
    fn list_messages(&self, query: &str) -> Result<Vec<Value>>;
    /// One message in `full` format, or `None` when Gmail answers that the mailbox no longer
    /// holds it.
    fn message(&self, id: &str) -> Result<Option<Value>>;
    /// Every message of one thread, in `full` format.
    fn thread(&self, id: &str) -> Result<Vec<Value>>;
    /// Mailbox changes after `start`, or `None` when Gmail answers that it no longer holds that
    /// history ID.
    fn history(&self, start: &str) -> Result<Option<History>>;
    /// Whether Gmail still holds the history after `start`, asked with one short request.
    fn holds_history(&self, start: &str) -> Result<bool>;
    /// The mailbox's current history ID.
    fn latest_history_id(&self) -> Result<String>;
}

pub(super) struct GoogleGmailApi {
    google: Google,
}

impl GoogleGmailApi {
    pub(super) fn load() -> Result<Self> {
        Ok(Self {
            google: Google::load()?,
        })
    }
}

impl GmailApi for GoogleGmailApi {
    fn list_messages(&self, query: &str) -> Result<Vec<Value>> {
        let pages = self.google.paginate(
            "gmail/v1/users/me/messages",
            &[
                ("q", query.to_owned()),
                ("maxResults", LIST_PAGE_SIZE.to_owned()),
            ],
            "messages",
        )?;
        Ok(pages.items)
    }

    fn message(&self, id: &str) -> Result<Option<Value>> {
        let (status, body) = self.google.reply(
            &format!("gmail/v1/users/me/messages/{}", client::percent_encode(id)),
            &[("format", "full".to_owned())],
        )?;
        // Only Gmail's own `notFound` says the message is gone. A 404 from anything else says
        // nothing about it, and skipping it would move the cursor past mail still there.
        if google::not_found(status, &body) {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            bail!("Gmail message {id} returned {status}: {body}");
        }
        Ok(Some(body))
    }

    fn thread(&self, id: &str) -> Result<Vec<Value>> {
        let response = self.google.get(
            &format!("gmail/v1/users/me/threads/{}", client::percent_encode(id)),
            &[("format", "full".to_owned())],
        )?;
        Ok(response
            .get("messages")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, Clone::clone))
    }

    fn history(&self, start: &str) -> Result<Option<History>> {
        let mut entries = Vec::new();
        let mut latest = None;
        let mut page_token: Option<String> = None;
        let mut seen_tokens = Vec::new();
        loop {
            let mut query = vec![
                ("startHistoryId", start.to_owned()),
                ("historyTypes", "messageAdded".to_owned()),
                ("maxResults", LIST_PAGE_SIZE.to_owned()),
            ];
            if let Some(token) = &page_token {
                query.push(("pageToken", token.clone()));
            }
            let (status, body) = self.google.reply("gmail/v1/users/me/history", &query)?;
            if google::not_found(status, &body) {
                return Ok(None);
            }
            if !(200..300).contains(&status) {
                bail!("Gmail history returned {status}: {body}");
            }
            entries.extend(history_entries(&body));
            latest = body
                .get("historyId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(latest);
            page_token = body
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned);
            match &page_token {
                Some(token) if seen_tokens.contains(token) => {
                    bail!("Gmail history repeated page token {token:?}");
                }
                Some(token) => seen_tokens.push(token.clone()),
                None => break,
            }
        }
        Ok(Some(History { entries, latest }))
    }

    fn holds_history(&self, start: &str) -> Result<bool> {
        let (status, body) = self.google.reply(
            "gmail/v1/users/me/history",
            &[
                ("startHistoryId", start.to_owned()),
                ("maxResults", "1".to_owned()),
            ],
        )?;
        if google::not_found(status, &body) {
            return Ok(false);
        }
        if !(200..300).contains(&status) {
            bail!("Gmail history returned {status}: {body}");
        }
        Ok(true)
    }

    fn latest_history_id(&self) -> Result<String> {
        let profile = self.google.get("gmail/v1/users/me/profile", &[])?;
        profile
            .get("historyId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("the Gmail profile has no historyId"))
    }
}

fn history_entries(body: &Value) -> Vec<HistoryEntry> {
    body.get("history")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |records| {
            records
                .iter()
                .filter_map(|record| {
                    let id = record.get("id").and_then(Value::as_str)?;
                    let added = record
                        .get("messagesAdded")
                        .and_then(Value::as_array)
                        .map_or_else(Vec::new, |added| {
                            added
                                .iter()
                                .filter_map(|entry| {
                                    entry
                                        .pointer("/message/id")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                })
                                .collect()
                        });
                    Some(HistoryEntry {
                        id: id.to_owned(),
                        added,
                    })
                })
                .collect()
        })
}

pub(super) fn emit_fetch_hello(
    request: &FetchRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: vec![STREAM.to_owned()],
    })
}

pub(super) fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: Vec::new(),
    })
}

/// The messages of one fetch and the cursor that follows them.
struct Collected {
    messages: Vec<Value>,
    cursor: String,
    /// Messages dated later than any clock has reached, which this window stores at its end.
    skewed: BTreeSet<String>,
}

pub(super) fn fetch_with_api<C: GmailApi + ?Sized>(
    request: &FetchRequest,
    config: &GmailConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut warnings = Vec::new();
    let result = collect(api, config, request, &mut warnings).and_then(|collected| {
        let records = collected
            .messages
            .iter()
            .map(|message| {
                let mut record = record_from_message_with_scheme(message, &request.source.scheme)?;
                if collected
                    .skewed
                    .contains(required_string(message, "id", "Gmail message")?)
                {
                    record.ts = request.window_end;
                }
                Ok(record)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((records, collected.cursor))
    });
    for warning in warnings {
        sink(Message::Warning {
            stream: Some(STREAM.to_owned()),
            message: warning,
        })?;
    }
    match result {
        Ok((records, cursor)) => {
            for record in records {
                sink(Message::Record(Box::new(record)))?;
            }
            sink(Message::StreamEnd {
                stream: STREAM.to_owned(),
                status: StreamStatus::Ok,
                cursor: Some(cursor),
                error: None,
            })
        }
        Err(error) => sink(Message::StreamEnd {
            stream: STREAM.to_owned(),
            status: StreamStatus::Error,
            cursor: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

/// Collects the messages of one fetch: by history when the cursor names a history ID, else by
/// a listing of what the cursor's time, or the backfill window, reaches.
fn collect<C: GmailApi + ?Sized>(
    api: &C,
    config: &GmailConfig,
    request: &FetchRequest,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let excluded = excluded_labels(&config.query);
    let cursor = request.cursors.get(STREAM).map(String::as_str);
    if let Some(slice) = cursor.and_then(SliceCursor::parse) {
        let listing = Listing::Resume(slice);
        return collect_from_listing(api, config, listing, request, &excluded, warnings);
    }
    let start = match cursor.map(TokenCursor::parse) {
        Some(TokenCursor::Token {
            token,
            stored_through,
        }) => {
            if let Some(history) = api.history(token)? {
                return collect_from_history(
                    api,
                    &history,
                    HistoryStart {
                        id: token,
                        stored_through,
                    },
                    &excluded,
                    request.window_end,
                    warnings,
                );
            }
            // Gmail no longer holds that history, so the mailbox is listed again. Without a
            // time to list from, nothing says how far back the gap reaches, and a listing of
            // the backfill window would step over everything older than it.
            let Some(stored_through) = stored_through else {
                bail!(
                    "Gmail history {token} is no longer available, and the cursor names no time through which this mailbox is stored; set the cursor to the RFC 3339 time to list from"
                );
            };
            warnings.push(format!(
                "Gmail history {token} is no longer available; listing from {} instead",
                stored_through.to_rfc3339_opts(SecondsFormat::Secs, true)
            ));
            stored_through
        }
        // A bare time, which an earlier build wrote, carries no history ID yet.
        Some(TokenCursor::Time(start)) => start,
        None => window_start(None, request.window_end, request.backfill_hours)?,
    };
    collect_from_listing(
        api,
        config,
        Listing::Begin(start),
        request,
        &excluded,
        warnings,
    )
}

/// Where a listing starts.
#[derive(Clone, Copy)]
enum Listing<'a> {
    /// A listing that begins at this time, and reads the history ID that follows it first.
    Begin(DateTime<Utc>),
    /// A catch-up a slice in, carrying the history ID read before its first slice and the time
    /// it began.
    Resume(SliceCursor<'a>),
}

/// Where a history walk starts: the cursor's history ID, and what it says is stored.
#[derive(Clone, Copy)]
struct HistoryStart<'a> {
    id: &'a str,
    stored_through: Option<DateTime<Utc>>,
}

/// Walks mailbox changes in order, holding the cursor at the last change it finished.
///
/// A message newer than the window end stops the walk: the window must not carry it, and the
/// held cursor brings it back next run. A message dated later than any clock has reached does
/// not: its date is wrong, and the window stores it at its end (see [`skewed_warning`]). So
/// does reaching `MAX_HISTORY_MESSAGES`, so a long gap is walked over several runs. A message
/// the history names and Gmail no longer holds is skipped with a warning, because waiting for it
/// would stop the stream for good.
fn collect_from_history<C: GmailApi + ?Sized>(
    api: &C,
    history: &History,
    start: HistoryStart<'_>,
    excluded: &BTreeSet<String>,
    window_end: DateTime<Utc>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    // The caller read the history just now; every message it names had arrived by then.
    let believable = google::latest_believable(window_end);
    let mut messages = Vec::new();
    let mut skewed = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut gone = 0_usize;
    let mut fetched = 0_usize;
    let mut last_complete: Option<&str> = None;
    for entry in &history.entries {
        if fetched >= MAX_HISTORY_MESSAGES {
            warnings.push(format!(
                "the Gmail history names more than {MAX_HISTORY_MESSAGES} messages since the last fetch; the next fetch reads on from where this one stopped"
            ));
            warn_gone(warnings, gone);
            return Ok(Collected {
                messages,
                cursor: start.resume_at(last_complete),
                skewed,
            });
        }
        let mut batch = Vec::new();
        let mut batch_skewed = Vec::new();
        let mut hold = false;
        for id in &entry.added {
            if !seen.insert(id.clone()) {
                continue;
            }
            fetched += 1;
            let Some(message) = api.message(id)? else {
                gone += 1;
                continue;
            };
            if is_excluded(&message, excluded) {
                continue;
            }
            let arrived = message_timestamp(&message)?;
            if arrived > believable {
                batch_skewed.push((id.clone(), arrived));
            } else if arrived > window_end {
                hold = true;
                break;
            }
            batch.push(message);
        }
        if hold {
            warn_gone(warnings, gone);
            return Ok(Collected {
                messages,
                cursor: start.resume_at(last_complete),
                skewed,
            });
        }
        messages.extend(batch);
        for (id, arrived) in batch_skewed {
            warnings.push(skewed_warning(&id, arrived, window_end));
            skewed.insert(id);
        }
        last_complete = Some(&entry.id);
    }
    // The walk read every change Gmail named, and none of them followed the window, so the
    // mailbox is stored through the window's end.
    warn_gone(warnings, gone);
    Ok(Collected {
        messages,
        cursor: token_cursor(history.latest.as_deref().unwrap_or(start.id), window_end),
        skewed,
    })
}

/// The warning for a message dated later than [`google::latest_believable`]. No clock has
/// reached that date, so the message did not arrive after the window: its date is wrong.
/// Holding the cursor for it would hold every later message until the clock passed the date, so
/// the window stores it at its end, and the record's raw data keeps the date Gmail gave.
fn skewed_warning(id: &str, arrived: DateTime<Utc>, window_end: DateTime<Utc>) -> String {
    format!(
        "Gmail message {id} is dated {}, more than {} minutes after both the window end and the time this fetch read it; the date is taken for a wrong clock, so this window stores the message at its end ({}) and its raw data keeps the date Gmail gave",
        arrived.to_rfc3339_opts(SecondsFormat::Secs, true),
        google::CLOCK_SKEW_MARGIN.num_minutes(),
        window_end.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

impl HistoryStart<'_> {
    /// The cursor of a walk that stopped inside the window, after the change `last_complete`
    /// when it finished any. The mailbox is stored no further than the cursor already said, so
    /// the time it carries stands unchanged.
    fn resume_at(&self, last_complete: Option<&str>) -> String {
        let resume = last_complete.unwrap_or(self.id);
        self.stored_through
            .map_or_else(|| resume.to_owned(), |stored| token_cursor(resume, stored))
    }
}

fn warn_gone(warnings: &mut Vec<String>, gone: usize) {
    if gone > 0 {
        warnings.push(format!(
            "{gone} message(s) named by the Gmail history are gone from the mailbox; they were skipped"
        ));
    }
}

/// Lists what the listing reaches from its start. A gap longer than [`MAX_SLICE_DAYS`] is
/// listed a slice at a time: the run lists the oldest slice and names its end.
///
/// The history ID the stream follows afterwards is read before the first slice is listed and
/// carried in the cursor across the slices, so the run whose slice reaches the window end names
/// an ID that covers mail imported during the catch-up with a date behind the slices, which no
/// slice lists. Gmail may drop a history ID within hours, so each later slice first asks
/// whether Gmail still holds it. When Gmail does not, the mail imported since cannot be named,
/// and the catch-up lists again from where it began under a new ID. A message it lists again is
/// stored once: the store keeps one copy of each version of a pointer.
fn collect_from_listing<C: GmailApi + ?Sized>(
    api: &C,
    config: &GmailConfig,
    listing: Listing<'_>,
    request: &FetchRequest,
    excluded: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let (began, start, next) = match listing {
        Listing::Begin(start) => (start, start, api.latest_history_id()?),
        Listing::Resume(slice) => {
            let began = slice.began.unwrap_or(slice.through);
            if api.holds_history(slice.token)? {
                (began, slice.through, slice.token.to_owned())
            } else {
                let began_at = began.to_rfc3339_opts(SecondsFormat::Secs, true);
                warnings.push(format!(
                    "Gmail history {}, read when this listing began at {began_at}, is no longer available; listing again from {began_at} under a new history ID",
                    slice.token
                ));
                (began, began, api.latest_history_id()?)
            }
        }
    };
    let end = slice_end(start, request.window_end);
    if end < request.window_end {
        let listed = fetch_messages(
            api,
            start,
            Reach::Slice(end),
            &config.query,
            excluded,
            warnings,
        )?;
        warnings.push(format!(
            "the mailbox is stored through {}, more than {MAX_SLICE_DAYS} days before the window end; this fetch lists it through {} and the next lists on from there",
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            end.to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
        return Ok(Collected {
            messages: listed.messages,
            cursor: slice_cursor(Some(began), end, &next),
            skewed: listed.skewed,
        });
    }
    let listed = fetch_messages(
        api,
        start,
        Reach::WindowEnd(request.window_end),
        &config.query,
        excluded,
        warnings,
    )?;
    // The history ID may have been read after mail that arrived after the window ended, which
    // this window must not carry. Naming it would leave that mail under the cursor and unread
    // for good, so the stream names the window's end, still carrying the ID, and the next run
    // lists from there, naming the ID once no mail follows the window.
    let cursor = if listed.after_window {
        slice_cursor(Some(began), request.window_end, &next)
    } else {
        token_cursor(&next, request.window_end)
    };
    Ok(Collected {
        messages: listed.messages,
        cursor,
        skewed: listed.skewed,
    })
}

/// Labels a fetch never stores, from the configured query's `-category:` terms.
pub(super) fn excluded_labels(query: &str) -> BTreeSet<String> {
    let mut labels = ALWAYS_EXCLUDED
        .iter()
        .map(|label| (*label).to_owned())
        .collect::<BTreeSet<_>>();
    for term in query.split_whitespace() {
        if let Some(category) = term.strip_prefix("-category:") {
            if !category.is_empty() {
                labels.insert(format!("CATEGORY_{}", category.to_uppercase()));
            }
        }
    }
    labels
}

fn is_excluded(message: &Value, excluded: &BTreeSet<String>) -> bool {
    message
        .get("labelIds")
        .and_then(Value::as_array)
        .is_some_and(|labels| {
            labels
                .iter()
                .filter_map(Value::as_str)
                .any(|label| excluded.contains(label))
        })
}

/// What one listing found: the messages the window carries, and whether mail followed it.
pub(super) struct Listed {
    messages: Vec<Value>,
    /// Whether the listing saw a message that arrived after the window ended.
    after_window: bool,
    /// Messages dated later than any clock has reached, which this window stores at its end.
    skewed: BTreeSet<String>,
}

/// How far one listing reaches.
#[derive(Clone, Copy)]
pub(super) enum Reach {
    /// A slice that ends at this time, before the window end; what follows is the next slice's.
    Slice(DateTime<Utc>),
    /// The window end, at this time. The listing asks Gmail with no upper bound, so it names mail
    /// dated any time after its start: mail that arrived after the window ended, which the next
    /// window stores, and mail dated later than any clock has reached, which this one does.
    WindowEnd(DateTime<Utc>),
}

pub(super) fn fetch_messages<C: GmailApi + ?Sized>(
    api: &C,
    start: DateTime<Utc>,
    reach: Reach,
    extra_query: &str,
    excluded: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Result<Listed> {
    let day_floor = (start - Duration::days(1)).format("%Y/%m/%d");
    let mut query = match reach {
        Reach::Slice(end) => format!(
            "after:{day_floor} before:{}",
            (end + Duration::days(1)).format("%Y/%m/%d")
        ),
        Reach::WindowEnd(_) => format!("after:{day_floor}"),
    };
    if !extra_query.trim().is_empty() {
        query.push(' ');
        query.push_str(extra_query.trim());
    }
    let candidates = api.list_messages(&query)?;
    let (end, believable) = match reach {
        Reach::Slice(end) => (end, None),
        Reach::WindowEnd(end) => (end, Some(google::latest_believable(end))),
    };
    let mut listed = Listed {
        messages: Vec::with_capacity(candidates.len()),
        after_window: false,
        skewed: BTreeSet::new(),
    };
    let mut gone = 0_usize;
    for candidate in candidates {
        let id = candidate
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Gmail candidate is missing id"))?;
        let Some(message) = api.message(id)? else {
            gone += 1;
            continue;
        };
        if is_excluded(&message, excluded) {
            continue;
        }
        let arrived = message_timestamp(&message)?;
        if believable.is_some_and(|believable| arrived > believable) {
            warnings.push(skewed_warning(id, arrived, end));
            listed.skewed.insert(id.to_owned());
        } else if arrived > end {
            listed.after_window = true;
            continue;
        }
        if arrived > start {
            listed.messages.push(message);
        }
    }
    if gone > 0 {
        warnings.push(format!(
            "{gone} message(s) the Gmail listing named are gone from the mailbox; they were skipped"
        ));
    }
    Ok(listed)
}

pub(super) fn resolve_with_api<C: GmailApi + ?Sized>(
    request: &ResolveRequest,
    _config: &GmailConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut records = Vec::new();
    for pointer in &request.pointers {
        let Some(locator) = pointer
            .strip_prefix(&request.source.scheme)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let messages = if let Some(id) = locator.strip_prefix("thread/") {
            if id.is_empty() {
                continue;
            }
            api.thread(id)?
        } else {
            if locator.is_empty() {
                continue;
            }
            api.message(locator)?.into_iter().collect()
        };
        for message in messages {
            records.push(record_from_message_with_scheme(
                &message,
                &request.source.scheme,
            )?);
        }
    }
    records.sort_by(|left, right| {
        left.ts
            .cmp(&right.ts)
            .then_with(|| left.pointer.cmp(&right.pointer))
    });
    for record in records {
        sink(Message::Record(Box::new(record)))?;
    }
    Ok(())
}
