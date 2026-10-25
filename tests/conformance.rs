use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::{
    config::SourceConfig,
    pointer::{Pointer, Schemes},
    source::{
        self, FetchRequest, Request, ResolveRequest, Session, SourceOutcome, SourceRef,
        StreamStatus,
    },
    store::Store,
};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);
static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const STREAMS: [&str; 2] = ["C0EXAMPLE01", "C0EXAMPLE02"];
const ECHO_ENV_NAMES: [&str; 8] = [
    "ECHO_FAIL_STREAM",
    "ECHO_BAD_SCHEME",
    "ECHO_DUPLICATE_END",
    "ECHO_NO_END",
    "ECHO_LATE",
    "ECHO_NO_HELLO",
    "ECHO_SLEEP",
    "ECHO_REQUEST_OUT",
];

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn fixture_path() -> Result<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let inherited = std::env::var("PATH").context("PATH is not set")?;
    Ok(PathBuf::from(format!(
        "{}:{inherited}",
        directory.display()
    )))
}

fn clear_echo_environment(command: &mut std::process::Command) {
    for name in ECHO_ENV_NAMES {
        command.env_remove(name);
    }
}

fn fake_slack_api(responses: Vec<String>) -> Result<(String, JoinHandle<Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = std::thread::spawn(move || -> Result<()> {
        for body in responses {
            let (mut stream, _) = listener.accept()?;
            let mut request = BufReader::new(stream.try_clone()?);
            loop {
                let mut line = String::new();
                let bytes_read = request.read_line(&mut line)?;
                if bytes_read == 0 || line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes())?;
        }
        Ok(())
    });
    Ok((format!("http://{address}"), server))
}

fn source_tree(root: &Path) -> Result<PathBuf> {
    let tree = root.join("tree");
    std::fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\"]\n",
    )
    .with_context(|| format!("writing {}", tree.display()))?;
    Ok(tree)
}

#[test]
fn source_run_prints_the_external_jsonl_stream() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("source-run")?;
    let tree = source_tree(&root)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--memory", tree_text, "source", "run", "echo"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut command);
    let output = command.output().context("running ctx source run")?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).context("source output is not UTF-8")?;
    let mut lines = stdout.lines();
    let hello: Value = serde_json::from_str(lines.next().context("source output has hello")?)?;
    assert_eq!(hello.get("type"), Some(&serde_json::json!("hello")));
    let record: Value = serde_json::from_str(lines.next().context("source output has record")?)?;
    assert_eq!(record.get("type"), Some(&serde_json::json!("record")));
    let end: Value = serde_json::from_str(lines.next().context("source output has stream end")?)?;
    assert_eq!(end.get("type"), Some(&serde_json::json!("stream_end")));
    Ok(())
}

#[test]
fn source_run_json_emits_one_validated_message_envelope() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("source-run-json")?;
    let tree = source_tree(&root)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--json", "--memory", tree_text, "source", "run", "echo"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut command);
    let output = command.output().context("running ctx --json source run")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value.get("version"), Some(&serde_json::json!(1)));
    assert_eq!(value.get("command"), Some(&serde_json::json!("source run")));
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("source"))
            .and_then(Value::as_str),
        Some("echo")
    );
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("op"))
            .and_then(Value::as_str),
        Some("fetch")
    );
    assert!(value
        .get("data")
        .and_then(|data| data.get("messages"))
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .first()
                .is_some_and(|message| message.get("type") == Some(&serde_json::json!("hello")))
        }));
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("outcome"))
            .and_then(|outcome| outcome.get("error")),
        Some(&Value::Null)
    );
    Ok(())
}

#[test]
fn source_run_json_exits_one_for_an_errored_stream() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("source-run-json-error")?;
    let tree = source_tree(&root)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--json", "--memory", tree_text, "source", "run", "echo"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?)
        .env("ECHO_FAIL_STREAM", "C0EXAMPLE01");
    clear_echo_environment(&mut command);
    command.env("ECHO_FAIL_STREAM", "C0EXAMPLE01");
    let output = command
        .output()
        .context("running errored ctx --json source run")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value.get("version"), Some(&serde_json::json!(1)));
    assert_eq!(value.get("command"), Some(&serde_json::json!("source run")));
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("outcome"))
            .and_then(|outcome| outcome.get("streams"))
            .and_then(|streams| streams.get("C0EXAMPLE01"))
            .and_then(|stream| stream.get("status")),
        Some(&serde_json::json!("error"))
    );
    Ok(())
}

