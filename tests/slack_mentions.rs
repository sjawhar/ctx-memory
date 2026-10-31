//! What ctx serves for a Slack message whose stored text shows a person it mentions as the bare
//! id, as one fetched before mentions were named does: `ctx resolve`, `ctx pending`,
//! `ctx delta` and the quote check read the message with the name the store holds for that
//! person, and the store and the window's files stay as the fetch wrote them.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::{pointer::Pointer, record::Record, store::Store};
use serde_json::{json, Value};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

const WINDOW: &str = "20261030T180000Z-0000";
/// A message the fetch stored before mentions were named: the text shows the person it asks
/// as the bare id, and `raw` keeps Slack's own text.
const ASKED: &str = "slack:C0EXAMPLE01/1792951800.000100";
const ASKED_RAW: &str =
    "Someone wants a job here. cc <@U0EXAMPLEA> looks like you reviewed his PRs. \
                         Any thoughts?";
const ASKED_STORED: &str =
    "Someone wants a job here. cc @U0EXAMPLEA looks like you reviewed his PRs. \
                            Any thoughts?";
/// A message the person it asks wrote, which is where the store learns their name.
const OWN: &str = "slack:C0EXAMPLE01/1792951860.000200";
/// A message fetched after mentions were named: the fetch named one person the store knows no
/// name for, and could name neither of the others.
const NAMED: &str = "slack:C0EXAMPLE01/1792951920.000300";
const NAMED_RAW: &str = "<@U0EXAMPLED> and <@U0EXAMPLEE|lin> and <@U0EXAMPLEF> to approve";
const NAMED_STORED: &str = "@Ada Lovelace and @lin and @U0EXAMPLEF to approve";

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-slack-mentions-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// One Slack record as a window's `records.jsonl` holds it.
fn record(pointer: &str, ts: &str, author: (&str, &str), text: &str, raw: &Value) -> Value {
    json!({
        "source": "slack",
        "record": {
            "pointer": pointer,
            "stream": "C0EXAMPLE01",
            "ts": ts,
            "label": "#general",
            "author": author.1,
            "author_id": author.0,
            "text": text,
            "aliases": [pointer.rsplit_once('/').map_or(pointer, |(_, ts)| ts)],
            "raw": raw,
        }
    })
}

/// A tree with one Slack source and one question that quotes the asked message with `claims`,
/// and one window that holds the three messages and a delta listing them.
///
/// `full_name` is the full name Slack sent with the asked person's own message, if it sent
/// one; their author line reads `Kim` either way.
fn fixture(root: &Path, full_name: Option<&str>, claims: &[&str]) -> Result<()> {
    write_tree(root, claims)?;
    let dir = root.join("data/windows").join(WINDOW);
    write(
        &dir.join("manifest.json"),
        &format!(
            "{{\"version\":1,\"window\":\"{WINDOW}\",\"generated_at\":\"2026-10-30T18:00:00Z\",\"window_end\":\"2026-10-30T18:00:00Z\",\"sources\":{{}},\"warnings\":[],\"exit_status\":\"ok\"}}"
        ),
    )?;
    let own_raw = match full_name {
        Some(full_name) => json!({
            "ts": "1792951860.000200",
            "user": "U0EXAMPLEA",
            "text": "I can look.",
            "user_profile": {"real_name": full_name, "display_name": "Kim"},
        }),
        None => json!({"ts": "1792951860.000200", "user": "U0EXAMPLEA", "text": "I can look."}),
    };
    let records = [
        record(
            ASKED,
            "2026-10-25T12:10:00Z",
            ("U0EXAMPLEB", "Grace Hopper"),
            ASKED_STORED,
            &json!({"ts": "1792951800.000100", "user": "U0EXAMPLEB", "text": ASKED_RAW}),
        ),
        record(
            OWN,
            "2026-10-25T12:11:00Z",
            ("U0EXAMPLEA", "Kim"),
            "I can look.",
            &own_raw,
        ),
        record(
            NAMED,
            "2026-10-25T12:12:00Z",
            ("U0EXAMPLEB", "Grace Hopper"),
            NAMED_STORED,
            &json!({"ts": "1792951920.000300", "user": "U0EXAMPLEB", "text": NAMED_RAW}),
        ),
    ];
    let mut jsonl = String::new();
    for line in &records {
        jsonl.push_str(&serde_json::to_string(line)?);
        jsonl.push('\n');
    }
    write(&dir.join("records.jsonl"), &jsonl)?;
    write(&dir.join("rendered.md"), "# slack\n")?;
    write_delta(&dir)
}

