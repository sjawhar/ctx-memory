//! What `run-cycle.sh` makes of a session once omp exits: its exit status says the work finished,
//! or one `run-cycle:` line says why it did not.
//!
//! The script runs as its container's root, so this pretend-root harness cannot take it through a
//! session (`filesystem.rs`). It runs instead everything the script runs after its `run_session`
//! function, the post-session code itself, read from the script, with `run_session` standing in
//! for omp: each session it starts leaves the events and exit status a test gives it, where the
//! real one leaves omp's stdout and status. The events are in the shape the pinned omp writes
//! them; `live_session.rs` runs the same reading over that omp's own output.

use std::path::Path;
use std::process::{Command, ExitStatus};

use anyhow::{ensure, Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{remove_scratch, runner, scratch, script_function};
use crate::policy::pass_complete_line;

/// One session the runner starts: omp's exit status and what omp wrote to its stdout.
pub(crate) struct Attempt<'a> {
    pub(crate) status: i32,
    pub(crate) events: &'a str,
}

/// How the runner ended after its sessions.
pub(crate) struct Verdict {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: String,
    /// How many sessions the runner started.
    pub(crate) sessions: usize,
    /// What the retry's transcript directory keeps as `events.jsonl` once the runner is done.
    retry_kept: String,
}

impl Verdict {
    /// The `run-cycle:` lines on standard error that hold `needle`.
    pub(crate) fn lines_saying(&self, needle: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.starts_with("run-cycle: ") && line.contains(needle))
            .collect()
    }
}

/// What the line a session stopped by its time limit says.
pub(crate) const TIME_LIMIT: &str = "time limit";

/// Runs what `run-cycle.sh` runs after its sessions, for an ingest cycle (`pass` is `None`) or a
/// maintenance pass, where the sessions it starts end as `attempts` say, in order, and the marker
/// never moves.
pub(crate) fn runner_verdict(pass: Option<&str>, attempts: &[Attempt<'_>]) -> Result<Verdict> {
    verdict(pass, attempts, &Around::default())
}

/// [`runner_verdict`], where the marker has moved once session `moved_by` (1 for the first) has
/// run, as a pass that ran `ctx marker advance` leaves it.
fn runner_verdict_moving_marker(
    pass: Option<&str>,
    attempts: &[Attempt<'_>],
    moved_by: usize,
) -> Result<Verdict> {
    verdict(
        pass,
        attempts,
        &Around {
            marker_moves: &[(moved_by, FIRST_PENDING)],
            ..Around::default()
        },
    )
}

/// The window the marker holds before the runner's sessions start.
const MARKER_BEFORE: &str = "20261101T000000Z-0000";

/// The one window the runner's own `ctx pending` read lists when a test names none, with one new
/// message.
const FIRST_PENDING: &str = "20261101T010000Z-0001";

/// What a test sets up around the runner's sessions. The default: the marker holds
/// [`MARKER_BEFORE`] throughout, every `ctx pending` read lists [`FIRST_PENDING`], and nothing is
/// beside the retry's transcript directory.
#[derive(Default)]
struct Around<'a> {
    /// Where the marker is once a session has run: `(session, window)`, 1 for the first session,
    /// as a session that ran `ctx marker advance` leaves it, and 0 for where it is before the
    /// first session. A later entry wins, and an empty window is no marker at all, as on a tree
    /// no cycle has handled.
    marker_moves: &'a [(usize, &'a str)],
    /// What `ctx pending --json` answers: the first entry before the first session (the read the
    /// runner hands the session), the second once one session has run (the retry's read). A read
    /// past the last entry answers the first, and an empty entry is a read that fails.
    pending: &'a [&'a str],
    /// Every `ctx marker show` fails, with status 3, once a session has run.
    marker_read_fails: bool,
    /// Every `ctx marker show` answers, with status 0, text that is not JSON once a session has
    /// run.
    marker_unreadable: bool,
    /// A capture beside the retry's transcript directory when the runner starts, as an earlier
    /// attempt of the same execution whose capture no seal could move leaves it.
    old_retry_capture: Option<&'a str>,
}

