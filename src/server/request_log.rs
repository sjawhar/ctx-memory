//! The request log: one JSON line on standard output for every request either door takes, so
//! who reads and writes the graph, and how, can be counted.
//!
//! Cloud Run parses a line of JSON into the entry's `jsonPayload`, takes `time` as the entry's
//! timestamp and `severity` as its severity. It logs the requests that reach the
//! identity-token door itself, but never one that comes through the Tailscale sidecar, which
//! reaches this process on loopback; this log covers both doors alike.
//!
//! A line says who called, which command and how it ended, never what the request carried: no
//! search query (only its length), no record text, no quote, no identity token. A record path
//! is the one thing a caller wrote that a line carries, as one line of plain text with any
//! credential of a shape the server refuses taken out. Standard error keeps the lines an alert
//! matches.
//!
//! A request is logged when its answer is ready, or, when hyper drops it unanswered because
//! its caller hung up, at that moment. A request never waits for standard output: a thread of
//! its own writes the lines, and a collector that stops reading costs lines, not answers.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::body::HttpBody as _;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
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

/// What a handler learns about its request that its line reports and its answer does not show:
/// who called, which command, and the parts of it a line may carry.
///
/// The request log puts one on each request it times, and a handler reads it as an
/// [`axum::Extension`]. The two share it, so a request dropped before its answer is ready still
/// logs what its handler had learned.
#[derive(Clone, Debug, Default)]
pub struct Note(Arc<Mutex<Facts>>);

#[derive(Debug, Default)]
struct Facts {
    caller: Option<String>,
    command: Option<String>,
    path: Option<String>,
    query_chars: Option<usize>,
    exit_code: Option<u8>,
    refusals: Vec<String>,
}

impl Note {
    /// The caller, once identified.
    pub fn caller(&self, caller: &Caller) {
        self.facts().caller = Some(caller.email.clone());
    }

    /// The ctx command the request runs, by its fixed name, never by text the caller wrote.
    pub fn command(&self, name: String) {
        self.facts().command = Some(name);
    }

    /// The record the request names.
    pub fn path(&self, path: &str) {
        self.facts().path = Some(one_line(path));
    }

    /// The text a search looks for, of which a line carries only the length.
    pub fn query(&self, query: &str) {
        self.facts().query_chars = Some(query.chars().count());
    }

    /// The exit code of the command that ran.
    pub fn exit_code(&self, code: u8) {
        self.facts().exit_code = Some(code);
    }

    /// The kinds of the findings that refused a write, in the order the answer gives them.
    pub fn refused<'kind>(&self, kinds: impl IntoIterator<Item = &'kind str>) {
        self.facts().refusals = kinds.into_iter().map(str::to_owned).collect();
    }

    /// The facts, whatever a holder that panicked left: each field is written whole.
    fn facts(&self) -> MutexGuard<'_, Facts> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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
    status: Option<u16>,
    duration_ms: u64,
    response_bytes: Option<u64>,
}

/// The answer a request got: its status, and how many bytes its body holds when that is known.
type Answer = (StatusCode, Option<u64>);

/// Times one request, lets it be answered, and logs it.
pub async fn log(State(door): State<Door>, mut request: Request, next: Next) -> Response {
    let note = Note::default();
    request.extensions_mut().insert(note.clone());
    let mut pending = Pending {
        door,
        route: route_command(request.uri().path()),
        head: request.method() == Method::HEAD,
        started: Instant::now(),
        note,
        logged: false,
    };
    let response = next.run(request).await;
    // axum takes the body off an answer to HEAD after this layer has seen it: none is sent.
    let bytes = if pending.head {
        Some(0)
    } else {
        response.body().size_hint().exact()
    };
    pending.log(Some((response.status(), bytes)));
    response
}

/// A request not yet logged. hyper drops a request whose caller hung up before its answer was
/// ready, and with it this, which then logs the request as unanswered. A write runs on after
/// that, and how it ended is in its commit or its `write failed:` line.
struct Pending {
    door: Door,
    route: Option<&'static str>,
    head: bool,
    started: Instant,
    note: Note,
    logged: bool,
}

