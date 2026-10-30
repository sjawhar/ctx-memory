mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::TryRecvError;
use std::thread;

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, TimeZone as _, Utc};
use ctx_memory::{
    atomic,
    config::TreeConfig,
    pointer::{Pointer, Schemes},
    record::Record,
    store::Store,
};
use serde_json::Value;
use support::fake_google::FakeGoogle;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

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

fn timestamp(seconds: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .context("constructing test timestamp")
}

fn record(pointer: &str, thread: Option<&str>, seconds: i64, text: &str) -> Result<Record> {
    Ok(Record {
        pointer: pointer.to_owned(),
        stream: "C0EXAMPLE01".to_owned(),
        ts: timestamp(seconds)?,
        label: Some("#general".to_owned()),
        author: Some("Alice".to_owned()),
        author_id: Some("U0EXAMPLE9".to_owned()),
        thread: thread.map(str::to_owned),
        title: None,
        url: None,
        text: text.to_owned(),
        aliases: Vec::new(),
        context: false,
        raw: Value::Null,
    })
}

fn pointer(text: &str) -> Pointer {
    Pointer {
        scheme: "slack".to_owned(),
        locator: text.to_owned(),
    }
}

/// The store of a tree whose state lives under `state_base`.
fn tree_store(state_base: &Path) -> Result<Store> {
    Store::open(
        &state_base.join("example/store.sqlite3"),
        &state_base.join("example/sources.json"),
        &state_base.join("example/resolved"),
    )
}

#[test]
fn generic_scheme_is_recognised_only_when_configured() {
    let schemes = Schemes::new(["linear".to_owned()]);
    assert_eq!(schemes.find_all("see linear:ENG-412/c-9f2e.").len(), 1);
    assert_eq!(schemes.find_all("note: http://x").len(), 0);
    assert_eq!(Schemes::new([]).find_all("linear:ENG-412").len(), 0);
}

#[test]
fn locator_does_not_swallow_trailing_punctuation() -> Result<()> {
    let schemes = Schemes::new(["linear".to_owned()]);
    let found = schemes
        .find_all("(linear:ENG-412)")
        .into_iter()
        .next()
        .context("finding configured pointer")?;
    assert_eq!(found.pointer.to_string(), "linear:ENG-412");
    Ok(())
}

#[test]
fn slack_permalink_maps_to_pointer_and_thread() -> Result<()> {
    let found = Schemes::new([])
        .find_all("https://acme.slack.com/archives/C0EXAMPLE01/p1792951800000100?thread_ts=1792951700.000100")
        .into_iter()
        .next()
        .context("finding Slack permalink")?;
    assert_eq!(
        found.pointer.to_string(),
        "slack:C0EXAMPLE01/1792951800.000100"
    );
    assert_eq!(
        found.thread.as_ref().map(ToString::to_string).as_deref(),
        Some("slack:C0EXAMPLE01/1792951700.000100")
    );
    Ok(())
}

#[test]
fn store_keeps_every_version_and_thread_orders_by_timestamp() -> Result<()> {
    let root = scratch("store")?;
    let store = Store::open(
        &root.join("store.sqlite3"),
        &root.join("sources.json"),
        &root.join("resolved"),
    )?;
    let fetched_at = DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?;
    let parent = "slack:C0EXAMPLE01/1700000000.000001";
    let reply_early = "slack:C0EXAMPLE01/1700000001.000001";
    let reply_late = "slack:C0EXAMPLE01/1700000002.000001";

    let records = vec![
        record(parent, None, 1_700_000_000, "first copy")?,
        record(reply_late, Some(parent), 1_700_000_002, "late reply")?,
        record(reply_early, Some(parent), 1_700_000_001, "early reply")?,
        record(parent, None, 1_700_000_003, "second copy")?,
    ];
    assert_eq!(store.put_all("slack", &records, fetched_at)?, 4);
    assert_eq!(store.put_all("slack", &records, fetched_at)?, 0);
    assert_eq!(
        store
            .get(&pointer("C0EXAMPLE01/1700000000.000001"))?
            .map(|found| found.text)
            .as_deref(),
        Some("second copy")
    );
    assert_eq!(
        store
            .versions(&pointer("C0EXAMPLE01/1700000000.000001"))?
            .into_iter()
            .map(|(_, found)| found.text)
            .collect::<Vec<_>>(),
        ["first copy", "second copy"]
    );
    let thread = store.thread(&pointer("C0EXAMPLE01/1700000000.000001"))?;
    let texts: Vec<_> = thread.into_iter().map(|found| found.text).collect();
    assert_eq!(texts, ["early reply", "late reply", "second copy"]);

    remove_scratch(&root)
}