/// Runs the post-session code with what `around` sets up.
fn verdict(pass: Option<&str>, attempts: &[Attempt<'_>], around: &Around<'_>) -> Result<Verdict> {
    let script = runner("run-cycle.sh")?;
    let lines: Vec<&str> = script.lines().collect();
    let launch = lines
        .iter()
        .position(|line| line.starts_with("run_session() {"))
        .context("run-cycle.sh has no run_session function")?;
    let launch_end = lines
        .get(launch..)
        .unwrap_or_default()
        .iter()
        .position(|line| *line == "}")
        .map(|offset| launch + offset)
        .context("run-cycle.sh's run_session function does not end")?;
    let after_launch = lines.get(launch_end + 1..).unwrap_or_default().join("\n");
    let minutes = lines
        .iter()
        .find_map(|line| line.strip_prefix("SESSION_MINUTES="))
        .context("run-cycle.sh sets no SESSION_MINUTES")?;

    let dir = scratch("verdict")?;
    let outcome = run_after_launch(&dir, pass, minutes, &after_launch, attempts, around);
    remove_scratch(&dir)?;
    outcome
}

/// A `ctx pending --json` answer listing `windows`, each `(window id, new messages)`, in the
/// shape the CLI prints it.
fn pending_answer(windows: &[(&str, usize)]) -> String {
    let windows: Vec<Value> = windows
        .iter()
        .map(|(window, new_messages)| {
            json!({
                "window": window, "dir": format!("windows/{window}"),
                "window_end": "2026-11-01T01:00:00Z", "status": "ok",
                "new_messages": new_messages, "listed_messages": new_messages
            })
        })
        .collect();
    json!({
        "version": 1, "command": "pending",
        "data": {"marker": {"version": 1, "window": MARKER_BEFORE}, "windows": windows, "later_windows": 0}
    })
    .to_string()
}

/// Leaves in `dir` what the sessions in `attempts` end with and what `around` sets up, where the
/// stand-ins of [`run_after_launch`] read them.
fn stage(dir: &Path, attempts: &[Attempt<'_>], around: &Around<'_>) -> Result<()> {
    if let Some(capture) = around.old_retry_capture {
        let sessions = dir.join("data/agent-sessions");
        std::fs::create_dir_all(sessions.join("ctx-agent-test-1-retry"))?;
        std::fs::write(sessions.join("ctx-agent-test-1-retry.events.json"), capture)?;
    }
    for (index, attempt) in attempts.iter().enumerate() {
        std::fs::write(dir.join(format!("events-{}", index + 1)), attempt.events)?;
        std::fs::write(
            dir.join(format!("status-{}", index + 1)),
            attempt.status.to_string(),
        )?;
    }
    for (session, window) in around.marker_moves {
        std::fs::write(dir.join(format!("marker-{session}")), window)?;
    }
    if around.marker_read_fails {
        std::fs::write(dir.join("marker-fails"), "")?;
    }
    if around.marker_unreadable {
        std::fs::write(dir.join("marker-unreadable"), "")?;
    }
    let first_pending = pending_answer(&[(FIRST_PENDING, 1)]);
    let reads = if around.pending.is_empty() {
        &[first_pending.as_str()][..]
    } else {
        around.pending
    };
    for (index, answer) in reads.iter().enumerate() {
        std::fs::write(dir.join(format!("pending-{index}")), answer)?;
    }
    Ok(())
}

fn run_after_launch(
    dir: &Path,
    pass: Option<&str>,
    minutes: &str,
    after_launch: &str,
    attempts: &[Attempt<'_>],
    around: &Around<'_>,
) -> Result<Verdict> {
    stage(dir, attempts, around)?;
    // What the script holds when its sessions start, and stand-ins for what reaches past this
    // machine: ctx-agent's checks pass, chown changes nothing, `ctx pending` answers what
    // pending-<sessions run so far> holds (pending-0 when there is no such file; an empty file
    // fails the read), and `ctx marker show` answers MARKER_BEFORE, or what marker-0 names, until
    // a session whose marker-<session> names the window it moved the marker to has run (an empty
    // name is no marker), fails once a session has run when marker-fails is there, and answers
    // text that is not JSON once a session has run when marker-unreadable is there. A session
    // leaves its events in the capture beside its transcript directory, where the real
    // run_session's omp writes them. `timeout` runs the command it is handed, so a context read
    // reaches the `ctx` stand-in and not a real one.
    let prelude = format!(
        r#"set -eu
{refuse}
{complete}
{seal}
{write_context}
{prepare_context}
{shell_rule}
{cycle_prompt}
SESSION_MINUTES={minutes}
AGENT_USER=ctx-agent
pass='{pass}'
windows=1
CTX_DATA_DIR="$VERDICT_DIR/data"
CLOUD_RUN_EXECUTION=ctx-agent-test-1
session_dir="$CTX_DATA_DIR/agent-sessions/$CLOUD_RUN_EXECUTION"
mkdir -p "$session_dir"
nonce=0123456789abcdef0123456789abcdef
context_prompt=
CONTEXT_BYTES=49152
context_seconds_left=60
context_subjects_left=30
pending="$(cat "$VERDICT_DIR/pending-0")"
as_agent_user() {{ case "$*" in *context.json*) return 1 ;; esac; }}
chown() {{ :; }}
timeout() {{ shift; "$@"; }}
ctx() {{
	ran="$(cat "$VERDICT_DIR/sessions" 2>/dev/null || echo 0)"
	case "$*" in
	*pending*)
		answer="$VERDICT_DIR/pending-$ran"
		[ -f "$answer" ] || answer="$VERDICT_DIR/pending-0"
		if [ ! -s "$answer" ]; then
			printf '%s\n' '{{"error":"the server could not be reached"}}'
			return 1
		fi
		cat "$answer"
		return
		;;
	esac
	if [ "$ran" -gt 0 ] && [ -f "$VERDICT_DIR/marker-fails" ]; then
		printf '%s\n' '{{"error":"the server could not be reached"}}'
		return 3
	fi
	if [ "$ran" -gt 0 ] && [ -f "$VERDICT_DIR/marker-unreadable" ]; then echo 'Bad Gateway'; return 0; fi
	marker={MARKER_BEFORE}
	session=0
	while [ "$session" -le "$ran" ]; do
		[ ! -f "$VERDICT_DIR/marker-$session" ] || marker="$(cat "$VERDICT_DIR/marker-$session")"
		session=$((session + 1))
	done
	if [ -z "$marker" ]; then
		printf '%s\n' '{{"version":1,"command":"marker show","data":{{"marker":null}}}}'
	else
		printf '{{"version":1,"command":"marker show","data":{{"marker":{{"version":1,"window":"%s"}}}}}}\n' "$marker"
	fi
}}
run_session() {{
	attempt=$(($(cat "$VERDICT_DIR/sessions" 2>/dev/null || echo 0) + 1))
	echo "$attempt" >"$VERDICT_DIR/sessions"
	if [ ! -f "$VERDICT_DIR/events-$attempt" ]; then
		echo "the runner started session $attempt, which this test did not expect" >&2
		return 99
	fi
	cp "$VERDICT_DIR/events-$attempt" "$session_dir.events.json"
	return "$(cat "$VERDICT_DIR/status-$attempt")"
}}
"#,
        refuse = script_function("refuse")?,
        complete = script_function("pass_complete_line")?,
        seal = script_function("seal_session")?,
        write_context = script_function("write_context")?,
        prepare_context = script_function("prepare_context")?,
        shell_rule = script_function("shell_rule")?,
        cycle_prompt = script_function("cycle_prompt")?,
        pass = pass.unwrap_or_default(),
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("{prelude}\n{after_launch}\n"))
        .env("VERDICT_DIR", dir)
        .output()
        .context("running run-cycle.sh's post-session code")?;
    let sessions = std::fs::read_to_string(dir.join("sessions"))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(Verdict {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        sessions,
        retry_kept: std::fs::read_to_string(
            dir.join("data/agent-sessions/ctx-agent-test-1-retry/events.jsonl"),
        )
        .unwrap_or_default(),
    })
}

/// omp's stdout for a session whose messages are `messages`, one event per line, as the pinned
/// release writes it: its header, a `message_start` and a `message_end` for each message, and
/// `agent_end`. omp starts each assistant message with an empty body and a placeholder
/// `stopReason` of "stop", so only its `message_end` says how that turn ended. An entry that is
/// an event of its own, with a `type` (a tool's `tool_execution_start` or `tool_execution_end`),
/// is written as it is, where it stands, and is not one of the messages.
fn events(messages: &[Value]) -> String {
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": "test-session"}),
        json!({"type": "agent_start"}),
    ];
    let mut conversation = Vec::new();
    for message in messages {
        if message.get("type").is_some() {
            lines.push(message.clone());
            continue;
        }
        conversation.push(message);
        let mut start = message.clone();
        if let Some(fields) = start
            .as_object_mut()
            .filter(|fields| fields.get("role").and_then(Value::as_str) == Some("assistant"))
        {
            fields.insert("content".to_owned(), json!([]));
            fields.insert("stopReason".to_owned(), json!("stop"));
            fields.remove("errorMessage");
            fields.remove("stopDetails");
        }
        lines.push(json!({"type": "message_start", "message": start}));
        lines.push(json!({"type": "message_end", "message": message}));
    }
    lines.push(json!({
        "type": "agent_end", "messages": conversation, "isTerminal": true, "yielded": true
    }));
    let mut stdout = String::new();
    for line in lines {
        stdout.push_str(&line.to_string());
        stdout.push('\n');
    }
    stdout
}

