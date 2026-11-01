//! `/v1/command`: run one allowlisted read command and answer with what it printed.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use clap::CommandFactory as _;
use serde_json::json;

use super::request_log::Note;
use super::{window_count, Following, Listener, Shared};
use crate::cli::{self, Remote};
use crate::context::Context;
use crate::git::Repo;
use crate::output::Sink;
use crate::protocol::{Captured, CommandRequest, Draft};
use crate::redact::one_line;
use crate::remote::{global_flag, Global};
use crate::tree::Doc;

/// The first words of the commands a remote caller may run.
///
/// Whether one of them runs is [`cli::Command::remote`]'s answer for the parsed command; this
/// list is what turns a first word no remote caller may use into a refusal before clap is
/// asked, rather than clap's usage error. `record` is here for `record show`: `record put` and
/// `record rm` parse as writes and are refused.
const ALLOWED: &[&str] = &[
    "rules", "sweep", "search", "dossier", "open", "resolve", "cite", "delta", "marker", "pending",
    "record",
];

/// Runs one command against a loaded tree and captures what it would have printed.
///
/// It reports through [`cli::run_reported`], the policy the binary itself uses, so a remote
/// caller sees the same text, on the same stream, with the same exit code as a local run.
#[must_use]
pub fn run_captured(argv: &[String], drafts: Vec<Draft>, context: &Context) -> Captured {
    let drafts: Vec<Doc> = drafts
        .into_iter()
        .map(|draft| Doc::from_text(PathBuf::from(draft.path), draft.content))
        .collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = cli::run_reported(
        std::iter::once("ctx").chain(argv.iter().map(String::as_str)),
        &mut Sink::new(&mut stdout, &mut stderr),
        |parsed, sink| cli::run_loaded_with(parsed, context, &drafts, sink),
    );
    Captured {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    }
}

/// Answers `POST /v1/command`. What the request log says of it goes in `note`.
pub async fn command(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let shared = &listener.shared;
    match listener.caller(&headers).await {
        Ok(caller) => note.caller(&caller),
        Err(error) => {
            return error_response(shared, StatusCode::UNAUTHORIZED, &format!("{error:#}"))
        }
    }
    let request: CommandRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                shared,
                StatusCode::BAD_REQUEST,
                &format!("invalid request: {error}"),
            )
        }
    };
    let parsed = cli::parse_argv(&request.argv).ok();
    describe(&request.argv, parsed.as_ref(), &note);
    if let Err(message) = allowed(&request.argv, parsed.as_ref()) {
        return error_response(shared, StatusCode::BAD_REQUEST, &message);
    }
    let record_text = parsed.as_ref().is_some_and(shows_a_record);
    // The index this copy builds as it comes up is read by `resolve`, `cite verify` and the
    // mentions an answer carries, and a command that took the tree and then waited for that
    // build would hold it, so every later command waited too. The wait is here instead,
    // before the tree is taken.
    shared.index_ready().await;
    let running = Arc::clone(shared);
    let captured = tokio::task::spawn_blocking(move || {
        let context = running.tree()?;
        anyhow::Ok(run_captured(&request.argv, request.drafts, &context))
    })
    .await;
    match captured {
        // What the command printed can quote the server's own paths, in an error or in its
        // output alike, so both streams go out with those named by their placeholders. The one
        // exception is what a `record show` that succeeded printed: that is a record of the
        // tree, which a cycle writes back whole, so a placeholder in it would change the
        // record. It names nothing the tree does not already hold.
        Ok(Ok(captured)) => {
            note.exit_code(captured.exit_code);
            (
                StatusCode::OK,
                Json(Captured {
                    stdout: if record_text && captured.exit_code == 0 {
                        captured.stdout
                    } else {
                        shared.hide_paths(&captured.stdout)
                    },
                    stderr: shared.hide_paths(&captured.stderr),
                    exit_code: captured.exit_code,
                }),
            )
                .into_response()
        }
        Ok(Err(error)) => error_response(
            shared,
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("{error:#}"),
        ),
        Err(error) => error_response(
            shared,
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("running the command failed: {error}"),
        ),
    }
}