#[test]
fn unsafe_source_ids_fail_before_fetch_resolve_or_source_run_create_state() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("unsafe-source-id")?;
    let tree = root.join("tree");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"../x\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let commands: [&[&str]; 3] = [
        &["fetch"],
        &["resolve", "echo:C0EXAMPLE01/record"],
        &["source", "run", "../x"],
    ];
    let mut results = Vec::new();
    for arguments in commands {
        let state = root.join(format!("state-{}", results.len()));
        let output = binary()
            .args(["--memory", tree_text])
            .args(arguments)
            .env("CTX_STATE_DIR", &state)
            .env("PATH", fixture_path()?)
            .output()
            .with_context(|| format!("running ctx {}", arguments.join(" ")))?;
        results.push((output, state.exists()));
    }
    remove_scratch(&root)?;

    for (output, state_exists) in results {
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("../x"));
        assert!(!state_exists);
    }
    Ok(())
}

#[test]
fn source_run_accepts_the_resolve_operation() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("source-run-resolve")?;
    let tree = source_tree(&root)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args([
            "--memory", tree_text, "source", "run", "echo", "--op", "resolve",
        ])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut command);
    let output = command
        .output()
        .context("running ctx source run --op resolve")?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hello: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(hello.get("type"), Some(&serde_json::json!("hello")));
    Ok(())
}

#[test]
fn source_opens_builtin_slack() -> Result<()> {
    let config: ctx_memory::config::TreeConfig = toml::from_str(
        "version = 1
id = \"example\"

[[sources]]
id = \"slack\"
kind = \"slack\"
workspace = \"example\"
channels = [\"C0EXAMPLE01\"]
",
    )?;
    let spec = config
        .sources
        .into_iter()
        .next()
        .context("test configuration contains a Slack source")?;
    let source = source::open(&spec, "slack");

    assert!(
        source.is_ok(),
        "opening the built-in Slack source failed: {}",
        source
            .err()
            .map_or_else(String::new, |error| format!("{error:#}"))
    );
    Ok(())
}

#[test]
fn resolve_live_fallback_stores_and_returns_external_records() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("resolve-live")?;
    let tree = source_tree(&root)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--json", "--memory", tree_text, "resolve", "echo:x"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut command);
    let output = command.output().context("running ctx resolve")?;
    let stored = (|| {
        let store = Store::open(&root.join("state/example/store.sqlite3"))?;
        store.get(&Pointer {
            scheme: "echo".to_owned(),
            locator: "x".to_owned(),
        })
    })();
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let text = value
        .get("data")
        .and_then(|data| data.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .context("live fallback returned resolved text")?;
    assert!(text.contains("echo echo:x"));
    assert_eq!(
        stored?.map(|record| record.text).as_deref(),
        Some("echo echo:x")
    );
    Ok(())
}

#[test]
fn cite_verify_live_checks_verbatim_external_records() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("cite-live")?;
    let tree = source_tree(&root)?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nquote: \"echo\"\n---\necho:x\n",
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut first = binary();
    first
        .args(["--json", "--memory", tree_text, "cite", "verify", "--live"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut first);
    let first = first
        .output()
        .context("running first ctx cite verify --live")?;
    let mut second = binary();
    second
        .args(["--json", "--memory", tree_text, "cite", "verify", "--live"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?);
    clear_echo_environment(&mut second);
    let second = second
        .output()
        .context("running second ctx cite verify --live")?;
    remove_scratch(&root)?;

    for output in [first, second] {
        assert_eq!(output.status.code(), Some(0));
        let value: Value = serde_json::from_slice(&output.stdout)?;
        let data = value.get("data").context("cite verification has data")?;
        assert_eq!(data.get("checked").and_then(Value::as_u64), Some(1));
        assert_eq!(
            data.get("broken").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }
    Ok(())
}

#[test]
fn source_run_discovers_streams_from_the_selected_tree() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("selected-tree-discovery")?;
    let cwd_memory = root.join("cwd/memory");
    std::fs::create_dir_all(&cwd_memory)?;
    std::fs::write(
        cwd_memory.join("person.md"),
        "---\nops_channel: C0EXAMPLEAA\n---\n",
    )?;
    let selected = root.join("b");
    std::fs::create_dir(&selected)?;
    std::fs::write(
        selected.join("ctx.toml"),
        "version = 1\nid = \"example-b\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\ndiscover_field = \"ops_channel\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    std::fs::write(
        selected.join("person.md"),
        "---\nops_channel: \"C0EXAMPLEBB # moved\"\n---\n",
    )?;
    let selected_text = selected
        .to_str()
        .context("selected tree path is not valid UTF-8")?;
    let request_path = root.join("request.json");
    let mut command = binary();
    command
        .args(["--memory", selected_text, "source", "run", "echo"])
        .current_dir(root.join("cwd"))
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?)
        .env("ECHO_REQUEST_OUT", &request_path);
    clear_echo_environment(&mut command);
    command.env("ECHO_REQUEST_OUT", &request_path);
    let output = command.output().context("running ctx source run")?;
    let request = std::fs::read(&request_path).context("reading echo request")?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request: Value = serde_json::from_slice(&request)?;
    let discovered = request
        .get("discovered")
        .and_then(Value::as_array)
        .context("source request has discovered streams")?;
    assert_eq!(discovered.len(), 1);
    let stream = discovered
        .first()
        .and_then(|entry| entry.get("stream"))
        .and_then(Value::as_str)
        .context("source request has discovered stream")?;
    assert_eq!(stream, "C0EXAMPLEBB");
    Ok(())
}

fn source_config(timeout_secs: Option<u64>) -> Result<SourceConfig> {
    let timeout =
        timeout_secs.map_or_else(String::new, |seconds| format!("timeout_secs = {seconds}\n"));
    let config: ctx_memory::config::TreeConfig = toml::from_str(&format!(
        "version = 1\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\"]\n{timeout}",
    ))?;
    config
        .sources
        .into_iter()
        .next()
        .context("test config contains echo source")
}

fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn fetch_request(root: &Path) -> Result<Request> {
    Ok(Request::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: "echo".to_owned(),
            kind: "echo".to_owned(),
            scheme: "echo".to_owned(),
        },
        config: serde_json::json!({ "streams": STREAMS }),
        window_end: timestamp("2026-10-25T18:00:00Z")?,
        backfill_hours: 24,
        cursors: std::collections::BTreeMap::default(),
        discovered: Vec::new(),
        state_dir: root.join("state"),
    }))
}