fn prompt() -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": "Run one ingest cycle."}]})
}

/// A model turn that asks for `ctx rules`.
fn tool_call(id: &str) -> Value {
    json!({
        "role": "assistant",
        "content": [{"type": "toolCall", "id": id, "name": "bash", "arguments": {"command": "ctx rules"}}],
        "stopReason": "toolUse"
    })
}

fn tool_result(id: &str, text: &str, error: bool) -> Value {
    json!({
        "role": "toolResult", "toolCallId": id, "toolName": "bash",
        "content": [{"type": "text", "text": text}], "isError": error
    })
}

/// A model turn that ends the session with `text` as its final answer.
pub(crate) fn answer(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"})
}

/// The turn omp writes when its time limit passes while the model is still answering.
fn aborted_at_the_time_limit() -> Value {
    json!({
        "role": "assistant", "content": [], "stopReason": "aborted",
        "errorMessage": "Deadline exceeded"
    })
}

/// A model refusal, as the pinned omp writes one.
fn refusal() -> Value {
    json!({
        "role": "assistant", "content": [], "stopReason": "error",
        "errorMessage": "The model refused this request.",
        "stopDetails": {
            "category": "reasoning_extraction", "explanation": "blocked (see terms) }",
            "type": "refusal"
        }
    })
}

/// A session that finished: it ran a tool and gave its final answer.
fn finished(text: &str) -> String {
    events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result("toolu_0", "Tree rules", false),
        answer(text),
    ])
}

/// A session the time limit stopped while its tool ran: omp aborts the tool, pairs the call with
/// that result and ends the session, and its model never gives a final answer.
fn stopped_after_a_tool_call() -> String {
    events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result("toolu_0", "Command aborted", true),
    ])
}

fn stopped_while_the_model_answers() -> String {
    events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result("toolu_0", "Tree rules", false),
        aborted_at_the_time_limit(),
    ])
}

fn refused() -> String {
    events(&[prompt(), refusal()])
}

/// A session that called the record tools as `calls` say, each `(tool, accepted)`, and then
/// ended on `last`: for each call, the model's turn that asks for it, omp's
/// `tool_execution_start` and `tool_execution_end` events, and the call's result, in the shape
/// the pinned omp writes them (`live_session.rs` reads a live session's). A write the server
/// refused fails with its findings, as runner/ctx-record.ts reports one.
pub(crate) fn wrote(calls: &[(&str, bool)], last: Value) -> String {
    let mut messages = vec![prompt()];
    for (index, &(tool, accepted)) in calls.iter().enumerate() {
        let id = format!("toolu_{index}");
        let arguments = if tool == "ctx_record_rm" {
            json!({"path": "decisions/example.md"})
        } else {
            json!({"records": [{"path": "decisions/example.md", "content": "---\ntype: decision\n---\n"}]})
        };
        let (text, details) = if accepted {
            ("stored", json!({"exitCode": 0}))
        } else {
            (
                "refused decisions/example.md\n  quote — the quote is not in its source",
                json!({}),
            )
        };
        let content = json!([{"type": "text", "text": text}]);
        messages.extend([
            json!({
                "role": "assistant", "stopReason": "toolUse",
                "content": [{"type": "toolCall", "id": id, "name": tool, "arguments": arguments}]
            }),
            json!({"type": "tool_execution_start", "toolCallId": id, "toolName": tool, "args": arguments}),
            json!({
                "type": "tool_execution_end", "toolCallId": id, "toolName": tool,
                "result": {"content": content, "details": details}, "isError": !accepted
            }),
            json!({
                "role": "toolResult", "toolCallId": id, "toolName": tool,
                "content": content, "isError": !accepted
            }),
        ]);
    }
    messages.push(last);
    events(&messages)
}

