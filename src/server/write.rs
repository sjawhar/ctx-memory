//! The write endpoints: the server is the only writer of records.
//!
//! One writer lock covers the whole sequence, so two callers cannot interleave. Every write
//! is pulled onto the newest `main`, checked the way `ctx sweep` and `ctx cite verify` check
//! it, committed under the caller's own name, and pushed. A push that loses a race with a
//! merge is rebased once and checked again before it is pushed a second time.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::command;
use super::identity::Caller;
use super::{Listener, Shared};
use crate::marker;
use crate::protocol::{
    Committed, DeleteRequest, Finding, MarkerRequest, MarkerResponse, PutRequest, Refused,
    DELETE_ROUTE, MARKER_ROUTE, PUT_ROUTE,
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
    Put { path: String, content: String },
    Delete { path: String },
}

impl Change {
    fn path(&self) -> &str {
        match self {
            Self::Put { path, .. } | Self::Delete { path } => path,
        }
    }

    const fn verb(&self) -> &'static str {
        match self {
            Self::Put { .. } => "put",
            Self::Delete { .. } => "delete",
        }
    }

    /// The route the change arrived on.
    const fn route(&self) -> &'static str {
        match self {
            Self::Put { .. } => PUT_ROUTE,
            Self::Delete { .. } => DELETE_ROUTE,
        }
    }
}

/// Answers `POST /v1/records/put`.
pub async fn put(State(listener): State<Listener>, headers: HeaderMap, body: Bytes) -> Response {
    let shared = &listener.shared;
    let caller = match listener.caller(&headers).await {
        Ok(caller) => caller,
        Err(error) => return unauthorized(shared, PUT_ROUTE, &error),
    };
    let request: PutRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return bad_request(shared, PUT_ROUTE, &format!("invalid request: {error}")),
    };
    let change = Change::Put {
        path: request.path,
        content: request.content,
    };
    write(listener, caller, change, request.session).await
}

/// Answers `POST /v1/records/delete`.
pub async fn delete(State(listener): State<Listener>, headers: HeaderMap, body: Bytes) -> Response {
    let shared = &listener.shared;
    let caller = match listener.caller(&headers).await {
        Ok(caller) => caller,
        Err(error) => return unauthorized(shared, DELETE_ROUTE, &error),
    };
    let request: DeleteRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return bad_request(shared, DELETE_ROUTE, &format!("invalid request: {error}"))
        }
    };
    let change = Change::Delete { path: request.path };
    write(listener, caller, change, request.session).await
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
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let shared = &listener.shared;
    let caller = match listener.caller(&headers).await {
        Ok(caller) => caller,
        Err(error) => return unauthorized(shared, MARKER_ROUTE, &error),
    };
    let request: MarkerRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return bad_request(shared, MARKER_ROUTE, &format!("invalid request: {error}"))
        }
    };
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
    let advancing = Arc::clone(shared);
    let advanced = tokio::task::spawn_blocking(move || -> Result<Moved> {
        let context = advancing.tree()?;
        if let Err(error) = advancing.hold_lease() {
            return Ok(Moved::Unheld(format!("{error:#}")));
        }
        Ok(Moved::Marker(marker::advance(
            &context,
            &request.window,
            &by,
        )?))
    })
    .await;
    match advanced {
        Ok(Ok(Moved::Marker(marker))) => {
            (StatusCode::OK, Json(MarkerResponse { marker })).into_response()
        }
        Ok(Ok(Moved::Unheld(why))) => unheld(shared, MARKER_ROUTE, &why),
        Ok(Err(error)) if error.downcast_ref::<marker::Refused>().is_some() => {
            bad_request(shared, MARKER_ROUTE, &format!("{error:#}"))
        }
        Ok(Err(error)) => failed(shared, MARKER_ROUTE, &format!("{error:#}")),
        Err(error) => failed(
            shared,
            MARKER_ROUTE,
            &format!("advancing the marker failed: {error}"),
        ),
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

async fn write(
    listener: Listener,
    caller: Caller,
    change: Change,
    session: Option<String>,
) -> Response {
    let shared = &listener.shared;
    let writing = Arc::clone(shared);
    let route = change.route();
    let session = session_label(session.as_deref()).unwrap_or_else(|| "unknown".to_owned());
    let applied =
        tokio::task::spawn_blocking(move || apply(&writing, &caller, &change, &session)).await;
    match applied {
        Ok(Ok(Applied::Committed(commit))) => {
            (StatusCode::OK, Json(Committed { commit })).into_response()
        }
        Ok(Ok(Applied::Refused(findings))) => {
            let kinds: Vec<&str> = findings
                .iter()
                .map(|finding| finding.kind.as_str())
                .collect();
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
        Ok(Ok(Applied::Conflict(why))) => not_written(shared, route, StatusCode::CONFLICT, &why),
        Ok(Ok(Applied::Unheld(why))) => unheld(shared, route, &why),
        Ok(Err(ApplyError::Caller(error))) => bad_request(shared, route, &format!("{error:#}")),
        Ok(Err(ApplyError::Server(error))) => failed(shared, route, &format!("{error:#}")),
        Ok(Err(ApplyError::Unheld(error))) => unheld(shared, route, &format!("{error:#}")),
        Ok(Err(ApplyError::Unknown(why))) => unheld(
            shared,
            route,
            &format!(
                "the write's outcome is unknown: {why}. If the commit reached the remote, the \
                 follow loop takes it within one pull interval; ask again before writing it twice"
            ),
        ),
        Err(error) => failed(shared, route, &format!("the write failed: {error}")),
    }
}

#[cfg(test)]
mod tests;