fn resolve_request(root: &Path, pointer: &str) -> Request {
    Request::Resolve(ResolveRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: "echo".to_owned(),
            kind: "echo".to_owned(),
            scheme: "echo".to_owned(),
        },
        config: serde_json::json!({ "streams": STREAMS }),
        pointers: vec![pointer.to_owned()],
        state_dir: root.join("state"),
    })
}

fn run_external(
    request: &Request,
    timeout_secs: Option<u64>,
    changes: &[(&str, &str)],
) -> Result<SourceOutcome> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let previous_path = std::env::var_os("PATH");
    let previous = ECHO_ENV_NAMES
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    std::env::set_var("PATH", fixture_path()?);
    for name in ECHO_ENV_NAMES {
        std::env::remove_var(name);
    }
    for (name, value) in changes {
        std::env::set_var(name, value);
    }

    let result = (|| {
        let config = source_config(timeout_secs)?;
        let source = source::open(&config, "echo")?;
        Ok(run_conformance(source.as_ref(), request))
    })();

    restore_environment("PATH", previous_path);
    for (name, value) in previous {
        restore_environment(name, value);
    }
    result
}

fn run_conformance(source: &dyn source::Source, request: &Request) -> SourceOutcome {
    let schemes = Schemes::new([request.source().scheme.clone()]);
    let mut session = Session::new(request, &schemes);
    let run = source.run(request, &mut |message| session.accept(message).map(|_| ()));
    session.finish(run)
}

fn restore_environment(name: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}

fn assert_stream(
    outcome: &SourceOutcome,
    stream: &str,
    status: StreamStatus,
    cursor: Option<&str>,
) -> Result<()> {
    let result = outcome
        .streams
        .get(stream)
        .with_context(|| format!("source outcome has stream {stream}"))?;
    assert_eq!(result.status, status);
    assert_eq!(result.cursor.as_deref(), cursor);
    Ok(())
}

#[test]
fn external_fetch_reports_successful_streams_and_cursors() -> Result<()> {
    let root = scratch("external-success")?;
    let request = fetch_request(&root)?;
    let outcome = run_external(&request, None, &[]);
    remove_scratch(&root)?;
    let outcome = outcome?;

    assert!(!outcome.has_errors());
    assert_eq!(outcome.records.len(), 2);
    assert_stream(
        &outcome,
        "C0EXAMPLE01",
        StreamStatus::Ok,
        Some("2026-10-25T18:00:00Z"),
    )?;
    assert_stream(
        &outcome,
        "C0EXAMPLE02",
        StreamStatus::Ok,
        Some("2026-10-25T18:00:00Z"),
    )
}