/// A maintenance pass's final report, ending with the line `ending` (none when `None`).
fn report(ending: Option<&str>) -> String {
    let body = "Checked 3 open records; changed 1.\n\n- decisions/a.md: [] -> [Ada], settled by slack:C0/1.1\n";
    match ending {
        Some(line) => format!("{body}\n{line}\n"),
        None => body.to_owned(),
    }
}

fn ensure_succeeded(verdict: &Verdict, what: &str) -> Result<()> {
    ensure!(
        verdict.status.success(),
        "{what}, yet the runner exited with {}: {}",
        verdict.status,
        verdict.stderr
    );
    ensure!(
        verdict.lines_saying(TIME_LIMIT).is_empty(),
        "{what}, yet the runner says it stopped at its time limit: {}",
        verdict.stderr
    );
    Ok(())
}

/// The runner failed, with exactly one `run-cycle:` line on standard error holding `reason`.
fn ensure_failed_saying(verdict: &Verdict, reason: &str, what: &str) -> Result<()> {
    ensure!(
        !verdict.status.success(),
        "{what}, yet the runner exited 0: {}",
        verdict.stderr
    );
    let lines = verdict.lines_saying(reason);
    ensure!(
        lines.len() == 1,
        "{what}, and the runner should say so in one line holding {reason:?}; it wrote: {}",
        verdict.stderr
    );
    Ok(())
}

/// The pinned omp exits 0 when its time limit stops a session after a tool call
/// (`agent-loop.ts:1762-1765`, `print-mode.ts:313-318,394`): the last turn asked for a tool and
/// the session ends without the model's final answer. The runner reads that from omp's events
/// and fails the cycle, with one line naming the time limit, rather than report a cut-short cycle
/// as done. A provider that ends a turn that calls a tool with stop reason "stop" ends the same
/// way: omp runs that turn's tools too, so its last message still asks for one.
#[test]
fn a_cycle_its_time_limit_stopped_after_a_tool_call_fails() -> Result<()> {
    let mut call_on_stop = tool_call("toolu_0");
    if let Some(turn) = call_on_stop.as_object_mut() {
        turn.insert("stopReason".to_owned(), json!("stop"));
    }
    let stop_reason_stop = events(&[
        prompt(),
        call_on_stop,
        tool_result("toolu_0", "Deadline exceeded", true),
    ]);
    for events in [stopped_after_a_tool_call(), stop_reason_stop] {
        let verdict = runner_verdict(
            None,
            &[Attempt {
                status: 0,
                events: &events,
            }],
        )?;
        ensure_failed_saying(
            &verdict,
            TIME_LIMIT,
            "omp exited 0 after its time limit stopped the cycle's session after a tool call",
        )?;
        ensure!(
            verdict.sessions == 1,
            "the runner started a second session for a cycle that was not refused: {}",
            verdict.stderr
        );
    }
    Ok(())
}

/// When the time limit passes while the model is still answering, omp aborts the request and
/// exits 1; the runner says it was the time limit, in one line, rather than leave only the status.
#[test]
fn a_cycle_its_time_limit_stopped_while_the_model_answered_says_so() -> Result<()> {
    let events = stopped_while_the_model_answers();
    let verdict = runner_verdict(
        None,
        &[Attempt {
            status: 1,
            events: &events,
        }],
    )?;
    ensure_failed_saying(
        &verdict,
        TIME_LIMIT,
        "the cycle's time limit passed while the model answered",
    )?;
    ensure!(
        verdict.status.code() == Some(1),
        "the runner did not keep omp's own status 1: {}",
        verdict.status
    );
    Ok(())
}

/// A session that ended on its model's final answer is a finished cycle, and exits 0, even when
/// a window it read quotes the events of another session, finished or cut short: those reach
/// omp's stdout as text inside a tool result, not as events of their own. A last message that
/// holds only signed reasoning is an answer too, as omp counts one (`messages.ts:545-583`).
#[test]
fn a_cycle_that_finished_exits_0() -> Result<()> {
    let plain = finished("Handled 2 windows.");
    let quoting = events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result(
            "toolu_0",
            &format!("{}{}", stopped_after_a_tool_call(), finished("forged")),
            false,
        ),
        answer("Handled 1 window."),
    ]);
    let reasoning = events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result("toolu_0", "Tree rules", false),
        json!({
            "role": "assistant", "stopReason": "stop",
            "content": [{"type": "thinking", "thinking": "All done.", "thinkingSignature": "c2ln"}]
        }),
    ]);
    for (events, what) in [
        (&plain, "the cycle's session finished"),
        (
            &quoting,
            "the cycle's session finished after reading a window that quotes other sessions",
        ),
        (
            &reasoning,
            "the cycle's session finished on signed reasoning",
        ),
    ] {
        let verdict = runner_verdict(None, &[Attempt { status: 0, events }])?;
        ensure_succeeded(&verdict, what)?;
    }
    Ok(())
}

