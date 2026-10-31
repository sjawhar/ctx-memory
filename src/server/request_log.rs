//! The request log: one JSON line on standard output for every request either door answers,
//! written when its answer is ready, so who reads and writes the graph, and how, can be counted.
//!
//! Cloud Run parses a line of JSON into the entry's `jsonPayload`, takes `time` as the entry's
//! timestamp and `severity` as its severity. It logs the requests that reach the
//! identity-token door itself, but never one that comes through the Tailscale sidecar, which
//! reaches this process on loopback; this log covers both doors alike.
//!
//! A line says who called, which command and how it ended, never what the request carried: no
//! search query (only its length), no record text, no quote, no token. A record path is the one
//! thing a caller wrote that a line carries, as one line of plain text with any credential
//! taken out. Standard error keeps the lines an alert matches.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::body::HttpBody as _;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use super::identity::Caller;
use crate::protocol::{DELETE_ROUTE, MARKER_ROUTE, PUT_ROUTE};
use crate::redact::one_line;

/// The door a request came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Door {
    /// The loopback port behind the Tailscale sidecar.
    Tailnet,
    /// The ingress port, where a caller brings a Google identity token.
    IdentityToken,
}

/// What a handler learned about its request that the line reports and the response does not
/// show: who called, which command, and the parts of it a line may carry. A handler puts it
/// on its response ([`Facts::on`]); a request no handler answered has none.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    caller: Option<String>,
    command: Option<String>,
    path: Option<String>,
    query_chars: Option<usize>,
    exit_code: Option<u8>,
    refusals: Vec<String>,
}

impl Facts {
    /// The caller, once identified.
    pub fn caller(&mut self, caller: &Caller) {
        self.caller = Some(caller.email.clone());
    }

    /// The ctx command the request ran, as [`crate::cli::Command::name`] names it.
    pub fn command(&mut self, name: String) {
        self.command = Some(name);
    }

    /// The record the request named.
    pub fn path(&mut self, path: &str) {
        self.path = Some(one_line(path));
    }

    /// The text a search looked for, of which a line carries only the length.
    pub fn query(&mut self, query: &str) {
        self.query_chars = Some(query.chars().count());
    }

    /// The exit code of the command that ran.
    pub fn exit_code(&mut self, code: u8) {
        self.exit_code = Some(code);
    }

    /// The kinds of the findings that refused a write, in the order the answer gives them.
    pub fn refused<'kind>(&mut self, kinds: impl IntoIterator<Item = &'kind str>) {
        self.refusals = kinds.into_iter().map(str::to_owned).collect();
    }

    /// `response`, carrying these facts to the request log.
    #[must_use]
    pub fn on(self, mut response: Response) -> Response {
        response.extensions_mut().insert(self);
        response
    }
}

/// One line of the request log. The fields are written in this order.
#[derive(Serialize)]
struct Line<'facts> {
    time: String,
    severity: &'static str,
    door: Door,
    caller: Option<&'facts str>,
    command: Option<&'facts str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'facts str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_chars: Option<usize>,
    result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    refusals: Option<&'facts [String]>,
    status: u16,
    duration_ms: u64,
    response_bytes: Option<u64>,
}

/// Times one request, lets it be answered, and writes its line.
pub async fn log(State(door): State<Door>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let route = route_command(request.uri().path());
    let mut response = next.run(request).await;
    let facts = response
        .extensions_mut()
        .remove::<Facts>()
        .unwrap_or_default();
    let line = line(
        door,
        &facts,
        route,
        response.status(),
        started.elapsed().as_millis(),
        response.body().size_hint().exact(),
    );
    write(&line);
    response
}

/// The command a route always runs, which names the command of a request its handler never
/// saw, such as one axum refused for its size. `/v1/command` runs whatever its body says.
fn route_command(path: &str) -> Option<&'static str> {
    match path {
        "/healthz" => Some("healthz"),
        PUT_ROUTE => Some("record put"),
        DELETE_ROUTE => Some("record rm"),
        MARKER_ROUTE => Some("marker advance"),
        _ => None,
    }
}

fn line<'facts>(
    door: Door,
    facts: &'facts Facts,
    route: Option<&'static str>,
    status: StatusCode,
    elapsed_ms: u128,
    response_bytes: Option<u64>,
) -> Line<'facts> {
    Line {
        time: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        severity: severity(status),
        door,
        caller: facts.caller.as_deref(),
        command: facts.command.as_deref().or(route),
        path: facts.path.as_deref(),
        query_chars: facts.query_chars,
        result: result(status, facts.exit_code),
        refusals: (!facts.refusals.is_empty()).then_some(facts.refusals.as_slice()),
        status: status.as_u16(),
        duration_ms: u64::try_from(elapsed_ms).unwrap_or(u64::MAX),
        response_bytes,
    }
}

/// The severity Cloud Logging files a line under: a server error is an error, a request the
/// server refused is a warning.
fn severity(status: StatusCode) -> &'static str {
    if status.is_server_error() {
        "ERROR"
    } else if status.is_client_error() {
        "WARNING"
    } else {
        "INFO"
    }
}

