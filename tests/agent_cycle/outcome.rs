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
/// maintenance pass, where the sessions it starts end as `attempts` say, in order.
pub(crate) fn runner_verdict(pass: Option<&str>, attempts: &[Attempt<'_>]) -> Result<Verdict> {
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
    let outcome = run_after_launch(&dir, pass, minutes, &after_launch, attempts);
    remove_scratch(&dir)?;
    outcome
}

fn run_after_launch(
    dir: &Path,
    pass: Option<&str>,
    minutes: &str,
    after_launch: &str,
    attempts: &[Attempt<'_>],
) -> Result<Verdict> {
    for (index, attempt) in attempts.iter().enumerate() {
        std::fs::write(dir.join(format!("events-{}", index + 1)), attempt.events)?;
        std::fs::write(
            dir.join(format!("status-{}", index + 1)),
            attempt.status.to_string(),
        )?;
    }
    // What the script holds when its sessions start, and stand-ins for what reaches past this
    // machine: ctx-agent's checks pass, chown changes nothing, and the marker never moves.
    let prelude = format!(
        r#"set -eu
{refuse}
{complete}
SESSION_MINUTES={minutes}
AGENT_USER=ctx-agent
pass='{pass}'
windows=1
CTX_DATA_DIR="$VERDICT_DIR/data"
CLOUD_RUN_EXECUTION=ctx-agent-test-1
session_dir="$CTX_DATA_DIR/agent-sessions/$CLOUD_RUN_EXECUTION"
mkdir -p "$session_dir"
as_agent_user() {{ :; }}
chown() {{ :; }}
ctx() {{ printf '%s\n' '{{"ok":true,"data":{{"marker":"m1"}}}}'; }}
run_session() {{
	attempt=$(($(cat "$VERDICT_DIR/sessions" 2>/dev/null || echo 0) + 1))
	echo "$attempt" >"$VERDICT_DIR/sessions"
	if [ ! -f "$VERDICT_DIR/events-$attempt" ]; then
		echo "the runner started session $attempt, which this test did not expect" >&2
		return 99
	fi
	cp "$VERDICT_DIR/events-$attempt" "$session_dir.events.json"
	cat "$session_dir.events.json"
	return "$(cat "$VERDICT_DIR/status-$attempt")"
}}
"#,
        refuse = script_function("refuse")?,
        complete = script_function("pass_complete_line")?,
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
    })
}

/// omp's stdout for a session whose messages are `messages`, one event per line, as the pinned
/// release writes it: its header, a `message_start` and a `message_end` for each message, and
/// `agent_end`. omp starts each assistant message with an empty body and a placeholder
/// `stopReason` of "stop", so only its `message_end` says how that turn ended.
fn events(messages: &[Value]) -> String {
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": "test-session"}),
        json!({"type": "agent_start"}),
    ];
    for message in messages {
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
        "type": "agent_end", "messages": messages, "isTerminal": true, "yielded": true
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
fn answer(text: &str) -> Value {
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
/// omp's stdout as text inside a tool result, not as events of their own.
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
    for (events, what) in [
        (&plain, "the cycle's session finished"),
        (
            &quoting,
            "the cycle's session finished after reading a window that quotes other sessions",
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

/// omp exiting 0 is not enough: a session whose events hold no final answer from its model, here
/// none at all, did not finish, and the runner fails it rather than read silence as success.
#[test]
fn a_cycle_with_no_final_answer_fails() -> Result<()> {
    let no_answer = events(&[prompt()]);
    let verdict = runner_verdict(
        None,
        &[Attempt {
            status: 0,
            events: &no_answer,
        }],
    )?;
    ensure_failed_saying(
        &verdict,
        "did not finish",
        "omp exited 0 although its model gave no final answer",
    )?;
    let empty = runner_verdict(
        None,
        &[Attempt {
            status: 0,
            events: "",
        }],
    )?;
    ensure_failed_saying(&empty, "did not finish", "omp exited 0 and wrote no event")
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

/// The time limit fails a maintenance pass as it fails a cycle, with the same one line, and a
/// refused pass is not retried.
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
    let refused = refused();
    let verdict = runner_verdict(
        Some("waiting-on"),
        &[Attempt {
            status: 1,
            events: &refused,
        }],
    )?;
    ensure!(
        !verdict.status.success() && verdict.sessions == 1,
        "a refused pass was retried or reported done ({} sessions): {}",
        verdict.sessions,
        verdict.stderr
    );
    Ok(())
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
