//! What `run-cycle.sh` writes to the job log while a session runs: one short line of JSON for each
//! tool call and each model turn, as each ends, and never a record's text, a quote or anything a
//! message said. omp's whole event stream goes to the capture beside the transcript directory,
//! which sealing moves into it (`filesystem.rs`), and never to the log: Cloud Logging stops
//! reading a line as JSON at about 100 KB and drops entries from a burst of thousands.
//!
//! The lines are made by the script's own `summarize_events`, run here over events in the shape
//! the pinned omp writes them; `live_session.rs` runs it over that omp's own output.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, ensure, Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{remove_scratch, scratch, script_function, write_executable};

/// The longest a job-log line may be: far under the about 100 KB at which Cloud Logging stops
/// reading a line as JSON and stores it as text, its fields gone.
const LINE_LIMIT: usize = 20 * 1024;

/// The session the runner names in every line, as `CTX_SESSION` names it.
const SESSION: &str = "ctx-agent-test-1";

/// The data directory the runner names to the summary, as `CTX_DATA_DIR` names it.
const DATA_DIR: &str = "/data";

/// The job-log lines `summarize_events` writes for omp's stdout `events`, each checked to be JSON
/// and under [`LINE_LIMIT`].
pub(crate) fn summarized(events: &str) -> Result<Vec<Value>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{}\nsummarize_events",
            script_function("summarize_events")?
        ))
        .env("CTX_SESSION", SESSION)
        .env("CTX_DATA_DIR", DATA_DIR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running run-cycle.sh's summarize_events")?;
    let mut stdin = child
        .stdin
        .take()
        .context("summarize_events has no standard input")?;
    let input = events.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| anyhow!("the thread writing the events panicked"))??;
    ensure!(
        output.status.success(),
        "summarize_events failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_log(&String::from_utf8(output.stdout)?)
}

/// Each line of `log` as JSON, every one under [`LINE_LIMIT`].
fn parse_log(log: &str) -> Result<Vec<Value>> {
    log.lines()
        .map(|line| {
            ensure!(
                line.len() < LINE_LIMIT,
                "a job-log line is {} bytes: {}",
                line.len(),
                line.chars().take(300).collect::<String>()
            );
            serde_json::from_str(line)
                .with_context(|| format!("a job-log line is not JSON: {line}"))
        })
        .collect()
}

/// The value at JSON `pointer` in `value`, null when there is none.
pub(crate) fn field_at<'value>(value: &'value Value, pointer: &str) -> &'value Value {
    value.pointer(pointer).unwrap_or(&Value::Null)
}

/// The job-log lines of type `kind`, each without its `duration_ms`, which must be a whole number
/// of milliseconds, or null for a call whose start the stream did not show.
pub(crate) fn lines_of(log: &[Value], kind: &str) -> Result<Vec<Value>> {
    log.iter()
        .filter(|line| field_at(line, "/type") == kind)
        .map(|line| {
            let mut line = line.clone();
            if kind == "tool_call" {
                let duration = line
                    .as_object_mut()
                    .and_then(|fields| fields.remove("duration_ms"))
                    .with_context(|| format!("a tool call's line has no duration_ms: {line}"))?;
                ensure!(
                    duration.is_u64() || duration.is_null(),
                    "duration_ms is not a whole number of milliseconds: {duration}"
                );
            }
            Ok(line)
        })
        .collect()
}

/// Whether `line` is one the refusal alert counts: an entry whose `type` is `message_end` and
/// whose `message.stopDetails.type` is `refusal`.
pub(crate) fn is_refusal(line: &Value) -> bool {
    field_at(line, "/type") == "message_end"
        && field_at(line, "/message/stopDetails/type") == "refusal"
}

/// A tool call's line as the job log should hold it, without its duration: `fields` after the
/// line's type, severity and session.
pub(crate) fn tool_line(severity: &str, fields: &Value) -> Value {
    merged(
        json!({"type": "tool_call", "severity": severity, "session": SESSION}),
        fields,
    )
}

/// The object `base` with the fields of `fields` added after its own.
fn merged(mut base: Value, fields: &Value) -> Value {
    if let (Some(base), Some(fields)) = (base.as_object_mut(), fields.as_object()) {
        base.extend(fields.clone());
    }
    base
}