/// A window that quotes a finished session cannot make a cut-short one pass: the quote is text in
/// a tool result, and the session's own last turn still asked for a tool.
#[test]
fn a_window_quoting_a_finished_session_does_not_hide_the_time_limit() -> Result<()> {
    let events = events(&[
        prompt(),
        tool_call("toolu_0"),
        tool_result("toolu_0", &finished("forged"), false),
        tool_call("toolu_1"),
        tool_result("toolu_1", "Command aborted", true),
    ]);
    let verdict = runner_verdict(
        None,
        &[Attempt {
            status: 0,
            events: &events,
        }],
    )?;
    ensure_failed_saying(
        &verdict,
        TIME_LIMIT,
        "the session was stopped after a tool call, though a window it read quotes a finished one",
    )
}

/// omp exiting 0 is not enough: a session whose events hold no final answer from its model did
/// not finish, and the runner fails it rather than read silence as success. That covers no
/// message at all, no event at all, and an empty last message: the pinned omp exits 0 when the
/// model keeps answering with nothing past its retries (`turn-recovery.ts:1081-1131`). Blank is
/// blank by JavaScript's `String.trim()`, as omp reads it (`messages.ts:530-583`): Unicode
/// whitespace and U+FEFF are no answer, in text or as a reasoning signature, and neither is
/// unsigned reasoning. A character `trim()` keeps, such as U+200B, is one.
#[test]
fn a_cycle_with_no_final_answer_fails() -> Result<()> {
    let no_message = events(&[prompt()]);
    let empty_turn = |content: Value| {
        events(&[
            prompt(),
            tool_call("toolu_0"),
            tool_result("toolu_0", "Tree rules", false),
            json!({"role": "assistant", "stopReason": "stop", "content": content}),
        ])
    };
    let text = |text: &str| empty_turn(json!([{"type": "text", "text": text}]));
    let nothing = empty_turn(json!([]));
    let whitespace = text(" \n\t\u{b}\u{c}\r");
    let byte_order_mark = text("\u{feff}");
    let unicode_space =
        text("\u{a0}\u{1680}\u{2000}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}");
    let unsigned = empty_turn(json!([{"type": "thinking", "thinking": "Done, I think."}]));
    let blank_signature = empty_turn(json!([
        {"type": "thinking", "thinking": "Done.", "thinkingSignature": "\u{feff} \u{a0}"}
    ]));
    for (events, what) in [
        (no_message.as_str(), "the model sent no message"),
        ("", "omp wrote no event"),
        (nothing.as_str(), "the model's last message is empty"),
        (
            whitespace.as_str(),
            "the model's last message is whitespace",
        ),
        (
            byte_order_mark.as_str(),
            "the model's last message is U+FEFF",
        ),
        (
            unicode_space.as_str(),
            "the model's last message is Unicode whitespace",
        ),
        (
            unsigned.as_str(),
            "the model's last message is unsigned reasoning",
        ),
        (
            blank_signature.as_str(),
            "the model's last message is reasoning with a blank signature",
        ),
    ] {
        let verdict = runner_verdict(None, &[Attempt { status: 0, events }])?;
        ensure_failed_saying(
            &verdict,
            "did not finish",
            &format!("omp exited 0 although {what}"),
        )?;
    }
    let zero_width = text("\u{200b}");
    let verdict = runner_verdict(
        None,
        &[Attempt {
            status: 0,
            events: &zero_width,
        }],
    )?;
    ensure_succeeded(
        &verdict,
        "the model's last message is U+200B, which String.trim() keeps",
    )
}

/// A maintenance pass is done when its last message is its report and the report ends with the
/// pass's completion line, the line its prompt tells it to end with; a report with no such line,
/// or with more after it, is a pass that did not get to its end. Markdown emphasis around the
/// line is still the line.
#[test]
fn a_maintenance_pass_must_end_with_its_completion_report() -> Result<()> {
    let complete = pass_complete_line("waiting-on")?;
    let done = finished(&report(Some(&complete)));
    let emphasized = finished(&report(Some(&format!("**{complete}**"))));
    let verdict = runner_verdict(
        Some("waiting-on"),
        &[Attempt {
            status: 0,
            events: &done,
        }],
    )?;
    ensure_succeeded(&verdict, "the pass ended with its completion report")?;
    let verdict = runner_verdict(
        Some("waiting-on"),
        &[Attempt {
            status: 0,
            events: &emphasized,
        }],
    )?;
    ensure_succeeded(&verdict, "the pass ended with its completion line in bold")?;

    let no_line = finished(&report(None));
    let more_after = finished(&format!(
        "{}\nNext I will run ctx sweep.",
        report(Some(&complete))
    ));
    for (events, what) in [
        (&no_line, "the pass's report has no completion line"),
        (
            &more_after,
            "the pass's report goes on past its completion line",
        ),
    ] {
        let verdict = runner_verdict(Some("waiting-on"), &[Attempt { status: 0, events }])?;
        ensure_failed_saying(&verdict, "completion report", what)?;
    }
    Ok(())
}

/// The time limit fails a maintenance pass as it fails a cycle, with the same one line, and a pass
/// it stopped gets no retry, because it was not refused.
#[test]
fn a_maintenance_pass_its_time_limit_stopped_fails() -> Result<()> {
    let after_tool = stopped_after_a_tool_call();
    let verdict = runner_verdict(
        Some("waiting-on"),
        &[Attempt {
            status: 0,
            events: &after_tool,
        }],
    )?;
    ensure_failed_saying(
        &verdict,
        TIME_LIMIT,
        "omp exited 0 after its time limit stopped the pass after a tool call",
    )?;
    ensure!(
        verdict.sessions == 1,
        "the runner retried a pass that was stopped by its time limit, not refused: {}",
        verdict.stderr
    );
    Ok(())
}