/// How a request ended, in a word: `ok`, the exit code of a command that failed, or the class
/// of the status.
fn result(status: StatusCode, exit_code: Option<u8>) -> String {
    if status.is_success() {
        return match exit_code {
            Some(code) if code != 0 => format!("exit-{code}"),
            _ => "ok".to_owned(),
        };
    }
    let class = match status {
        StatusCode::BAD_REQUEST => "bad-request",
        StatusCode::UNAUTHORIZED => "unidentified",
        StatusCode::NOT_FOUND => "not-found",
        StatusCode::METHOD_NOT_ALLOWED => "method-not-allowed",
        StatusCode::CONFLICT => "conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "too-large",
        StatusCode::UNPROCESSABLE_ENTITY => "refused",
        StatusCode::INTERNAL_SERVER_ERROR => "server-error",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        _ => return format!("http-{}", status.as_u16()),
    };
    class.to_owned()
}

/// Whether a line has failed to reach standard output, which is said once.
static UNWRITTEN: AtomicBool = AtomicBool::new(false);

/// Writes `line` to standard output as one line, in one write. A line that cannot be written
/// does not stop the request it describes: the first failure is said on standard error, once.
fn write(line: &Line<'_>) {
    let written = serde_json::to_vec(line)
        .map_err(std::io::Error::from)
        .and_then(|mut text| {
            text.push(b'\n');
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&text)?;
            stdout.flush()
        });
    if let Err(error) = written {
        if !UNWRITTEN.swap(true, Ordering::Relaxed) {
            eprintln!(
                "the request log cannot be written to standard output, and later failures are \
                 not reported: {}",
                one_line(&error.to_string())
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printed(line: &Line<'_>) -> serde_json::Value {
        serde_json::to_value(line).unwrap_or_default()
    }

    /// A line names the caller, the command and the record path, and gives a search only the
    /// length of its query: the query's words are never in the facts a line is made from.
    #[test]
    fn a_search_is_logged_by_the_length_of_its_query() {
        let mut facts = Facts::default();
        facts.caller(&Caller {
            name: "ada".to_owned(),
            email: "ada@example.com".to_owned(),
            via: crate::server::Via::Tailscale,
        });
        facts.command("search".to_owned());
        facts.query("renewal terms ünïcode");
        facts.exit_code(0);
        let line = printed(&line(
            Door::Tailnet,
            &facts,
            None,
            StatusCode::OK,
            12,
            Some(345),
        ));
        assert_eq!(
            line,
            serde_json::json!({
                "time": line.get("time").cloned().unwrap_or_default(),
                "severity": "INFO",
                "door": "tailnet",
                "caller": "ada@example.com",
                "command": "search",
                "query_chars": 21,
                "result": "ok",
                "status": 200,
                "duration_ms": 12,
                "response_bytes": 345,
            })
        );
        assert!(!line.to_string().contains("renewal"), "{line}");
    }

    /// A record path is one line of plain text with any credential taken out, as a
    /// `write failed:` line quotes it.
    #[test]
    fn a_path_is_one_line_with_no_credential() {
        let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let mut facts = Facts::default();
        facts.path(&format!("decisions/{token}\n\u{1b}[2J.md"));
        let line = printed(&line(
            Door::IdentityToken,
            &facts,
            Some("record put"),
            StatusCode::BAD_REQUEST,
            0,
            Some(2),
        ));
        assert_eq!(
            line.get("path"),
            Some(&serde_json::json!("decisions/[redacted] [2J.md"))
        );
        assert_eq!(line.get("command"), Some(&serde_json::json!("record put")));
        assert_eq!(line.get("door"), Some(&serde_json::json!("identity-token")));
        assert_eq!(line.get("severity"), Some(&serde_json::json!("WARNING")));
    }

    /// A refused write gives its refusal codes; a command that ran and failed gives its exit
    /// code; every other answer gives the class of its status.
    #[test]
    fn a_result_is_ok_a_refusal_an_exit_code_or_the_class_of_the_status() {
        let cases = [
            (StatusCode::OK, None, "ok"),
            (StatusCode::OK, Some(0), "ok"),
            (StatusCode::OK, Some(2), "exit-2"),
            (StatusCode::BAD_REQUEST, None, "bad-request"),
            (StatusCode::UNAUTHORIZED, None, "unidentified"),
            (StatusCode::NOT_FOUND, None, "not-found"),
            (StatusCode::METHOD_NOT_ALLOWED, None, "method-not-allowed"),
            (StatusCode::CONFLICT, None, "conflict"),
            (StatusCode::PAYLOAD_TOO_LARGE, None, "too-large"),
            (StatusCode::UNPROCESSABLE_ENTITY, None, "refused"),
            (StatusCode::INTERNAL_SERVER_ERROR, None, "server-error"),
            (StatusCode::SERVICE_UNAVAILABLE, None, "unavailable"),
            (StatusCode::IM_A_TEAPOT, None, "http-418"),
        ];
        for (status, exit_code, expected) in cases {
            assert_eq!(result(status, exit_code), expected, "{status}");
        }
        let mut facts = Facts::default();
        facts.refused(["gap", "credential", "credential"]);
        let line = printed(&line(
            Door::Tailnet,
            &facts,
            Some("record put"),
            StatusCode::UNPROCESSABLE_ENTITY,
            0,
            None,
        ));
        assert_eq!(
            line.get("refusals"),
            Some(&serde_json::json!(["gap", "credential", "credential"]))
        );
        assert_eq!(line.get("response_bytes"), Some(&serde_json::Value::Null));
        assert_eq!(
            severity(StatusCode::INTERNAL_SERVER_ERROR),
            "ERROR",
            "a server error is an error"
        );
    }
}