/// The tree: its configuration, with one Slack source, an open question the window's delta
/// matches, and one more open question for each of `claims`, `questions/quote-<n>.md`, whose
/// one quote is that claim, citing the asked message.
fn write_tree(root: &Path, claims: &[&str]) -> Result<()> {
    let tree = root.join("tree");
    write(
        &tree.join("ctx.toml"),
        &format!(
            "version = 1\nid = \"example\"\ndata_dir = {:?}\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
            root.join("data").display().to_string()
        ),
    )?;
    write(
        &tree.join("questions/hire.md"),
        "---\ntype: question\nstatus: open\nrevisit_by: 2026-12-01\n---\nWho knows the candidate.\n",
    )?;
    for (index, claim) in claims.iter().enumerate() {
        let mut question = String::from(
            "---\ntype: question\nstatus: open\nrevisit_by: 2026-12-01\ntier: 2\nquotes:\n",
        );
        writeln!(
            question,
            "  - quote: {claim:?}\n    source: \"{ASKED}\"\n    tier: 2\n---\nWho knows the candidate."
        )?;
        write(&tree.join(format!("questions/quote-{index}.md")), &question)?;
    }
    Ok(())
}

/// The window's delta, as a fetch wrote it: each message's excerpt cut from its stored text.
fn write_delta(dir: &Path) -> Result<()> {
    let message = |pointer: &str, ts: &str, excerpt: &str| {
        json!({
            "pointer": pointer,
            "label": "#general",
            "ts": ts,
            "author": "Grace Hopper",
            "is_reply": false,
            "thread": null,
            "excerpt": excerpt,
            "matches": [{"record": "questions/hire.md", "how": "pointer"}],
        })
    };
    let delta = json!({
        "version": 1,
        "window": WINDOW,
        "window_end": "2026-10-30T18:00:00Z",
        "generated_at": "2026-10-30T18:00:00Z",
        "previous_window": null,
        "new_messages": [
            message(ASKED, "2026-10-25T12:10:00Z", ASKED_STORED),
            message(NAMED, "2026-10-25T12:12:00Z", NAMED_STORED),
        ],
        "unreviewed": [{
            "record": "questions/hire.md",
            "kind": "question",
            "status": "open",
            "title": "hire",
            "owner": null,
            "last_reviewed": null,
            "replies": [{
                "pointer": ASKED,
                "ts": "2026-10-25T12:10:00Z",
                "author": "Grace Hopper",
                "how": "pointer",
                "excerpt": ASKED_STORED,
            }],
        }],
        "closed_since_previous": [],
        "opened_since_previous": [],
    });
    write(&dir.join("delta.json"), &serde_json::to_string(&delta)?)
}

fn ctx(root: &Path, arguments: &[&str]) -> Result<Output> {
    let tree = root.join("tree");
    let tree = tree.to_str().context("tree path is not valid UTF-8")?;
    Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree])
        .args(arguments)
        .env("CTX_STATE_DIR", root.join("state"))
        .env_remove("CTX_DATA_DIR")
        .env_remove("CTX_SERVER")
        .output()
        .with_context(|| format!("running ctx {}", arguments.join(" ")))
}

fn data(output: &Output) -> Result<Value> {
    let value: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "decoding ctx output: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    value
        .get("data")
        .cloned()
        .context("ctx output has a data envelope")
}