/// A refused maintenance pass gets one fresh session, as a refused cycle does: a pass moves no
/// marker, so running it again is safe. The fresh session is judged like the first, and the
/// marker is read after it too: a pass that ends with its report succeeds; a second refusal, a
/// report-less end or a moved marker fails the job. A pass whose first session moved the marker
/// gets no fresh session.
#[test]
fn a_refused_maintenance_pass_gets_one_fresh_session_judged_like_the_first() -> Result<()> {
    let pass = Some("waiting-on");
    let refused = refused();
    let done = finished(&report(Some(&pass_complete_line("waiting-on")?)));
    let no_report = finished(&report(None));
    let first_refused_then = |events: &str| -> Result<Verdict> {
        runner_verdict(
            pass,
            &[
                Attempt {
                    status: 1,
                    events: &refused,
                },
                Attempt { status: 0, events },
            ],
        )
    };

    let verdict = first_refused_then(&done)?;
    ensure!(
        verdict.sessions == 2,
        "a refused pass did not get its fresh session ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    ensure_succeeded(
        &verdict,
        "the refused pass's fresh session ended with its report",
    )?;

    let verdict = runner_verdict(
        pass,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 1,
                events: &refused,
            },
        ],
    )?;
    ensure!(
        verdict.sessions == 2 && verdict.status.code() == Some(1),
        "a pass refused twice did not fail with status 1 after two sessions ({} sessions, {}): {}",
        verdict.sessions,
        verdict.status,
        verdict.stderr
    );

    let verdict = first_refused_then(&no_report)?;
    ensure!(
        verdict.sessions == 2,
        "a refused pass did not get its fresh session ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    ensure_failed_saying(
        &verdict,
        "completion report",
        "the refused pass's fresh session ended without its report",
    )?;

    let attempts = [
        Attempt {
            status: 1,
            events: &refused,
        },
        Attempt {
            status: 0,
            events: &done,
        },
    ];
    let verdict = runner_verdict_moving_marker(pass, &attempts, 2)?;
    ensure!(
        verdict.sessions == 2,
        "a refused pass did not get its fresh session ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    ensure_failed_saying(
        &verdict,
        "moved the marker",
        "the refused pass's fresh session moved the marker",
    )?;

    let verdict = runner_verdict_moving_marker(pass, &attempts, 1)?;
    ensure!(
        verdict.sessions == 1,
        "the runner gave a fresh session to a refused pass that had moved the marker: {}",
        verdict.stderr
    );
    ensure_failed_saying(
        &verdict,
        "moved the marker",
        "the refused pass's first session moved the marker",
    )
}

/// A refused cycle still gets its one fresh session, and only a refused one: the retry that
/// finishes makes the cycle a success; a retry stopped by the time limit fails it with the time
/// limit line; a second refusal is the cycle's outcome; and a first session the time limit
/// stopped is not retried, because it was not refused.
#[test]
fn a_refused_cycle_gets_one_fresh_session_judged_like_the_first() -> Result<()> {
    let refused = refused();
    let finished = finished("Handled 2 windows.");
    let after_tool = stopped_after_a_tool_call();

    let verdict = runner_verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 0,
                events: &finished,
            },
        ],
    )?;
    ensure!(
        verdict.sessions == 2,
        "a refused cycle did not get its fresh session ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    ensure_succeeded(&verdict, "the refused cycle's fresh session finished")?;

    let verdict = runner_verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 0,
                events: &after_tool,
            },
        ],
    )?;
    ensure!(
        verdict.sessions == 2,
        "a refused cycle did not get its fresh session ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    ensure_failed_saying(
        &verdict,
        TIME_LIMIT,
        "the refused cycle's fresh session was stopped by its time limit after a tool call",
    )?;

    let verdict = runner_verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 1,
                events: &refused,
            },
        ],
    )?;
    ensure!(
        verdict.sessions == 2 && verdict.status.code() == Some(1),
        "a cycle refused twice did not fail with status 1 after two sessions ({} sessions, {}): {}",
        verdict.sessions,
        verdict.status,
        verdict.stderr
    );

    let verdict = runner_verdict(
        None,
        &[Attempt {
            status: 0,
            events: &after_tool,
        }],
    )?;
    ensure!(
        verdict.sessions == 1,
        "the runner retried a session that was stopped by its time limit, not refused: {}",
        verdict.stderr
    );
    Ok(())
}

/// A capture an earlier attempt of the same execution left beside the retry's transcript
/// directory, which no seal could move, is moved into that directory before the retry's session
/// writes its own capture there, so the retry never overwrites it.
#[test]
fn a_retry_keeps_the_capture_an_earlier_attempt_left_beside_its_directory() -> Result<()> {
    let refused = refused();
    let finished = finished("Handled 2 windows.");
    let old = "{\"type\":\"agent_end\",\"note\":\"the earlier attempt's events\"}\n";
    let verdict = verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 0,
                events: &finished,
            },
        ],
        &Around {
            old_retry_capture: Some(old),
            ..Around::default()
        },
    )?;
    ensure_succeeded(&verdict, "the refused cycle's fresh session finished")?;
    ensure!(
        verdict.sessions == 2 && verdict.retry_kept == old,
        "the retry did not keep the earlier attempt's capture in its directory (kept {:?}): {}",
        verdict.retry_kept,
        verdict.stderr
    );
    Ok(())
}

/// What the line a cycle that skipped windows fails with says.
const SKIPPED: &str = "wrote no record";

/// Three windows after [`MARKER_BEFORE`], oldest first.
const FIRST: &str = "20261102T080000Z-0001";
const SECOND: &str = "20261102T090000Z-0002";
const THIRD: &str = "20261102T100000Z-0003";