#[test]
fn concurrent_reader_never_sees_a_partial_write() -> Result<()> {
    let root = scratch("concurrent-store")?;
    let store_path = root.join("store.sqlite3");
    let sources_path = root.join("sources.json");
    let resolved_path = root.join("resolved");
    let writer_store = Store::open(&store_path, &sources_path, &resolved_path)?;
    let reader_store = Store::open(&store_path, &sources_path, &resolved_path)?;
    let fetched_at = DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader_pointer = pointer("C0EXAMPLE01/1700000000.000000");

    let writer = thread::spawn(move || {
        let outcome = (|| -> Result<()> {
            for batch in 0..10 {
                let mut records = Vec::with_capacity(100);
                for offset in 0..100 {
                    let ordinal = batch * 100 + offset;
                    let pointer = format!("slack:C0EXAMPLE01/1700000000.{ordinal:06}");
                    records.push(record(
                        &pointer,
                        None,
                        1_700_000_000 + i64::from(ordinal),
                        "stored text",
                    )?);
                }
                assert_eq!(writer_store.put_all("slack", &records, fetched_at)?, 100);
            }
            Ok(())
        })();
        let _ = sender.send(outcome);
    });

    loop {
        reader_store.get(&reader_pointer)?;
        match receiver.try_recv() {
            Ok(result) => {
                result?;
                break;
            }
            Err(TryRecvError::Empty) => thread::yield_now(),
            Err(TryRecvError::Disconnected) => return Err(anyhow!("writer disconnected")),
        }
    }
    writer
        .join()
        .map_err(|_| anyhow!("writer thread panicked"))?;
    remove_scratch(&root)
}

#[test]
fn atomic_replace_replaces_complete_file_contents() -> Result<()> {
    let root = scratch("atomic")?;
    let path = root.join("state.json");
    std::fs::write(&path, b"old")?;
    atomic::write_replace(&path, b"replacement")?;
    assert_eq!(std::fs::read(&path)?, b"replacement");
    remove_scratch(&root)
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn run_ctx(tree: &Path, state_base: &Path, arguments: &[&str]) -> Result<std::process::Output> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    binary()
        .args(["--json", "--memory", tree])
        .args(arguments)
        .env("CTX_STATE_DIR", state_base)
        .output()
        .context("running ctx")
}

fn response_data(output: &std::process::Output) -> Result<Value> {
    serde_json::from_slice(&output.stdout).context("parsing ctx JSON output")
}

fn path_with(first: &Path) -> Result<std::ffi::OsString> {
    let path = std::env::var_os("PATH").context("reading PATH")?;
    let mut paths = vec![first.to_path_buf()];
    paths.extend(std::env::split_paths(&path));
    std::env::join_paths(paths).context("building fixture PATH")
}

fn echo_fixture_path() -> Result<std::ffi::OsString> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    path_with(&fixture)
}