fn text_at<'v>(value: &'v Value, pointer: &str) -> Result<&'v str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("{pointer} in {value}"))
}

/// The store `ctx` keeps for the fixture tree under `root`.
fn open_store(root: &Path) -> Result<Store> {
    Store::open(
        &root.join("state/example/store.sqlite3"),
        &root.join("data/sources.json"),
        &root.join("state/example/resolved"),
    )
}

/// The stored record of `pointer`, read straight from the store.
fn stored_record(root: &Path, pointer: &str) -> Result<Record> {
    let (scheme, locator) = pointer.split_once(':').context("a canonical pointer")?;
    open_store(root)?
        .get(&Pointer {
            scheme: scheme.to_owned(),
            locator: locator.to_owned(),
        })?
        .context("the store holds the message")
}

/// The stored text of `pointer`, read straight from the store.
fn stored(root: &Path, pointer: &str) -> Result<String> {
    Ok(stored_record(root, pointer)?.text)
}

/// Stores `records`, each a line as [`record`] writes it, as a fetch at `fetched_at` would.
fn put(root: &Path, records: &[Value], fetched_at: &str) -> Result<()> {
    let records = records
        .iter()
        .map(|line| {
            serde_json::from_value::<Record>(line.get("record").cloned().unwrap_or_default())
                .context("decoding a test record")
        })
        .collect::<Result<Vec<_>>>()?;
    let fetched_at = DateTime::parse_from_rfc3339(fetched_at)?.with_timezone(&Utc);
    open_store(root)?.put_all("slack", &records, fetched_at)?;
    Ok(())
}