#[test]
fn external_fetch_keeps_other_streams_when_one_stream_errors() -> Result<()> {
    let root = scratch("external-stream-error")?;
    let request = fetch_request(&root)?;
    let outcome = run_external(&request, None, &[("ECHO_FAIL_STREAM", "C0EXAMPLE01")]);
    remove_scratch(&root)?;
    let outcome = outcome?;

    assert_stream(&outcome, "C0EXAMPLE01", StreamStatus::Error, None)?;
    assert_stream(
        &outcome,
        "C0EXAMPLE02",
        StreamStatus::Ok,
        Some("2026-10-25T18:00:00Z"),
    )
}

#[test]
fn protocol_violations_fail_every_unfinished_stream() -> Result<()> {
    for (label, changes) in [
        ("bad-scheme", vec![("ECHO_BAD_SCHEME", "1")]),
        ("missing-end", vec![("ECHO_NO_END", "C0EXAMPLE01")]),
        ("late-record", vec![("ECHO_LATE", "1")]),
        ("missing-hello", vec![("ECHO_NO_HELLO", "1")]),
    ] {
        let root = scratch(label)?;
        let request = fetch_request(&root)?;
        let outcome = run_external(&request, None, &changes);
        remove_scratch(&root)?;
        let outcome = outcome?;

        assert!(outcome.has_errors());
        if label == "missing-end" {
            assert_stream(&outcome, "C0EXAMPLE01", StreamStatus::Error, None)?;
            assert_stream(
                &outcome,
                "C0EXAMPLE02",
                StreamStatus::Ok,
                Some("2026-10-25T18:00:00Z"),
            )?;
        } else {
            assert_stream(&outcome, "C0EXAMPLE01", StreamStatus::Error, None)?;
            assert_stream(&outcome, "C0EXAMPLE02", StreamStatus::Error, None)?;
        }
    }
    Ok(())
}

#[test]
fn timed_out_source_marks_every_unfinished_stream_errored() -> Result<()> {
    let root = scratch("external-timeout")?;
    let request = fetch_request(&root)?;
    let outcome = run_external(&request, Some(1), &[("ECHO_SLEEP", "2")]);
    remove_scratch(&root)?;
    let outcome = outcome?;

    assert!(outcome.has_errors());
    assert!(outcome
        .error
        .as_deref()
        .is_some_and(|error| error.contains("timed out")));
    assert_stream(&outcome, "C0EXAMPLE01", StreamStatus::Error, None)?;
    assert_stream(&outcome, "C0EXAMPLE02", StreamStatus::Error, None)
}

#[test]
fn external_resolve_returns_records_for_requested_pointers() -> Result<()> {
    let root = scratch("external-resolve")?;
    let request = resolve_request(&root, "echo:x");
    let outcome = run_external(&request, None, &[]);
    remove_scratch(&root)?;
    let outcome = outcome?;

    assert!(!outcome.has_errors());
    assert_eq!(outcome.records.len(), 1);
    assert_eq!(
        outcome
            .records
            .first()
            .map(|record| record.pointer.as_str()),
        Some("echo:x")
    );
    Ok(())
}

#[test]
fn built_in_slack_conforms_against_fake_api() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking the test environment: {error}"))?;
    let root = scratch("slack-conformance")?;
    let window_end = timestamp("2026-10-25T18:00:00Z")?;
    let request = Request::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: serde_json::json!({
            "workspace": "acme",
            "channels": ["C0EXAMPLE01"],
        }),
        window_end,
        backfill_hours: 24,
        cursors: std::collections::BTreeMap::new(),
        discovered: Vec::new(),
        state_dir: root.join("state"),
    });
    let (api, server) = fake_slack_api(vec![
        r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
        r#"{"ok":true,"messages":[]}"#.to_owned(),
    ])?;
    let previous = std::env::var_os("SLACK_TOKEN");
    std::env::set_var("SLACK_TOKEN", "test-token");
    let source = source::SlackSource::with_api(api);
    let outcome = run_conformance(&source, &request);
    restore_environment("SLACK_TOKEN", previous);
    server
        .join()
        .map_err(|_| anyhow::anyhow!("fake Slack API server panicked"))??;
    remove_scratch(&root)?;

    assert!(!outcome.has_errors());
    assert_eq!(outcome.records.len(), 0);
    assert_stream(
        &outcome,
        "C0EXAMPLE01",
        StreamStatus::Ok,
        Some("2026-10-25T18:00:00Z"),
    )
}
