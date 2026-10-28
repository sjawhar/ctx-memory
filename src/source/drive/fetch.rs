//! Drive feed and backfill fetching.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::record::Record;
use crate::source::{
    slice_cursor, slice_end, token_cursor, window_start, FetchRequest, Message, Op, ResolveRequest,
    SliceCursor, TokenCursor, MAX_SLICE_DAYS, PROTOCOL_VERSION,
};

use super::api::DriveApi;
use super::records::{file_timestamp, record_from_file, required_file_string, Bounds, Place};
use super::DriveConfig;

/// Everything the user can see, through the Drive changes feed.
pub(super) const STREAM_CHANGES: &str = "drive";
/// Meet transcripts and notes, which Drive files under the Meet artifacts folder.
pub(super) const STREAM_MEET: &str = "meet";
/// Drive entries that point at content instead of holding it.
const REFERENCE_MIMES: [&str; 2] = [
    "application/vnd.google-apps.folder",
    "application/vnd.google-apps.shortcut",
];
/// The most changed files one run reads from the changes feed. A longer feed is read over
/// several runs, a page at a time: the cursor names the page the next run starts at.
pub(super) const MAX_CHANGED_FILES: usize = 500;

fn fetch_streams(config: &DriveConfig) -> Vec<String> {
    let mut streams = Vec::new();
    if config.changes {
        streams.push(STREAM_CHANGES.to_owned());
    }
    if config.meet {
        streams.push(STREAM_MEET.to_owned());
    }
    streams
}

pub(super) fn emit_fetch_hello(
    request: &FetchRequest,
    config: &DriveConfig,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: true,
        streams: fetch_streams(config),
    })
}

pub(super) fn emit_resolve_hello(
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

/// One stream's records, warnings, and the cursor that follows them.
#[derive(Default)]
struct Collected {
    records: Vec<Record>,
    warnings: Vec<String>,
    cursor: String,
    /// Folders this stream already read, which another stream must not repeat.
    covered_parents: BTreeSet<String>,
}

pub(super) fn fetch_with_api<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    // Meet runs first: the folders it read are the ones the changes stream must not repeat.
    let meet = config.meet.then(|| collect_meet(request, config, api));
    let covered = match meet.as_ref() {
        Some(Ok(collected)) => collected.covered_parents.clone(),
        _ => BTreeSet::new(),
    };
    if config.changes {
        let collected = collect_changes(request, config, api, &covered);
        super::emit_stream(
            STREAM_CHANGES,
            collected.map(|c| (c.warnings, c.records, c.cursor)),
            sink,
        )?;
    }
    if let Some(collected) = meet {
        super::emit_stream(
            STREAM_MEET,
            collected.map(|c| (c.warnings, c.records, c.cursor)),
            sink,
        )?;
    }
    Ok(())
}