/// omp's stdout holding `events`, one per line.
fn stdout_of(events: &[Value]) -> String {
    let mut stdout = String::new();
    for event in events {
        stdout.push_str(&event.to_string());
        stdout.push('\n');
    }
    stdout
}

fn start(id: &str, tool: &str, args: &Value) -> Value {
    json!({"type": "tool_execution_start", "toolCallId": id, "toolName": tool, "args": args})
}

fn end(id: &str, tool: &str, text: &str, details: &Value, error: bool) -> Value {
    json!({
        "type": "tool_execution_end", "toolCallId": id, "toolName": tool,
        "result": {"content": [{"type": "text", "text": text}], "details": details},
        "isError": error
    })
}

fn message_end(message: &Value) -> Value {
    json!({"type": "message_end", "message": message})
}

/// A model refusal, as the pinned omp writes one, with the fields a provider may add: an error
/// status, prose in the error and the explanation, and a fallback credit token
/// (`anthropic-wire.ts` `StopDetails` at the pinned release).
fn refusal() -> Value {
    json!({
        "role": "assistant", "content": [], "stopReason": "error",
        "errorMessage": "The model refused this request: SECRET",
        "errorStatus": 400,
        "stopDetails": {
            "category": "reasoning_extraction", "explanation": "blocked: SECRET }",
            "type": "refusal", "fallback_credit_token": "SECRET-credit"
        }
    })
}

/// One tool call of a session and the line it must leave: its tool and arguments, the text and
/// details it ended with, whether omp reported it as an error, and the expected line's severity
/// and fields.
struct Case {
    tool: &'static str,
    args: Value,
    text: &'static str,
    details: Value,
    error: bool,
    severity: &'static str,
    line: Value,
}

fn case(
    tool: &'static str,
    args: Value,
    text: &'static str,
    details: Value,
    error: bool,
    severity: &'static str,
    line: Value,
) -> Case {
    Case {
        tool,
        args,
        text,
        details,
        error,
        severity,
        line,
    }
}

/// Shell calls: the ctx subcommands the session runs, and how each ended. Every text a record, a
/// source, a search or a message holds is marked `SECRET`. Only `ctx record show` and
/// `ctx cite verify` take record paths, so a search's query never reads as one, even in a record
/// path's shape, and an argument that is not one record path is withheld.
fn shell_cases() -> Vec<Case> {
    let bash = |fields: Value| merged(json!({"tool": "bash"}), &fields);
    vec![
        case(
            "bash",
            json!({"command": "ctx record show decisions/a.md"}),
            "---\ntype: decision\n---\nSECRET body\n",
            json!({}),
            false,
            "INFO",
            bash(json!({"command": "record show", "paths": ["decisions/a.md"], "result": "ok"})),
        ),
        case(
            "bash",
            json!({"command": "ctx search \"what SECRET said in SECRET.md\" --json"}),
            "SECRET hits\n\nCommand exited with code 1",
            json!({"exitCode": 1}),
            true,
            "WARNING",
            bash(json!({"command": "search", "result": "exit-1"})),
        ),
        case(
            "bash",
            json!({"command": "ctx record show \"decisions/SECRET a.md\""}),
            "no such record",
            json!({"exitCode": 1}),
            true,
            "WARNING",
            bash(json!({"command": "record show", "paths": ["(withheld)"], "result": "exit-1"})),
        ),
        case(
            "bash",
            json!({"command": "ctx record show what SECRET.md"}),
            "error: unexpected argument",
            json!({"exitCode": 2}),
            true,
            "WARNING",
            bash(json!({"command": "record show", "paths": ["(withheld)"], "result": "exit-2"})),
        ),
        case(
            "bash",
            json!({"command": "ctx cite verify --live decisions/a.md 'decisions/b.md'"}),
            "SECRET quote checked",
            json!({}),
            false,
            "INFO",
            bash(
                json!({"command": "cite verify", "paths": ["decisions/a.md", "decisions/b.md"], "result": "ok"}),
            ),
        ),
        case(
            "bash",
            json!({"command": "curl https://SECRET.example/?q=SECRET"}),
            "Tool \"bash\" requires approval but no interactive UI available.",
            json!({}),
            true,
            "WARNING",
            bash(json!({"result": "denied"})),
        ),
        case(
            "bash",
            json!({"command": "ctx resolve --thread slack:C0EXAMPLE01/1700000000.000001"}),
            "SECRET thread\n\n[Command timed out after 300 seconds]",
            json!({"timedOut": true}),
            true,
            "WARNING",
            bash(json!({"command": "resolve", "result": "timeout"})),
        ),
    ]
}

