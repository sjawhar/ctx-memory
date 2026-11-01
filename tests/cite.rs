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

/// A record often quotes one source several times, and its title or body can repeat the same
/// words. The finding names the line of the entry that failed: not the record's first
/// citation of that source, not an earlier entry, and not the title.
#[test]
fn a_broken_quote_is_named_at_the_line_that_declares_it() -> Result<()> {
    let root = test_root("broken-quote-line")?;
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
         title: \"gamma delta epsilon, as agreed\"\n\
         quotes:\n\
         \x20 - quote: \"alpha beta gamma\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"gamma delta epsilon\"\n\
         \x20   source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         \x20   tier: 1\n\
         ---\n\
         gamma delta epsilon.\n",
    )?;
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
            "alpha beta gamma",
        )?],
        Utc.timestamp_opt(1_700_000_010, 0)
            .single()
            .context("fetch time is valid")?,
    )?;

    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;

    let data = data?;
    let broken = entries(&data, "broken")?;
    assert_eq!(broken.len(), 1, "{data}");
    let finding = broken.first().context("the broken quote is reported")?;
    assert_eq!(
        finding.get("line").and_then(Value::as_u64),
        Some(9),
        "the finding does not name the second entry's own line: {data}"
    );
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

/// A web address names where a summary or a secondhand account can be read, such as an
/// answered question on a tracker or a pull request, which is what tiers 3 and 4 are for. No
/// source resolves it and nothing fetches it, so it is counted among the quotes nothing
/// checked, and passes.
#[test]
fn a_tier_3_quote_sourced_by_a_web_address_is_allowed() -> Result<()> {
    let root = test_root("web-address-tier-3")?;
    let (tree, state_base) = provenance_tree(
        &root,
        "---\n\
         type: decision\n\
         status: open\n\
         quotes:\n\
         \x20 - quote: \"the answer as the tracker records it\"\n\
         \x20   source: \"https://tracker.example.com/ask/1\"\n\
         \x20   tier: 3\n\
         \x20 - quote: \"a recollection of the same answer\"\n\
         \x20   source: \"http://tracker.example.com/ask/2\"\n\
         \x20   tier: 4\n\
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
        Some(2),
        "{data}"
    );
    Ok(())
}

/// Tiers 1 and 2 say the record holds a source's own words, checked against that source. A
/// web address is checked against nothing, so it carries neither tier.
#[test]
fn a_tier_1_or_2_quote_sourced_by_a_web_address_is_unchecked() -> Result<()> {
    for tier in ["1", "2"] {
        let root = test_root(&format!("web-address-tier-{tier}"))?;
        let (tree, state_base) = provenance_tree(
            &root,
            &format!(
                "---\n\
                 type: decision\n\
                 status: open\n\
                 tier: {tier}\n\
                 quote: \"the answer as the tracker records it\"\n\
                 source: \"https://tracker.example.com/ask/1\"\n\
                 ---\n\
                 Evidence.\n"
            ),
        )?;

        let data = verify(&tree, &state_base);
        remove_test_root(&root)?;

        let data = data?;
        assert_eq!(
            pointers(&data, "unchecked")?,
            ["https://tracker.example.com/ask/1"],
            "tier {tier}: {data}"
        );
    }
    Ok(())
}

/// How a record writes the claim a test checks: under `source`, as a `sources` entry, or as a
/// `quotes` entry beside a tier 1 quote its Slack source holds.
#[derive(Clone, Copy, Debug)]
enum Form {
    Source,
    Sources,
    Quotes,
}

