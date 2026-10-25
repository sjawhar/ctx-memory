use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{DateTime, NaiveDate, Utc};
use ctx_memory::{
    config::TreeConfig,
    delta::{self, Delta, Match, OpenFlag, OpenRecord, OpenSnapshot, Owners, ReplyRef},
    pointer::Schemes,
    record::Record,
    source,
    tree::Tree,
    windows::{Manifest, SourcedRecord, Status, Window, WindowId},
};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-p12-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(root: &Path) -> Result<()> {
    fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .with_context(|| format!("parsing test timestamp {value}"))
}

fn date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("parsing test date {value}"))
}

#[derive(Clone, Copy)]
struct RecordOptions<'a> {
    label: Option<&'a str>,
    thread: Option<&'a str>,
    aliases: &'a [&'a str],
    context: bool,
}

fn record(
    pointer: &str,
    stream: &str,
    timestamp_value: &str,
    text: &str,
    options: RecordOptions<'_>,
) -> Result<Record> {
    Ok(Record {
        pointer: pointer.to_owned(),
        stream: stream.to_owned(),
        ts: timestamp(timestamp_value)?,
        label: options.label.map(str::to_owned),
        author: Some("Bob".to_owned()),
        author_id: None,
        thread: options.thread.map(str::to_owned),
        title: None,
        url: None,
        text: text.to_owned(),
        aliases: options
            .aliases
            .iter()
            .map(|alias| (*alias).to_owned())
            .collect(),
        context: options.context,
        raw: Value::Null,
    })
}

fn window(root: &Path, name: &str, records: Vec<SourcedRecord>) -> Result<Window> {
    let directory = root.join(name);
    fs::create_dir_all(&directory).with_context(|| format!("creating {}", directory.display()))?;
    let id = WindowId::parse(name)?;
    let manifest = Manifest {
        version: 1,
        window: name.to_owned(),
        generated_at: timestamp("2026-10-25T12:00:00Z")?,
        window_end: timestamp("2026-10-25T12:00:00Z")?,
        sources: BTreeMap::new(),
        warnings: Vec::new(),
        exit_status: Status::Ok,
    };
    write(
        &directory.join("manifest.json"),
        &serde_json::to_string_pretty(&manifest).context("serializing test manifest")?,
    )?;
    let mut jsonl = String::new();
    for sourced in records {
        jsonl.push_str(&serde_json::to_string(&sourced).context("serializing test record")?);
        jsonl.push('\n');
    }
    write(&directory.join("records.jsonl"), &jsonl)?;
    write(&directory.join("rendered.md"), "")?;
    Ok(Window { id, dir: directory })
}

fn configured_tree(root: &Path) -> Result<(Tree, TreeConfig)> {
    write(
        &root.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\ndiscover_field = \"ops_channel\"\nstreams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\"]\n",
    )?;
    let config = TreeConfig::load(root)?;
    let tree = Tree::load(root, &config)?;
    Ok((tree, config))
}

fn match_for<'a>(delta: &'a Delta, pointer: &str) -> Result<&'a ctx_memory::delta::NewMessage> {
    delta
        .new_messages
        .iter()
        .find(|message| message.pointer == pointer)
        .with_context(|| format!("delta has message {pointer}"))
}

#[test]
fn delta_matches_thread_alias_quote_owner_and_owned_streams() -> Result<()> {
    let root = scratch("match-kinds")?;
    let (tree, config, current) = matching_fixture(&root)?;
    let owners = Owners::discover(&tree, &config.sources);
    let schemes = source::configured_schemes(&config.sources)?;
    let delta = delta::compute(
        &tree,
        &schemes,
        &owners,
        &current,
        None,
        timestamp("2026-10-25T12:01:00Z")?,
    )?;
    remove_scratch(&root)?;

    let gmail = match_for(&delta, "gmail:aaaaaaaaaaaaaaaa")?;
    assert_eq!(
        gmail.matches,
        vec![Match {
            record: "questions/thread.md".to_owned(),
            how: "pointer".to_owned(),
        }]
    );
    let reply = match_for(&delta, "echo:C0EXAMPLE02/reply")?;
    assert_eq!(
        reply.matches,
        vec![
            Match {
                record: "entities/alice.md".to_owned(),
                how: "owner".to_owned(),
            },
            Match {
                record: "questions/alias.md".to_owned(),
                how: "alias".to_owned(),
            },
            Match {
                record: "questions/quote.md".to_owned(),
                how: "quote".to_owned(),
            },
            Match {
                record: "questions/stream.md".to_owned(),
                how: "stream".to_owned(),
            },
        ]
    );
    let unowned = match_for(&delta, "echo:C0EXAMPLE01/unowned")?;
    assert_eq!(unowned.matches, [] as [ctx_memory::delta::Match; 0]);
    Ok(())
}