/// Shell calls that are more than one simple `ctx` command: a second command after an operator or
/// a newline, a heredoc, a comment, a word built of quoted pieces, a newline inside the command's
/// own words. Their record paths are withheld whole, and no word in them is read as one, so nothing
/// past the first command reaches the log.
fn compound_shell_cases() -> Vec<Case> {
    let withheld = |result: &str| json!({"tool": "bash", "command": "cite verify", "paths": ["(withheld)"], "result": result});
    let denied = "Tool \"bash\" requires approval but no interactive UI available.";
    vec![
        case(
            "bash",
            json!({"command": "ctx cite verify decisions/a.md && ctx search SECRET.md"}),
            denied,
            json!({}),
            true,
            "WARNING",
            withheld("denied"),
        ),
        case(
            "bash",
            json!({"command": "ctx cite verify <<EOF\nSECRET_BODY.md\nEOF"}),
            denied,
            json!({}),
            true,
            "WARNING",
            withheld("denied"),
        ),
        case(
            "bash",
            json!({"command": "ctx cite verify decisions/a.md # SECRET_QUOTE.md"}),
            denied,
            json!({}),
            true,
            "WARNING",
            withheld("denied"),
        ),
        case(
            "bash",
            json!({"command": "ctx cite verify decisions/a.md\nctx search SECRET.md"}),
            denied,
            json!({}),
            true,
            "WARNING",
            withheld("denied"),
        ),
        case(
            "bash",
            json!({"command": "ctx cite verify decisions/\"a.md\""}),
            "checked",
            json!({}),
            false,
            "INFO",
            withheld("ok"),
        ),
        case(
            "bash",
            json!({"command": "ctx\ncite verify SECRET.md"}),
            denied,
            json!({}),
            true,
            "WARNING",
            json!({"tool": "bash", "result": "denied"}),
        ),
        case(
            "bash",
            json!({"command": "ctx cite\nverify SECRET.md"}),
            denied,
            json!({}),
            true,
            "WARNING",
            json!({"tool": "bash", "command": "cite", "result": "denied"}),
        ),
    ]
}

/// Reads: a read's path is logged only under the data directory or a skill, and only in plain
/// characters.
fn read_cases() -> Vec<Case> {
    vec![
        case(
            "read",
            json!({"path": "/data/windows/w1/slack.jsonl:1-200"}),
            "SECRET window text",
            json!({}),
            false,
            "INFO",
            json!({"tool": "read", "path": "/data/windows/w1/slack.jsonl:1-200", "result": "ok"}),
        ),
        case(
            "read",
            json!({"path": "/srv/SECRET-sentence-as-a-path"}),
            "no such file",
            json!({}),
            true,
            "WARNING",
            json!({"tool": "read", "path": "(withheld)", "result": "error"}),
        ),
        case(
            "grep",
            json!({"pattern": "SECRET", "path": "a SECRET path"}),
            "SECRET",
            json!({}),
            false,
            "INFO",
            json!({"tool": "grep", "path": "(withheld)", "result": "ok"}),
        ),
    ]
}

