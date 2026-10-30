use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{TimeZone as _, Utc};
use ctx_memory::{config::TreeConfig, record::Record, store::Store, sweep, tree::Tree};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-cite-{label}-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_test_root(root: &Path) -> Result<()> {
    std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

fn write(root: &Path, relative: &str, content: &str) -> Result<()> {
    let path = root.join(relative);
    let parent = path
        .parent()
        .with_context(|| format!("finding parent for {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))
}

fn stored(pointer: &str, seconds: i64, text: &str) -> Result<Record> {
    Ok(Record {
        pointer: pointer.to_owned(),
        stream: "C0EXAMPLE01".to_owned(),
        ts: Utc
            .timestamp_opt(seconds, 0)
            .single()
            .context("constructing test timestamp")?,
        label: Some("#general".to_owned()),
        author: Some("Alice".to_owned()),
        author_id: Some("U0EXAMPLE9".to_owned()),
        thread: None,
        title: None,
        url: None,
        text: text.to_owned(),
        aliases: Vec::new(),
        context: false,
        raw: Value::Null,
    })
}

fn verify(tree: &Path, state_base: &Path) -> Result<Value> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree, "cite", "verify"])
        .env("CTX_STATE_DIR", state_base)
        .output()
        .context("running ctx cite verify")?;
    let response: Value =
        serde_json::from_slice(&output.stdout).context("parsing ctx JSON output")?;
    response
        .get("data")
        .cloned()
        .context("cite verify response carries data")
}

#[test]
fn cite_verify_checks_every_quote() -> Result<()> {
    let root = test_root("every-quote")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    write(
        &tree,
        "ctx.toml",
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"slack\"\nkind = \"slack\"\n",
    )?;
    write(
        &tree,
        "decision.md",
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"alpha beta gamma\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"beta gamma delta\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000002\"\n\
         \x20   tier: 1\n\
         ---\n\
         Evidence.\n",
    )?;
    let store = Store::open(
        &state_base.join("example/store.sqlite3"),
        &state_base.join("example/sources.json"),
        &state_base.join("example/resolved"),
    )?;
    store.put_all(
        "slack",
        &[
            stored(
                "slack:C0EXAMPLE01/1700000000.000001",
                1_700_000_000,
                "alpha beta gamma delta",
            )?,
            stored(
                "slack:C0EXAMPLE01/1700000000.000002",
                1_700_000_001,
                "the price moved to 120 dollars",
            )?,
        ],
        Utc.timestamp_opt(1_700_000_010, 0)
            .single()
            .context("fetch time is valid")?,
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    let broken = data
        .get("broken")
        .and_then(Value::as_array)
        .context("verification reports broken citations")?;
    let pointers: Vec<&str> = broken
        .iter()
        .filter_map(|entry| entry.get("pointer").and_then(Value::as_str))
        .collect();
    assert_eq!(pointers, ["slack:C0EXAMPLE01/1700000000.000002"]);
    assert_eq!(
        broken
            .first()
            .and_then(|entry| entry.get("why"))
            .and_then(Value::as_str),
        Some("quote does not appear in the resolved text")
    );
    assert_eq!(data.get("checked").and_then(Value::as_u64), Some(2));
    Ok(())
}

#[test]
fn sweep_accepts_quotes_list() -> Result<()> {
    let root = test_root("quotes-list")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[decisions/renewal]]\n",
    )?;
    write(
        &root,
        "decisions/renewal.md",
        "---\n\
         type: decision\n\
         status: open\n\
         revisit_by: 2099-01-01\n\
         quotes:\n\
         \x20 - quote: \"The renewal was agreed.\"\n\
         \x20   source: \"example:planning-note\"\n\
         \x20   tier: 1\n\
         ---\n\
         The renewal is recorded here.\n",
    )?;

    let result = Tree::load(&root, &TreeConfig::default()).and_then(|tree| sweep::run(&tree));
    remove_test_root(&root)?;

    let findings = result?;
    assert!(
        findings.unquoted.is_empty(),
        "a record with a quotes list carries a quote: {:?}",
        findings.unquoted
    );
    assert!(findings.frontmatter_invalid.is_empty());
    assert!(findings.structural_clean());
    Ok(())
}

