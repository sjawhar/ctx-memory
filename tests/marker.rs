use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-marker-{label}-test-{}-{suffix}",
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
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
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
        .env("CTX_SESSION", "test-session")
        .env("PATH", fixture_path()?);
    Ok(command)
}

fn fetch(tree: &Path, state: &Path, window_end: &str) -> Result<String> {
    let output = command(tree, state)?
        .args(["fetch", "--window-end", window_end])
        .output()
        .context("running ctx fetch")?;
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding fetch output")?;
    value
        .get("data")
        .and_then(|data| data.get("window"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("fetch output includes its completed window")
}

fn data(output: &Output) -> Result<Value> {
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding command output")?;
    value
        .get("data")
        .cloned()
        .context("command output has a data envelope")
}

fn error_message(output: &Output) -> Result<String> {
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding command output")?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("command output reports an error")
}

#[test]
fn marker_advance_rejects_older_window() -> Result<()> {
    let root = scratch("older-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let older = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let newer = fetch(&tree, &state, "2026-10-25T19:00:00Z")?;

    let advanced = command(&tree, &state)?
        .args(["marker", "advance", &newer])
        .output()
        .context("advancing the marker")?;
    let rejected = command(&tree, &state)?
        .args(["marker", "advance", &older])
        .output()
        .context("advancing the marker backwards")?;
    let shown = command(&tree, &state)?
        .args(["marker", "show"])
        .output()
        .context("showing the marker")?;
    let marker_file = state.join("example/marker.json").exists();
    remove_scratch(&root)?;

    assert_eq!(advanced.status.code(), Some(0));
    assert_eq!(
        data(&advanced)?
            .get("marker")
            .and_then(|marker| marker.get("window"))
            .and_then(Value::as_str),
        Some(newer.as_str())
    );
    assert_eq!(
        data(&advanced)?
            .get("marker")
            .and_then(|marker| marker.get("by"))
            .and_then(Value::as_str),
        Some("test-session")
    );
    assert!(marker_file, "the marker is stored next to the windows");
    assert_eq!(rejected.status.code(), Some(2));
    assert!(error_message(&rejected)?.contains("older"));
    assert_eq!(
        data(&shown)?
            .get("marker")
            .and_then(|marker| marker.get("window"))
            .and_then(Value::as_str),
        Some(newer.as_str())
    );
    Ok(())
}

#[test]
fn marker_advance_rejects_an_incomplete_window() -> Result<()> {
    let root = scratch("incomplete-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let window = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    // A fetch publishes a window by renaming the staged `.tmp-` directory it wrote
    // (`src/windows.rs` `complete`). Returning this one to that name leaves a window whose
    // files are all present but which no fetch completed.
    let windows = state.join("example/windows");
    fs::rename(
        windows.join(&window),
        windows.join(format!(".tmp-{window}")),
    )
    .context("returning the completed window to its staged name")?;

    let rejected = command(&tree, &state)?
        .args(["marker", "advance", &window])
        .output()
        .context("advancing the marker to a staged window")?;
    let marker = state.join("example/marker.json").exists();
    remove_scratch(&root)?;

    assert_eq!(rejected.status.code(), Some(2));
    assert!(error_message(&rejected)?.contains(&window));
    assert!(!marker, "a refused advance leaves the marker unset");
    Ok(())
}

#[test]
fn pending_lists_windows_after_marker() -> Result<()> {
    let root = scratch("pending")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let first = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let second = fetch(&tree, &state, "2026-10-25T19:00:00Z")?;

    let before = command(&tree, &state)?
        .arg("pending")
        .output()
        .context("listing pending windows")?;
    let advanced = command(&tree, &state)?
        .args(["marker", "advance", &first])
        .output()
        .context("advancing the marker")?;
    let after = command(&tree, &state)?
        .arg("pending")
        .output()
        .context("listing pending windows after the marker moved")?;
    let all = command(&tree, &state)?
        .args(["marker", "advance", &second])
        .output()
        .context("advancing the marker to the newest window")?;
    let empty = command(&tree, &state)?
        .arg("pending")
        .output()
        .context("listing pending windows with nothing left")?;
    remove_scratch(&root)?;

    assert_eq!(advanced.status.code(), Some(0));
    assert_eq!(all.status.code(), Some(0));
    for output in [&before, &after, &empty] {
        assert_eq!(output.status.code(), Some(0));
    }
    let windows = |output: &Output| -> Result<Vec<String>> {
        Ok(data(output)?
            .get("windows")
            .and_then(Value::as_array)
            .context("pending output lists windows")?
            .iter()
            .filter_map(|window| {
                window
                    .get("window")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect())
    };
    assert_eq!(windows(&before)?, vec![first.clone(), second.clone()]);
    assert_eq!(windows(&after)?, vec![second.clone()]);
    assert_eq!(windows(&empty)?, Vec::<String>::new());
    assert_eq!(
        data(&before)?
            .get("delta")
            .and_then(|delta| delta.get("new_messages"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "both windows carry the same pointer, so the combined delta reports it once"
    );
    assert_eq!(
        data(&empty)?.get("delta").and_then(Value::as_object),
        None,
        "no pending window means no combined delta"
    );
    Ok(())
}

/// A scheduled cycle has no tree and cannot ask where the share keeps a window, so `pending`
/// names each window's directory under the data directory, the one path the cycle and the
/// server agree on wherever each mounts the share.
#[test]
fn pending_names_each_window_directory_under_the_data_directory() -> Result<()> {
    let root = scratch("pending-dir")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let window = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let pending = command(&tree, &state)?
        .arg("pending")
        .output()
        .context("listing pending windows")?;
    let paths = command(&tree, &state)?
        .args(["state", "path"])
        .output()
        .context("reading the state paths")?;
    let data_dir = data(&paths)?
        .get("data_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("state path names the data directory")?;
    let dir = data(&pending)?
        .get("windows")
        .and_then(Value::as_array)
        .and_then(|windows| windows.first())
        .and_then(|first| first.get("dir"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let holds_records = dir
        .as_deref()
        .is_some_and(|dir| data_dir.join(dir).join("records.jsonl").is_file());
    remove_scratch(&root)?;

    assert_eq!(dir, Some(format!("windows/{window}")));
    assert!(holds_records, "the named directory holds no records.jsonl");
    Ok(())
}
