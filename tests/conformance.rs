use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

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
const ECHO_ENV_NAMES: [&str; 7] = [
    "ECHO_FAIL_STREAM",
    "ECHO_BAD_SCHEME",
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
fn source_run_rejects_unimplemented_builtin_kinds() -> Result<()> {
    let root = scratch("builtin-source")?;
    let tree = root.join("tree");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "source", "run", "slack"])
        .output()
        .context("running unavailable built-in source")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).context("ctx error is not UTF-8")?;
    assert!(error.contains("slack"));
    assert!(error.contains("not yet available"));
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
