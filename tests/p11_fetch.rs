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
    ctx(tree, state, &["--json"])
}

/// `ctx` with `global` flags before its command, on `tree`, with only the echo knobs a test sets.
fn ctx(tree: &Path, state: &Path, global: &[&str]) -> Result<Command> {
    let tree = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_ctx"));
    command
        .args(global)
        .args(["--memory", tree])
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
        "ECHO_PID_OUT",
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
        // machine; the kernel kills the source when the fetch dies, and its `sleep` runs out.
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

/// The `backfill_hours` the source was asked for in a fetch ending at `window_end`.
fn requested_backfill(
    tree: &Path,
    state: &Path,
    window_end: &str,
    environment: &[(&str, &str)],
) -> Result<u64> {
    let request = state
        .parent()
        .context("the state directory has a parent")?
        .join(format!("request-{window_end}.json"));
    let mut fetch = command(tree, state)?;
    fetch
        .args(["fetch", "--window-end", window_end])
        .env("ECHO_REQUEST_OUT", &request);
    for (name, value) in environment {
        fetch.env(name, value);
    }
    fetch.output().context("running ctx fetch")?;
    let request: Value = serde_json::from_slice(
        &fs::read(&request).with_context(|| format!("reading {}", request.display()))?,
    )?;
    request
        .get("backfill_hours")
        .and_then(Value::as_u64)
        .context("the request names its backfill hours")
}

#[test]
fn a_stream_whose_first_read_failed_reads_its_next_backfill_from_where_that_began() -> Result<()> {
    let root = scratch("backfill-failed-stream")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    // C0EXAMPLE02 has no cursor, so it reads the backfill window, from 2026-10-24T18:00:00Z;
    // it fails, so nothing of that read is stored.
    let first = requested_backfill(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[("ECHO_FAIL_STREAM", "C0EXAMPLE02")],
    )?;
    // Three hours later, a window of the configured length would start at 21:00 and step over
    // the three hours before it; the backfill reaches back to 18:00 the day before.
    let second = requested_backfill(&tree, &state, "2026-10-25T21:00:00Z", &[])?;
    // That read was stored, so the next fetch reads the configured window again.
    let third = requested_backfill(&tree, &state, "2026-10-25T22:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!((first, second, third), (24, 27, 24));
    Ok(())
}

#[test]
fn a_backfill_reaches_back_to_the_first_read_that_was_not_stored() -> Result<()> {
    let root = scratch("backfill-repeated-failure")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let failing = [("ECHO_FAIL_STREAM", "C0EXAMPLE02")];
    let first = requested_backfill(&tree, &state, "2026-10-25T18:00:00Z", &failing)?;
    let second = requested_backfill(&tree, &state, "2026-10-25T19:00:00Z", &failing)?;
    // Half an hour more than a whole hour: the window reaches back past that start rather than
    // short of it.
    let third = requested_backfill(&tree, &state, "2026-10-25T20:30:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!((first, second, third), (24, 25, 27));
    Ok(())
}

#[test]
fn a_source_that_failed_before_naming_its_streams_reads_its_next_backfill_from_where_it_began(
) -> Result<()> {
    let root = scratch("backfill-no-hello")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    requested_backfill(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[("ECHO_NO_HELLO", "1")],
    )?;
    let second = requested_backfill(&tree, &state, "2026-10-25T20:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!(second, 26);
    Ok(())
}

#[test]
fn the_fetch_after_an_interrupted_one_reads_its_backfill_from_where_that_began() -> Result<()> {
    let root = scratch("backfill-interrupted")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let request = root.join("interrupted-request.json");
    let mut interrupted = command(&tree, &state)?;
    interrupted
        .args(["fetch", "--window-end", "2026-10-25T18:00:00Z"])
        .env("ECHO_REQUEST_OUT", &request)
        .env("ECHO_SLEEP", "60");
    let mut interrupted = interrupted.spawn().context("starting fetch")?;
    wait_for_file(&request)?;
    interrupted.kill().context("stopping the fetch")?;
    let _ = interrupted
        .wait()
        .context("waiting for the stopped fetch")?;
    let next = requested_backfill(&tree, &state, "2026-10-25T19:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!(next, 25);
    Ok(())
}

#[test]
fn a_failed_stream_that_has_a_cursor_leaves_the_backfill_alone() -> Result<()> {
    let root = scratch("backfill-cursor-failure")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    requested_backfill(&tree, &state, "2026-10-25T18:00:00Z", &[])?;
    // C0EXAMPLE02 holds its cursor, so the next fetch reads on from it, not from the backfill.
    requested_backfill(
        &tree,
        &state,
        "2026-10-25T19:00:00Z",
        &[("ECHO_FAIL_STREAM", "C0EXAMPLE02")],
    )?;
    let third = requested_backfill(&tree, &state, "2026-10-25T21:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!(third, 24);
    Ok(())
}

#[test]
fn a_source_removed_and_added_back_reads_from_its_unstored_start() -> Result<()> {
    let root = scratch("backfill-removed-source")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let configured = fs::read_to_string(tree.join("ctx.toml"))?;
    requested_backfill(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[("ECHO_FAIL_STREAM", "C0EXAMPLE02")],
    )?;
    // The source is out of the configuration for one fetch. Its cursors stay in `cursors.json`,
    // and so does its backfill start.
    fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"other\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    fetch(&tree, &state, "2026-10-25T19:00:00Z")?;
    fs::write(tree.join("ctx.toml"), configured)?;
    let back = requested_backfill(&tree, &state, "2026-10-25T21:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!(back, 27);
    Ok(())
}

/// The warnings of the window a fetch ending at `window_end` completed.
fn window_warnings(tree: &Path, state: &Path, window_end: &str) -> Result<Vec<String>> {
    let fetched = fetch(tree, state, window_end)?;
    let window = window_id(&fetched)?;
    let manifest: Value = serde_json::from_slice(
        &fs::read(state.join(format!("example/windows/{window}/manifest.json")))
            .context("reading the window manifest")?,
    )?;
    Ok(manifest
        .get("warnings")
        .and_then(Value::as_array)
        .context("the manifest lists its warnings")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

#[test]
fn a_widened_backfill_is_named_in_the_window() -> Result<()> {
    let root = scratch("backfill-warning")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    requested_backfill(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[("ECHO_FAIL_STREAM", "C0EXAMPLE02")],
    )?;
    let warnings = window_warnings(&tree, &state, "2026-10-25T21:00:00Z")?;
    remove_scratch(&root)?;

    assert!(
        warnings.iter().any(|warning| {
            warning.starts_with("source echo:") && warning.contains("2026-10-24T18:00:00Z")
        }),
        "{warnings:?}"
    );
    Ok(())
}

#[test]
fn a_source_whose_streams_all_have_cursors_names_no_backfill_in_its_window() -> Result<()> {
    let root = scratch("backfill-no-warning")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    fetch(&tree, &state, "2026-10-25T12:00:00Z")?;
    // The source fails before it names its streams, but every stream it has holds a cursor, so
    // no stream read the backfill.
    requested_backfill(
        &tree,
        &state,
        "2026-10-25T18:00:00Z",
        &[("ECHO_NO_HELLO", "1")],
    )?;
    let warnings = window_warnings(&tree, &state, "2026-10-25T21:00:00Z")?;
    remove_scratch(&root)?;

    assert!(
        !warnings.iter().any(|warning| warning.contains("backfill")),
        "{warnings:?}"
    );
    Ok(())
}

#[test]
fn a_backfill_file_this_build_cannot_read_stops_the_fetch_before_it_stages_a_window() -> Result<()>
{
    let root = scratch("backfill-unreadable")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    fs::create_dir_all(state.join("example"))?;
    fs::write(
        state.join("example/backfill.json"),
        r#"{"version":2,"sources":{}}"#,
    )?;
    let fetched = fetch(&tree, &state, "2026-10-25T18:00:00Z")?;
    let windows = state.join("example/windows");
    let staged = windows.exists() && fs::read_dir(&windows)?.next().is_some();
    remove_scratch(&root)?;

    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&fetched.stdout),
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_ne!(fetched.status.code(), Some(0), "{said}");
    assert!(said.contains("unsupported backfill version 2"), "{said}");
    assert!(!staged, "the fetch staged a window");
    Ok(())
}

#[test]
fn a_backfill_file_with_a_field_this_build_does_not_know_still_holds_its_start() -> Result<()> {
    let root = scratch("backfill-unknown-field")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    fs::create_dir_all(state.join("example"))?;
    // As a later build might write it.
    fs::write(
        state.join("example/backfill.json"),
        r#"{"version":1,"sources":{"echo":"2026-10-24T18:00:00Z"},"streams":{}}"#,
    )?;
    let backfill = requested_backfill(&tree, &state, "2026-10-25T21:00:00Z", &[])?;
    remove_scratch(&root)?;

    assert_eq!(backfill, 27);
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

#[test]
fn a_fetch_prints_the_warnings_of_its_window() -> Result<()> {
    let root = scratch("fetch-warnings")?;
    let tree = tree(&root)?;
    let state = root.join("state");
    let failing = ("ECHO_FAIL_STREAM", "C0EXAMPLE02");
    requested_backfill(&tree, &state, "2026-10-25T18:00:00Z", &[failing])?;
    // Stream 02 fails again, so its backfill start is held through both later fetches.
    let plain = ctx(&tree, &state, &[])?
        .args(["fetch", "--window-end", "2026-10-25T21:00:00Z"])
        .env(failing.0, failing.1)
        .output()
        .context("running a plain ctx fetch")?;
    let json = fetch(&tree, &state, "2026-10-25T22:00:00Z")?;
    remove_scratch(&root)?;

    let warning = "source echo: its backfill reaches back to 2026-10-24T18:00:00Z, where a read an earlier fetch did not store began";
    let printed = String::from_utf8_lossy(&plain.stdout);
    assert!(
        printed
            .lines()
            .any(|line| line == format!("warning: {warning}")),
        "{printed}"
    );
    let json: Value = serde_json::from_slice(&json.stdout)?;
    let warnings = json
        .pointer("/data/warnings")
        .and_then(Value::as_array)
        .context("the JSON answer lists the window's warnings")?;
    assert!(
        warnings
            .iter()
            .any(|listed| listed.as_str() == Some(warning)),
        "{json}"
    );
    Ok(())
}

/// Stopping a fetch, observed through `/proc`.
#[cfg(target_os = "linux")]
mod stopping {
    use super::*;

    /// The state letter and process group of `pid` in `/proc`, or `None` once it has gone.
    fn process(pid: &str) -> Option<(char, String)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name, in parentheses, may hold spaces; the fields after it do not.
        let (_, after) = stat.rsplit_once(')')?;
        let mut fields = after.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let group = fields.nth(1)?.to_owned();
        Some((state, group))
    }

    /// Whether `pid` still runs. A zombie has stopped, and waits only for its parent to read it.
    fn running(pid: &str) -> bool {
        process(pid).is_some_and(|(state, _)| state != 'Z')
    }

    /// Whether any process of process group `group` still runs.
    fn group_running(group: &str) -> Result<bool> {
        for entry in fs::read_dir("/proc")? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|name| name.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            if process(pid).is_some_and(|(state, of)| state != 'Z' && of == group) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Waits up to 20 seconds for `stopped` to hold: ample for a signal to land on a loaded
    /// machine, and far short of the ten minutes the echo source waits.
    fn eventually(mut stopped: impl FnMut() -> Result<bool>) -> Result<bool> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if stopped()? {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(50));
        }
        stopped()
    }

    /// The command line of a fetch.
    const FETCH: &[&str] = &["fetch", "--window-end", "2026-10-25T18:00:00Z"];

    /// A `ctx` running an echo source that has started a ten-minute `sleep`. Dropping it kills
    /// whatever of the two is left, so a failing test leaves nothing running.
    struct Waiting {
        ctx: std::process::Child,
        /// The source's process id, which is also its process group.
        source: String,
        /// The `sleep` the source started.
        sleeper: String,
    }

    impl Drop for Waiting {
        fn drop(&mut self) {
            let _ = self.ctx.kill();
            let _ = self.ctx.wait();
            // Only a group this test knows the host id of; an empty `source` names none.
            if !self.source.is_empty() {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", self.source)])
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }

    /// Starts `command` with an echo source that sleeps ten minutes, far longer than
    /// `eventually` waits, so a source that stops did not just finish; returns once the source
    /// has started its `sleep`. Each of `signals` starts with `disposition`: the test harness may
    /// run with a signal ignored, as a background job runs with SIGINT ignored, and an ignored
    /// signal stays ignored across exec.
    fn start(
        mut command: Command,
        root: &Path,
        signals: &[libc::c_int],
        disposition: libc::sighandler_t,
    ) -> Result<Waiting> {
        use std::os::unix::process::CommandExt as _;

        let pid_file = root.join("source.pid");
        command
            .env("ECHO_PID_OUT", &pid_file)
            .env("ECHO_SLEEP", "600")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let signals = signals.to_vec();
        // `signal` is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                for signal in &signals {
                    libc::signal(*signal, disposition);
                }
                Ok(())
            });
        }
        let ctx = command.spawn().context("starting ctx")?;
        let mut waiting = Waiting {
            ctx,
            source: String::new(),
            sleeper: String::new(),
        };
        wait_for_file(&pid_file)?;
        let named = fs::read_to_string(&pid_file)?;
        let mut ids = named.split_whitespace();
        ids.next()
            .context("the source names itself")?
            .clone_into(&mut waiting.source);
        ids.next()
            .context("the source names its sleep")?
            .clone_into(&mut waiting.sleeper);
        Ok(waiting)
    }

    /// `ctx <arguments>` on a fresh tree under `root`, as [`start`] runs it.
    fn waiting(
        root: &Path,
        arguments: &[&str],
        signals: &[libc::c_int],
        disposition: libc::sighandler_t,
    ) -> Result<Waiting> {
        let tree = tree(root)?;
        let mut command = command(&tree, &root.join("state"))?;
        command.args(arguments);
        start(command, root, signals, disposition)
    }

    fn send(signal: &str, pid: &str) -> Result<()> {
        let sent = Command::new("kill")
            .args([format!("-{signal}").as_str(), "--", pid])
            .status()
            .context("running kill")?;
        if !sent.success() {
            bail!("kill -{signal} {pid} failed");
        }
        Ok(())
    }

    /// Whether neither the source, nor the `sleep` it started, nor anything else of its group
    /// still runs.
    fn all_stopped(waiting: &Waiting) -> Result<bool> {
        Ok(!running(&waiting.source)
            && !running(&waiting.sleeper)
            && !group_running(&waiting.source)?)
    }

    #[test]
    fn a_fetch_asked_to_stop_stops_its_source_and_what_the_source_started() -> Result<()> {
        use std::os::unix::process::ExitStatusExt as _;

        for (name, number) in [
            ("TERM", libc::SIGTERM),
            ("INT", libc::SIGINT),
            ("HUP", libc::SIGHUP),
        ] {
            let root = scratch(&format!("stop-{name}"))?;
            let mut fetch = waiting(&root, FETCH, &[number], libc::SIG_DFL)?;
            send(name, &fetch.ctx.id().to_string())?;
            let status = fetch.ctx.wait().context("waiting for the stopped fetch")?;
            let stopped = eventually(|| all_stopped(&fetch))?;
            drop(fetch);
            remove_scratch(&root)?;

            assert_eq!(status.signal(), Some(number), "the fetch ends by SIG{name}");
            assert!(stopped, "SIG{name} left the source or its sleep running");
        }
        Ok(())
    }

    #[test]
    fn a_fetch_started_with_an_interrupt_ignored_keeps_ignoring_it() -> Result<()> {
        let root = scratch("stop-ignored")?;
        let mut fetch = waiting(&root, FETCH, &[libc::SIGINT], libc::SIG_IGN)?;
        send("INT", &fetch.ctx.id().to_string())?;
        thread::sleep(Duration::from_secs(1));
        let still = fetch.ctx.try_wait()?.is_none() && running(&fetch.source);
        send("TERM", &fetch.ctx.id().to_string())?;
        let _ = fetch.ctx.wait()?;
        let stopped = eventually(|| all_stopped(&fetch))?;
        drop(fetch);
        remove_scratch(&root)?;

        assert!(still, "an ignored SIGINT stopped the fetch or its source");
        assert!(stopped, "SIGTERM left the source or its sleep running");
        Ok(())
    }

    #[test]
    fn a_fetch_killed_outright_still_stops_its_source() -> Result<()> {
        let root = scratch("stop-killed")?;
        let mut fetch = waiting(&root, FETCH, &[], libc::SIG_DFL)?;
        send("KILL", &fetch.ctx.id().to_string())?;
        let _ = fetch.ctx.wait()?;
        // A killed fetch runs no code of its own, so only the source itself is told, by the
        // kernel; what the source started is left to it.
        let stopped = eventually(|| Ok(!running(&fetch.source)))?;
        drop(fetch);
        remove_scratch(&root)?;

        assert!(stopped, "a killed fetch left its source running");
        Ok(())
    }

    #[test]
    fn a_source_run_asked_to_stop_stops_its_source() -> Result<()> {
        let root = scratch("stop-source-run")?;
        let mut run = waiting(
            &root,
            &["source", "run", "echo", "--op", "fetch"],
            &[libc::SIGTERM],
            libc::SIG_DFL,
        )?;
        send("TERM", &run.ctx.id().to_string())?;
        let _ = run.ctx.wait()?;
        let stopped = eventually(|| all_stopped(&run))?;
        drop(run);
        remove_scratch(&root)?;

        assert!(stopped, "SIGTERM left the source or its sleep running");
        Ok(())
    }

    /// The flags that run a command as the first process of a new PID namespace, as a container
    /// runs its entry point, with no privilege; `--kill-child` ends the namespace with `unshare`.
    const FIRST_PROCESS: &[&str] = &[
        "--user",
        "--map-root-user",
        "--pid",
        "--fork",
        "--kill-child",
    ];

    #[test]
    fn a_fetch_that_is_the_first_process_of_its_namespace_ends_when_asked_to_stop() -> Result<()> {
        use std::os::unix::process::ExitStatusExt as _;

        let usable = Command::new("unshare")
            .args(FIRST_PROCESS)
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !usable {
            eprintln!("skipped: this machine cannot run unshare {FIRST_PROCESS:?}");
            return Ok(());
        }
        let root = scratch("stop-first-process")?;
        let tree = tree(&root)?;
        let state = root.join("state");
        let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
        let mut command = Command::new("unshare");
        command
            .args(FIRST_PROCESS)
            .args([env!("CARGO_BIN_EXE_ctx"), "--json", "--memory", tree_text])
            .args(FETCH)
            .env("CTX_STATE_DIR", &state)
            .env("PATH", fixture_path()?);
        let mut fetch = start(command, &root, &[libc::SIGTERM], libc::SIG_DFL)?;
        // The source names itself and its `sleep` by their ids inside the namespace, which name
        // other processes, or none, on this host. Killing `unshare` (`--kill-child`) ends the
        // whole namespace, so the guard needs no group to kill.
        fetch.source.clear();
        fetch.sleeper.clear();
        // The namespace's first process is `ctx`, the only child of `unshare`. A process that is
        // first in its namespace gets only the signals it has a handler for, and a default
        // action it asks for on itself does nothing.
        let unshare = fetch.ctx.id();
        let ctx = fs::read_to_string(format!("/proc/{unshare}/task/{unshare}/children"))?
            .trim()
            .to_owned();
        send("TERM", &ctx)?;
        let ended = eventually(|| Ok(fetch.ctx.try_wait()?.is_some()))?;
        let status = fetch.ctx.try_wait()?;
        let published = fs::read_dir(state.join("example/windows"))?
            .filter_map(Result::ok)
            .any(|entry| !entry.file_name().to_string_lossy().starts_with('.'));
        drop(fetch);
        remove_scratch(&root)?;

        assert!(
            ended,
            "SIGTERM did not end a fetch that is first in its namespace"
        );
        assert!(
            status.is_some_and(|status| status.code() == Some(128 + libc::SIGTERM)
                || status.signal() == Some(libc::SIGTERM)),
            "{status:?}"
        );
        assert!(
            !published,
            "the stopped fetch went on and published its window"
        );
        Ok(())
    }
}