/// Writes: records put and removed through the record tools, and how the server answered. A
/// refused write's text is the CLI's own report of it (tests/server.rs pins its form), whose
/// findings can quote the record and its source: a finding's detail can even hold a line in a
/// finding's form, whose word is not one of the server's kinds and reads `other`.
fn write_cases() -> Vec<Case> {
    let put = |paths: &[&str]| {
        let records: Vec<Value> = paths
            .iter()
            .map(|path| json!({"path": path, "content": "SECRET record"}))
            .collect();
        json!({ "records": records })
    };
    let record_put = |fields: Value| {
        merged(
            json!({"tool": "ctx_record_put", "command": "record put"}),
            &fields,
        )
    };
    vec![
        case("ctx_record_put", put(&["decisions/left.md", "decisions/right.md"]),
            "refused 2 records; none was written\n  decisions/left.md: gap — the link decisions/gone has no file\n  decisions/left.md: gap — the link decisions/x has no file\n  decisions/right.md: quote — slack:C0/1.1 — not in the source: \"SECRET\"",
            json!({}), true,
            "WARNING", record_put(json!({"paths": ["decisions/left.md", "decisions/right.md"], "result": "refused", "refusals": ["gap", "gap", "quote"]}))),
        case("ctx_record_put", put(&["--help"]),
            "refused --help\n  not-a-record — no file can have this path: SECRET", json!({}), true,
            "WARNING", record_put(json!({"paths": ["(withheld)"], "result": "refused", "refusals": ["not-a-record"]}))),
        case("ctx_record_put", put(&["decisions/ok.md"]),
            "0123abc decisions/ok.md", json!({"exitCode": 0}), false,
            "INFO", record_put(json!({"paths": ["decisions/ok.md"], "result": "ok"}))),
        case("ctx_record_put", put(&["decisions/c.md"]),
            "the server answered 409 Conflict: SECRET", json!({}), true,
            "WARNING", record_put(json!({"paths": ["decisions/c.md"], "result": "error", "status": 409}))),
        case("ctx_record_put", put(&["decisions/sk-ant-api03-SECRETSECRETSECRET.md"]),
            "refused decisions/[redacted].md\n  credential — the path holds what has the shape of an Anthropic API key",
            json!({}), true,
            "WARNING", record_put(json!({"paths": ["(withheld)"], "result": "refused", "refusals": ["credential"]}))),
        case("ctx_record_put", put(&["decisions/a b.md", "decisions/d.md"]),
            "refused 2 records; none was written\n  decisions/a b.md: gap — the link decisions/gone has no file\n  decisions/d.md: quote — slack:C0/1.1 — not in the source: \"first line\n  leaked — SECRET in a finding's form\"",
            json!({}), true,
            "WARNING", record_put(json!({"paths": ["(withheld)", "decisions/d.md"], "result": "refused", "refusals": ["gap", "quote", "other"]}))),
        case("ctx_record_put", put(&["decisions/release — notes.md"]),
            "refused 1 records; none was written\n  decisions/release — notes.md: gap — the link decisions/gone has no file",
            json!({}), true,
            "WARNING", record_put(json!({"paths": ["(withheld)"], "result": "refused", "refusals": ["gap"]}))),
        case("ctx_record_rm", json!({"path": "decisions/old.md"}),
            "ok", json!({"exitCode": 0}), false,
            "INFO", json!({"tool": "ctx_record_rm", "command": "record rm", "paths": ["decisions/old.md"], "result": "ok"})),
    ]
}

