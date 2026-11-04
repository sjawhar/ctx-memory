//! The write endpoints: the server is the only writer of records.
//!
//! One writer lock covers the whole sequence, so two callers cannot interleave. Every write
//! is pulled onto the newest `main`, checked the way `ctx sweep` and `ctx cite verify` check
//! it, committed under the caller's own name, and pushed. A push that loses a race with a
//! merge is rebased once and checked again before it is pushed a second time. A batch is one
//! write: its records are checked together and land in one commit, or none of them does.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::de::DeserializeOwned;

use super::command;
use super::identity::Caller;
use super::request_log::Note;
use super::{Listener, Shared};
use crate::context::Context;
use crate::marker;
use crate::protocol::{
    Committed, DeleteRequest, Finding, MarkerRequest, MarkerResponse, PageRequest, PageResponse,
    PutBatchRequest, PutRequest, RecordFile, Refused, DELETE_ROUTE, MARKER_ROUTE, PAGED_SINCE,
    PAGE_ROUTE, PUT_BATCH_ROUTE, PUT_ROUTE, VERSION_HEADER,
};
use crate::redact::credentials_in;

use self::response::{
    bad_request, deferred, failed, log_failure, not_written, remote_failed, too_old, unauthorized,
    unheld,
};
use self::transaction::{apply, ApplyError};

mod response;
mod transaction;
mod validation;

/// What one write did.
enum Applied {
    Committed(String),
    Refused(Vec<Finding>),
    Conflict(String),
    /// This copy may not write: another copy holds the write lease.
    Unheld(Unheld),
}

/// What one marker move did: the marker moved or a page was recorded, and this is the answer.
enum Moved {
    Done(Response),
    /// This copy may not write: another copy holds the write lease.
    Unheld(Unheld),
}

/// Why a write found it may not take the write lease, before it changed anything.
struct Unheld {
    why: String,
    /// The seconds a caller waits before it asks again, when this is a rollout's handover
    /// ([`Shared::deferral`]); `None` when it is a write that failed.
    pause: Option<u64>,
}

impl Unheld {
    fn of(shared: &Shared, error: &anyhow::Error) -> Self {
        Self {
            why: format!("{error:#}"),
            pause: shared.deferral(error),
        }
    }
}

/// The answer to a write that may not take the write lease: deferred during a rollout's
/// handover, failed otherwise.
fn refuse(shared: &Shared, route: &str, refusal: &Unheld) -> Response {
    match refusal.pause {
        Some(pause) => deferred(shared, route, &refusal.why, pause),
        None => unheld(shared, route, &refusal.why),
    }
}

/// What the caller asked to change.
enum Change {
    /// Write each record whole, in one commit: one record, or a batch whose links are checked
    /// against the tree with every record of the batch in it.
    Put(Vec<RecordFile>),
    Delete {
        path: String,
    },
}

impl Change {
    /// Each file the change names, as the caller wrote its path, with the whole text a put
    /// writes there, or `None` for a delete.
    fn files(&self) -> Vec<(&str, Option<&str>)> {
        match self {
            Self::Put(records) => records
                .iter()
                .map(|record| (record.path.as_str(), Some(record.content.as_str())))
                .collect(),
            Self::Delete { path } => vec![(path.as_str(), None)],
        }
    }

    const fn verb(&self) -> &'static str {
        match self {
            Self::Put(_) => "put",
            Self::Delete { .. } => "delete",
        }
    }
}

/// One file a write changes: its record path, as [`record_path`](crate::tree::record_path)
/// made it, and the whole text a put writes there, or `None` for a delete.
struct Edit<'change> {
    path: String,
    content: Option<&'change str>,
}

/// Answers `POST /v1/records/put`.
pub async fn put(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (caller, request) =
        match admit::<PutRequest>(&listener, PUT_ROUTE, &headers, &body, &note).await {
            Ok(admitted) => admitted,
            Err(refused) => return *refused,
        };
    note.path(&request.path);
    let change = Change::Put(vec![RecordFile {
        path: request.path,
        content: request.content,
    }]);
    write(listener, caller, PUT_ROUTE, change, request.session, &note).await
}