fn github_stored(pointer: &str, seconds: i64, text: &str) -> Result<Record> {
    Ok(Record {
        pointer: pointer.to_owned(),
        stream: "github:example-org/example-repo".to_owned(),
        ts: Utc
            .timestamp_opt(seconds, 0)
            .single()
            .context("constructing test timestamp")?,
        label: Some("pull_request".to_owned()),
        author: Some("ada".to_owned()),
        author_id: Some("MDQ6VXNlcjE".to_owned()),
        thread: None,
        title: Some("Add the GitHub source".to_owned()),
        url: Some("https://github.com/example-org/example-repo/pull/12".to_owned()),
        text: text.to_owned(),
        aliases: Vec::new(),
        context: false,
        raw: Value::Null,
    })
}

#[test]
fn cite_verify_checks_a_github_quote() -> Result<()> {
    let root = test_root("github-quote")?;
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    write(
        &tree,
        "ctx.toml",
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"github\"\nkind = \"github\"\nscheme = \"github\"\n",
    )?;
    write(
        &tree,
        "decision.md",
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"ship the GitHub source\"\n\
         \x20   source: \"github:example-org/example-repo#12\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"this phrase is not in the pull request\"\n\
         \x20   source: \"github:example-org/example-repo#12/comment/900001\"\n\
         \x20   tier: 1\n\
         ---\n\
         Evidence.\n",
    )?;
    let store = Store::open(
        &state_base.join("example/store.sqlite3"),
        &state_base.join("example/sources.json"),
        &state_base.join("example/resolved"),
    )?;
    store.put_all(
        "github",
        &[
            github_stored(
                "github:example-org/example-repo#12",
                1_700_000_000,
                "Let us ship the GitHub source this week.",
            )?,
            github_stored(
                "github:example-org/example-repo#12/comment/900001",
                1_700_000_001,
                "Looks good to me.",
            )?,
        ],
        Utc.timestamp_opt(1_700_000_010, 0)
            .single()
            .context("fetch time is valid")?,
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    // GitHub is a verbatim source, so no quote is silently skipped.
    assert_eq!(
        data.get("quotes_unchecked").and_then(Value::as_u64),
        Some(0),
        "a GitHub-cited quote must be checked, not skipped: {data}"
    );
    let skipped = data
        .get("skipped")
        .and_then(Value::as_object)
        .context("verification reports skipped schemes")?;
    assert!(
        !skipped.contains_key("github"),
        "github pointers must not be skipped: {skipped:?}"
    );
    assert_eq!(data.get("checked").and_then(Value::as_u64), Some(2));
    let broken = data
        .get("broken")
        .and_then(Value::as_array)
        .context("verification reports broken citations")?;
    let pointers: Vec<&str> = broken
        .iter()
        .filter_map(|entry| entry.get("pointer").and_then(Value::as_str))
        .collect();
    assert_eq!(
        pointers,
        ["github:example-org/example-repo#12/comment/900001"]
    );
    Ok(())
}

/// The pointers a verification lists under `key`, in report order.
fn pointers(data: &Value, key: &str) -> Result<Vec<String>> {
    Ok(data
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("verification reports {key}"))?
        .iter()
        .filter_map(|entry| {
            entry
                .get("pointer")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect())
}

/// Writes a tree whose one record is `record`, citing Slack and a configured `ext` source that
/// has never declared whether its records are its own words.
fn provenance_tree(root: &Path, record: &str) -> Result<(PathBuf, PathBuf)> {
    let tree = root.join("tree");
    let state_base = root.join("state");
    std::fs::create_dir(&tree)?;
    write(
        &tree,
        "ctx.toml",
        "version = 1\n\
         id = \"example\"\n\n\
         [[sources]]\n\
         id = \"slack\"\n\
         kind = \"slack\"\n\n\
         [[sources]]\n\
         id = \"ext\"\n\
         kind = \"ext\"\n\
         scheme = \"ext\"\n",
    )?;
    write(&tree, "decision.md", record)?;
    let store = Store::open(
        &state_base.join("example/store.sqlite3"),
        &state_base.join("example/sources.json"),
        &state_base.join("example/resolved"),
    )?;
    store.put_all(
        "slack",
        &[stored(
            "slack:C0EXAMPLE01/1700000000.000001",
            1_700_000_000,
            "alpha beta gamma delta",
        )?],
        Utc.timestamp_opt(1_700_000_010, 0)
            .single()
            .context("fetch time is valid")?,
    )?;
    Ok((tree, state_base))
}

/// A record that names one verbatim source does not vouch for its other claims. The second
/// claim here cites a configured source that has never said whether its records are its own
/// words, so nothing can check that quote and the verification must say so.
#[test]
fn a_mixed_record_reports_the_claim_from_an_undeclared_source() -> Result<()> {
    let root = test_root("mixed-undeclared")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"alpha beta gamma\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"invented words\"\n\
         \x20   source: \"ext:foo/1\"\n\
         \x20   tier: 1\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(pointers(&data, "unchecked")?, ["ext:foo/1"], "{data}");
    assert_eq!(pointers(&data, "broken")?, Vec::<String>::new(), "{data}");
    assert_eq!(
        data.get("checked").and_then(Value::as_u64),
        Some(1),
        "{data}"
    );
    Ok(())
}

/// "I have never heard of this source" is not "this source is not verbatim". A tier 1 claim
/// whose source is free text names no provenance the tool can check, so it is refused.
#[test]
fn a_tier_1_quote_sourced_by_free_text_is_unchecked() -> Result<()> {
    let root = test_root("free-text-tier-1")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quote: \"invented words\"\n\
         source: \"overheard in standup\"\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        ["overheard in standup"],
        "{data}"
    );
    assert_eq!(
        data.get("quotes_unchecked").and_then(Value::as_u64),
        Some(1),
        "{data}"
    );
    Ok(())
}

/// Tiers 3 and 4 are what a summary and a secondhand account are for: a claim that names no
/// pointer at all is exactly what those tiers mean, so the verification lets it through.
#[test]
fn a_tier_3_quote_sourced_by_free_text_is_allowed() -> Result<()> {
    let root = test_root("free-text-tier-3")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 3\n\
         quote: \"a paraphrase of the standup\"\n\
         source: \"overheard in standup\"\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        Vec::<String>::new(),
        "{data}"
    );
    assert_eq!(
        data.get("quotes_unchecked").and_then(Value::as_u64),
        Some(1),
        "{data}"
    );
    Ok(())
}