/// Runs `ctx cite verify` on the fixture tree under `root` and returns the files it reports a
/// broken quote in.
fn broken_quotes(root: &Path) -> Result<Vec<String>> {
    let verified = data(&ctx(root, &["cite", "verify"])?)?;
    let mut files = verified
        .get("broken")
        .and_then(Value::as_array)
        .with_context(|| format!("cite verify lists broken quotes: {verified}"))?
        .iter()
        .filter_map(|broken| broken.get("file").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Everything ctx serves for the asked message names the person it asks, by the full name a
/// message of theirs carries when Slack sent one, else by the name their author line gives;
/// the store and the window's files keep what the fetch wrote.
#[test]
fn a_message_stored_before_mentions_were_named_is_served_with_the_name() -> Result<()> {
    for (full_name, served) in [(Some("Kim Lee"), "@Kim Lee"), (None, "@Kim")] {
        let root = scratch("served")?;
        fixture(&root, full_name, &[])?;
        let records = root.join(format!("data/windows/{WINDOW}/records.jsonl"));
        let written = fs::read(&records)?;

        let resolved = ctx(&root, &["resolve", ASKED])?;
        let thread = ctx(&root, &["resolve", "--thread", NAMED])?;
        let pending = ctx(&root, &["pending"])?;
        let delta = ctx(&root, &["delta", "--window", WINDOW])?;
        let stored_after = stored(&root, ASKED)?;
        let files_after = fs::read(&records)?;
        remove_scratch(&root)?;

        let expected = format!("cc {served} looks like you reviewed his PRs. Any thoughts?");
        let resolved = data(&resolved)?;
        let text = text_at(&resolved, "/results/0/text")?;
        assert!(
            text.contains(&expected),
            "{full_name:?}: ctx resolve: {text}"
        );
        assert!(
            !text.contains("U0EXAMPLEA"),
            "{full_name:?}: ctx resolve: {text}"
        );
        let thread = data(&thread)?;
        assert!(
            text_at(&thread, "/results/0/text")?.contains(NAMED_STORED),
            "{full_name:?}: a mention no stored message names keeps the name the stored text \
             gives, else its label, else its id: {thread}"
        );
        for (command, answer) in [("pending", data(&pending)?), ("delta", data(&delta)?)] {
            let delta = if command == "pending" {
                answer.get("delta").context("pending lists a delta")?
            } else {
                &answer
            };
            for excerpt in ["/new_messages/0/excerpt", "/unreviewed/0/replies/0/excerpt"] {
                let excerpt = text_at(delta, excerpt)?;
                assert!(
                    excerpt.contains(&expected),
                    "{full_name:?}: ctx {command}: {excerpt}"
                );
            }
            assert_eq!(
                text_at(delta, "/new_messages/1/excerpt")?,
                NAMED_STORED,
                "{full_name:?}: ctx {command}"
            );
        }
        assert_eq!(
            stored_after, ASKED_STORED,
            "the store keeps the fetch's text"
        );
        assert_eq!(files_after, written, "the window keeps the fetch's records");
    }
    Ok(())
}

/// A quote's mention holds only as a spelling of the person the message mentions there: as
/// ctx served it, by a name the store holds for that id, by the id, or as Slack wrote it. A
/// quote that changes only the id, or only the first name, of the person does not hold, and
/// neither does one that stops inside the mention; one that stops before it does.
#[test]
fn a_quote_holds_only_where_its_mention_names_that_person() -> Result<()> {
    let root = scratch("quotes")?;
    fixture(
        &root,
        Some("Kim Lee"),
        &[
            "cc @Kim Lee looks like you reviewed his PRs",
            "cc @Kim looks like you reviewed his PRs",
            "cc @U0EXAMPLEA looks like you reviewed his PRs",
            "cc <@U0EXAMPLEA> looks like you reviewed his PRs",
            "cc @U0EXAMPLED Lee looks like you reviewed his PRs",
            "cc @U0EXAMPLED looks like you reviewed his PRs",
            "cc @Ada Lee looks like you reviewed his PRs",
            "Someone wants a job here. cc @U0EXAMPLE",
            "Someone wants a job here. cc <@U0EXAMPLE",
            "Someone wants a job here. cc",
        ],
    )?;
    let broken = broken_quotes(&root)?;
    remove_scratch(&root)?;

    assert_eq!(
        broken,
        [
            "questions/quote-4.md",
            "questions/quote-5.md",
            "questions/quote-6.md",
            "questions/quote-7.md",
            "questions/quote-8.md",
        ],
        "only the quotes that put another id or name in the mention, or stop inside it, are \
         broken"
    );
    Ok(())
}

/// A Slack record that keeps no `raw.text` is checked as its stored text reads: what looks
/// like a mention there may be typed text, so no name stands for it.
#[test]
fn a_stored_text_without_slack_s_own_text_is_checked_as_it_reads() -> Result<()> {
    let root = scratch("no-raw")?;
    write_tree(
        &root,
        &[
            "Please type @Kim Lee literally.",
            "Please type <@U0EXAMPLEA> literally.",
        ],
    )?;
    fs::create_dir_all(root.join("data"))?;
    put(
        &root,
        &[
            record(
                ASKED,
                "2026-10-25T12:10:00Z",
                ("U0EXAMPLEB", "Grace Hopper"),
                "Please type <@U0EXAMPLEA> literally.",
                &json!({}),
            ),
            record(
                OWN,
                "2026-10-25T12:11:00Z",
                ("U0EXAMPLEA", "Kim"),
                "Here.",
                &json!({
                    "ts": "1792951860.000200",
                    "user": "U0EXAMPLEA",
                    "text": "Here.",
                    "user_profile": {"real_name": "Kim Lee", "display_name": "Kim"},
                }),
            ),
        ],
        "2026-10-25T13:00:00Z",
    )?;
    let broken = broken_quotes(&root)?;
    remove_scratch(&root)?;

    assert_eq!(broken, ["questions/quote-0.md"]);
    Ok(())
}

/// A quote taken from what ctx served still holds once the person's name changes, and the
/// message itself is not edited: the store holds both names for that person's id. A name the
/// store held for nobody does not hold until it does.
#[test]
fn a_quote_holds_after_the_person_s_name_changes() -> Result<()> {
    let root = scratch("renamed")?;
    fixture(
        &root,
        Some("Kim Lee"),
        &[
            "cc @Kim Lee looks like you reviewed his PRs",
            "cc @Kimberly Lee looks like you reviewed his PRs",
        ],
    )?;
    let before = broken_quotes(&root)?;
    put(
        &root,
        &[record(
            "slack:C0EXAMPLE01/1792952100.000500",
            "2026-10-26T09:00:00Z",
            ("U0EXAMPLEA", "Kim"),
            "Back from leave.",
            &json!({
                "ts": "1792952100.000500",
                "user": "U0EXAMPLEA",
                "text": "Back from leave.",
                "user_profile": {"real_name": "Kimberly Lee", "display_name": "Kim"},
            }),
        )],
        "2026-10-26T09:05:00Z",
    )?;
    let after = broken_quotes(&root)?;
    let resolved = data(&ctx(&root, &["resolve", ASKED])?)?;
    let stored_after = stored(&root, ASKED)?;
    remove_scratch(&root)?;

    assert_eq!(before, ["questions/quote-1.md"]);
    assert_eq!(
        after,
        Vec::<String>::new(),
        "both names hold after the change"
    );
    let text = text_at(&resolved, "/results/0/text")?;
    assert!(
        text.contains("cc @Kimberly Lee looks like you reviewed his PRs"),
        "ctx resolve serves the newest full name: {text}"
    );
    assert_eq!(stored_after, ASKED_STORED, "the message was not edited");
    Ok(())
}

/// An edit that moves a mention to another person whom the fetch named alike leaves the
/// message's stored text as it was, so the store keeps the `raw` of the first version. A
/// record whose stored text names every person is served as stored, so ctx never names the
/// person that stale `raw` points at.
#[test]
fn a_record_that_names_every_person_is_served_as_stored() -> Result<()> {
    const EDITED: &str = "slack:C0EXAMPLE01/1792952000.000400";
    let root = scratch("retarget")?;
    write_tree(&root, &[])?;
    fs::create_dir_all(root.join("data"))?;
    let message = |mentioned: &str| {
        record(
            EDITED,
            "2026-10-25T12:20:00Z",
            ("U0EXAMPLEB", "Grace Hopper"),
            "@Kim please review the plan",
            &json!({
                "ts": "1792952000.000400",
                "user": "U0EXAMPLEB",
                "text": format!("<@{mentioned}> please review the plan"),
            }),
        )
    };
    let own = |pointer: &str, ts: &str, id: &str, full_name: &str| {
        record(
            pointer,
            "2026-10-25T12:00:00Z",
            (id, "Kim"),
            "Here.",
            &json!({
                "ts": ts,
                "user": id,
                "text": "Here.",
                "user_profile": {"real_name": full_name, "display_name": "Kim"},
            }),
        )
    };
    put(
        &root,
        &[
            message("U0EXAMPLEA"),
            own(
                "slack:C0EXAMPLE01/1792951000.000010",
                "1792951000.000010",
                "U0EXAMPLEA",
                "Kim Lee",
            ),
            own(
                "slack:C0EXAMPLE01/1792951000.000020",
                "1792951000.000020",
                "U0EXAMPLED",
                "Kim Park",
            ),
        ],
        "2026-10-25T13:00:00Z",
    )?;
    put(&root, &[message("U0EXAMPLED")], "2026-10-25T14:00:00Z")?;
    let kept = stored_record(&root, EDITED)?;
    let resolved = data(&ctx(&root, &["resolve", EDITED])?)?;
    remove_scratch(&root)?;

    assert_eq!(
        kept.raw.get("text").and_then(Value::as_str),
        Some("<@U0EXAMPLEA> please review the plan"),
        "the store keeps the first version's raw text"
    );
    let text = text_at(&resolved, "/results/0/text")?;
    assert!(
        text.ends_with("\n@Kim please review the plan"),
        "ctx resolve serves the stored text: {text}"
    );
    assert!(
        !text.contains("Kim Lee") && !text.contains("Kim Park"),
        "{text}"
    );
    Ok(())
}