#[test]
fn verify_reports_a_quote_missing_from_the_resolved_text() -> Result<()> {
    let root = scratch("resolve-verify")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    let document = tree.join("decision.md");
    std::fs::write(
        &document,
        "---\ntype: decision\nstatus: open\nquote: \"beta gamma\"\nsource: \"slack:C0EXAMPLE01/1700000000.000001\"\n---\nEvidence.\n",
    )?;
    let store = tree_store(&state_base)?;
    store.put_all(
        "slack",
        &[record(
            "slack:C0EXAMPLE01/1700000000.000001",
            None,
            1_700_000_000,
            "alpha beta gamma delta",
        )?],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;

    let resolved = run_ctx(
        &tree,
        &state_base,
        &["resolve", "--thread", "slack:C0EXAMPLE01/1700000000.000001"],
    )?;
    assert_eq!(resolved.status.code(), Some(0));
    let resolve_response = response_data(&resolved)?;
    let resolve_data = resolve_response
        .get("data")
        .context("resolve response has data")?;
    let results = resolve_data
        .get("results")
        .and_then(Value::as_array)
        .context("resolve response has results")?;
    let resolved_record = results.first().context("resolve response has one result")?;
    let resolved_text = resolved_record
        .get("text")
        .and_then(Value::as_str)
        .context("resolve response has text")?;
    assert!(resolved_text.contains("alpha beta gamma delta"));

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    assert_eq!(verified.status.code(), Some(0));
    let verification_data = response_data(&verified)?
        .get("data")
        .context("verification response has data")?
        .clone();
    assert_eq!(
        verification_data.get("checked").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        verification_data
            .get("broken")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    std::fs::write(
        &document,
        "---\ntype: decision\nstatus: open\nquote: \"omega\"\nsource: \"slack:C0EXAMPLE01/1700000000.000001\"\n---\nEvidence.\n",
    )?;
    let broken = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    assert_eq!(broken.status.code(), Some(1));
    let broken_data = response_data(&broken)?
        .get("data")
        .context("broken verification response has data")?
        .clone();
    let broken_entries = broken_data
        .get("broken")
        .and_then(Value::as_array)
        .context("broken verification response has broken records")?;
    let entry = broken_entries
        .first()
        .context("broken verification reports a missing quote")?;
    assert_eq!(
        entry.get("why").and_then(Value::as_str),
        Some("quote does not appear in the resolved text")
    );

    let state = run_ctx(&tree, &state_base, &["state", "path"])?;
    assert_eq!(state.status.code(), Some(0));
    let state_data = response_data(&state)?
        .get("data")
        .context("state response has data")?
        .clone();
    let expected_store = state_base
        .join("example/store.sqlite3")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        state_data.get("store").and_then(Value::as_str),
        Some(expected_store.as_str())
    );

    remove_scratch(&root)
}

#[test]
fn calendar_kind_uses_gcal_without_registering_calendar_as_generic() -> Result<()> {
    let schemes = Schemes::new([]);
    assert_eq!(
        schemes.find_all("calendar:x"),
        [] as [ctx_memory::pointer::Found; 0]
    );
    assert_eq!(schemes.find_all("gcal:primary/evt1").len(), 1);

    let root = scratch("calendar-scheme")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"calendar\"\nkind = \"calendar\"\n",
    )?;
    std::fs::write(
        tree.join("event.md"),
        "---\ntype: decision\nstatus: open\nquote: \"an event\"\n---\ncalendar:x\ngcal:primary/evt1\n",
    )?;

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    // A quote nothing could check fails `cite verify`, as it fails the writer.
    assert_eq!(verified.status.code(), Some(1));
    let data = response_data(&verified)?
        .get("data")
        .context("calendar verification response has data")?
        .clone();
    assert_eq!(data.get("checked").and_then(Value::as_u64), Some(0));
    let skipped = data
        .get("skipped")
        .and_then(Value::as_object)
        .context("calendar verification response has skipped schemes")?;
    assert_eq!(skipped.get("calendar"), None);
    assert_eq!(skipped.get("gcal").and_then(Value::as_u64), Some(1));

    remove_scratch(&root)
}