/// What `ctx cite verify` reports for a tree whose one record makes, in `form`, a claim at
/// `tier` citing `source`, written into a YAML double-quoted string as it stands, whose quote
/// the stored Slack message does not hold.
fn verify_claim(label: &str, form: Form, tier: u8, source: &str) -> Result<Value> {
    let claim = match form {
        Form::Source => format!(
            "tier: {tier}\nquote: \"words the message never held\"\nsource: \"{source}\"\n"
        ),
        Form::Sources => format!(
            "tier: {tier}\nquote: \"words the message never held\"\nsources:\n  - pointer: \"{source}\"\n    tier: {tier}\n"
        ),
        Form::Quotes => format!(
            "quotes:\n  - quote: \"alpha beta gamma\"\n    source: \"slack:C0EXAMPLE01/1700000000.000001\"\n    tier: 1\n  - quote: \"words the message never held\"\n    source: \"{source}\"\n    tier: {tier}\n"
        ),
    };
    let root = test_root(label)?;
    let (tree, state_base) = provenance_tree(
        &root,
        &format!("---\ntype: decision\nstatus: open\n{claim}---\nEvidence.\n"),
    )?;
    let data = verify(&tree, &state_base);
    remove_test_root(&root)?;
    data
}

/// [`verify_claim`] for a claim under `source`.
fn verify_one_claim(label: &str, tier: u8, source: &str) -> Result<Value> {
    verify_claim(label, Form::Source, tier, source)
}

/// The canonical permalink of the stored Slack message is a Slack pointer, alone or inside a
/// Markdown link or an assignment, so even a tier 3 quote that cites it is checked against the
/// message, and one the message does not hold is broken.
#[test]
fn a_tier_3_quote_citing_a_slack_permalink_is_checked() -> Result<()> {
    let permalink = "https://acme.slack.com/archives/C0EXAMPLE01/p1700000000000001";
    for (label, source) in [
        ("bare", permalink.to_owned()),
        ("markdown", format!("[message]({permalink})")),
        ("assignment", format!("source={permalink}")),
    ] {
        let data = verify_one_claim(&format!("permalink-tier-3-{label}"), 3, &source)?;
        assert_eq!(
            pointers(&data, "broken")?,
            ["slack:C0EXAMPLE01/1700000000.000001"],
            "{label}: {data}"
        );
    }
    Ok(())
}

/// A form of that permalink the scanner does not read as a pointer still links the message, so
/// it is no web address a summary may cite unchecked: at tiers 3 and 4 too it is unchecked,
/// under `source`, in `sources` and in a `quotes` entry beside a checked quote, rather than a
/// way around the check the canonical link gets. That holds for every form the URL parser
/// reads as the Slack host: a port, capitals, a trailing dot, a user name with or without
/// punctuation in it, a tab or a line break inside the scheme or the host, and the link inside
/// prose, a Markdown link or an assignment.
#[test]
fn a_variant_of_a_slack_permalink_is_unchecked_at_every_tier() -> Result<()> {
    let port = "https://acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001";
    let path = "/archives/C0EXAMPLE01/p1700000000000001";
    for (label, source, yaml) in [
        ("port", port.to_owned(), port.to_owned()),
        (
            "case",
            format!("https://ACME.Slack.COM{path}"),
            format!("https://ACME.Slack.COM{path}"),
        ),
        (
            "dot",
            format!("https://acme.slack.com.{path}"),
            format!("https://acme.slack.com.{path}"),
        ),
        (
            "user",
            format!("https://user@acme.slack.com{path}"),
            format!("https://user@acme.slack.com{path}"),
        ),
        (
            "prose",
            format!("said in {port}"),
            format!("said in {port}"),
        ),
        (
            "markdown",
            format!("[message]({port})"),
            format!("[message]({port})"),
        ),
        (
            "assignment",
            format!("source={port}"),
            format!("source={port}"),
        ),
        (
            "paren-user",
            format!("source=https://a)b@acme.slack.com:443{path}"),
            format!("source=https://a)b@acme.slack.com:443{path}"),
        ),
        (
            "bracket-user",
            format!("https://a]b@acme.slack.com{path}"),
            format!("https://a]b@acme.slack.com{path}"),
        ),
        (
            "host-tab",
            format!("https://acme.sla\tck.com{path}"),
            format!("https://acme.sla\\tck.com{path}"),
        ),
        (
            "scheme-tab",
            format!("ht\ttps://acme.slack.com:443{path}"),
            format!("ht\\ttps://acme.slack.com:443{path}"),
        ),
        (
            "host-newline",
            format!("https://acme.sla\nck.com{path}"),
            format!("https://acme.sla\\nck.com{path}"),
        ),
    ] {
        for form in [Form::Source, Form::Sources, Form::Quotes] {
            for tier in [3, 4] {
                let data = verify_claim(
                    &format!("permalink-variant-{label}-{form:?}-{tier}"),
                    form,
                    tier,
                    &yaml,
                )?;
                assert_eq!(
                    pointers(&data, "unchecked")?,
                    [source.as_str()],
                    "{label} in {form:?} at tier {tier}: {data}"
                );
            }
        }
    }
    Ok(())
}