impl Pending {
    fn log(&mut self, answer: Option<Answer>) {
        self.logged = true;
        let elapsed = self.started.elapsed().as_millis();
        let facts = self.note.facts();
        queue(&line(self.door, &facts, self.route, answer, elapsed));
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.logged {
            self.log(None);
        }
    }
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
    answer: Option<Answer>,
    elapsed_ms: u128,
) -> Line<'facts> {
    let status = answer.map(|(status, _)| status);
    Line {
        time: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        severity: status.map_or("WARNING", severity),
        door,
        caller: facts.caller.as_deref(),
        command: facts.command.as_deref().or(route),
        path: facts.path.as_deref(),
        query_chars: facts.query_chars,
        result: status.map_or_else(
            || "unanswered".to_owned(),
            |status| result(status, facts.exit_code),
        ),
        refusals: (!facts.refusals.is_empty()).then_some(facts.refusals.as_slice()),
        status: status.map(|status| status.as_u16()),
        duration_ms: u64::try_from(elapsed_ms).unwrap_or(u64::MAX),
        response_bytes: answer.and_then(|(_, bytes)| bytes),
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

/// How many lines may wait for standard output. A collector that stops reading fills the pipe,
/// then this queue, and after that each new line is dropped: the server answers on.
const QUEUE_LINES: usize = 1024;

/// What the writer thread is handed.
enum Entry {
    /// A line, newline included.
    Line(Vec<u8>),
    /// Said back once every line queued before it is written.
    Flush(mpsc::Sender<()>),
}

/// The queue of the thread that writes the lines, started with the first line; `None` when
/// the thread could not start, which is said once.
static WRITER: LazyLock<Option<SyncSender<Entry>>> = LazyLock::new(start_writer);

/// Whether a line was refused by standard output, which is said once.
static UNWRITTEN: AtomicBool = AtomicBool::new(false);

/// Whether a line was dropped because the queue was full, which is said once.
static DROPPED: AtomicBool = AtomicBool::new(false);

fn start_writer() -> Option<SyncSender<Entry>> {
    let (writer, entries) = mpsc::sync_channel(QUEUE_LINES);
    let started = std::thread::Builder::new()
        .name("request-log".to_owned())
        .spawn(move || {
            let mut stdout = std::io::stdout();
            for entry in entries {
                match entry {
                    Entry::Line(text) => {
                        if let Err(error) = stdout.write_all(&text).and_then(|()| stdout.flush()) {
                            unwritten(&error.to_string());
                        }
                    }
                    Entry::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
    match started {
        Ok(_) => Some(writer),
        Err(error) => {
            unwritten(&format!("its thread did not start: {error}"));
            None
        }
    }
}

/// Hands `line` to the writer thread, without waiting.
fn queue(line: &Line<'_>) {
    let mut text = match serde_json::to_vec(line) {
        Ok(text) => text,
        Err(error) => return unwritten(&error.to_string()),
    };
    text.push(b'\n');
    let Some(writer) = WRITER.as_ref() else {
        return;
    };
    match writer.try_send(Entry::Line(text)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => say_once(
            &DROPPED,
            "the request log is behind standard output: lines are dropped until it catches up",
        ),
        Err(TrySendError::Disconnected(_)) => unwritten("its thread has stopped"),
    }
}

/// Waits, for at most `wait`, for every line queued so far to reach standard output. The server
/// calls it once its listeners have stopped, so its last requests' lines are not lost when the
/// process exits; a collector that stopped reading costs the wait, not the shutdown.
pub fn flush(wait: Duration) {
    let Some(writer) = WRITER.as_ref() else {
        return;
    };
    let deadline = Instant::now() + wait;
    let (done, flushed) = mpsc::channel();
    let mut entry = Entry::Flush(done);
    loop {
        match writer.try_send(entry) {
            Ok(()) => break,
            Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                entry = back;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
    let _ = flushed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
}

fn unwritten(why: &str) {
    say_once(
        &UNWRITTEN,
        &format!(
            "the request log cannot be written to standard output, and later failures are not \
             reported: {}",
            one_line(why)
        ),
    );
}

/// Says `message` on standard error the first time `said` is set, from a thread of its own:
/// standard error can be as full as standard output — the same pipe, or another nobody reads
/// — and the request that met the failure must not wait on it. A message that cannot be
/// written is let be: `eprintln!` would panic.
fn say_once(said: &AtomicBool, message: &str) {
    if said.swap(true, Ordering::Relaxed) {
        return;
    }
    let message = message.to_owned();
    let _ = std::thread::Builder::new()
        .name("request-log-report".to_owned())
        .spawn(move || {
            let _ = writeln!(std::io::stderr().lock(), "{message}");
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printed(line: &Line<'_>) -> serde_json::Value {
        serde_json::to_value(line).unwrap_or_default()
    }

    fn facts(note: &Note) -> MutexGuard<'_, Facts> {
        note.facts()
    }

    /// A line names the caller, the command and the record path, and gives a search only the
    /// length of its query: the query's words are never in the facts a line is made from.
    #[test]
    fn a_search_is_logged_by_the_length_of_its_query() {
        let note = Note::default();
        note.caller(&Caller {
            name: "ada".to_owned(),
            email: "ada@example.com".to_owned(),
            via: crate::server::Via::Tailscale,
        });
        note.command("search".to_owned());
        note.query("renewal terms ünïcode");
        note.exit_code(0);
        let line = printed(&line(
            Door::Tailnet,
            &facts(&note),
            None,
            Some((StatusCode::OK, Some(345))),
            12,
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
        let note = Note::default();
        note.path(&format!("decisions/{token}\n\u{1b}[2J.md"));
        let line = printed(&line(
            Door::IdentityToken,
            &facts(&note),
            Some("record put"),
            Some((StatusCode::BAD_REQUEST, Some(2))),
            0,
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
    /// code; a request dropped before its answer is unanswered; every other answer gives the
    /// class of its status.
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
        let note = Note::default();
        note.refused(["gap", "credential", "credential"]);
        let refused = printed(&line(
            Door::Tailnet,
            &facts(&note),
            Some("record put"),
            Some((StatusCode::UNPROCESSABLE_ENTITY, None)),
            0,
        ));
        assert_eq!(
            refused.get("refusals"),
            Some(&serde_json::json!(["gap", "credential", "credential"]))
        );
        assert_eq!(
            refused.get("response_bytes"),
            Some(&serde_json::Value::Null)
        );
        let unanswered = printed(&line(
            Door::Tailnet,
            &facts(&note),
            Some("record put"),
            None,
            1500,
        ));
        for (field, value) in [
            ("result", serde_json::json!("unanswered")),
            ("status", serde_json::Value::Null),
            ("response_bytes", serde_json::Value::Null),
            ("severity", serde_json::json!("WARNING")),
            ("duration_ms", serde_json::json!(1500)),
        ] {
            assert_eq!(unanswered.get(field), Some(&value), "{unanswered}");
        }
        assert_eq!(
            severity(StatusCode::INTERNAL_SERVER_ERROR),
            "ERROR",
            "a server error is an error"
        );
    }
}