/// Follows the changes feed when the cursor is a changes token, or lists by time when it names a
/// time or this is the first fetch.
fn collect_changes<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
    covered: &BTreeSet<String>,
) -> Result<Collected> {
    let Some(cursor) = request.cursors.get(STREAM_CHANGES).map(String::as_str) else {
        let start = window_start(None, request.window_end, request.backfill_hours)?;
        return list_changes(request, config, api, covered, start, None);
    };
    if let Some(slice) = SliceCursor::parse(cursor) {
        return list_changes(
            request,
            config,
            api,
            covered,
            slice.through,
            Some(slice.token),
        );
    }
    let (token, stored_through) = match TokenCursor::parse(cursor) {
        // A bare time, which an earlier build wrote, carries no token yet.
        TokenCursor::Time(time) => {
            return list_changes(request, config, api, covered, time, None);
        }
        TokenCursor::Token {
            token,
            stored_through,
        } => (token, stored_through),
    };
    if let Some(collected) = follow_changes(request, config, api, covered, token, stored_through)? {
        return Ok(collected);
    }
    // Drive refused the token, so the feed it opened is gone. Only the time the cursor carries
    // says how far back a listing must reach; without it a listing of the backfill window would
    // step over everything older than it.
    let Some(stored_through) = stored_through else {
        bail!(
            "Drive refused changes token {token}, and the cursor names no time through which Drive is stored; set the cursor to the RFC 3339 time to list from"
        );
    };
    let mut collected = list_changes(request, config, api, covered, stored_through, None)?;
    collected.warnings.insert(
        0,
        format!(
            "Drive refused changes token {token}; listing from {} again under a new token",
            stored_through.to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
    );
    Ok(collected)
}

/// Follows the changes feed from `token` a page at a time, stopping after the page that brings
/// the files it read to `MAX_CHANGED_FILES`; the cursor names the page the next run starts at.
/// `None` when Drive refuses `token` itself.
///
/// A file the feed names after the window end is left for the next run: the cursor names the
/// page that holds it, so that page is read again then and every page before it is not. A file
/// dated later than any clock has reached is not such a file, and is stored now (see
/// [`Place::Skewed`]). A run that stops inside the feed keeps the time the cursor carried; one
/// that reads to its end stores Drive through the window's end.
fn follow_changes<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
    covered: &BTreeSet<String>,
    token: &str,
    stored_through: Option<DateTime<Utc>>,
) -> Result<Option<Collected>> {
    let resume_at = |page_token: &str| {
        stored_through.map_or_else(
            || page_token.to_owned(),
            |stored| token_cursor(page_token, stored),
        )
    };
    let mut collected = Collected::default();
    let mut page_token = token.to_owned();
    let mut read = 0_usize;
    loop {
        let Some(page) = api.changes(&page_token)? else {
            if page_token == token {
                return Ok(None);
            }
            bail!("Drive refused page token {page_token:?}, which its own changes feed named");
        };
        let bounds = Bounds::read_now(request.window_end);
        let mut held = 0_usize;
        for file in &page.files {
            if is_reference(file) || has_parent_in(file, covered) {
                continue;
            }
            if bounds.place(file_timestamp(file, "modifiedTime")?) == Place::After {
                held += 1;
                continue;
            }
            let record = record_from_file(
                file,
                STREAM_CHANGES,
                &request.source.scheme,
                api,
                config.max_export_bytes,
                bounds,
                &mut collected.warnings,
            )?;
            collected.records.push(record);
            read += 1;
        }
        if held > 0 {
            collected.warnings.push(format!(
                "{held} Drive file(s) changed after the window ended; the cursor is held at their page for the next fetch"
            ));
            collected.cursor = resume_at(&page_token);
            return Ok(Some(collected));
        }
        if !page.more {
            collected.cursor = token_cursor(&page.next_page_token, request.window_end);
            return Ok(Some(collected));
        }
        if page.next_page_token == page_token {
            bail!("the Drive changes feed repeated page token {page_token:?}");
        }
        page_token = page.next_page_token;
        if read >= MAX_CHANGED_FILES {
            collected.warnings.push(format!(
                "more than {MAX_CHANGED_FILES} Drive files changed since the last fetch; the next fetch reads on from where this one stopped"
            ));
            collected.cursor = resume_at(&page_token);
            return Ok(Some(collected));
        }
    }
}

