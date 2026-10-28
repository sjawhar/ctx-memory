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
        "ECHO_WAIT_FOR",
        "ECHO_THREAD_STATE",
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

/// Waits until `path` exists. A fetch starts `ctx`, loads the tree and starts its source
/// before the source writes anything, and on a loaded machine that takes seconds: two
/// seconds, which this waited once, failed every run under load.
fn wait_for_file(path: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            bail!("timed out waiting for {}", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn concurrent_fetch_returns_the_single_flight_error() -> Result<()> {
    let root = scratch("concurrent")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let request = root.join("request.json");
    let resume = root.join("resume");
    let mut first = command(&tree, &state)?;
    // The first fetch holds its lock until the test lets its source go on, so the second one
    // meets it however long either takes to start.
    first
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_REQUEST_OUT", &request)
        .env("ECHO_WAIT_FOR", &resume);
    let first = first.spawn().context("starting first fetch")?;
    let second =
        wait_for_file(&request).and_then(|()| fetch(&tree, &state, "2026-10-25T18:00:00Z"));
    fs::write(&resume, "")?;
    let first_output = first
        .wait_with_output()
        .context("waiting for first fetch")?;
    let second = second?;
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
fn fetch_windows_are_unique_and_each_moves_the_cursor() -> Result<()> {
    let root = scratch("unique-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let first = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let second = fetch(&tree, &state, "2026-10-25T19:00:00Z")?;
    let first_id = window_id(&first)?;
    let second_id = window_id(&second)?;
    let windows = state.join("example/windows");
    let completed = fs::read_dir(&windows)
        .with_context(|| format!("reading {}", windows.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    let cursors: Value = serde_json::from_slice(
        &fs::read(state.join("example/cursors.json")).context("reading written cursors")?,
    )?;
    remove_scratch(&root)?;

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_ne!(first_id, second_id);
    assert!(first_id.starts_with("20261025T180000Z-"));
    assert!(second_id.starts_with("20261025T190000Z-"));
    assert_eq!(completed.len(), 2);
    let stream = cursors
        .get("sources")
        .and_then(|sources| sources.get("echo"))
        .and_then(|source| source.get("C0EXAMPLE01"))
        .context("the echo stream has a stored cursor")?;
    assert_eq!(
        stream.get("cursor").and_then(Value::as_str),
        Some("2026-10-25T19:00:00Z")
    );
    assert_eq!(
        stream.get("window").and_then(Value::as_str),
        Some(second_id.as_str())
    );
    Ok(())
}

#[test]
fn data_dir_holds_windows_cursors_and_marker() -> Result<()> {
    let root = scratch("data-dir")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let data = root.join("data");
    let fetched = command(&tree, &state)?
        .env("CTX_DATA_DIR", &data)
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .output()
        .context("fetching into a shared data directory")?;
    let window = window_id(&fetched)?;
    let advanced = command(&tree, &state)?
        .env("CTX_DATA_DIR", &data)
        .args(["marker", "advance", &window])
        .output()
        .context("advancing the marker in a shared data directory")?;
    let layout = [
        (data.join(format!("windows/{window}/manifest.json")), true),
        (data.join("cursors.json"), true),
        (data.join("marker.json"), true),
        (state.join("example/store.sqlite3"), true),
        (state.join("example/windows"), false),
        (state.join("example/cursors.json"), false),
        (state.join("example/marker.json"), false),
    ]
    .map(|(path, expected)| (path.exists(), expected, path));
    remove_scratch(&root)?;

    assert_eq!(fetched.status.code(), Some(0));
    assert_eq!(advanced.status.code(), Some(0));
    for (found, expected, path) in &layout {
        assert_eq!(found, expected, "{} exists", path.display());
    }
    Ok(())
}

#[test]
fn an_interrupted_fetch_leaves_a_staged_window_and_no_cursor() -> Result<()> {
    let root = scratch("staged-window")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let request = root.join("request.json");
    let mut fetch = command(&tree, &state)?;
    fetch
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_REQUEST_OUT", &request)
        // Long enough that the test stops the fetch while it is staged, however slow the
        // machine; the source sleeps on alone after that, and exits when it wakes.
        .env("ECHO_SLEEP", "60");
    let mut fetch = fetch.spawn().context("starting fetch")?;
    wait_for_file(&request)?;
    fetch.kill().context("stopping staged fetch")?;
    let _ = fetch.wait().context("waiting for staged fetch")?;

    let pending = command(&tree, &state)?
        .arg("pending")
        .output()
        .context("listing pending windows")?;
    let windows = state.join("example/windows");
    let entries = fs::read_dir(&windows)
        .with_context(|| format!("reading {}", windows.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    let cursors = state.join("example/cursors.json").exists();
    remove_scratch(&root)?;

    assert!(entries.iter().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-20261025T180000Z-")
    }));
    assert!(!cursors, "an interrupted fetch moves no cursor");
    assert_eq!(pending.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&pending.stdout)?;
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("windows"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "a staged window is not pending"
    );
    Ok(())
}

#[test]
fn fetch_advances_ok_streams_only() -> Result<()> {
    let root = scratch("cursors")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let mut failing = command(&tree, &state)?;
    failing
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_FAIL_STREAM", "C0EXAMPLE02");
    let fetched = failing.output().context("fetching with a failed stream")?;
    let window = window_id(&fetched)?;
    let cursors: Value = serde_json::from_slice(
        &fs::read(state.join("example/cursors.json")).context("reading written cursors")?,
    )?;
    let shown = command(&tree, &state)?
        .args(["cursor", "show"])
        .output()
        .context("showing cursors")?;
    remove_scratch(&root)?;

    assert_eq!(fetched.status.code(), Some(1));
    let streams = cursors
        .get("sources")
        .and_then(|sources| sources.get("echo"))
        .context("the echo source has stored cursors")?;
    assert_eq!(
        streams
            .get("C0EXAMPLE01")
            .and_then(|stream| stream.get("cursor"))
            .and_then(Value::as_str),
        Some("2026-10-25T18:00:00Z")
    );
    assert_eq!(
        streams
            .get("C0EXAMPLE01")
            .and_then(|stream| stream.get("window"))
            .and_then(Value::as_str),
        Some(window.as_str())
    );
    assert!(
        streams.get("C0EXAMPLE02").is_none(),
        "a failed stream holds its own cursor"
    );
    assert_eq!(shown.status.code(), Some(0));
    let fetched: Value = serde_json::from_slice(&fetched.stdout)?;
    let advanced = fetched
        .get("data")
        .and_then(|data| data.get("cursors"))
        .and_then(|cursors| cursors.get("advanced"))
        .and_then(Value::as_array)
        .context("fetch reports the cursors it moved")?;
    assert_eq!(advanced.len(), 1);
    assert_eq!(
        advanced
            .first()
            .and_then(|stream| stream.get("stream"))
            .and_then(Value::as_str),
        Some("C0EXAMPLE01")
    );
    Ok(())
}

#[test]
fn an_older_window_end_does_not_move_a_newer_cursor() -> Result<()> {
    let root = scratch("older-window-end")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let newer = fetch(&tree, &state, "2026-10-25T19:00:00Z")?;
    let newer_id = window_id(&newer)?;
    let older = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let cursors: Value = serde_json::from_slice(
        &fs::read(state.join("example/cursors.json")).context("reading written cursors")?,
    )?;
    remove_scratch(&root)?;

    assert_eq!(newer.status.code(), Some(0));
    assert_eq!(
        older.status.code(),
        Some(0),
        "a late window completes instead of failing the fetch"
    );
    assert_eq!(
        cursors
            .get("sources")
            .and_then(|sources| sources.get("echo"))
            .and_then(|source| source.get("C0EXAMPLE01"))
            .and_then(|stream| stream.get("window"))
            .and_then(Value::as_str),
        Some(newer_id.as_str())
    );
    let older: Value = serde_json::from_slice(&older.stdout)?;
    let skipped = older
        .get("data")
        .and_then(|data| data.get("cursors"))
        .and_then(|cursors| cursors.get("skipped"))
        .and_then(Value::as_array)
        .context("fetch reports the cursors it held back")?;
    assert!(skipped.iter().any(|stream| {
        stream.get("stream") == Some(&Value::String("C0EXAMPLE01".to_owned()))
            && stream
                .get("why")
                .and_then(Value::as_str)
                .is_some_and(|why| why.contains("newer window"))
    }));
    Ok(())
}

#[test]
fn duplicate_stream_end_errors_the_stream_and_holds_its_cursor() -> Result<()> {
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
    let cursors = state.join("example/cursors.json").exists();
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
    assert!(!cursors, "the errored stream keeps no cursor");
    let fetched: Value = serde_json::from_slice(&fetched.stdout)?;
    assert!(fetched
        .get("data")
        .and_then(|data| data.get("cursors"))
        .and_then(|cursors| cursors.get("skipped"))
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

#[test]
fn cursor_advance_is_gone() -> Result<()> {
    let root = scratch("cursor-advance-gone")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let fetched = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let window = window_id(&fetched)?;
    let output = command(&tree, &state)?
        .args(["cursor", "advance", &window])
        .output()
        .context("running the removed cursor advance command")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .context("the removed command reports an error")?;
    assert!(
        message.contains("unrecognized subcommand 'advance'"),
        "{message}"
    );
    Ok(())
}

#[test]
fn source_state_lives_beside_the_windows() -> Result<()> {
    // A source keeps between runs what the windows do not hold: the Slack source keeps the
    // threads whose parent is older than the window it is fetching (`src/source/slack/fetch.rs`
    // `read_thread_index`/`fetch_channel`). A fetch on a machine that does not hold that file
    // reads no replies for those threads and still moves the stream's cursor past them, so the
    // replies are lost. That state must therefore travel with the windows in `data_dir`.
    let root = scratch("source-state")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let data = root.join("data");
    let request = root.join("request.json");
    let fetched = command(&tree, &state)?
        .env("CTX_DATA_DIR", &data)
        .env("ECHO_REQUEST_OUT", &request)
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .output()
        .context("fetching into a shared data directory")?;
    let recorded = fs::read(&request).context("reading the request the source received")?;
    remove_scratch(&root)?;

    assert_eq!(fetched.status.code(), Some(0));
    let recorded: Value = serde_json::from_slice(&recorded).context("decoding the request")?;
    let state_dir = recorded
        .get("state_dir")
        .and_then(Value::as_str)
        .context("the fetch request names the source state directory")?;
    assert!(
        Path::new(state_dir).starts_with(&data),
        "source state {state_dir} is not under the data directory {}",
        data.display()
    );
    Ok(())
}

#[test]
fn a_fetch_with_a_fresh_state_directory_still_reads_tracked_threads() -> Result<()> {
    // Every production fetch runs in a new container with an empty state directory. The reply
    // a source reaches only through the file its previous fetch wrote must still arrive, and
    // the first fetch has already moved the cursor past it.
    let root = scratch("fresh-state")?;
    let tree = tree(&root)?;
    let data = root.join("data");
    let first = command(&tree, &root.join("state-first"))?
        .env("CTX_DATA_DIR", &data)
        .env("ECHO_THREAD_STATE", "1")
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .output()
        .context("fetching on the machine that tracked the thread")?;
    let second = command(&tree, &root.join("state-second"))?
        .env("CTX_DATA_DIR", &data)
        .env("ECHO_THREAD_STATE", "1")
        .args(["fetch", "--window-end", "2026-10-25T19:00:00Z"])
        .output()
        .context("fetching on a machine with an empty state directory")?;
    let window = window_id(&second)?;
    let records = data.join(format!("windows/{window}/records.jsonl"));
    let records =
        fs::read_to_string(&records).with_context(|| format!("reading {}", records.display()))?;
    remove_scratch(&root)?;

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    let mut pointers = Vec::new();
    for line in records.lines() {
        let record: Value = serde_json::from_str(line).context("decoding a window record")?;
        pointers.push(
            record
                .get("record")
                .and_then(|record| record.get("pointer"))
                .and_then(Value::as_str)
                .context("a stored record names its pointer")?
                .to_owned(),
        );
    }
    assert!(
        pointers
            .iter()
            .any(|pointer| pointer == "echo:C0EXAMPLE01/reply"),
        "the reply to a tracked thread is lost on a fresh state directory: {pointers:?}"
    );
    Ok(())
}