/// How many record writes the server accepted in the session whose omp stdout is `events`, as
/// `run-cycle.sh` counts them: its `record_writes` function, run by `sh` over the capture.
pub(crate) fn record_writes(events: &str) -> Result<usize> {
    let dir = scratch("record-writes")?;
    let capture = dir.join("events.json");
    std::fs::write(&capture, events)?;
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "set -eu\n{}\nrecord_writes \"$1\"",
            script_function("record_writes")?
        ))
        .arg("sh")
        .arg(&capture)
        .output();
    remove_scratch(&dir)?;
    let output = output.context("running run-cycle.sh's record_writes")?;
    ensure!(
        output.status.success(),
        "record_writes failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)?
        .trim()
        .parse()
        .context("record_writes printed no number")
}

/// A session that moves the marker past a window with new messages, and writes no record in the
/// whole cycle, has skipped that window for good: no later cycle reads it again. A session did
/// exactly that on a real sample, moving the marker past five windows that held new messages
/// with no record written, and still ended on its final answer, so omp and the runner reported a
/// success. The runner reads the marker once the cycle is over and fails it, in one line that
/// names each window the marker passed that held new messages: not an empty one it passed, nor
/// one still pending. A session that also failed otherwise, at its time limit, still has them
/// named, since the marker it moved stands.
#[test]
fn a_cycle_that_moved_the_marker_past_new_messages_without_a_record_fails() -> Result<()> {
    let pending = pending_answer(&[(FIRST, 4), (SECOND, 0), (THIRD, 2)]);
    let around = Around {
        marker_moves: &[(1, SECOND)],
        pending: &[&pending],
        ..Around::default()
    };
    let finished = finished("Handled 2 windows.");
    let skipped = verdict(
        None,
        &[Attempt {
            status: 0,
            events: &finished,
        }],
        &around,
    )?;
    ensure_failed_saying(
        &skipped,
        SKIPPED,
        "the session moved the marker past a window with new messages and wrote no record",
    )?;
    let line = skipped.lines_saying(SKIPPED).concat();
    ensure!(
        line.contains(FIRST) && !line.contains(SECOND) && !line.contains(THIRD),
        "the line does not name exactly the window with new messages the marker passed: {line}"
    );

    let cut_short = stopped_after_a_tool_call();
    let stopped = verdict(
        None,
        &[Attempt {
            status: 0,
            events: &cut_short,
        }],
        &around,
    )?;
    ensure_failed_saying(&stopped, TIME_LIMIT, "the time limit stopped the session")?;
    ensure_failed_saying(
        &stopped,
        SKIPPED,
        "the time limit stopped a session that had moved the marker past new messages unwritten",
    )
}

/// A record the cycle wrote, put or removed, is the session working its windows, so the marker
/// may pass windows with new messages; so may a session that wrote nothing when each window it
/// passed was empty, or that moved no marker. A write the server refused wrote nothing.
#[test]
fn only_a_cycle_that_wrote_nothing_past_new_messages_fails_for_skipping() -> Result<()> {
    let held = pending_answer(&[(FIRST, 4), (SECOND, 0)]);
    let empty_first = pending_answer(&[(FIRST, 0), (SECOND, 3)]);
    let put = wrote(&[("ctx_record_put", true)], answer("Handled 2 windows."));
    let removed = wrote(
        &[("ctx_record_put", false), ("ctx_record_rm", true)],
        answer("Handled 2 windows."),
    );
    let refused_write = wrote(&[("ctx_record_put", false)], answer("Handled 2 windows."));
    let nothing = finished("Handled the windows.");
    for (events, pending, moves, skipped, what) in [
        (
            &put,
            &held,
            &[(1, SECOND)][..],
            false,
            "the session wrote a record",
        ),
        (
            &removed,
            &held,
            &[(1, SECOND)][..],
            false,
            "the session removed a record after a refused write",
        ),
        (
            &nothing,
            &empty_first,
            &[(1, FIRST)][..],
            false,
            "the marker passed only an empty window",
        ),
        (
            &nothing,
            &held,
            &[][..],
            false,
            "the session moved no marker",
        ),
        (
            &refused_write,
            &held,
            &[(1, SECOND)][..],
            true,
            "the session's only write was refused",
        ),
    ] {
        let verdict = verdict(
            None,
            &[Attempt { status: 0, events }],
            &Around {
                marker_moves: moves,
                pending: &[pending],
                ..Around::default()
            },
        )?;
        if skipped {
            ensure_failed_saying(&verdict, SKIPPED, what)?;
        } else {
            ensure_succeeded(&verdict, what)?;
        }
    }
    Ok(())
}

/// A refused cycle's two sessions are one cycle: the windows the marker passed are those either
/// session's `ctx pending` read listed, and a record either session wrote counts for the cycle.
#[test]
fn a_refused_cycle_is_judged_for_skipped_windows_across_both_sessions() -> Result<()> {
    let first_read = pending_answer(&[(FIRST, 4), (SECOND, 0)]);
    let retry_read = pending_answer(&[(SECOND, 0), (THIRD, 2)]);
    let around = Around {
        marker_moves: &[(1, FIRST), (2, THIRD)],
        pending: &[&first_read, &retry_read],
        ..Around::default()
    };
    let finished = finished("Handled 2 windows.");
    let refused = refused();
    let unwritten = verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 0,
                events: &finished,
            },
        ],
        &around,
    )?;
    ensure!(
        unwritten.sessions == 2,
        "a refused cycle did not get its fresh session: {}",
        unwritten.stderr
    );
    ensure_failed_saying(
        &unwritten,
        SKIPPED,
        "neither session wrote a record, and the marker passed windows with new messages",
    )?;
    let line = unwritten.lines_saying(SKIPPED).concat();
    ensure!(
        line.contains(FIRST) && line.contains(THIRD) && !line.contains(SECOND),
        "the line does not name each window with new messages either session passed: {line}"
    );

    let wrote_then_refused = wrote(&[("ctx_record_put", true)], refusal());
    let written = verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &wrote_then_refused,
            },
            Attempt {
                status: 0,
                events: &finished,
            },
        ],
        &around,
    )?;
    ensure!(
        written.sessions == 2,
        "a refused cycle did not get its fresh session: {}",
        written.stderr
    );
    ensure_succeeded(
        &written,
        "the refused session wrote a record before the fresh one finished",
    )
}

