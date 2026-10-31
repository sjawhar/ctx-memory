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
use ctx_memory::{pointer::Pointer, store::Store};
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

/// The tree: its configuration, with one Slack source, and an open question whose quotes are
/// `claims`, each citing the asked message.
fn write_tree(root: &Path, claims: &[&str]) -> Result<()> {
    let tree = root.join("tree");
    write(
        &tree.join("ctx.toml"),
        &format!(
            "version = 1\nid = \"example\"\ndata_dir = {:?}\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
            root.join("data").display().to_string()
        ),
    )?;
    let mut provenance = String::new();
    if !claims.is_empty() {
        provenance.push_str("tier: 2\nquotes:\n");
        for claim in claims {
            writeln!(
                provenance,
                "  - quote: {claim:?}\n    source: \"{ASKED}\"\n    tier: 2"
            )?;
        }
    }
    write(
        &tree.join("questions/hire.md"),
        &format!(
            "---\ntype: question\nstatus: open\nrevisit_by: 2026-12-01\n{provenance}---\nWho knows the candidate.\n"
        ),
    )
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

/// The stored text of `pointer`, read straight from the store.
fn stored(root: &Path, pointer: &str) -> Result<String> {
    let (scheme, locator) = pointer.split_once(':').context("a canonical pointer")?;
    let store = Store::open(
        &root.join("state/example/store.sqlite3"),
        &root.join("data/sources.json"),
        &root.join("state/example/resolved"),
    )?;
    let record = store
        .get(&Pointer {
            scheme: scheme.to_owned(),
            locator: locator.to_owned(),
        })?
        .context("the store holds the message")?;
    Ok(record.text)
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

/// A quote taken from what `ctx resolve` served verifies against the stored message, and so
/// does one taken while the mention read as its id; a quote that names someone else does not.
#[test]
fn a_quote_from_the_served_or_the_stored_text_verifies() -> Result<()> {
    let served = "cc @Kim Lee looks like you reviewed his PRs";
    let old = "cc @U0EXAMPLEA looks like you reviewed his PRs";
    let root = scratch("quotes")?;
    fixture(&root, Some("Kim Lee"), &[served, old])?;
    let verified = ctx(&root, &["cite", "verify"])?;
    let other = scratch("quotes-other")?;
    fixture(
        &other,
        Some("Kim Lee"),
        &["cc @Ada Smith looks like you reviewed his PRs"],
    )?;
    let refused = ctx(&other, &["cite", "verify"])?;
    remove_scratch(&root)?;
    remove_scratch(&other)?;

    let verified_data = data(&verified)?;
    assert_eq!(
        verified_data
            .get("broken")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "{verified_data}"
    );
    assert_eq!(verified.status.code(), Some(0), "{verified_data}");
    let refused_data = data(&refused)?;
    assert_eq!(
        refused_data
            .get("broken")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "{refused_data}"
    );
    Ok(())
}