/// A scheme no configured source owns claims a provenance the tool cannot check, which no tier
/// excuses: tier 4 does not make `foo:bar` resolvable.
#[test]
fn an_unregistered_scheme_is_unchecked_at_every_tier() -> Result<()> {
    let root = test_root("unregistered-scheme")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 4\n\
         quote: \"invented words\"\n\
         source: \"foo:bar\"\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(pointers(&data, "unchecked")?, ["foo:bar"], "{data}");
    Ok(())
}

/// A source that resolved but renders its records, rather than storing the words themselves,
/// can never settle a quote. The record's other, verbatim, citation does not cover it.
#[test]
fn a_claim_citing_a_non_verbatim_source_is_unchecked() -> Result<()> {
    let root = test_root("non-verbatim-claim")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"alpha beta gamma\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"invented words\"\n\
         \x20   source: \"gcal:primary/evt1\"\n\
         \x20   tier: 1\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        ["gcal:primary/evt1"],
        "{data}"
    );
    assert_eq!(
        data.get("checked").and_then(Value::as_u64),
        Some(1),
        "{data}"
    );
    Ok(())
}

/// The same holds when the calendar is the record's only source: a rendered event is not the
/// speaker's own words, so the quote it carries was checked against nothing.
#[test]
fn a_record_sourced_only_by_the_calendar_is_unchecked() -> Result<()> {
    let root = test_root("calendar-only")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quote: \"invented words\"\n\
         source: \"gcal:primary/evt1\"\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        ["gcal:primary/evt1"],
        "{data}"
    );
    Ok(())
}

/// The entries a verification lists under `key`.
fn entries<'a>(data: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    data.get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("verification reports {key}"))
}

/// A record's tier is what search ranks it by, and tier 1 or 2 says the record holds a
/// source's own words. A quote's own tier overrides the record's, so a record that declares
/// tier 1 while its only quote is a tier 3 summary with no pointer has had none of those words
/// checked: the verification must name the record, at the line that declares its tier.
#[test]
fn a_tier_1_record_whose_only_quote_is_a_tier_3_summary_is_unchecked() -> Result<()> {
    let root = test_root("tier-1-record-tier-3-quote")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quotes:\n\
         \x20 - quote: \"invented words\"\n\
         \x20   source: \"overheard in standup\"\n\
         \x20   tier: 3\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(pointers(&data, "unchecked")?, ["tier 1"], "{data}");
    let unchecked = entries(&data, "unchecked")?
        .first()
        .context("the record is reported")?;
    assert_eq!(
        unchecked.get("line").and_then(Value::as_u64),
        Some(4),
        "{data}"
    );
    Ok(())
}