fn matching_fixture(root: &Path) -> Result<(Tree, TreeConfig, Window)> {
    write(
        &root.join("questions/thread.md"),
        "---\ntype: question\nstatus: open\n---\nA cited conversation is gmail:thread/bbbbbbbbbbbbbbbb.\n",
    )?;
    write(
        &root.join("questions/quote.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-24\nquote: five parent words appear here\n---\n",
    )?;
    write(
        &root.join("questions/alias.md"),
        "---\ntype: question\nstatus: open\n---\nThe provider alias is ALIAS-EXAMPLE-01.\n",
    )?;
    write(
        &root.join("entities/alice.md"),
        "---\ntype: person\nname: Alice\nstatus: active\nops_channel: C0EXAMPLE02 #ops-alice\n---\n",
    )?;
    write(
        &root.join("questions/stream.md"),
        "---\ntype: question\nstatus: open\n---\nDiscuss this in #ops-alice.\n",
    )?;
    write(
        &root.join("questions/unowned.md"),
        "---\ntype: question\nstatus: open\n---\nDiscuss this in #general.\n",
    )?;
    let (tree, config) = configured_tree(root)?;
    let current = window(
        &root.join("state/example/windows"),
        "20261025T120000Z-0001",
        vec![
            SourcedRecord {
                source: "gmail".to_owned(),
                record: record(
                    "gmail:aaaaaaaaaaaaaaaa",
                    "me",
                    "2026-10-25T10:00:00Z",
                    "A message in the cited thread.",
                    RecordOptions {
                        label: Some("gmail"),
                        thread: Some("gmail:thread/bbbbbbbbbbbbbbbb"),
                        aliases: &[],
                        context: false,
                    },
                )?,
            },
            SourcedRecord {
                source: "echo".to_owned(),
                record: record(
                    "echo:C0EXAMPLE02/parent",
                    "C0EXAMPLE02",
                    "2026-10-25T09:00:00Z",
                    "Five parent words appear here exactly.",
                    RecordOptions {
                        label: Some("#ops-alice"),
                        thread: None,
                        aliases: &[],
                        context: true,
                    },
                )?,
            },
            SourcedRecord {
                source: "echo".to_owned(),
                record: record(
                    "echo:C0EXAMPLE02/reply",
                    "C0EXAMPLE02",
                    "2026-10-25T10:00:00Z",
                    "A reply in Alice's stream.",
                    RecordOptions {
                        label: Some("#ops-alice"),
                        thread: Some("echo:C0EXAMPLE02/parent"),
                        aliases: &["ALIAS-EXAMPLE-01"],
                        context: false,
                    },
                )?,
            },
            SourcedRecord {
                source: "echo".to_owned(),
                record: record(
                    "echo:C0EXAMPLE01/unowned",
                    "C0EXAMPLE01",
                    "2026-10-25T11:00:00Z",
                    "An unrelated message.",
                    RecordOptions {
                        label: Some("#general"),
                        thread: None,
                        aliases: &[],
                        context: false,
                    },
                )?,
            },
        ],
    )?;
    Ok((tree, config, current))
}

#[test]
fn delta_matches_normalised_quote_fragments_in_replies_and_parents() -> Result<()> {
    let root = scratch("quote-fragments")?;
    write(
        &root.join("questions/reply.md"),
        "---\ntype: question\nstatus: open\nquote: \"“Reply carries *five* precise words”\"\n---\n",
    )?;
    write(
        &root.join("questions/parent.md"),
        "---\ntype: question\nstatus: open\n---\n> “Parent carries five precise words... omitted”\n",
    )?;
    let (tree, config) = configured_tree(&root)?;
    let owners = Owners::discover(&tree, &config.sources);
    let schemes = source::configured_schemes(&config.sources)?;
    let current = window(
        &root.join("state/example/windows"),
        "20261025T120000Z-0001",
        vec![
            SourcedRecord {
                source: "echo".to_owned(),
                record: record(
                    "echo:C0EXAMPLE02/parent",
                    "C0EXAMPLE02",
                    "2026-10-25T09:00:00Z",
                    "Parent carries five precise words.",
                    RecordOptions {
                        label: Some("#ops-alice"),
                        thread: None,
                        aliases: &[],
                        context: true,
                    },
                )?,
            },
            SourcedRecord {
                source: "echo".to_owned(),
                record: record(
                    "echo:C0EXAMPLE02/reply",
                    "C0EXAMPLE02",
                    "2026-10-25T10:00:00Z",
                    "Reply carries five precise words.",
                    RecordOptions {
                        label: Some("#ops-alice"),
                        thread: Some("echo:C0EXAMPLE02/parent"),
                        aliases: &[],
                        context: false,
                    },
                )?,
            },
        ],
    )?;

    let delta = delta::compute(
        &tree,
        &schemes,
        &owners,
        &current,
        None,
        timestamp("2026-10-25T12:01:00Z")?,
    )?;
    remove_scratch(&root)?;

    assert_eq!(
        match_for(&delta, "echo:C0EXAMPLE02/reply")?.matches,
        vec![
            Match {
                record: "questions/parent.md".to_owned(),
                how: "quote".to_owned(),
            },
            Match {
                record: "questions/reply.md".to_owned(),
                how: "quote".to_owned(),
            },
        ]
    );
    Ok(())
}

#[test]
fn delta_diffs_open_items_from_the_previous_snapshot() -> Result<()> {
    let root = scratch("snapshot")?;
    write(
        &root.join("commitments/closed.md"),
        "---\ntype: commitment\nstatus: closed\nwhat: Completed example\n---\n",
    )?;
    write(
        &root.join("commitments/new.md"),
        "---\ntype: commitment\nstatus: open\nwhat: New example\n---\n",
    )?;
    let (tree, config) = configured_tree(&root)?;
    let owners = Owners::discover(&tree, &config.sources);
    let schemes = Schemes::new(["echo".to_owned()]);
    let windows = root.join("state/example/windows");
    let previous = window(&windows, "20261024T120000Z-0001", Vec::new())?;
    let previous_snapshot = OpenSnapshot {
        version: 1,
        generated_at: timestamp("2026-10-24T12:00:00Z")?,
        open: vec![
            OpenRecord {
                path: "commitments/closed.md".to_owned(),
                kind: "commitment".to_owned(),
                status: "open".to_owned(),
                title: "Completed example".to_owned(),
                last_reviewed: None,
            },
            OpenRecord {
                path: "commitments/deleted.md".to_owned(),
                kind: "commitment".to_owned(),
                status: "open".to_owned(),
                title: "Deleted example".to_owned(),
                last_reviewed: None,
            },
        ],
    };
    delta::write(
        &previous,
        &previous_snapshot,
        &Delta {
            version: 1,
            window: previous.id.to_string(),
            window_end: timestamp("2026-10-24T12:00:00Z")?,
            generated_at: timestamp("2026-10-24T12:00:00Z")?,
            previous_window: None,
            new_messages: Vec::new(),
            unreviewed: Vec::new(),
            closed_since_previous: Vec::new(),
            opened_since_previous: Vec::new(),
        },
    )?;
    let current = window(&windows, "20261025T120000Z-0001", Vec::new())?;

    let delta = delta::compute(
        &tree,
        &schemes,
        &owners,
        &current,
        Some(&previous),
        timestamp("2026-10-25T12:00:00Z")?,
    )?;
    remove_scratch(&root)?;

    assert_eq!(
        delta.previous_window.as_deref(),
        Some("20261024T120000Z-0001")
    );
    assert_eq!(
        delta
            .closed_since_previous
            .iter()
            .map(|change| (change.record.as_str(), change.now.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("commitments/closed.md", Some("closed")),
            ("commitments/deleted.md", None),
        ]
    );
    assert_eq!(delta.opened_since_previous, vec!["commitments/new.md"]);
    Ok(())
}

#[test]
fn staleness_tracks_current_last_reviewed_date() -> Result<()> {
    let root = scratch("staleness")?;
    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-24\n---\n",
    )?;
    let (tree, _) = configured_tree(&root)?;
    let windows = root.join("state/example/windows");
    let current = window(&windows, "20261025T120000Z-0001", Vec::new())?;
    let snapshot = delta::open_snapshot(&tree, timestamp("2026-10-25T12:00:00Z")?);
    let delta = Delta {
        version: 1,
        window: current.id.to_string(),
        window_end: timestamp("2026-10-25T12:00:00Z")?,
        generated_at: timestamp("2026-10-25T12:00:00Z")?,
        previous_window: None,
        new_messages: Vec::new(),
        unreviewed: vec![OpenFlag {
            record: "questions/example.md".to_owned(),
            kind: "question".to_owned(),
            status: "open".to_owned(),
            title: "questions/example.md".to_owned(),
            owner: None,
            last_reviewed: Some(date("2026-10-24")?),
            replies: vec![ReplyRef {
                pointer: "echo:C0EXAMPLE01/reply".to_owned(),
                ts: timestamp("2026-10-25T10:00:00Z")?,
                author: Some("Bob".to_owned()),
                how: "pointer".to_owned(),
                excerpt: "A review reply.".to_owned(),
            }],
        }],
        closed_since_previous: Vec::new(),
        opened_since_previous: Vec::new(),
    };
    delta::write(&current, &snapshot, &delta)?;

    let stale = delta::staleness(&windows, &tree)?;
    assert!(
        !stale
            .get("questions/example.md")
            .context("the new reply is unreviewed")?
            .first()
            .context("a reply is present")?
            .same_day
    );

    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-25\n---\n",
    )?;
    let (reviewed_same_day, _) = configured_tree(&root)?;
    let same_day = delta::staleness(&windows, &reviewed_same_day)?;
    assert!(same_day
        .get("questions/example.md")
        .context("same-day reply remains visible")?
        .iter()
        .all(|reply| reply.same_day));

    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-26\n---\n",
    )?;
    let (reviewed_after, _) = configured_tree(&root)?;
    let after = delta::staleness(&windows, &reviewed_after)?;
    remove_scratch(&root)?;
    assert!(!after.contains_key("questions/example.md"));
    Ok(())
}