/// What the request log says of a command: its name, the record a `record show`, `record put`
/// or `record rm` names, and the length of what a search looks for, never its words.
fn describe(argv: &[String], parsed: Option<&cli::Cli>, note: &Note) {
    match parsed.map(|parsed| &parsed.command) {
        // An external subcommand's name is whatever the caller wrote.
        Some(cli::Command::External(_)) => {}
        Some(command) => {
            note.command(command.name());
            match command {
                cli::Command::Search { query, .. } => note.query(query),
                cli::Command::Dossier { entity } => note.query(entity),
                cli::Command::Record {
                    command:
                        cli::RecordCommand::Show { path }
                        | cli::RecordCommand::Put {
                            path: Some(path), ..
                        }
                        | cli::RecordCommand::Rm { path },
                } => note.path(path),
                _ => {}
            }
        }
        None => {
            if let Some(name) = refused_name(argv) {
                note.command(name);
            }
        }
    }
}

/// The name of a command clap refused, made of names clap holds and nothing the caller wrote:
/// its first word, when a remote caller may run that command, and the subcommand after it when
/// clap knows one by that word, so `record show x --bogus` is `record show`. No path or query
/// length is read from an argv clap refused.
fn refused_name(argv: &[String]) -> Option<String> {
    let mut words = argv
        .iter()
        .map(String::as_str)
        .filter(|word| global_flag(word).is_none());
    let first = words.next().filter(|word| ALLOWED.contains(word))?;
    let commands = cli::Cli::command();
    let group = commands.find_subcommand(first)?;
    let subcommand = words
        .next()
        .and_then(|word| group.find_subcommand(word))
        .map(|subcommand| format!("{first} {}", subcommand.get_name()));
    Some(subcommand.unwrap_or_else(|| first.to_owned()))
}

/// Whether a parsed command is a `record show`, whose output, when it succeeds, is a record of
/// the tree.
fn shows_a_record(parsed: &cli::Cli) -> bool {
    matches!(
        parsed.command,
        cli::Command::Record {
            command: cli::RecordCommand::Show { .. }
        }
    )
}

/// What `/healthz` answers with.
#[derive(Clone, Debug)]
pub struct Health {
    head: String,
    windows: usize,
    /// Empty when this copy can still take the remote's work and its own writes.
    wedged: String,
}

/// Answers `GET /healthz`.
///
/// A copy whose clone cannot be the start of a write answers 503, not 200: a rebase left in
/// progress, a clone and a remote that have each moved on, or a clone `ctx` made that holds a
/// commit the remote does not have. From those states every write fails or would publish what
/// nobody checked, and the follow loop stops seeing merges, while the process is alive and
/// answering: a probe that only looked at the process would leave a copy serving a tree that
/// is going stale. The check is local — the follow loop is what talks to the remote — so a
/// probe costs no network call.
///
/// How the follow loop is doing is in `following`: `ok`, and while it fails, `since` and
/// `error`. A failing loop keeps the answer 200. It fails when the remote or GitHub cannot be
/// reached or will not mint a token, and a restart fixes neither: a copy that cannot reach
/// the remote at start cannot clone at all, so restarting it would stop it serving the tree it
/// has. The field is what a monitor reads.
///
/// The probe is open to anyone who reaches the port, so what it says about a failure is fixed
/// words. Why the loop or the check fails — a URL, a repository, what GitHub or git answered —
/// goes to the server's log.
///
/// A check that fails answers 503 as well, as an unusable clone does: a clone git cannot open,
/// for any reason but there being no repository at all, is one no write can start from and no
/// merge reaches.
///
/// A probe never waits for the tree lock. A write or a follow pass holds it for as long as
/// git takes, up to the network limit per call, and a probe that waited behind it could time
/// out and restart a copy that was only busy. While the lock is held the probe answers from
/// the last check it made, with `"busy": true`. The server makes the first check before it
/// binds a port, so there is always one to answer from.
pub async fn healthz(State(listener): State<Listener>) -> Response {
    let shared = listener.shared.clone();
    let following = following_json(&shared.following());
    let health = tokio::task::spawn_blocking(move || -> anyhow::Result<(Health, bool)> {
        let Some(context) = shared.try_tree()? else {
            return Ok((shared.last_health(), true));
        };
        let health = check_health(&context)?;
        shared.remember_health(health.clone());
        Ok((health, false))
    })
    .await;
    match health {
        Ok(Ok((health, busy))) if health.wedged.is_empty() => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "busy": busy,
                "head": health.head,
                "windows": health.windows,
                "following": following,
            })),
        )
            .into_response(),
        Ok(Ok((health, busy))) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "busy": busy,
                "head": health.head,
                "windows": health.windows,
                "error": health.wedged,
                "following": following,
            })),
        )
            .into_response(),
        Ok(Err(error)) => {
            eprintln!(
                "the health check failed: {}",
                one_line(&format!("{error:#}"))
            );
            unchecked(&following)
        }
        Err(error) => {
            eprintln!("the health check stopped: {}", one_line(&error.to_string()));
            unchecked(&following)
        }
    }
}