/// A source that holds a web address is that address alone, one a reader can open: no host, a
/// user name, a control character, a port out of range, whitespace, words before or after it,
/// or a Markdown link or an assignment around it each make a claim nothing checks, at every
/// tier, in whatever case the scheme is written.
#[test]
fn a_web_address_that_is_not_the_whole_source_or_not_valid_is_unchecked() -> Result<()> {
    for (label, source, yaml) in [
        ("no-host", "https://:443/ask/1", "https://:443/ask/1"),
        ("upper-no-host", "HTTPS://:443/ask/1", "HTTPS://:443/ask/1"),
        (
            "user",
            "https://user@example.com/ask/1",
            "https://user@example.com/ask/1",
        ),
        (
            "control",
            "https://example.com/\u{1b}[2J",
            "https://example.com/\\e[2J",
        ),
        (
            "upper-control",
            "HTTPS://example.com/\u{1b}[2J",
            "HTTPS://example.com/\\e[2J",
        ),
        (
            "bad-port",
            "HTTPS://example.com:99999/ask/1",
            "HTTPS://example.com:99999/ask/1",
        ),
        (
            "host-space",
            "https://exa mple.com/ask/1",
            "https://exa mple.com/ask/1",
        ),
        ("empty-host-space", "https:// /ask/1", "https:// /ask/1"),
        (
            "words-after",
            "https://example.com/ask/1, as Ada answered",
            "https://example.com/ask/1, as Ada answered",
        ),
        (
            "words-before",
            "as Ada answered in https://example.com/ask/1",
            "as Ada answered in https://example.com/ask/1",
        ),
        (
            "markdown",
            "[the answer](https://tracker.example.com/ask/1)",
            "[the answer](https://tracker.example.com/ask/1)",
        ),
        (
            "assignment",
            "source=https://tracker.example.com/ask/1",
            "source=https://tracker.example.com/ask/1",
        ),
    ] {
        for tier in [3, 4] {
            let data = verify_one_claim(&format!("malformed-web-{label}-{tier}"), tier, yaml)?;
            assert_eq!(
                pointers(&data, "unchecked")?,
                [source],
                "{label} at tier {tier}: {data}"
            );
        }
    }
    Ok(())
}

/// Free text that names no web address stays free text, which tiers 3 and 4 allow.
#[test]
fn a_tier_3_quote_citing_free_text_that_mentions_the_web_is_allowed() -> Result<()> {
    let data = verify_one_claim("free-text-web", 3, "the http answer on the tracker, item 1")?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        Vec::<String>::new(),
        "{data}"
    );
    Ok(())
}

/// The Slack check reads the host of the address a record gives; it does not follow the
/// address. An address on another host is a location like any other, even when its query holds
/// a variant of a Slack permalink or the page redirects there: nothing fetches it. A canonical
/// permalink anywhere in the source text is still a Slack pointer, and its quote is checked.
#[test]
fn the_slack_check_reads_the_given_address_and_follows_nothing() -> Result<()> {
    let elsewhere = "https://redirect.example.com/to?u=https://acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001";
    let data = verify_one_claim("web-elsewhere-variant", 3, elsewhere)?;
    assert_eq!(
        pointers(&data, "unchecked")?,
        Vec::<String>::new(),
        "{data}"
    );
    let canonical = "https://redirect.example.com/to?u=https://acme.slack.com/archives/C0EXAMPLE01/p1700000000000001";
    let data = verify_one_claim("web-elsewhere-canonical", 3, canonical)?;
    assert_eq!(
        pointers(&data, "broken")?,
        ["slack:C0EXAMPLE01/1700000000.000001"],
        "{data}"
    );
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
