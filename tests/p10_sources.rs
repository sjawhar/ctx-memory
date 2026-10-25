use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::{
    config::{SourceConfig, TreeConfig},
    source::{self, FetchRequest, Request, Session, Source, SourceOutcome, SourceRef},
};
use serde_json::{json, Value};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);
static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-p10-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn fake_gws(root: &Path) -> Result<PathBuf> {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let program = bin.join("gws");
    std::fs::write(
        &program,
        r#"#!/bin/sh
set -eu

if [ -n "${P10_GWS_ARGS_OUT-}" ]; then
    printf '%s\n' "$@" >> "$P10_GWS_ARGS_OUT"
    printf '\n' >> "$P10_GWS_ARGS_OUT"
fi

case "$1:$2:$3" in
    calendar:events:list)
        printf '%s\n' '{"items":[{"id":"EXAMPLE-EVENT","updated":"2026-10-25T17:00:00Z","summary":"Calendar event","start":{"dateTime":"2026-10-25T17:00:00Z"}}]}'
        ;;
    drive:files:list)
        printf '%s\n' '{"files":[]}'
        ;;
    gmail:users:threads)
        printf '%s\n' '{"messages":[{"id":"bbbbbbbbbbbbbbbb","threadId":"cccccccccccccccc","internalDate":"1792947600000","payload":{"headers":[{"name":"Subject","value":"Second message"}],"mimeType":"text/plain","body":{"data":"c2Vjb25kIEdtYWlsIG1lc3NhZ2U"}}},{"id":"aaaaaaaaaaaaaaaa","threadId":"cccccccccccccccc","internalDate":"1792944000000","payload":{"headers":[{"name":"Subject","value":"First message"}],"mimeType":"text/plain","body":{"data":"Zmlyc3QgR21haWwgbWVzc2FnZQ"}}}]}'
        ;;
    gmail:users:messages)
        printf '%s\n' '{"id":"aaaaaaaaaaaaaaaa","threadId":"cccccccccccccccc","internalDate":"1792944000000","payload":{"headers":[{"name":"Subject","value":"First message"}],"mimeType":"text/plain","body":{"data":"Zmlyc3QgR21haWwgbWVzc2FnZQ"}}}'
        ;;
    *)
        printf '%s\n' '{"items":[]}'
        ;;
esac
"#,
    )
    .with_context(|| format!("writing {}", program.display()))?;
    let mut permissions = std::fs::metadata(&program)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&program, permissions)
        .with_context(|| format!("marking {} executable", program.display()))?;
    Ok(bin)
}

fn source_spec(kind: &str, settings: &str) -> Result<SourceConfig> {
    let config: TreeConfig = toml::from_str(&format!(
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"{kind}\"\nkind = \"{kind}\"\n{settings}"
    ))?;
    config
        .sources
        .into_iter()
        .next()
        .context("test configuration contains its source")
}

fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn fetch_request(root: &Path, kind: &str, scheme: &str, config: Value) -> Result<Request> {
    Ok(Request::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: kind.to_owned(),
            kind: kind.to_owned(),
            scheme: scheme.to_owned(),
        },
        config,
        window_end: timestamp("2026-10-25T18:00:00Z")?,
        backfill_hours: 24,
        cursors: BTreeMap::new(),
        discovered: Vec::new(),
        state_dir: root.join("state"),
    }))
}

fn run_conformance(source: &dyn Source, request: &Request) -> SourceOutcome {
    let schemes = ctx_memory::pointer::Schemes::new([request.source().scheme.clone()]);
    let mut session = Session::new(request, &schemes);
    let run = source.run(request, &mut |message| session.accept(message).map(|_| ()));
    session.finish(run)
}

#[test]
fn calendar_fetch_uses_the_configured_calendar_id() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking test environment: {error}"))?;
    let root = scratch("calendar-id")?;
    let bin = fake_gws(&root)?;
    let calls = root.join("calls");
    let inherited_path = std::env::var("PATH").context("PATH is not set")?;
    std::env::set_var("PATH", format!("{}:{inherited_path}", bin.display()));
    std::env::set_var("P10_GWS_ARGS_OUT", &calls);

    let spec = source_spec("calendar", "calendars = [\"team@example.com\"]\n")?;
    let request = fetch_request(
        &root,
        "calendar",
        "gcal",
        json!({ "calendars": ["team@example.com"] }),
    )?;
    let source = source::open(&spec, "gcal")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let calls = std::fs::read_to_string(&calls).context("reading gws calls")?;

    std::env::set_var("PATH", inherited_path);
    std::env::remove_var("P10_GWS_ARGS_OUT");
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert!(calls.contains("team@example.com"));
    Ok(())
}

#[test]
fn drive_folder_with_all_initial_listing_has_no_time_filter() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking test environment: {error}"))?;
    let root = scratch("drive-initial-all")?;
    let bin = fake_gws(&root)?;
    let calls = root.join("calls");
    let inherited_path = std::env::var("PATH").context("PATH is not set")?;
    std::env::set_var("PATH", format!("{}:{inherited_path}", bin.display()));
    std::env::set_var("P10_GWS_ARGS_OUT", &calls);

    let spec = source_spec(
        "drive",
        "folders = [{ id = \"FOLDER-EXAMPLE\", initial = \"all\" }]\n",
    )?;
    let request = fetch_request(
        &root,
        "drive",
        "drive",
        json!({ "folders": [{ "id": "FOLDER-EXAMPLE", "initial": "all" }] }),
    )?;
    let source = source::open(&spec, "drive")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let calls = std::fs::read_to_string(&calls).context("reading gws calls")?;

    std::env::set_var("PATH", inherited_path);
    std::env::remove_var("P10_GWS_ARGS_OUT");
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert!(calls.contains("'FOLDER-EXAMPLE' in parents and trashed = false"));
    assert!(!calls.contains("modifiedTime >"));
    Ok(())
}

#[test]
fn gmail_thread_resolution_works_without_a_slack_token_and_orders_messages() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow::anyhow!("locking test environment: {error}"))?;
    let root = scratch("gmail-resolve")?;
    let bin = fake_gws(&root)?;
    let tree = root.join("tree");
    std::fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"gmail\"\nkind = \"gmail\"\n",
    )?;
    let inherited_path = std::env::var("PATH").context("PATH is not set")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args([
            "--json",
            "--memory",
            tree.to_str().context("tree path is not valid UTF-8")?,
            "resolve",
            "gmail:thread/cccccccccccccccc",
        ])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .env_remove("SLACK_TOKEN")
        .output()
        .context("running Gmail thread resolution")?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    let text = response
        .get("data")
        .and_then(|data| data.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .context("Gmail thread resolution returned text")?;
    let first = text
        .find("first Gmail message")
        .context("first message present")?;
    let second = text
        .find("second Gmail message")
        .context("second message present")?;
    assert!(first < second);
    Ok(())
}
