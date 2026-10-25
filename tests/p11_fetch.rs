use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-p11-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn fixture_path() -> Result<PathBuf> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let inherited = std::env::var("PATH").context("PATH is not set")?;
    Ok(PathBuf::from(format!("{}:{inherited}", fixtures.display())))
}

fn tree(root: &Path) -> Result<PathBuf> {
    let tree = root.join("tree");
    fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\"]\n",
    )
    .with_context(|| format!("writing {}", tree.join("ctx.toml").display()))?;
    Ok(tree)
}

fn command(tree: &Path, state: &Path) -> Result<Command> {
    let tree = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_ctx"));
    command
        .args(["--json", "--memory", tree])
        .env("CTX_STATE_DIR", state)
        .env("PATH", fixture_path()?);
    for name in [
        "ECHO_AFTER_END",
        "ECHO_BAD_SCHEME",
        "ECHO_DUPLICATE_END",
        "ECHO_ERROR_CURSOR",
        "ECHO_EXIT",
        "ECHO_FAIL_STREAM",
        "ECHO_LATE",
        "ECHO_NO_END",
        "ECHO_NO_HELLO",
        "ECHO_REQUEST_OUT",
        "ECHO_SLEEP",
    ] {
        command.env_remove(name);
    }
    Ok(command)
}

fn fetch(tree: &Path, state: &Path, window_end: &str) -> Result<Output> {
    command(tree, state)?
        .args(["fetch", "--window-end", window_end])
        .output()
        .context("running ctx fetch")
}