/// Every tool call the session makes is one line when it ends, naming its tool, the ctx
/// subcommand and the record paths it names, and how it ended: ok, refused with the kinds of the
/// findings that refused it (each kind as often as the server's answer gives it, as the server's
/// own log counts them), a failed command's exit code, a timeout, a call the approval rules
/// denied, a write the server failed with its status, and a call still open when the stream ends.
/// Every model turn is one line with its stop reason and token use. Nothing a record, a quote, a
/// search or a window holds reaches the log. A record path in no record path's shape, a path that
/// holds a credential, and a read's path outside the data directory or in more than plain
/// characters are withheld. Lines that are not JSON, and events the log does not report, are
/// passed over.
#[test]
fn the_job_log_has_one_line_per_tool_call_and_per_turn_and_no_record_text() -> Result<()> {
    let cases: Vec<Case> = shell_cases()
        .into_iter()
        .chain(compound_shell_cases())
        .chain(read_cases())
        .chain(write_cases())
        .collect();
    let mut events = vec![
        json!({"type": "session", "version": 3, "id": "test-session"}),
        json!({"type": "agent_start"}),
        message_end(
            &json!({"role": "user", "content": [{"type": "text", "text": "SECRET prompt"}]}),
        ),
    ];
    for (index, case) in cases.iter().enumerate() {
        let id = format!("t{index}");
        events.push(start(&id, case.tool, &case.args));
        events.push(end(&id, case.tool, case.text, &case.details, case.error));
    }
    let advance = json!({"command": "ctx marker advance 2026-10-31T10"});
    events.push(message_end(&json!({
        "role": "assistant", "stopReason": "toolUse",
        "content": [
            {"type": "thinking", "thinking": "SECRET reasoning"},
            {"type": "text", "text": "SECRET answer"},
            {"type": "toolCall", "id": "open", "name": "bash", "arguments": advance},
        ],
        "usage": {"input": 10, "output": 20, "cacheRead": 5, "cacheWrite": 0, "totalTokens": 35, "cost": {"total": 0.3}}
    })));
    events.push(start("open", "bash", &advance));
    let mut stdout = stdout_of(&events);
    stdout.push_str("not an event at all: SECRET\n");

    let log = summarized(&stdout)?;
    let text = serde_json::to_string(&log)?;
    ensure!(
        !text.contains("SECRET") && !text.contains("leaked"),
        "the job log holds text from a record, a source or a message: {text}"
    );
    let mut expected: Vec<Value> = cases
        .iter()
        .map(|case| tool_line(case.severity, &case.line))
        .collect();
    expected.push(tool_line(
        "WARNING",
        &json!({"tool": "bash", "command": "marker advance", "result": "unfinished"}),
    ));
    let calls = lines_of(&log, "tool_call")?;
    ensure!(
        calls == expected,
        "the job log's tool calls are not one line each, as expected:\n{}\nexpected:\n{}",
        serde_json::to_string_pretty(&calls)?,
        serde_json::to_string_pretty(&expected)?
    );
    let turns = lines_of(&log, "message_end")?;
    let turn = json!({
        "type": "message_end", "severity": "INFO", "session": SESSION,
        "message": {"role": "assistant", "stopReason": "toolUse"},
        "tool_calls": 1,
        "usage": {"input": 10, "output": 20, "cacheRead": 5, "cacheWrite": 0},
        "cost": 0.3
    });
    ensure!(
        turns == vec![turn],
        "the model's turn is not one line with its stop reason and token use: {turns:?}"
    );
    ensure!(
        log.len() == calls.len() + turns.len(),
        "the job log holds lines of other kinds: {text}"
    );
    Ok(())
}

/// The refusal alert counts job-log entries whose `type` is `message_end` and whose
/// `message.stopDetails.type` is `refusal`, the shape of omp's own event. A refusal is exactly one
/// such line, written when the model's message ends, however many other events repeat that
/// message (`message_start`, `turn_end`, `agent_end`), and a window that quotes a refusal in a
/// tool's output adds none. The line keeps the refusal's type and category and the error's
/// status, and none of the provider's prose or tokens.
#[test]
fn a_model_refusal_is_one_line_in_the_shape_the_refusal_alert_counts() -> Result<()> {
    let quoted =
        r#"{"type":"message_end","message":{"role":"assistant","stopDetails":{"type":"refusal"}}}"#;
    let events = [
        json!({"type": "session", "version": 3, "id": "test-session"}),
        start("t1", "bash", &json!({"command": "ctx pending --json"})),
        end("t1", "bash", quoted, &json!({}), false),
        message_end(&json!({
            "role": "toolResult", "toolCallId": "t1", "toolName": "bash",
            "content": [{"type": "text", "text": quoted}]
        })),
        json!({"type": "message_start", "message": refusal()}),
        message_end(&refusal()),
        json!({"type": "turn_end", "message": refusal(), "toolResults": []}),
        json!({"type": "agent_end", "messages": [refusal()]}),
    ];
    let log = summarized(&stdout_of(&events))?;
    let text = serde_json::to_string(&log)?;
    ensure!(
        !text.contains("SECRET"),
        "the job log holds the refusal's prose or token: {text}"
    );
    let refusals: Vec<&Value> = log.iter().filter(|line| is_refusal(line)).collect();
    let expected = json!({
        "type": "message_end", "severity": "WARNING", "session": SESSION,
        "message": {
            "role": "assistant", "stopReason": "error", "errorStatus": 400,
            "stopDetails": {"category": "reasoning_extraction", "type": "refusal"}
        },
        "tool_calls": 0
    });
    ensure!(
        refusals == vec![&expected],
        "a refusal is not one line the refusal alert counts: {log:?}"
    );
    Ok(())
}