#[test]
fn ctx_open_shows_unreviewed_replies_for_configured_sources() -> Result<()> {
    let root = scratch("open")?;
    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-24\n---\n",
    )?;
    let (tree, _) = configured_tree(&root)?;
    let state = root.join("state");
    let current = window(
        &state.join("example/windows"),
        "20261025T120000Z-0001",
        Vec::new(),
    )?;
    let snapshot = delta::open_snapshot(&tree, timestamp("2026-10-25T12:00:00Z")?);
    delta::write(
        &current,
        &snapshot,
        &Delta {
            version: 1,
            window: current.id.to_string(),
            window_end: timestamp("2026-10-25T12:00:00Z")?,
            generated_at: timestamp("2026-10-25T12:00:00Z")?,
            previous_window: None,
            new_messages: Vec::new(),
            unreviewed: vec![OpenFlag {
                record: "questions/example.md".to_owned(),
                kind: "question".to_owned(),
                status: "open".to_owned(),
                title: "questions/example.md".to_owned(),
                owner: None,
                last_reviewed: Some(date("2026-10-24")?),
                replies: vec![ReplyRef {
                    pointer: "echo:C0EXAMPLE01/reply".to_owned(),
                    ts: timestamp("2026-10-25T10:00:00Z")?,
                    author: Some("Bob".to_owned()),
                    how: "pointer".to_owned(),
                    excerpt: "A matching reply.".to_owned(),
                }],
            }],
            closed_since_previous: Vec::new(),
            opened_since_previous: Vec::new(),
        },
    )?;

    let output = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory"])
        .arg(&root)
        .arg("open")
        .env("CTX_STATE_DIR", &state)
        .output()
        .context("running ctx open")?;
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).context("decoding ctx open output")?;
    assert_eq!(
        json.get("data")
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("unreviewed"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );

    let human = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--memory"])
        .arg(&root)
        .arg("open")
        .env("CTX_STATE_DIR", &state)
        .output()
        .context("running human ctx open")?;
    assert!(human.status.success());
    let stdout = String::from_utf8(human.stdout).context("decoding human ctx open output")?;
    assert!(stdout.contains("unreviewed 2026-10-25 10:00:00 UTC echo:C0EXAMPLE01/reply"));
    remove_scratch(&root)?;
    Ok(())
}

