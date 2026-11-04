//! HTTP failure answers and their stable operational log line.

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;

use crate::redact::one_line;

use super::Shared;

/// What a `write failed:` line says when the caller was not identified. Why it was not quotes
/// what the caller sent — an identity token's key id, an email — so the line says only that.
const UNIDENTIFIED: &str = "the caller was not identified";

pub(super) fn unauthorized(shared: &Shared, route: &str, error: &anyhow::Error) -> Response {
    log_failure(route, StatusCode::UNAUTHORIZED, UNIDENTIFIED);
    super::command::error_response(shared, StatusCode::UNAUTHORIZED, &format!("{error:#}"))
}

pub(super) fn bad_request(shared: &Shared, route: &str, message: &str) -> Response {
    not_written(shared, route, StatusCode::BAD_REQUEST, message)
}

/// The answer to a client whose protocol version is older than what it asked needs: `409`,
/// naming the version the client speaks, the one this server speaks and the one `what` needs,
/// so the caller is told the two releases differ rather than that it named something wrongly.
pub(super) fn too_old(
    shared: &Shared,
    route: &str,
    client: u32,
    what: &str,
    since: u32,
) -> Response {
    let message = format!(
        "this client speaks ctx protocol {client} and the server protocol {}: {what} needs \
         protocol {since} or later. Roll the client to the server's release",
        crate::protocol::VERSION
    );
    not_written(shared, route, StatusCode::CONFLICT, &message)
}

/// Another copy of the server holds the write lease, so this copy wrote nothing. A caller
/// that tries again reaches the copy that holds it, or this one once it has taken it over.
pub(super) fn unheld(shared: &Shared, route: &str, message: &str) -> Response {
    not_written(shared, route, StatusCode::SERVICE_UNAVAILABLE, message)
}

pub(super) fn failed(shared: &Shared, route: &str, message: &str) -> Response {
    not_written(shared, route, StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// The answer to a write that did not happen, with why, logged as [`log_failure`] says. The
/// log line keeps the server's paths, which its operator reads; the answer does not.
pub(super) fn not_written(
    shared: &Shared,
    route: &str,
    status: StatusCode,
    message: &str,
) -> Response {
    log_failure(route, status, message);
    super::command::error_response(shared, status, message)
}

/// What every line about a write that did not happen starts with. An alert matches it.
const WRITE_FAILED: &str = "write failed: ";

/// Says on standard error that a write did not happen, in the one form an alert matches:
/// `write failed: <route> <status> <reason>`. The request log on standard output records every
/// request too, but as data to count, not as a line an alert matches.
pub(super) fn log_failure(route: &str, status: StatusCode, reason: &str) {
    eprintln!("{}", failure_line(route, status, reason));
}

/// The line [`log_failure`] prints. The reason says why the write failed, never what it
/// carried, as [`one_line`] makes it. git's own errors reach here with the installation token
/// already taken out of them.
pub(super) fn failure_line(route: &str, status: StatusCode, reason: &str) -> String {
    format!(
        "{WRITE_FAILED}{route} {} {}",
        status.as_u16(),
        one_line(reason)
    )
}

/// What every line about a deferred write starts with: one deferred by a rollout's handover,
/// or by a remote that kept failing its push. It is not [`WRITE_FAILED`]: the write did not
/// happen, but the caller is told when to ask again and this server is not at fault, so an
/// alert on failed writes does not match it.
const WRITE_DEFERRED: &str = "write deferred: ";

/// The seconds a caller is asked to wait, in `Retry-After`, before it sends again a write whose
/// pushes the remote kept failing on its own side.
///
/// The client sends a deferred write again until 180 seconds have passed since its first try,
/// requests included, so at this pause it tries up to five more times, each with the server's
/// own retries, spread across those three minutes: during the outage this answers, the remote
/// took some pushes and failed others, minutes apart.
const REMOTE_PAUSE: u64 = 30;

/// Another copy of the server still holds the write lease while this new copy takes over
/// from it, so this copy wrote nothing. The answer is `503` with `Retry-After`, the seconds
/// until this copy may take the lease over, and the line on standard error is
/// `write deferred: <route> 503 a handover: … <reason>`.
pub(super) fn deferred(shared: &Shared, route: &str, message: &str, pause: u64) -> Response {
    let why = format!("a handover: this copy may take the write lease over in {pause} s");
    defer(shared, route, &why, message, pause)
}

/// Every push of the write failed on the remote's side and the remote does not hold the
/// commit, so nothing was published and the clone is back where the write found it. The
/// answer is `503` with `Retry-After` of [`REMOTE_PAUSE`] seconds, and the line on standard
/// error is `write deferred: <route> 503 the remote failed … <reason>`.
pub(super) fn remote_failed(shared: &Shared, route: &str, message: &str) -> Response {
    let why =
        format!("the remote failed every push on its own side; ask again in {REMOTE_PAUSE} s");
    defer(shared, route, &why, message, REMOTE_PAUSE)
}

/// The answer to a deferred write: `503` with `Retry-After: <pause>` and `message`, and one
/// `write deferred:` line on standard error that says `why` and then `message`.
fn defer(shared: &Shared, route: &str, why: &str, message: &str, pause: u64) -> Response {
    let status = StatusCode::SERVICE_UNAVAILABLE;
    eprintln!("{}", deferred_line(route, status, why, message));
    let mut response = super::command::error_response(shared, status, message);
    response
        .headers_mut()
        .insert(axum::http::header::RETRY_AFTER, HeaderValue::from(pause));
    // The request log files this answer as `deferred`, a warning, not as a server error.
    response
        .extensions_mut()
        .insert(super::super::request_log::Deferred);
    response
}

/// The line [`defer`] prints, one line as [`one_line`] makes it.
fn deferred_line(route: &str, status: StatusCode, why: &str, reason: &str) -> String {
    format!(
        "{WRITE_DEFERRED}{route} {} {}",
        status.as_u16(),
        one_line(&format!("{why}: {reason}"))
    )
}