/// Tier 1 is an outside party's own words and tier 2 a colleague's. A meeting at tier 2 that
/// quotes nobody holds neither, whatever its kind, so the verification names it too.
#[test]
fn a_tier_2_meeting_with_no_quote_is_unchecked() -> Result<()> {
    let root = test_root("tier-2-meeting-no-quote")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: meeting\n\
         title: Planning review\n\
         date: 2026-10-25\n\
         attendees:\n\
         \x20 - Alice\n\
         tier: 2\n\
         ---\n\
         > Alice: the review starts this week.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(pointers(&data, "unchecked")?, ["tier 2"], "{data}");
    let why = entries(&data, "unchecked")?
        .first()
        .and_then(|entry| entry.get("why"))
        .and_then(Value::as_str)
        .context("the finding says why")?;
    assert!(why.contains("without a quote"), "{why}");
    Ok(())
}

/// The rule asks for one checked quote at tier 1 or 2, not for every quote to be one: a tier
/// 1 record whose tier 1 quote holds in its source may also carry a tier 3 summary.
#[test]
fn a_tier_1_record_with_a_checked_tier_1_quote_may_carry_a_summary() -> Result<()> {
    let root = test_root("tier-1-record-checked-quote")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quotes:\n\
         \x20 - quote: \"alpha beta gamma\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"a paraphrase of the standup\"\n\
         \x20   source: \"overheard in standup\"\n\
         \x20   tier: 3\n\
         ---\n\
         Evidence.\n",
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        Vec::<String>::new(),
        "{data}"
    );
    assert_eq!(pointers(&data, "broken")?, Vec::<String>::new(), "{data}");
    Ok(())
}

/// A record stating `The vendor was never told about the renewal.`, backed by `quote` at tier
/// 1 from `source`.
fn absence_backed_by(quote: &str) -> String {
    format!(
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         statement: The vendor was never told about the renewal.\n\
         quote: \"{quote}\"\n\
         source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         ---\n\
         Evidence.\n"
    )
}

/// What the sweep and the verification report for a tree whose one record is `record`.
fn sweep_and_verify(label: &str, record: &str) -> Result<(sweep::Findings, Value)> {
    let root = test_root(label)?;
    let (tree, state_base) = provenance_tree(&root, record)?;
    let findings = Tree::load(&tree, &TreeConfig::default()).and_then(|tree| sweep::run(&tree));
    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;
    Ok((findings?, data?))
}

/// A tier 1 quote that holds in its source is the evidence for the absence the record states,
/// so neither the sweep nor the verification reports the record.
#[test]
fn a_statement_of_absence_backed_by_a_checked_tier_1_quote_is_allowed() -> Result<()> {
    let (findings, data) =
        sweep_and_verify("absence-checked", &absence_backed_by("alpha beta gamma"))?;
    assert_eq!(findings.negatives, Vec::new());
    assert_eq!(
        pointers(&data, "unchecked")?,
        Vec::<String>::new(),
        "{data}"
    );
    assert_eq!(pointers(&data, "broken")?, Vec::<String>::new(), "{data}");
    Ok(())
}

/// The excuse is the quote's check, not the quote's presence. The sweep leaves the check to
/// the verification, which reports a tier 1 quote its source does not hold, so the writer
/// refuses the record rather than let that quote stand as evidence for the absence.
#[test]
fn a_statement_of_absence_backed_by_a_quote_that_does_not_hold_is_refused() -> Result<()> {
    let (_, data) = sweep_and_verify("absence-unheld", &absence_backed_by("invented words"))?;
    assert_eq!(
        pointers(&data, "broken")?,
        ["slack:C0EXAMPLE01/1700000000.000001"],
        "{data}"
    );
    Ok(())
}