fn window_id(output: &Output) -> Result<String> {
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding fetch output")?;
    value
        .get("data")
        .and_then(|data| data.get("window"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("fetch output includes its completed window")
}

fn wait_for_file(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("timed out waiting for {}", path.display())
}

#[test]
fn concurrent_fetch_returns_the_single_flight_error() -> Result<()> {
    let root = scratch("concurrent")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let request = root.join("request.json");
    let mut first = command(&tree, &state)?;
    first
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_REQUEST_OUT", &request)
        .env("ECHO_SLEEP", "2");
    let first = first.spawn().context("starting first fetch")?;
    wait_for_file(&request)?;

    let second = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let first_output = first
        .wait_with_output()
        .context("waiting for first fetch")?;
    remove_scratch(&root)?;

    assert!(first_output.status.success());
    assert_eq!(second.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&second.stdout)?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .context("second fetch reports an error")?;
    assert!(message.contains("fetch already running"));
    Ok(())
}

#[test]
fn fetch_windows_are_unique_and_do_not_advance_cursors() -> Result<()> {
    let root = scratch("unique-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let first = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let second = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let first_id = window_id(&first)?;
    let second_id = window_id(&second)?;
    let windows = state.join("example/windows");
    let completed = fs::read_dir(&windows)
        .with_context(|| format!("reading {}", windows.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_ne!(first_id, second_id);
    assert!(first_id.starts_with("20261025T180000Z-"));
    assert!(second_id.starts_with("20261025T180000Z-"));
    assert_eq!(completed.len(), 2);
    assert!(!state.join("example/cursors.json").exists());
    remove_scratch(&root)?;
    Ok(())
}

#[test]
fn staged_window_is_not_eligible_for_cursor_advance() -> Result<()> {
    let root = scratch("staged-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let request = root.join("request.json");
    let mut fetch = command(&tree, &state)?;
    fetch
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_REQUEST_OUT", &request)
        .env("ECHO_SLEEP", "2");
    let mut fetch = fetch.spawn().context("starting fetch")?;
    wait_for_file(&request)?;
    fetch.kill().context("stopping staged fetch")?;
    let _ = fetch.wait().context("waiting for staged fetch")?;

    let output = command(&tree, &state)?
        .args(["cursor", "advance", "newest"])
        .output()
        .context("advancing the newest window")?;
    let windows = state.join("example/windows");
    let entries = fs::read_dir(&windows)
        .with_context(|| format!("reading {}", windows.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(entries.iter().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-20261025T180000Z-")
    }));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .context("cursor advance reports an error")?;
    assert!(message.contains("no completed fetch windows"));
    Ok(())
}

#[test]
fn cursor_advance_applies_only_ok_streams_and_rejects_older_windows() -> Result<()> {
    let root = scratch("cursors")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let mut old_command = command(&tree, &state)?;
    old_command
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_FAIL_STREAM", "C0EXAMPLE02");
    let old = old_command.output().context("fetching older window")?;
    let old_id = window_id(&old)?;
    assert_eq!(old.status.code(), Some(1));
    assert!(!state.join("example/cursors.json").exists());

    let advanced = command(&tree, &state)?
        .args(["cursor", "advance", &old_id])
        .output()
        .context("advancing older window")?;
    assert!(advanced.status.success());
    let cursors: Value = serde_json::from_slice(
        &fs::read(state.join("example/cursors.json")).context("reading written cursors")?,
    )?;
    assert_eq!(
        cursors
            .get("sources")
            .and_then(|sources| sources.get("echo"))
            .and_then(|source| source.get("C0EXAMPLE01"))
            .and_then(|stream| stream.get("cursor"))
            .and_then(Value::as_str),
        Some("2026-10-25T18:00:00Z")
    );
    assert!(cursors
        .get("sources")
        .and_then(|sources| sources.get("echo"))
        .and_then(|source| source.get("C0EXAMPLE02"))
        .is_none());

    let mut new_command = command(&tree, &state)?;
    new_command
        .args(["fetch", "--window-end", "2026-10-25T19:00:00Z"])
        .env("ECHO_FAIL_STREAM", "C0EXAMPLE02");
    let new = new_command.output().context("fetching newer window")?;
    let new_id = window_id(&new)?;
    assert_eq!(new.status.code(), Some(1));
    let advanced = command(&tree, &state)?
        .args(["cursor", "advance", &new_id])
        .output()
        .context("advancing newer window")?;
    assert!(advanced.status.success());

    let show = command(&tree, &state)?
        .args(["cursor", "show"])
        .output()
        .context("showing cursors")?;
    assert!(show.status.success());
    let rejected = command(&tree, &state)?
        .args(["cursor", "advance", &old_id])
        .output()
        .context("advancing stale window")?;
    remove_scratch(&root)?;

    assert_eq!(rejected.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&rejected.stdout)?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .context("stale advance reports an error")?;
    assert!(message.contains("older"));
    Ok(())
}

#[test]
fn duplicate_stream_end_errors_manifest_stream_and_cursor_advance_skips_it() -> Result<()> {
    let root = scratch("duplicate-stream-end")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let mut fetch = command(&tree, &state)?;
    fetch
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_DUPLICATE_END", "C0EXAMPLE01");
    let fetched = fetch.output().context("fetching duplicate stream end")?;
    let window = window_id(&fetched)?;
    let manifest: Value = serde_json::from_slice(&fs::read(
        state.join(format!("example/windows/{window}/manifest.json")),
    )?)?;
    let advanced = command(&tree, &state)?
        .args(["cursor", "advance", &window])
        .output()
        .context("advancing duplicate stream window")?;
    remove_scratch(&root)?;

    let stream = manifest
        .get("sources")
        .and_then(|sources| sources.get("echo"))
        .and_then(|source| source.get("streams"))
        .and_then(|streams| streams.get("C0EXAMPLE01"))
        .context("manifest has the duplicate stream")?;
    assert_eq!(fetched.status.code(), Some(1));
    assert_eq!(
        stream.get("status"),
        Some(&Value::String("error".to_owned()))
    );
    assert_eq!(stream.get("cursor_next"), Some(&Value::Null));
    assert!(stream
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| error.contains("more than once")));
    assert!(advanced.status.success());
    let advanced: Value = serde_json::from_slice(&advanced.stdout)?;
    assert!(advanced
        .get("data")
        .and_then(|data| data.get("advanced"))
        .and_then(Value::as_array)
        .is_some_and(|streams| {
            streams.iter().all(|stream| {
                stream.get("source") != Some(&Value::String("echo".to_owned()))
                    || stream.get("stream") != Some(&Value::String("C0EXAMPLE01".to_owned()))
            })
        }));
    assert!(advanced
        .get("data")
        .and_then(|data| data.get("skipped"))
        .and_then(Value::as_array)
        .is_some_and(|streams| {
            streams.iter().any(|stream| {
                stream.get("source") == Some(&Value::String("echo".to_owned()))
                    && stream.get("stream") == Some(&Value::String("C0EXAMPLE01".to_owned()))
                    && stream.get("why") == Some(&Value::String("stream errored".to_owned()))
            })
        }));
    Ok(())
}