#[test]
fn ctx_delta_writes_delta_artifacts_for_selected_window() -> Result<()> {
    let root = scratch("cli")?;
    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\n---\n",
    )?;
    let (_, _) = configured_tree(&root)?;
    let state = root.join("state");
    let current = window(
        &state.join("example/windows"),
        "20261025T120000Z-0001",
        Vec::new(),
    )?;

    let output = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory"])
        .arg(&root)
        .args(["delta", "--window", current.id.as_str(), "--write"])
        .env("CTX_STATE_DIR", &state)
        .output()
        .context("running ctx delta")?;
    let delta_path = current.dir.join("delta.json");
    let snapshot_path = current.dir.join("open-snapshot.json");

    assert!(output.status.success());
    let json: Value =
        serde_json::from_slice(&output.stdout).context("decoding ctx delta output")?;
    assert_eq!(json.get("version").and_then(Value::as_u64), Some(1));
    assert_eq!(
        json.get("data")
            .and_then(|data| data.get("version"))
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(delta_path.is_file());
    assert!(snapshot_path.is_file());
    remove_scratch(&root)?;
    Ok(())
}

#[test]
fn ctx_fetch_writes_delta_artifacts_before_completing_the_window() -> Result<()> {
    let root = scratch("fetch")?;
    write(
        &root.join("questions/example.md"),
        "---\ntype: question\nstatus: open\n---\n",
    )?;
    let (_, _) = configured_tree(&root)?;
    let state = root.join("state");
    let inherited = std::env::var("PATH").context("PATH is not set")?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory"])
        .arg(&root)
        .args(["fetch", "--window-end", "2026-10-25T12:00:00Z"])
        .env("CTX_STATE_DIR", &state)
        .env("PATH", format!("{}:{inherited}", fixtures.display()))
        .output()
        .context("running ctx fetch")?;
    assert!(output.status.success());
    let json: Value =
        serde_json::from_slice(&output.stdout).context("decoding ctx fetch output")?;
    let window = json
        .get("data")
        .and_then(|data| data.get("window"))
        .and_then(Value::as_str)
        .context("fetch reports a window")?;
    let completed = state.join("example/windows").join(window);
    assert!(completed.join("delta.json").is_file());
    assert!(completed.join("delta.md").is_file());
    assert!(completed.join("open-snapshot.json").is_file());
    remove_scratch(&root)?;
    Ok(())
}