/// What `following.error` says while the follow loop fails.
const FOLLOW_FAILING: &str = "the follow loop is failing; the server's log says why";

/// What `/healthz` says when the check itself failed.
const CHECK_FAILED: &str = "the health check failed; the server's log says why";

/// The answer to a probe whose check failed: 503, as for a clone no write can start from.
fn unchecked(following: &serde_json::Value) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "ok": false, "error": CHECK_FAILED, "following": following })),
    )
        .into_response()
}

/// The follow loop's state, as `/healthz` reports it.
fn following_json(following: &Following) -> serde_json::Value {
    match following.failing_since {
        None => json!({ "ok": true }),
        Some(since) => json!({
            "ok": false,
            "since": since.to_rfc3339(),
            "error": FOLLOW_FAILING,
        }),
    }
}

/// Checks the loaded tree's clone and counts its windows.
pub(super) fn check_health(context: &Context) -> anyhow::Result<Health> {
    let repo = Repo::open_optional(&context.tree.root)?;
    let head = repo
        .as_ref()
        .and_then(|repo| repo.head().ok())
        .unwrap_or_default();
    let wedged = match repo {
        Some(repo) => repo.wedged()?.unwrap_or_default().to_owned(),
        None => String::new(),
    };
    Ok(Health {
        head,
        windows: window_count(context)?,
        wedged,
    })
}

/// Checks a command is one a remote caller may run, before the tree is touched. `parsed` is
/// what clap made of `argv`, when it parses.
///
/// A command that does not parse passes this check: `run_captured` reports the parse error
/// the way the command line does, with the same text and the same exit code.
///
/// The name alone does not decide it. A caller the server admits holds no write lease and
/// owns none of the files a write touches, so only a command [`cli::Command::remote`] calls a
/// read runs here: `delta` reads, `delta --write` replaces the newest window's files and is
/// refused.
fn allowed(argv: &[String], parsed: Option<&cli::Cli>) -> Result<(), String> {
    if argv.is_empty() {
        return Err("argv is empty".to_owned());
    }
    if argv
        .iter()
        .any(|argument| global_flag(argument) == Some(Global::Memory))
    {
        return Err("--memory names the server's own tree and cannot be set remotely".to_owned());
    }
    let first = command_name(argv).ok_or_else(|| "argv names no command".to_owned())?;
    if !ALLOWED.contains(&first) {
        return Err(format!("{first} is not a remote command"));
    }
    let Some(parsed) = parsed else {
        return Ok(());
    };
    match parsed.command.remote() {
        Remote::Forward => Ok(()),
        Remote::Put { .. }
        | Remote::PutBatch { .. }
        | Remote::Rm { .. }
        | Remote::MarkerAdvance { .. } => Err(format!(
            "{} writes, and has its own endpoint: records go to /v1/records/put, \
             /v1/records/put-batch and /v1/records/delete, the marker to /v1/marker/advance",
            parsed.command.name()
        )),
        Remote::Local => Err(format!(
            "`{}` is not a remote command: a remote caller holds no write lease, and this one \
             changes files the fetch job owns or is the server's own work",
            argv.join(" ")
        )),
    }
}

/// The command's first word. A global flag may stand before it — `ctx --json pending` is what
/// a caller writes — so it is the first argument that is not one.
fn command_name(argv: &[String]) -> Option<&str> {
    argv.iter()
        .find(|argument| global_flag(argument).is_none())
        .map(String::as_str)
}

/// The body of every error answer the server gives: `{"error": <message>}`, with the server's
/// own directories named by their placeholders ([`Shared::hide_paths`]). This is where an
/// error leaves the server, so it is the one place they are taken out.
pub(super) fn error_response(shared: &Shared, status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": shared.hide_paths(message) }))).into_response()
}