/// `ctx cite verify <file>` checks the file it is given, however the caller writes its path, and
/// refuses a path that names no record rather than report it checked. Today a file argument is
/// compared with each record's tree-relative path as written, so `./decision.md`, an absolute
/// path, or a path relative to a subdirectory of the tree matches nothing, and a path that names
/// no record at all also matches nothing: each run checks nothing, prints `checked 0` and exits 0.
/// Observed on the real binary: a tier 1 record whose quote its source does not hold exits 1 as
/// `decisions/bad.md` and 0 as `./decisions/bad.md`; and in remote mode
/// `ctx cite verify recs/cited.md`, a draft on the caller's machine that the server's tree does
/// not hold, answers `checked 0` with exit 0.
#[test]
fn cite_verify_checks_or_refuses_every_file_it_is_given() -> Result<()> {
    let root = test_root("named-file")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quote: \"invented words\"\n\
         source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         ---\n\
         Evidence.\n",
    )?;
    let absolute = tree.join("decision.md");
    let outcome = (|| -> Result<Vec<(String, Option<i32>, String)>> {
        let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
        let absolute_text = absolute.to_str().context("test path is not valid UTF-8")?;
        let mut runs = Vec::new();
        for named in ["decision.md", "./decision.md", absolute_text, "missing.md"] {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
                .args(["--json", "--memory", tree_text, "cite", "verify", named])
                .current_dir(&tree)
                .env("CTX_STATE_DIR", &state_base)
                .output()
                .context("running ctx cite verify")?;
            runs.push((
                named.to_owned(),
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            ));
        }
        Ok(runs)
    })();
    remove_test_root(&root)?;

    let runs = outcome?;
    let passed: Vec<&(String, Option<i32>, String)> = runs
        .iter()
        .filter(|(_, code, _)| *code == Some(0))
        .collect();
    assert!(
        passed.is_empty(),
        "cite verify passed a file it never checked (a broken tier 1 quote, or no record at \
         all): {passed:?}"
    );
    Ok(())
}

/// A record whose frontmatter does not parse has no claims a check can read, and `cite verify`
/// passed it: it resolved the pointer it found in the text (`checked 1`), checked no quote,
/// applied no tier rule and exited 0, for a file named on the command line and for the whole
/// tree alike (round-5 review of d9cb9f7, N5-2). The writer refuses such a record, so a cycle
/// that verified first was told the opposite of what the writer would do.
#[test]
fn cite_verify_reports_a_record_whose_frontmatter_does_not_parse() -> Result<()> {
    let root = test_root("unparsed-frontmatter")?;
    // The quote is left open, so its closing mark is the source's opening one.
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quote: \"alpha beta gamma\n\
         source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         ---\n\
         Evidence.\n",
    )?;
    let outcome = (|| -> Result<Vec<(Option<i32>, Value)>> {
        let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
        let mut runs = Vec::new();
        for named in [&["decision.md"][..], &[]] {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
                .args(["--json", "--memory", tree_text, "cite", "verify"])
                .args(named)
                .current_dir(&tree)
                .env("CTX_STATE_DIR", &state_base)
                .output()
                .context("running ctx cite verify")?;
            let response: Value = serde_json::from_slice(&output.stdout)
                .with_context(|| String::from_utf8_lossy(&output.stderr).into_owned())?;
            runs.push((
                output.status.code(),
                response.get("data").cloned().unwrap_or(Value::Null),
            ));
        }
        Ok(runs)
    })();
    remove_test_root(&root)?;

    for (code, data) in outcome? {
        assert_eq!(
            code,
            Some(1),
            "cite verify passed a record nothing parsed: {data}"
        );
        assert_eq!(pointers(&data, "unchecked")?, ["frontmatter"], "{data}");
        let why = entries(&data, "unchecked")?
            .first()
            .and_then(|entry| entry.get("why"))
            .and_then(Value::as_str)
            .context("the finding says why")?;
        assert!(why.contains("does not parse"), "{why}");
    }
    Ok(())
}

/// A record whose frontmatter does not parse carries no claim `cite verify` can read, so a run
/// that names it compares no quote. Today that run exits 0 with no finding: here it reports
/// `checked 1`, for the source pointer it finds in the text and resolves, and never compares the
/// tier 1 quote beside it, which its source does not hold. A person or a cycle that verifies
/// before writing reads that as "the quotes hold": the same false pass the earlier fix ended for
/// a path that names no record. Observed on the real binary, locally and in remote mode: a draft
/// whose tier 1 quote is an unterminated YAML string answers `checked 0`, exit 0, while the
/// server refuses the same record as `frontmatter-invalid`.
#[test]
fn cite_verify_refuses_a_named_file_whose_frontmatter_does_not_parse() -> Result<()> {
    let root = test_root("unparsed-front")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"invented words\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         ---\n\
         Evidence.\n",
    )?;
    let output = (|| -> Result<std::process::Output> {
        let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
        std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
            .args([
                "--json",
                "--memory",
                tree_text,
                "cite",
                "verify",
                "decision.md",
            ])
            .current_dir(&tree)
            .env("CTX_STATE_DIR", &state_base)
            .output()
            .context("running ctx cite verify")
    })();
    remove_test_root(&root)?;

    let output = output?;
    assert_ne!(
        output.status.code(),
        Some(0),
        "cite verify passed a file whose frontmatter it could not read: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}