#[test]
fn unsupported_artifact_versions_fail_without_replacing_existing_files() -> Result<()> {
    let root = scratch("artifact-version")?;
    let current = window(
        &root.join("state/example/windows"),
        "20261025T120000Z-0001",
        Vec::new(),
    )?;
    for name in ["open-snapshot.json", "delta.json", "delta.md"] {
        write(&current.dir.join(name), "existing artifact")?;
    }
    let mut snapshot = OpenSnapshot {
        version: 0,
        generated_at: timestamp("2026-10-25T12:00:00Z")?,
        open: Vec::new(),
    };
    let mut delta = Delta {
        version: 1,
        window: current.id.to_string(),
        window_end: timestamp("2026-10-25T12:00:00Z")?,
        generated_at: timestamp("2026-10-25T12:00:00Z")?,
        previous_window: None,
        new_messages: Vec::new(),
        unreviewed: Vec::new(),
        closed_since_previous: Vec::new(),
        opened_since_previous: Vec::new(),
    };

    let rejected = delta::write(&current, &snapshot, &delta);
    assert!(rejected.is_err());
    snapshot.version = 1;
    delta.version = 2;
    let rejected = delta::write(&current, &snapshot, &delta);
    assert!(rejected.is_err());
    for name in ["open-snapshot.json", "delta.json", "delta.md"] {
        assert_eq!(
            fs::read_to_string(current.dir.join(name))?,
            "existing artifact"
        );
    }
    remove_scratch(&root)?;
    Ok(())
}