/// No line nears Cloud Logging's limit, however large the events: a batch of a thousand records
/// with long paths, refused with hundreds of findings, lists the first 50 paths and kinds and
/// counts the rest; an error's prose, a refusal's explanation and every other stop-details field
/// but its type and category are dropped, a megabyte-long key among them, and a category longer
/// than a name is withheld; a megabyte of tool output and a session's whole conversation in
/// `agent_end` reach no line.
#[test]
fn every_job_log_line_stays_far_under_the_log_entry_limit() -> Result<()> {
    let long_path = |n: usize| format!("decisions/{}-{n:04}.md", "a".repeat(230));
    let records: Vec<Value> = (0..1000)
        .map(|n| json!({"path": long_path(n), "content": "x".repeat(2000)}))
        .collect();
    let mut findings = String::new();
    for n in 0..600 {
        findings.push_str("\n  ");
        findings.push_str(&long_path(n));
        findings.push_str(": schema — /title is not a string ");
        findings.push_str(&"y".repeat(500));
    }
    let huge = "z".repeat(1 << 20);
    let mut stop_details = json!({"category": huge, "explanation": huge, "type": "refusal"});
    if let Some(fields) = stop_details.as_object_mut() {
        fields.insert(huge.clone(), json!(huge));
    }
    let events = [
        start("t1", "ctx_record_put", &json!({"records": records})),
        end(
            "t1",
            "ctx_record_put",
            &format!("refused 1000 records; none was written{findings}"),
            &json!({}),
            true,
        ),
        start("t2", "bash", &json!({"command": "ctx open --json"})),
        end("t2", "bash", &huge, &json!({}), false),
        message_end(&json!({
            "role": "assistant", "content": [], "stopReason": "error",
            "errorMessage": huge,
            "stopDetails": stop_details
        })),
        json!({"type": "agent_end", "messages": [{"role": "user", "content": [{"type": "text", "text": huge}]}]}),
    ];
    let log = summarized(&stdout_of(&events))?;
    let calls = lines_of(&log, "tool_call")?;
    let batch = calls
        .first()
        .context("the batch has no line in the job log")?;
    ensure!(
        field_at(batch, "/paths").as_array().map(Vec::len) == Some(50)
            && field_at(batch, "/paths_omitted") == 950
            && field_at(batch, "/refusals").as_array().map(Vec::len) == Some(50)
            && field_at(batch, "/refusals_omitted") == 550,
        "the batch's line does not list 50 paths and kinds and count the rest: {batch}"
    );
    let turns = lines_of(&log, "message_end")?;
    ensure!(
        log.len() == 3
            && turns
                .first()
                .map(|turn| field_at(turn, "/message/stopDetails"))
                == Some(&json!({"category": "(withheld)", "type": "refusal"})),
        "the job log does not hold one line for each call and the turn, its stop details cut to \
         their type and a category of a name's shape: {log:?}"
    );
    Ok(())
}

/// jq reads a raw line in pieces of 4095 bytes and turns a character split across two pieces into
/// replacement characters (jq 1.7.1 and 1.8.1 alike). A refused write whose finding's dash falls
/// on such a split still counts that finding's kind: each of these refusals puts the dash a byte
/// further along its line, across the split.
#[test]
fn a_refusal_kind_survives_a_dash_jq_splits_while_reading() -> Result<()> {
    let events: Vec<Value> = (3930..4060)
        .map(|pad| {
            end(
                &format!("t{pad}"),
                "ctx_record_put",
                &format!(
                    "refused decisions/a.md{}\n  gap — the link decisions/gone has no file",
                    "x".repeat(pad)
                ),
                &json!({}),
                true,
            )
        })
        .collect();
    let log = summarized(&stdout_of(&events))?;
    let lost: Vec<&Value> = log
        .iter()
        .filter(|line| field_at(line, "/refusals") != &json!(["gap"]))
        .collect();
    ensure!(
        log.len() == events.len() && lost.is_empty(),
        "a refusal lost its kind where jq split its dash: {lost:?}"
    );
    Ok(())
}