/// Answers `POST /v1/records/put-batch`: several records in one commit, or none of them.
pub async fn put_batch(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (caller, request) =
        match admit::<PutBatchRequest>(&listener, PUT_BATCH_ROUTE, &headers, &body, &note).await {
            Ok(admitted) => admitted,
            Err(refused) => return *refused,
        };
    note.paths(request.records.iter().map(|record| record.path.as_str()));
    let change = Change::Put(request.records);
    write(
        listener,
        caller,
        PUT_BATCH_ROUTE,
        change,
        request.session,
        &note,
    )
    .await
}

/// Answers `POST /v1/records/delete`.
pub async fn delete(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (caller, request) =
        match admit::<DeleteRequest>(&listener, DELETE_ROUTE, &headers, &body, &note).await {
            Ok(admitted) => admitted,
            Err(refused) => return *refused,
        };
    note.path(&request.path);
    let change = Change::Delete { path: request.path };
    write(
        listener,
        caller,
        DELETE_ROUTE,
        change,
        request.session,
        &note,
    )
    .await
}

/// Answers `POST /v1/marker/advance`.
///
/// The marker is on the share every copy reads, and only the holder of the write lease moves
/// it. The lease is checked once the tree is this request's: a write can hold the tree for as
/// long as git takes, and a check made before that wait could be stale by the time the marker
/// moves. A window the caller names wrongly is answered 400; a marker the server cannot read
/// or write is its own trouble, answered 500.
///
/// A client older than paging ([`PAGED_SINCE`]) means by a move past a window to pass all of
/// it. Such a move past a window with pages left is answered `409` rather than refused as the
/// caller's mistake ([`too_old`]); every other move means what it did, and answers it as before.
pub async fn marker_advance(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match admit_speaking::<MarkerRequest>(&listener, MARKER_ROUTE, &headers, &body, &note, 0).await
    {
        Ok((caller, client, MarkerRequest { session, window })) => {
            let shared = Arc::clone(&listener.shared);
            move_marker(
                &listener.shared,
                caller,
                MARKER_ROUTE,
                session,
                move |context, by| match marker::advance(context, &window, by) {
                    Ok(marker) => {
                        Ok((StatusCode::OK, Json(MarkerResponse { marker })).into_response())
                    }
                    Err(error) if client < PAGED_SINCE && passes_pages_left(&error) => Ok(too_old(
                        &shared,
                        MARKER_ROUTE,
                        client,
                        "a move of the marker past a window with pages left",
                        PAGED_SINCE,
                    )),
                    Err(error) => Err(error),
                },
            )
            .await
        }
        Err(refused) => *refused,
    }
}

/// Whether `error` refused a move of the marker past a window with pages left.
fn passes_pages_left(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<marker::Refused>()
        .is_some_and(marker::Refused::pages_left)
}

/// Answers `POST /v1/marker/page`: records a page of the window after the marker, beside the
/// marker and under the same lease, as [`marker_advance`] moves the marker. A window or a page
/// the caller names wrongly is answered 400, and a client older than paging ([`PAGED_SINCE`])
/// `409`.
pub async fn marker_page(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match admit_speaking::<PageRequest>(&listener, PAGE_ROUTE, &headers, &body, &note, PAGED_SINCE)
        .await
    {
        Ok((
            caller,
            _,
            PageRequest {
                ends_at,
                session,
                window,
            },
        )) => {
            move_marker(
                &listener.shared,
                caller,
                PAGE_ROUTE,
                session,
                move |context, by| {
                    let page = marker::page(context, &window, &ends_at, by)?;
                    Ok((StatusCode::OK, Json(PageResponse { page })).into_response())
                },
            )
            .await
        }
        Err(refused) => *refused,
    }
}

/// How each write route starts: the caller is identified and the body read, or the request is
/// answered here. The caller goes in `note` for the request log.
async fn admit<Body: DeserializeOwned>(
    listener: &Listener,
    route: &str,
    headers: &HeaderMap,
    body: &[u8],
    note: &Note,
) -> Result<(Caller, Body), Box<Response>> {
    let caller = identify(listener, route, headers, note).await?;
    Ok((caller, read_body(&listener.shared, route, body)?))
}