/// A refused cycle whose retry cannot read what is pending still has the first session's read:
/// the retry goes on without context, its failed answer adds no window, and a window with new
/// messages that the first session's read listed and the marker passed, with nothing written, is
/// still named.
#[test]
fn a_refused_cycle_whose_retry_cannot_read_pending_is_judged_by_the_first_read() -> Result<()> {
    let first_read = pending_answer(&[(FIRST, 4), (SECOND, 0)]);
    let refused = refused();
    let finished = finished("Handled 1 window.");
    let verdict = verdict(
        None,
        &[
            Attempt {
                status: 1,
                events: &refused,
            },
            Attempt {
                status: 0,
                events: &finished,
            },
        ],
        &Around {
            marker_moves: &[(1, FIRST), (2, SECOND)],
            pending: &[&first_read, ""],
            ..Around::default()
        },
    )?;
    ensure!(
        verdict.sessions == 2
            && verdict
                .lines_saying("ctx pending failed before the retry")
                .len()
                == 1,
        "the retry did not go on without context after its pending read failed: {}",
        verdict.stderr
    );
    ensure_failed_saying(
        &verdict,
        SKIPPED,
        "the first session passed a window with new messages and neither session wrote a record",
    )?;
    let line = verdict.lines_saying(SKIPPED).concat();
    ensure!(
        line.contains(FIRST) && !line.contains(SECOND),
        "the line does not name the window the first session's read listed with new messages: {line}"
    );
    Ok(())
}

/// A cycle that wrote nothing reads the marker once it is over, and a read that fails ends the
/// job with that read's status and one line saying so, as a pass's marker read does. A cycle that
/// wrote a record does not read the marker at all.
#[test]
fn a_cycle_that_cannot_read_the_marker_afterwards_fails_with_that_status() -> Result<()> {
    let around = Around {
        marker_read_fails: true,
        ..Around::default()
    };
    let nothing = finished("Handled 1 window.");
    let unwritten = verdict(
        None,
        &[Attempt {
            status: 0,
            events: &nothing,
        }],
        &around,
    )?;
    ensure_failed_saying(
        &unwritten,
        "ctx marker show failed after the cycle (exit 3)",
        "the marker read after a cycle that wrote nothing failed",
    )?;
    ensure!(
        unwritten.status.code() == Some(3),
        "the runner did not exit with the failed read's status 3: {}",
        unwritten.status
    );
    let put = wrote(&[("ctx_record_put", true)], answer("Handled 1 window."));
    let written = verdict(
        None,
        &[Attempt {
            status: 0,
            events: &put,
        }],
        &around,
    )?;
    ensure_succeeded(
        &written,
        "the cycle wrote a record, so it had no need to read the marker",
    )
}

/// A marker answer the runner cannot read as JSON, such as a gateway's error page, leaves it
/// unable to tell which windows the marker passed: the cycle that wrote nothing fails, with one
/// line saying so, rather than pass as if no window had been skipped.
#[test]
fn a_cycle_whose_marker_answer_cannot_be_read_fails_saying_so() -> Result<()> {
    let nothing = finished("Handled 1 window.");
    let verdict = verdict(
        None,
        &[Attempt {
            status: 0,
            events: &nothing,
        }],
        &Around {
            marker_unreadable: true,
            ..Around::default()
        },
    )?;
    ensure_failed_saying(
        &verdict,
        "could not tell whether the cycle moved the marker past windows it wrote nothing for",
        "the marker answer after a cycle that wrote nothing was not JSON",
    )?;
    ensure!(
        verdict.status.code() == Some(1),
        "the runner did not exit 1: {}",
        verdict.status
    );
    Ok(())
}

/// On a tree no cycle has handled the marker is null. A session that moved it nowhere passed no
/// window, so the cycle succeeds; one that moved it past a window with new messages, with nothing
/// written, fails naming that window.
#[test]
fn a_cycle_on_a_tree_with_no_marker_is_judged_by_where_the_marker_ends() -> Result<()> {
    let pending = pending_answer(&[(FIRST, 4)]);
    let nothing = finished("Handled 1 window.");
    for (moves, skipped, what) in [
        (
            &[(0, "")][..],
            false,
            "the session left a tree with no marker without one",
        ),
        (
            &[(0, ""), (1, FIRST)][..],
            true,
            "the session moved a tree's first marker past a window with new messages",
        ),
    ] {
        let verdict = verdict(
            None,
            &[Attempt {
                status: 0,
                events: &nothing,
            }],
            &Around {
                marker_moves: moves,
                pending: &[&pending],
                ..Around::default()
            },
        )?;
        if skipped {
            ensure_failed_saying(&verdict, FIRST, what)?;
        } else {
            ensure_succeeded(&verdict, what)?;
            ensure!(
                verdict.lines_saying(SKIPPED).is_empty(),
                "{what}, yet the runner names skipped windows: {}",
                verdict.stderr
            );
        }
    }
    Ok(())
}