/// The runner's job log follows the session: a tool call's line is written while the session
/// still runs, not when it ends, and only the summary reaches the log, a model refusal's line
/// among it. The capture beside the transcript directory, which sealing moves into it, holds every
/// event omp wrote, byte for byte, a 300 KB one among them. The runner answers with the session's
/// own exit status. The stand-in omp holds the session open until the job log shows its first
/// call, and fails if it never does.
#[test]
fn the_runner_logs_each_tool_call_while_the_session_runs() -> Result<()> {
    let root = scratch("job-log-stream")?;
    let streamed = run_session_with_stand_ins(&root);
    remove_scratch(&root)?;
    let streamed = streamed?;
    ensure!(
        streamed.status == Some(5),
        "the runner did not answer with the session's own status (it answered {:?}): {}",
        streamed.status,
        streamed.stderr
    );
    let log = parse_log(&streamed.log)?;
    let calls = lines_of(&log, "tool_call")?;
    let rules = tool_line(
        "INFO",
        &json!({"tool": "bash", "command": "rules", "result": "ok"}),
    );
    ensure!(
        calls == vec![rules],
        "the job log does not hold the session's one tool call: {log:?}"
    );
    ensure!(
        log.iter().filter(|line| is_refusal(line)).count() == 1 && log.len() == 2,
        "the job log does not hold just the call and the model's refusal: {log:?}"
    );
    ensure!(
        streamed.capture == streamed.written,
        "the capture does not hold every event omp wrote, byte for byte"
    );
    Ok(())
}

/// What the script's own `run_session` did with stand-ins for `setpriv` and omp.
struct Streamed {
    /// The runner's exit status: `run_session`'s.
    status: Option<i32>,
    stderr: String,
    /// What it wrote to its standard output, the job log.
    log: String,
    /// The capture it left beside the transcript directory.
    capture: String,
    /// What the stand-in omp wrote to its standard output.
    written: String,
}

fn run_session_with_stand_ins(root: &Path) -> Result<Streamed> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir_all(root.join("data/agent-sessions"))?;
    let before = stdout_of(&[
        json!({"type": "session", "version": 3, "id": "test-session"}),
        start("t1", "bash", &json!({"command": "ctx rules"})),
        end("t1", "bash", "Tree rules", &json!({}), false),
        message_end(&json!({
            "role": "toolResult", "toolCallId": "t1", "toolName": "bash",
            "content": [{"type": "text", "text": "w".repeat(300 * 1024)}]
        })),
    ]);
    let after = stdout_of(&[
        message_end(&refusal()),
        json!({"type": "agent_end", "messages": [refusal()]}),
    ]);
    std::fs::write(root.join("before.jsonl"), &before)?;
    std::fs::write(root.join("after.jsonl"), &after)?;
    let log = root.join("job.log");
    write_executable(
        &bin.join("setpriv"),
        "#!/bin/sh\nwhile [ \"${1#--}\" != \"$1\" ]; do shift; done\nexec \"$@\"\n",
    )?;
    write_executable(
        &bin.join("omp"),
        &format!(
            "#!/bin/sh\ncat '{before}'\ni=0\nuntil grep -q '\"tool\":\"bash\"' '{log}' 2>/dev/null; do\n\ti=$((i + 1))\n\tif [ \"$i\" -gt 600 ]; then echo 'the job log showed no tool call while the session ran' >&2; exit 3; fi\n\tsleep 0.1\ndone\ncat '{after}'\nexit 5\n",
            before = root.join("before.jsonl").display(),
            after = root.join("after.jsonl").display(),
            log = log.display(),
        ),
    )?;
    let script = format!(
        "set -eu\n{summarize}\n{run}\nAGENT_USER=ctx-agent\nagent_group=0\nCTX_MODEL=gateway/example-model\nSESSION_MINUTES=50\nprompt='Run the probe.'\nsession_dir=\"$1/data/agent-sessions/{SESSION}\"\nstatus=0\nrun_session >\"$1/job.log\" || status=$?\nexit \"$status\"\n",
        summarize = script_function("summarize_events")?,
        run = script_function("run_session")?,
    );
    let path = std::env::var("PATH").unwrap_or_default();
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("run-session")
        .arg(root)
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("CTX_SESSION", SESSION)
        .stdin(Stdio::null())
        .output()
        .context("running run-cycle.sh's run_session")?;
    Ok(Streamed {
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        log: std::fs::read_to_string(&log).unwrap_or_default(),
        capture: std::fs::read_to_string(
            root.join(format!("data/agent-sessions/{SESSION}.events.json")),
        )
        .unwrap_or_default(),
        written: before + &after,
    })
}