/// [`admit`] for a marker route, whose meaning depends on the protocol version the client
/// speaks: once the caller is identified, the version it names is read ([`client_protocol`]),
/// and a client older than `since` is answered `409` before its body is read, since that body
/// may mean something else to it. Answers the caller, its version and the body.
async fn admit_speaking<Body: DeserializeOwned>(
    listener: &Listener,
    route: &str,
    headers: &HeaderMap,
    body: &[u8],
    note: &Note,
    since: u32,
) -> Result<(Caller, u32, Body), Box<Response>> {
    let caller = identify(listener, route, headers, note).await?;
    let shared = &listener.shared;
    let client =
        client_protocol(headers).map_err(|why| Box::new(bad_request(shared, route, &why)))?;
    if client < since {
        return Err(Box::new(too_old(shared, route, client, route, since)));
    }
    Ok((caller, client, read_body(shared, route, body)?))
}

/// The caller of one write, or its `401`. The caller goes in `note` for the request log.
async fn identify(
    listener: &Listener,
    route: &str,
    headers: &HeaderMap,
    note: &Note,
) -> Result<Caller, Box<Response>> {
    let caller = listener
        .caller(headers)
        .await
        .map_err(|error| Box::new(unauthorized(&listener.shared, route, &error)))?;
    note.caller(&caller);
    Ok(caller)
}

/// The body of one write, or its `400`.
fn read_body<Body: DeserializeOwned>(
    shared: &Shared,
    route: &str,
    body: &[u8],
) -> Result<Body, Box<Response>> {
    serde_json::from_slice(body).map_err(|error| {
        Box::new(bad_request(
            shared,
            route,
            &format!("invalid request: {error}"),
        ))
    })
}

/// The protocol version the client of one request speaks, as it names it in
/// [`VERSION_HEADER`]: 0 when it names none, as every client from before that header does.
/// Such a client still writes records and moves the marker past a window of one page, which mean
/// what they did; only what changed since its version is refused it.
///
/// # Errors
///
/// Returns why the request names no one version, which the client is answered with.
fn client_protocol(headers: &HeaderMap) -> std::result::Result<u32, String> {
    let mut named = headers.get_all(VERSION_HEADER).iter();
    let Some(value) = named.next() else {
        return Ok(0);
    };
    if named.next().is_some() {
        return Err(format!(
            "the request names more than one {VERSION_HEADER} header"
        ));
    }
    value
        .to_str()
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| format!("{VERSION_HEADER} is not a protocol version, a whole number"))
}

/// Moves the marker, or records a page of the window after it, for a caller already
/// identified: `change` does it on the tree, under the write lease, as `by`.
async fn move_marker(
    shared: &Arc<Shared>,
    caller: Caller,
    route: &'static str,
    session: Option<String>,
    change: impl FnOnce(&Context, &str) -> Result<Response> + Send + 'static,
) -> Response {
    // A local move names the session that made it; through the server the marker and the page
    // name the signed-in caller and the session it says it is. Both are on the share every copy
    // and the fetch job read, so a session holding a credential is refused as a write's is,
    // without repeating it.
    let session = session_label(session.as_deref());
    match session.as_deref().map(credentials_in).transpose() {
        Ok(Some(kinds)) if !kinds.is_empty() => {
            return bad_request(
                shared,
                route,
                &format!(
                    "the session holds what has the shape of {}; a credential never goes into \
                     the marker: take it out, and revoke it if it is real",
                    kinds.join(" and ")
                ),
            );
        }
        Ok(_) => {}
        Err(error) => return failed(shared, route, &format!("{error:#}")),
    }
    let by = match session {
        Some(session) => format!("{} ({session})", caller.email),
        None => caller.email,
    };
    // A write checks its records, which reads the index this copy builds as it comes up, so
    // it waits for that build here rather than under the tree ([`Shared::index_ready`]).
    shared.index_ready().await;
    // The answer, and the `write failed:` line of a marker that did not move, are made in the
    // blocking task: hyper drops a request whose caller hung up, not the task, and the line
    // must still come.
    let moving = Arc::clone(shared);
    let answered = tokio::task::spawn_blocking(move || {
        let moved = held_change(&moving, &by, change);
        answer_move(&moving, route, moved)
    })
    .await;
    answered.unwrap_or_else(|error| {
        failed(shared, route, &format!("moving the marker failed: {error}"))
    })
}

/// Runs `change` as `by` under the write lease, once the tree is this request's.
fn held_change(
    shared: &Shared,
    by: &str,
    change: impl FnOnce(&Context, &str) -> Result<Response>,
) -> Result<Moved> {
    let context = shared.tree()?;
    if let Err(error) = shared.hold_lease() {
        return Ok(Moved::Unheld(Unheld::of(shared, &error)));
    }
    Ok(Moved::Done(change(&context, by)?))
}