/// Lists the files changed since `start`, a slice at a time when the window end is more than
/// [`MAX_SLICE_DAYS`] away: such a run lists one slice and names its end.
///
/// The changes token the stream follows afterwards is taken before the first slice is listed
/// (`carried` is that token, once a run has), carried in the cursor across the slices, and named
/// by the run whose slice reaches the window end. Its feed then names a file changed during the
/// catch-up whose modified time is behind the slices — uploaded or shared with an old time —
/// which no slice lists.
///
/// The run whose slice reaches the window end lists with no upper bound: a token taken by that
/// run opens a feed that never names a file changed between the window's end and that read.
/// The window must not carry such a file, so the run leaves it out and names the window's end,
/// still carrying the token, as Gmail does; the next run lists from there and names the token
/// once no file follows the window. A file dated later than any clock has reached did not change
/// after the window, and is stored now (see [`Place::Skewed`]).
fn list_changes<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
    covered: &BTreeSet<String>,
    start: DateTime<Utc>,
    carried: Option<&str>,
) -> Result<Collected> {
    let end = slice_end(start, request.window_end);
    let token = match carried {
        Some(carried) => carried.to_owned(),
        None => api.start_page_token()?,
    };
    let mut collected = Collected::default();
    let files = if end < request.window_end {
        collected.warnings.push(format!(
            "Drive is stored through {}, more than {MAX_SLICE_DAYS} days before the window end; this fetch lists it through {} and the next lists on from there",
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            end.to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
        collected.cursor = slice_cursor(None, end, &token);
        api.files(&window_query(start, end))?
    } else {
        collected.cursor = token_cursor(&token, request.window_end);
        api.files(&since_query(start))?
    };
    let bounds = Bounds::read_now(request.window_end);
    let mut after_window = 0_usize;
    for file in &files {
        if is_reference(file) || has_parent_in(file, covered) {
            continue;
        }
        // Only the listing with no upper bound names such a file.
        if bounds.place(file_timestamp(file, "modifiedTime")?) == Place::After {
            after_window += 1;
            continue;
        }
        let record = record_from_file(
            file,
            STREAM_CHANGES,
            &request.source.scheme,
            api,
            config.max_export_bytes,
            bounds,
            &mut collected.warnings,
        )?;
        collected.records.push(record);
    }
    if after_window > 0 {
        collected.warnings.push(format!(
            "{after_window} Drive file(s) changed after the window ended; the cursor names the window end, carrying the changes token, and the next fetch lists from there"
        ));
        collected.cursor = slice_cursor(None, request.window_end, &token);
    }
    Ok(collected)
}

/// Lists the Meet folder for the window: its own files, and each meeting folder it holds. A
/// gap longer than [`MAX_SLICE_DAYS`] is listed a slice at a time, the cursor naming its end.
///
/// Meet files one folder per meeting, so the notes and the transcript of a meeting are one
/// level below the folder the configuration names.
fn collect_meet<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
) -> Result<Collected> {
    let start = window_start(
        request.cursors.get(STREAM_MEET).map(String::as_str),
        request.window_end,
        request.backfill_hours,
    )?;
    let end = slice_end(start, request.window_end);
    let mut collected = Collected {
        cursor: end.to_rfc3339_opts(SecondsFormat::Secs, true),
        ..Collected::default()
    };
    let folders = api.files(&format!(
        "name = '{}' and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
        quoted(&config.meet_folder)
    ))?;
    if folders.is_empty() {
        // Nothing of this window was read, so the cursor must not move past it: a folder that
        // is missing because Drive answered thinly would otherwise cost the whole window.
        collected.cursor = request
            .cursors
            .get(STREAM_MEET)
            .cloned()
            .unwrap_or_else(|| start.to_rfc3339_opts(SecondsFormat::Secs, true));
        collected.warnings.push(format!(
            "no Drive folder named {:?}; no Meet transcripts or notes were fetched, and the cursor is held for the next fetch",
            config.meet_folder
        ));
        return Ok(collected);
    }
    for folder in &folders {
        let id = required_file_string(folder, "id")?;
        collected.covered_parents.insert(id.to_owned());
        let entries = api.files(&format!(
            "'{}' in parents and {}",
            quoted(id),
            window_query(start, end)
        ))?;
        let bounds = Bounds::read_now(request.window_end);
        for entry in &entries {
            if is_folder(entry) {
                collect_meeting_folder(entry, api, config, request, &mut collected)?;
            } else if !is_reference(entry) {
                let record = record_from_file(
                    entry,
                    STREAM_MEET,
                    &request.source.scheme,
                    api,
                    config.max_export_bytes,
                    bounds,
                    &mut collected.warnings,
                )?;
                collected.records.push(record);
            }
        }
    }
    if end < request.window_end {
        collected.warnings.push(format!(
            "the Meet folder is stored through {}, more than {MAX_SLICE_DAYS} days before the window end; this fetch lists it through {} and the next lists on from there",
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            collected.cursor
        ));
    }
    Ok(collected)
}

/// Reads every artifact of one meeting's folder, whatever its own timestamp is.
fn collect_meeting_folder<C: DriveApi + ?Sized>(
    folder: &Value,
    api: &C,
    config: &DriveConfig,
    request: &FetchRequest,
    collected: &mut Collected,
) -> Result<()> {
    let id = required_file_string(folder, "id")?;
    collected.covered_parents.insert(id.to_owned());
    let files = api.files(&format!("'{}' in parents and trashed = false", quoted(id)))?;
    let bounds = Bounds::read_now(request.window_end);
    for file in &files {
        if is_reference(file) {
            continue;
        }
        let record = record_from_file(
            file,
            STREAM_MEET,
            &request.source.scheme,
            api,
            config.max_export_bytes,
            bounds,
            &mut collected.warnings,
        )?;
        collected.records.push(record);
    }
    Ok(())
}

/// A folder or a shortcut points at content rather than holding it.
fn is_reference(file: &Value) -> bool {
    file.get("mimeType")
        .and_then(Value::as_str)
        .is_some_and(|mime| REFERENCE_MIMES.contains(&mime))
}

fn is_folder(file: &Value) -> bool {
    file.get("mimeType").and_then(Value::as_str) == Some("application/vnd.google-apps.folder")
}

/// Whether another stream already read the folder this file sits in.
fn has_parent_in(file: &Value, covered: &BTreeSet<String>) -> bool {
    file.get("parents")
        .and_then(Value::as_array)
        .is_some_and(|parents| {
            parents
                .iter()
                .filter_map(Value::as_str)
                .any(|parent| covered.contains(parent))
        })
}

/// Quotes one value inside a Drive query string.
fn quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn window_query(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "modifiedTime > '{}' and modifiedTime <= '{}' and trashed = false",
        start.to_rfc3339_opts(SecondsFormat::Secs, true),
        end.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// Everything changed since `start`, with no upper bound.
fn since_query(start: DateTime<Utc>) -> String {
    format!(
        "modifiedTime > '{}' and trashed = false",
        start.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}