#[test]
fn external_source_requires_an_explicit_scheme() -> Result<()> {
    let root = scratch("external-scheme")?;
    std::fs::write(
        root.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"linear\"\nkind = \"linear\"\n",
    )?;

    let result = TreeConfig::load(&root);
    remove_scratch(&root)?;

    let error = result
        .err()
        .context("external source without a scheme unexpectedly loaded")?;
    assert!(format!("{error:#}").contains("scheme"));
    Ok(())
}

#[test]
fn sources_must_not_share_an_effective_scheme() -> Result<()> {
    let root = scratch("duplicate-scheme")?;
    std::fs::write(
        root.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack-one\"\nkind = \"slack\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"slack\"\n",
    )?;

    let result = TreeConfig::load(&root);
    remove_scratch(&root)?;

    let error = result
        .err()
        .context("duplicate effective source schemes unexpectedly loaded")?;
    let error = format!("{error:#}");
    assert!(error.contains("slack-one"));
    assert!(error.contains("echo"));
    Ok(())
}

#[test]
fn quote_in_a_sibling_reply_of_the_source_pointer_verifies() -> Result<()> {
    let root = scratch("quote-sibling")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"the sibling contains the evidence\"\nsource: \"slack:C0EXAMPLE01/1700000000.000002\"\n---\n",
    )?;
    let store = tree_store(&state_base)?;
    store.put_all(
        "slack",
        &[
            record(
                "slack:C0EXAMPLE01/1700000000.000001",
                None,
                1_700_000_000,
                "thread parent",
            )?,
            record(
                "slack:C0EXAMPLE01/1700000000.000002",
                Some("slack:C0EXAMPLE01/1700000000.000001"),
                1_700_000_001,
                "source reply context",
            )?,
            record(
                "slack:C0EXAMPLE01/1700000000.000003",
                Some("slack:C0EXAMPLE01/1700000000.000001"),
                1_700_000_002,
                "the sibling contains the evidence",
            )?,
        ],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    assert_eq!(verified.status.code(), Some(0));
    let data = response_data(&verified)?
        .get("data")
        .context("sibling verification response has data")?
        .clone();
    assert_eq!(
        data.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
    assert_eq!(
        data.get("quotes_unchecked").and_then(Value::as_u64),
        Some(0)
    );

    remove_scratch(&root)
}

#[test]
fn quote_matching_only_a_body_pointer_is_unchecked() -> Result<()> {
    let root = scratch("quote-body")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"body-only evidence\"\nsource: \"meetings/example/transcript.txt:12\"\n---\nContext: slack:C0EXAMPLE01/1700000000.000004\n",
    )?;
    let store = tree_store(&state_base)?;
    store.put_all(
        "slack",
        &[record(
            "slack:C0EXAMPLE01/1700000000.000004",
            None,
            1_700_000_000,
            "body-only evidence",
        )?],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    // A quote nothing could check fails `cite verify`, as it fails the writer.
    assert_eq!(verified.status.code(), Some(1));
    let data = response_data(&verified)?
        .get("data")
        .context("body-only verification response has data")?
        .clone();
    assert_eq!(
        data.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
    assert_eq!(
        data.get("quotes_unchecked").and_then(Value::as_u64),
        Some(1)
    );

    remove_scratch(&root)
}

#[test]
fn normalized_source_quote_matches_markup_mentions_and_ellipsis() -> Result<()> {
    let root = scratch("quote-normalized")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"portal hello \\\"world\\\" - @Alice ... later\"\nsource: \"slack:C0EXAMPLE01/1700000000.000005\"\n---\n",
    )?;
    let store = tree_store(&state_base)?;
    store.put_all(
        "slack",
        &[record(
            "slack:C0EXAMPLE01/1700000000.000005",
            None,
            1_700_000_000,
            "<https://example.com|Portal>  *HELLO*\n“WORLD” — <@U0EXAMPLE9> after later",
        )?],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    assert_eq!(verified.status.code(), Some(0));
    let data = response_data(&verified)?
        .get("data")
        .context("normalization verification response has data")?
        .clone();
    assert_eq!(
        data.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );

    remove_scratch(&root)
}

/// A quote taken while a mention read as its id still holds once the message reads with the
/// person's name, even where no earlier version of the message is stored: the check also
/// reads Slack's own text of the message, which the record keeps in `raw`. A labelled mention
/// used to read as `@U…|label`.
#[test]
fn a_quote_taken_while_a_mention_read_as_its_id_still_holds() -> Result<()> {
    for (raw, rendered, quote) in [
        (
            "Please ask <@U0EXAMPLE9> to approve the change",
            "Please ask @Grace Hopper to approve the change",
            "please ask @U0EXAMPLE9 to approve the change",
        ),
        (
            "Please ask <@U0EXAMPLE8|lin> to approve the change",
            "Please ask @Lin Okafor to approve the change",
            "please ask @U0EXAMPLE8|lin to approve the change",
        ),
    ] {
        let root = scratch("quote-mention-id")?;
        let tree = root.join("tree");
        let state_base = root.join("state");
        std::fs::create_dir(&tree)?;
        std::fs::write(
            tree.join("ctx.toml"),
            "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
        )?;
        std::fs::write(
            tree.join("decision.md"),
            format!("---\ntype: decision\nstatus: open\nquote: \"{quote}\"\nsource: \"slack:C0EXAMPLE01/1700000000.000005\"\n---\n"),
        )?;
        let mut message = record(
            "slack:C0EXAMPLE01/1700000000.000005",
            None,
            1_700_000_000,
            rendered,
        )?;
        message.raw = serde_json::json!({ "text": raw });
        let store = tree_store(&state_base)?;
        store.put_all(
            "slack",
            &[message],
            DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
        )?;

        let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
        remove_scratch(&root)?;

        let data = response_data(&verified)?
            .get("data")
            .context("verification response has data")?
            .clone();
        assert_eq!(
            data.get("broken").and_then(Value::as_array).map(Vec::len),
            Some(0),
            "{quote}: {data}"
        );
        assert_eq!(verified.status.code(), Some(0), "{quote}");
    }
    Ok(())
}

#[test]
fn live_verification_fills_a_partially_stored_source_thread() -> Result<()> {
    let root = scratch("quote-live-thread")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"live sibling evidence\"\nsource: \"echo:parent\"\n---\n",
    )?;
    let store = tree_store(&state_base)?;
    store.set_source_verbatim("echo", true)?;
    store.put_all(
        "echo",
        &[record("echo:parent", None, 1_700_000_000, "local parent")?],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;

    let local = binary()
        .args(["--json", "--memory", tree_text, "cite", "verify"])
        .env("CTX_STATE_DIR", &state_base)
        .env("PATH", echo_fixture_path()?)
        .output()
        .context("running local citation verification")?;
    assert_eq!(local.status.code(), Some(1));

    let live = binary()
        .args(["--json", "--memory", tree_text, "cite", "verify", "--live"])
        .env("CTX_STATE_DIR", &state_base)
        .env("PATH", echo_fixture_path()?)
        .env("ECHO_THREAD_SIBLING", "1")
        .output()
        .context("running live citation verification")?;
    remove_scratch(&root)?;

    assert_eq!(live.status.code(), Some(0));
    let live_response = response_data(&live)?;
    let data = live_response
        .get("data")
        .context("live verification response has data")?;
    assert_eq!(
        data.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
    Ok(())
}

#[test]
fn quote_mismatch_names_the_verbatim_source_that_was_checked() -> Result<()> {
    let root = scratch("quote-mismatch-source")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"calendar\"\nkind = \"calendar\"\ncalendars = [\"primary\"]\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"a different quote\"\nsource: \"gcal:primary/EVENT-EXAMPLE; slack:C0EXAMPLE01/1700000000.000006\"\n---\n",
    )?;
    let store = tree_store(&state_base)?;
    let fetched_at = DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?;
    store.put_all(
        "calendar",
        &[record(
            "gcal:primary/EVENT-EXAMPLE",
            None,
            1_700_000_000,
            "When: 2023-11-14T22:13:20Z",
        )?],
        fetched_at,
    )?;
    store.put_all(
        "slack",
        &[record(
            "slack:C0EXAMPLE01/1700000000.000006",
            None,
            1_700_000_000,
            "actual Slack text",
        )?],
        fetched_at,
    )?;

    let verified = run_ctx(&tree, &state_base, &["cite", "verify"])?;
    assert_eq!(verified.status.code(), Some(1));
    let verified_response = response_data(&verified)?;
    let data = verified_response
        .get("data")
        .context("mixed-source verification response has data")?;
    let broken = data
        .get("broken")
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .context("mixed-source verification has a broken entry")?;
    assert_eq!(
        broken.get("pointer").and_then(Value::as_str),
        Some("slack:C0EXAMPLE01/1700000000.000006")
    );

    remove_scratch(&root)
}

#[test]
fn live_verification_fills_a_partially_stored_gmail_thread() -> Result<()> {
    let root = scratch("quote-live-gmail-thread")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    let server = FakeGoogle::start(vec![(
        "/gmail/v1/users/me/threads/cccccccccccccccc",
        serde_json::json!({"messages": [
            {
                "id": "aaaaaaaaaaaaaaaa",
                "threadId": "cccccccccccccccc",
                "internalDate": "1792944000000",
                "payload": {
                    "headers": [{"name": "Subject", "value": "First message"}],
                    "mimeType": "text/plain",
                    "body": {"data": "bG9jYWwgcGFyZW50"}
                }
            },
            {
                "id": "bbbbbbbbbbbbbbbb",
                "threadId": "cccccccccccccccc",
                "internalDate": "1792947600000",
                "payload": {
                    "headers": [{"name": "Subject", "value": "Second message"}],
                    "mimeType": "text/plain",
                    "body": {"data": "bGl2ZSBHbWFpbCBzaWJsaW5n"}
                }
            }
        ]}),
    )])?;
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"gmail\"\nkind = \"gmail\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"live Gmail sibling\"\nsource: \"gmail:aaaaaaaaaaaaaaaa\"\n---\n",
    )?;
    let store = tree_store(&state_base)?;
    store.put_all(
        "gmail",
        &[record(
            "gmail:aaaaaaaaaaaaaaaa",
            Some("gmail:thread/cccccccccccccccc"),
            1_700_000_000,
            "local parent",
        )?],
        DateTime::from_timestamp(1_700_000_010, 0).context("fetch time is valid")?,
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;

    let local = binary()
        .args(["--json", "--memory", tree_text, "cite", "verify"])
        .env("CTX_STATE_DIR", &state_base)
        .env("CTX_GOOGLE_API_BASE", server.base())
        .env("CTX_CREDENTIAL_GOOGLE", "ya29.p7-gmail")
        .output()
        .context("running local Gmail citation verification")?;
    assert_eq!(local.status.code(), Some(1));

    let live = binary()
        .args(["--json", "--memory", tree_text, "cite", "verify", "--live"])
        .env("CTX_STATE_DIR", &state_base)
        .env("CTX_GOOGLE_API_BASE", server.base())
        .env("CTX_CREDENTIAL_GOOGLE", "ya29.p7-gmail")
        .output()
        .context("running live Gmail citation verification")?;
    remove_scratch(&root)?;

    assert_eq!(live.status.code(), Some(0));
    let response = response_data(&live)?;
    let data = response
        .get("data")
        .context("live Gmail verification response has data")?;
    assert_eq!(
        data.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
    Ok(())
}
