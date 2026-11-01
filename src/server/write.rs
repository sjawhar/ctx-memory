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
use crate::marker;
use crate::protocol::{
    Committed, DeleteRequest, Finding, MarkerRequest, MarkerResponse, PutBatchRequest, PutRequest,
    RecordFile, Refused, DELETE_ROUTE, MARKER_ROUTE, PUT_BATCH_ROUTE, PUT_ROUTE,
};
use crate::redact::credentials_in;

use self::response::{bad_request, failed, log_failure, not_written, unauthorized, unheld};
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
    Unheld(String),
}

/// What one marker advance did.
enum Moved {
    Marker(marker::Marker),
    /// This copy may not write: another copy holds the write lease.
    Unheld(String),
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
pub async fn marker_advance(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match admit::<MarkerRequest>(&listener, MARKER_ROUTE, &headers, &body, &note).await {
        Ok((caller, request)) => advance(&listener.shared, caller, request).await,
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
    let shared = &listener.shared;
    let caller = match listener.caller(headers).await {
        Ok(caller) => caller,
        Err(error) => return Err(Box::new(unauthorized(shared, route, &error))),
    };
    note.caller(&caller);
    serde_json::from_slice(body)
        .map(|request| (caller, request))
        .map_err(|error| {
            Box::new(bad_request(
                shared,
                route,
                &format!("invalid request: {error}"),
            ))
        })
}

/// Moves the marker for a caller already identified, as [`marker_advance`] says.
async fn advance(shared: &Arc<Shared>, caller: Caller, request: MarkerRequest) -> Response {
    // A local advance names the session that moved the marker; through the server the marker
    // names the signed-in caller and the session it says it is. The marker is on the share
    // every copy and the fetch job read, so a session holding a credential is refused as a
    // write's is, without repeating it.
    let session = session_label(request.session.as_deref());
    match session.as_deref().map(credentials_in).transpose() {
        Ok(Some(kinds)) if !kinds.is_empty() => {
            return bad_request(
                shared,
                MARKER_ROUTE,
                &format!(
                    "the session holds what has the shape of {}; a credential never goes into \
                     the marker: take it out, and revoke it if it is real",
                    kinds.join(" and ")
                ),
            );
        }
        Ok(_) => {}
        Err(error) => return failed(shared, MARKER_ROUTE, &format!("{error:#}")),
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
    let advancing = Arc::clone(shared);
    let answered = tokio::task::spawn_blocking(move || {
        let moved = move_marker(&advancing, &request.window, &by);
        answer_advance(&advancing, moved)
    })
    .await;
    answered.unwrap_or_else(|error| {
        failed(
            shared,
            MARKER_ROUTE,
            &format!("advancing the marker failed: {error}"),
        )
    })
}

/// Moves the marker to `window` under the write lease, once the tree is this request's.
fn move_marker(shared: &Shared, window: &str, by: &str) -> Result<Moved> {
    let context = shared.tree()?;
    if let Err(error) = shared.hold_lease() {
        return Ok(Moved::Unheld(format!("{error:#}")));
    }
    Ok(Moved::Marker(marker::advance(&context, window, by)?))
}

/// The answer to one marker advance, with its `write failed:` line when the marker did not move.
fn answer_advance(shared: &Shared, moved: Result<Moved>) -> Response {
    match moved {
        Ok(Moved::Marker(marker)) => {
            (StatusCode::OK, Json(MarkerResponse { marker })).into_response()
        }
        Ok(Moved::Unheld(why)) => unheld(shared, MARKER_ROUTE, &why),
        Err(error) if error.downcast_ref::<marker::Refused>().is_some() => {
            bad_request(shared, MARKER_ROUTE, &format!("{error:#}"))
        }
        Err(error) => failed(shared, MARKER_ROUTE, &format!("{error:#}")),
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
    let shared = &listener.shared;
    shared.index_ready().await;
    let writing = Arc::clone(shared);
    let session = session_label(session.as_deref()).unwrap_or_else(|| "unknown".to_owned());
    let note = note.clone();
    let answered = tokio::task::spawn_blocking(move || {
        let applied = apply(&writing, &caller, &change, &session);
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
        Ok(Applied::Unheld(why)) => unheld(shared, route, &why),
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
    }
}

#[cfg(test)]
mod tests;
