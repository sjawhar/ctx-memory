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

/// A tree whose echo source reads `streams`, with `ingest` as its `[ingest]` table.
fn tree_with(root: &Path, streams: &[&str], ingest: &str) -> Result<PathBuf> {
    let tree = root.join("tree");
    fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    let streams = streams
        .iter()
        .map(|stream| format!("\"{stream}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        tree.join("ctx.toml"),
        format!(
            "version = 1\nid = \"example\"\n\n[ingest]\n{ingest}\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [{streams}]\n"
        ),
    )
    .with_context(|| format!("writing {}", tree.join("ctx.toml").display()))?;
    Ok(tree)
}

/// Runs `ctx fetch` for the window that ends at `window_end`, with `env` set, and returns the
/// window it completed.
fn fetch_with(tree: &Path, state: &Path, window_end: &str, env: &[(&str, &str)]) -> Result<String> {
    let mut fetch = command(tree, state)?;
    fetch.args(["fetch", "--window-end", window_end]);
    for (name, value) in env {
        fetch.env(name, value);
    }
    let output = fetch.output().context("running ctx fetch")?;
    data(&output)?
        .get("window")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("fetch output includes its completed window")
}

/// The data directory `ctx state path` names.
fn data_dir(tree: &Path, state: &Path) -> Result<PathBuf> {
    let paths = command(tree, state)?
        .args(["state", "path"])
        .output()
        .context("reading the state paths")?;
    data(&paths)?
        .get("data_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("state path names the data directory")
}

/// `ctx pending`, with `env` set: each window's count of new messages, and the pointers of the
/// combined delta's.
fn pending_with(
    tree: &Path,
    state: &Path,
    env: &[(&str, &str)],
) -> Result<(Vec<u64>, Vec<String>)> {
    let mut pending = command(tree, state)?;
    pending.arg("pending");
    for (name, value) in env {
        pending.env(name, value);
    }
    let output = pending.output().context("listing pending windows")?;
    assert_eq!(output.status.code(), Some(0));
    let data = data(&output)?;
    let counts = data
        .get("windows")
        .and_then(Value::as_array)
        .context("pending output lists windows")?
        .iter()
        .filter_map(|window| window.get("new_messages").and_then(Value::as_u64))
        .collect();
    let pointers = data
        .get("delta")
        .and_then(|delta| delta.get("new_messages"))
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("pointer").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok((counts, pointers))
}

/// A source that delivers a message it delivered before, unchanged, hands the agent nothing
/// new: the second window leaves it out, and its manifest says how many it left out.
#[test]
fn a_message_fetched_again_unchanged_is_not_new() -> Result<()> {
    let root = scratch("refetched")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let second = fetch(&tree, &state, "2026-10-25T19:00:00Z")?;
    let (counts, pointers) = pending_with(&tree, &state, &[])?;
    let window = data_dir(&tree, &state)?.join("windows").join(&second);
    let records = fs::read_to_string(window.join("records.jsonl"))?;
    let manifest = fs::read_to_string(window.join("manifest.json"))?;
    remove_scratch(&root)?;

    assert_eq!(counts, vec![1, 0], "the second window has nothing new");
    assert_eq!(pointers, vec!["echo:C0EXAMPLE01/record".to_owned()]);
    assert!(
        records.trim().is_empty(),
        "the repeat is not in the window: {records}"
    );
    assert!(
        manifest.contains("1 message(s) were delivered again unchanged since an earlier window"),
        "{manifest}"
    );
    Ok(())
}

/// A reply to a message delivered before keeps that message in its window, as context for the
/// reply, and only the reply is new.
#[test]
fn a_reply_keeps_the_message_it_answers_as_context() -> Result<()> {
    let root = scratch("reply-context")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let threads = [("ECHO_THREAD_STATE", "1")];
    fetch_with(&tree, &state, "2026-10-25T18:00:00Z", &threads)?;
    let second = fetch_with(&tree, &state, "2026-10-25T19:00:00Z", &threads)?;
    let (counts, pointers) = pending_with(&tree, &state, &[])?;
    let records = fs::read_to_string(
        data_dir(&tree, &state)?
            .join("windows")
            .join(&second)
            .join("records.jsonl"),
    )?;
    remove_scratch(&root)?;

    assert_eq!(
        counts,
        vec![1, 1],
        "the reply is the second window's one new message"
    );
    assert!(pointers.contains(&"echo:C0EXAMPLE01/reply".to_owned()));
    let parent = records
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|line| {
            line.pointer("/record/pointer").and_then(Value::as_str)
                == Some("echo:C0EXAMPLE01/record")
        })
        .context("the answered message stays in the window")?;
    assert_eq!(
        parent.pointer("/record/context").and_then(Value::as_bool),
        Some(true),
        "it stays as context, not as a new message"
    );
    Ok(())
}

/// A stream the deployment excludes never reaches a window or a delta. An external source's
/// request carries no `exclude` field: one written for protocol 1 may refuse a field it does
/// not know, and the fetch drops what the patterns name all the same.
#[test]
fn an_excluded_stream_never_reaches_a_window_or_a_delta() -> Result<()> {
    let root = scratch("excluded-fetch")?;
    let tree = tree_with(&root, &["C0EXAMPLE01", "C0EXAMPLE02"], "")?;
    let state = root.join("state");
    let request = root.join("request.json");
    let request_path = request.to_str().context("scratch path is not UTF-8")?;
    let window = fetch_with(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[
            ("CTX_EXCLUDE", "echo:C0EXAMPLE02/*"),
            ("ECHO_REQUEST_OUT", request_path),
        ],
    )?;
    // Read without the exclusion: what was never fetched cannot come back.
    let (counts, pointers) = pending_with(&tree, &state, &[])?;
    let records = fs::read_to_string(
        data_dir(&tree, &state)?
            .join("windows")
            .join(&window)
            .join("records.jsonl"),
    )?;
    let sent = fs::read_to_string(&request)?;
    remove_scratch(&root)?;

    assert_eq!(counts, vec![1]);
    assert_eq!(pointers, vec!["echo:C0EXAMPLE01/record".to_owned()]);
    assert!(!records.contains("C0EXAMPLE02"), "{records}");
    assert!(
        !sent.contains(r#""exclude":"#),
        "an external source's request is the protocol 1 request it knows: {sent}"
    );
    Ok(())
}

/// An exclusion the tree configures after a window was fetched leaves that window's excluded
/// messages out of `pending` and `delta`, and leaves its files as they were.
#[test]
fn an_exclusion_shrinks_a_window_fetched_before_it() -> Result<()> {
    let root = scratch("excluded-later")?;
    let tree = tree_with(&root, &["C0EXAMPLE01", "C0EXAMPLE02"], "")?;
    let state = root.join("state");
    let window = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let delta_file = data_dir(&tree, &state)?
        .join("windows")
        .join(&window)
        .join("delta.json");
    let before = fs::read(&delta_file)?;
    let (all, _) = pending_with(&tree, &state, &[])?;
    fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[ingest]\nexclude = [\"echo:C0EXAMPLE02/*\"]\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\"]\n",
    )?;
    let (counts, pointers) = pending_with(&tree, &state, &[])?;
    let delta = command(&tree, &state)?
        .args(["delta", "--window", &window])
        .output()
        .context("recomputing the window's delta")?;
    let after = fs::read(&delta_file)?;
    remove_scratch(&root)?;

    assert_eq!(all, vec![2]);
    assert_eq!(counts, vec![1]);
    assert_eq!(pointers, vec!["echo:C0EXAMPLE01/record".to_owned()]);
    let recomputed = data(&delta)?
        .get("new_messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("pointer").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .context("delta lists its new messages")?;
    assert_eq!(recomputed, vec!["echo:C0EXAMPLE01/record".to_owned()]);
    assert_eq!(
        before, after,
        "the window's delta.json is as the fetch wrote it"
    );
    Ok(())
}

/// `ctx delta` prints the delta the fetch wrote and never computes one: a server answers it to
/// any reader, and computing a large window holds the server for minutes. A window with no
/// delta is refused, with the command that computes one.
#[test]
fn a_delta_read_prints_the_stored_delta_and_never_computes() -> Result<()> {
    let root = scratch("delta-read")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let window = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let delta_file = data_dir(&tree, &state)?
        .join("windows")
        .join(&window)
        .join("delta.json");
    let stored: Value = serde_json::from_slice(&fs::read(&delta_file)?)?;
    let read = command(&tree, &state)?
        .args(["delta", "--window", &window])
        .output()
        .context("reading the window's delta")?;
    fs::remove_file(&delta_file)?;
    let missing = command(&tree, &state)?
        .args(["delta", "--window", &window])
        .output()
        .context("reading a window with no delta")?;
    let computed = delta_file.exists();
    remove_scratch(&root)?;

    assert_eq!(read.status.code(), Some(0));
    assert_eq!(
        data(&read)?.get("generated_at"),
        stored.get("generated_at"),
        "the read is the delta the fetch wrote, not a new one"
    );
    assert_ne!(missing.status.code(), Some(0));
    assert!(error_message(&missing)?.contains("--write"));
    assert!(!computed, "a read writes no delta");
    Ok(())
}
