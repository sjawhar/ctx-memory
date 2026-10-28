//! HTTP failure answers and their stable operational log line.

use axum::http::StatusCode;
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
/// `write failed: <route> <status> <reason>`. A write that arrives over the tailnet passes no
/// request log but this line.
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