/// The answer to one marker move, with its `write failed:` line when the marker did not move.
fn answer_move(shared: &Shared, route: &str, moved: Result<Moved>) -> Response {
    match moved {
        Ok(Moved::Done(response)) => response,
        Ok(Moved::Unheld(refusal)) => refuse(shared, route, &refusal),
        Err(error) if error.downcast_ref::<marker::Refused>().is_some() => {
            bad_request(shared, route, &format!("{error:#}"))
        }
        Err(error) => failed(shared, route, &format!("{error:#}")),
    }
}

/// The session a caller names, as one line of plain text, or `None` when it names none.
///
/// The session goes into a commit message and the marker. A line break in it would start a
/// line of its own, and a caller could add any trailer it liked — `Signed-off-by`,
/// `Co-authored-by` — under a commit the server made, so every control character becomes a
/// space.
fn session_label(session: Option<&str>) -> Option<String> {
    let label: String = session?
        .chars()
        .map(|letter| if letter.is_control() { ' ' } else { letter })
        .collect();
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

/// Applies one write for a caller already identified.
///
/// The answer, its `write failed:` line and the refusal codes `note` carries to the request log
/// are made in the blocking task: hyper drops a request whose caller hung up, not the task, and
/// a write that fails after its caller left must still say so where an alert reads it.
async fn write(
    listener: Listener,
    caller: Caller,
    route: &'static str,
    change: Change,
    session: Option<String>,
    note: &Note,
) -> Response {
    // When the request reached the handler, before its waits for the index and the tree: the
    // client's request timeout runs from about then, and the write's retries are bounded by it.
    let started = std::time::Instant::now();
    let shared = &listener.shared;
    shared.index_ready().await;
    let writing = Arc::clone(shared);
    let session = session_label(session.as_deref()).unwrap_or_else(|| "unknown".to_owned());
    let note = note.clone();
    let answered = tokio::task::spawn_blocking(move || {
        let applied = apply(&writing, &caller, &change, &session, started);
        answer_write(&writing, route, applied, &note)
    })
    .await;
    answered.unwrap_or_else(|error| failed(shared, route, &format!("the write failed: {error}")))
}

/// The answer to one write, with its `write failed:` line when it did not happen.
fn answer_write(
    shared: &Shared,
    route: &str,
    applied: Result<Applied, ApplyError>,
    note: &Note,
) -> Response {
    match applied {
        Ok(Applied::Committed(commit)) => {
            (StatusCode::OK, Json(Committed { commit })).into_response()
        }
        Ok(Applied::Refused(findings)) => {
            let kinds: Vec<&str> = findings
                .iter()
                .map(|finding| finding.kind.as_str())
                .collect();
            note.refused(kinds.iter().copied());
            log_failure(
                route,
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("refused: {}", kinds.join(", ")),
            );
            // A finding quotes what a check said, which can name the server's own files.
            let findings: Vec<Finding> = findings
                .into_iter()
                .map(|finding| Finding {
                    path: shared.hide_paths(&finding.path),
                    detail: shared.hide_paths(&finding.detail),
                    kind: finding.kind,
                })
                .collect();
            (StatusCode::UNPROCESSABLE_ENTITY, Json(Refused { findings })).into_response()
        }
        Ok(Applied::Conflict(why)) => not_written(shared, route, StatusCode::CONFLICT, &why),
        Ok(Applied::Unheld(refusal)) => refuse(shared, route, &refusal),
        Err(ApplyError::Caller(error)) => bad_request(shared, route, &format!("{error:#}")),
        Err(ApplyError::Server(error)) => failed(shared, route, &format!("{error:#}")),
        Err(ApplyError::Unheld(error)) => unheld(shared, route, &format!("{error:#}")),
        Err(ApplyError::Unknown(why)) => unheld(
            shared,
            route,
            &format!(
                "the write's outcome is unknown: {why}. If the commit reached the remote, the \
                 follow loop takes it within one pull interval; ask again before writing it twice"
            ),
        ),
        Err(ApplyError::RemoteFailed(why)) => remote_failed(shared, route, &why),
    }
}

#[cfg(test)]
mod tests;
