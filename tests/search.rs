mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use ctx_memory::config::TreeConfig;
use ctx_memory::embed::{Embed, Embedding};
use ctx_memory::index::Index;
use ctx_memory::tree::Tree;
use serde_json::{json, Value};
use support::fake_embed::{self, ConceptEmbedder};
use support::fake_http::FakeHttp;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-search-{label}-{}-{suffix}",
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

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn ctx(tree: &Path, arguments: &[&str]) -> Result<Value> {
    ctx_with_env(tree, &[], arguments)
}

/// Runs `ctx --json` against a tree with a known environment.
///
/// Meaning search reads the environment, so every test clears what the box running it may
/// have set and names what it wants itself.
fn ctx_with_env(tree: &Path, environment: &[(&str, &str)], arguments: &[&str]) -> Result<Value> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    let mut command = binary();
    command.args(["--memory", tree, "--json"]).args(arguments);
    for name in [
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_CLOUD_LOCATION",
        "CTX_EMBED_MODEL",
        "CTX_VERTEX_API_BASE",
        "GCE_METADATA_HOST",
        "CTX_DATA_DIR",
    ] {
        command.env_remove(name);
    }
    for (name, value) in environment {
        command.env(name, value);
    }
    let output = command.output().context("running ctx")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn write_config(root: &Path, vitals: &str) -> Result<()> {
    write(
        root,
        "ctx.toml",
        &format!("version = 1\n[dossier]\nvitals = [{vitals}]\n"),
    )
}

#[test]
fn dossier_finds_documents_by_frontmatter_name() -> Result<()> {
    let root = test_root("frontmatter-name")?;
    write_config(&root, "")?;
    write(
        &root,
        "people/alice.md",
        "---\nname: Alice Example\n---\n# Profile\n",
    )?;
    write(
        &root,
        "meetings/review.md",
        "The group asked Alice Example to review the proposal. [[people/alice]]\n",
    )?;
    write(
        &root,
        "records/Alice Example/archive.md",
        "# Unrelated record\n",
    )?;
    write(
        &root,
        "records/frontmatter.md",
        "---\nsubject: Alice Example\n---\n# Unrelated record\n",
    )?;

    let response = ctx(&root, &["dossier", "Alice Example"]);
    remove_test_root(&root)?;
    let response = response?;
    let data = response.get("data").context("dossier data is missing")?;

    assert_eq!(
        data.get("page").and_then(|page| page.get("path")),
        Some(&serde_json::json!("people/alice.md"))
    );
    let hit_paths = data
        .get("hits")
        .and_then(Value::as_array)
        .context("dossier hits are missing")?
        .iter()
        .filter_map(|hit| hit.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(hit_paths.contains(&"people/alice.md"));
    assert!(hit_paths.contains(&"meetings/review.md"));
    assert!(hit_paths.contains(&"records/Alice Example/archive.md"));
    assert!(hit_paths.contains(&"records/frontmatter.md"));
    Ok(())
}

#[test]
fn dossier_selects_title_and_alias_matches() -> Result<()> {
    let root = test_root("title-alias")?;
    write_config(&root, "")?;
    write(
        &root,
        "projects/falcon.md",
        "---\ntitle: Falcon Project\n---\n# Project\n",
    )?;
    write(
        &root,
        "people/alice.md",
        "---\naliases: [Lead Alice]\n---\n# Profile\n",
    )?;

    let title_response = ctx(&root, &["dossier", "fAlCoN pRoJeCt"]);
    let alias_response = ctx(&root, &["dossier", "lEaD aLiCe"]);
    remove_test_root(&root)?;

    assert_eq!(
        title_response?
            .get("data")
            .and_then(|data| data.get("page"))
            .and_then(|page| page.get("path")),
        Some(&serde_json::json!("projects/falcon.md"))
    );
    assert_eq!(
        alias_response?
            .get("data")
            .and_then(|data| data.get("page"))
            .and_then(|page| page.get("path")),
        Some(&serde_json::json!("people/alice.md"))
    );
    Ok(())
}

#[test]
fn exact_entity_match_wins_over_substring_candidates() -> Result<()> {
    let root = test_root("exact-before-substring")?;
    write_config(&root, "")?;
    write(&root, "people/alice.md", "# Profile\n")?;
    write(&root, "people/alice-archive.md", "# Archive\n")?;

    let response = ctx(&root, &["dossier", "alice"]);
    remove_test_root(&root)?;
    let data = response?
        .get("data")
        .cloned()
        .context("dossier data is missing")?;

    assert_eq!(
        data.get("page").and_then(|page| page.get("path")),
        Some(&serde_json::json!("people/alice.md"))
    );
    assert_eq!(data.get("candidates"), Some(&serde_json::json!([])));
    Ok(())
}

#[test]
fn empty_dossier_is_an_error() -> Result<()> {
    let root = test_root("empty")?;
    write_config(&root, "")?;

    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "dossier", "  "])
        .output()
        .context("running ctx")?;
    remove_test_root(&root)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("dossier entity must not be empty"));
    Ok(())
}

#[test]
fn ambiguous_entity_lists_candidates_instead_of_choosing() -> Result<()> {
    let root = test_root("ambiguous")?;
    write_config(&root, "")?;
    write(
        &root,
        "people/alice-a.md",
        "---\nname: Alice Anderson\n---\n# Profile\n",
    )?;
    write(
        &root,
        "people/alice-b.md",
        "---\nname: Alice Baker\n---\n# Profile\n",
    )?;

    let response = ctx(&root, &["dossier", "alice"]);
    remove_test_root(&root)?;
    let response = response?;
    let data = response.get("data").context("dossier data is missing")?;

    assert_eq!(data.get("page"), Some(&Value::Null));
    assert_eq!(
        data.get("candidates"),
        Some(&serde_json::json!([
            "people/alice-a.md",
            "people/alice-b.md"
        ]))
    );
    Ok(())
}

#[test]
fn dossier_json_carries_the_page_vitals() -> Result<()> {
    let root = test_root("vitals")?;
    write_config(&root, "\"status\", \"missing\"")?;
    write(
        &root,
        "people/alice.md",
        "---\nname: Alice Example\nstatus: active\ndepartment: research\n---\n# Profile\n",
    )?;

    let response = ctx(&root, &["dossier", "Alice Example"]);
    remove_test_root(&root)?;
    let response = response?;

    assert_eq!(
        response
            .get("data")
            .and_then(|data| data.get("page"))
            .and_then(|page| page.get("vitals")),
        Some(&serde_json::json!({"status": "active"}))
    );
    Ok(())
}

fn hit_paths(response: &Value) -> Result<Vec<String>> {
    Ok(response
        .get("data")
        .and_then(|data| data.get("hits"))
        .and_then(Value::as_array)
        .context("search response carries hits")?
        .iter()
        .filter_map(|hit| hit.get("path").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

#[test]
fn search_finds_frontmatter_quote() -> Result<()> {
    let root = test_root("frontmatter-quote")?;
    write(
        &root,
        "decisions/provenance.md",
        "---\n\
         type: decision\n\
         status: open\n\
         title: Provenance\n\
         tier: 2\n\
         quote: \"Use a durable source for every claim.\"\n\
         source: \"example:planning-note\"\n\
         ---\n\
         The decision applies to the pilot team.\n",
    )?;

    let response = ctx(&root, &["search", "durable source"]);
    remove_test_root(&root)?;

    assert_eq!(hit_paths(&response?)?, ["decisions/provenance.md"]);
    Ok(())
}

#[test]
fn search_finds_title() -> Result<()> {
    let root = test_root("title")?;
    write(
        &root,
        "meetings/planning-review.md",
        "---\n\
         type: meeting\n\
         title: Quarterly planning review\n\
         tier: 2\n\
         ---\n\
         Attendees agreed on the schedule.\n",
    )?;

    let response = ctx(&root, &["search", "quarterly planning"]);
    remove_test_root(&root)?;

    assert_eq!(hit_paths(&response?)?, ["meetings/planning-review.md"]);
    Ok(())
}

/// Text output written to a pipe, or under `NO_COLOR`, carries no terminal escape codes.
///
/// The ctx server captures a command's stdout and returns it in `/v1/command`, which agents
/// read and parse, so styling meant for a terminal must not reach that text. Observed on the
/// real surface (final acceptance at b0a545d): `ctx search` piped to
/// `od -c` with `NO_COLOR=1`, locally and through the server from another box, printed
/// `[ 033 [ 1 m E 1 033 [ 0 m ]`. It was older than remote mode, which is what makes agents
/// read it.
#[test]
fn piped_text_output_carries_no_escape_codes() -> Result<()> {
    let root = test_root("no-escape")?;
    write(
        &root,
        "meetings/planning-review.md",
        "---\n\
         type: meeting\n\
         title: Quarterly planning review\n\
         tier: 2\n\
         ---\n\
         Attendees agreed on the schedule.\n",
    )?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "search", "quarterly planning"])
        .env("NO_COLOR", "1")
        .env_remove("CTX_DATA_DIR")
        .env_remove("CTX_EMBED_MODEL")
        .output()
        .context("running ctx search")?;
    remove_test_root(&root)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Quarterly planning review"),
        "the hit is missing: {stdout:?}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "piped output under NO_COLOR carries escape codes: {stdout:?}"
    );
    Ok(())
}

/// A record body that would drive a terminal: it sets the window title (`OSC 0`, ended by
/// `BEL`), clears the screen (`CSI 2J`), backs over a character, and starts a sequence with
/// the one-character C1 `CSI` (U+009B).
const HOSTILE_BODY: &str =
    "Attendees \u{1b}]0;owned\u{7}agreed on \u{1b}[2Jthe\u{8} quarterly \u{9b}31mschedule.\n";

/// Every control character in `text` but a tab or a newline.
fn controls(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect()
}

/// A tree whose one meeting carries [`HOSTILE_BODY`]; only its body holds `quarterly`, so the
/// hit's line is the body's.
fn hostile_tree(label: &str) -> Result<PathBuf> {
    let root = test_root(label)?;
    write(
        &root,
        "meetings/planning-review.md",
        &format!("---\ntype: meeting\ntitle: Planning review\ntier: 2\n---\n{HOSTILE_BODY}"),
    )?;
    Ok(root)
}

/// Record text carries what its sources said, which anyone can write. Piped, `ctx search`
/// prints none of its control characters, the C1 ones included; what followed each is plain
/// text.
#[test]
fn piped_search_output_carries_none_of_the_source_s_controls() -> Result<()> {
    let root = hostile_tree("no-control")?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "search", "quarterly"])
        .env_remove("NO_COLOR")
        .env_remove("CTX_DATA_DIR")
        .env_remove("CTX_EMBED_MODEL")
        .output()
        .context("running ctx search");
    remove_test_root(&root)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("]0;ownedagreed on [2Jthe quarterly 31mschedule"),
        "the text around the controls is missing: {stdout:?}"
    );
    assert_eq!(controls(&stdout), Vec::<char>::new(), "{stdout:?}");
    Ok(())
}

/// `--json` keeps the record's text whole, and writes every control in it as an escape, so the
/// output itself carries none: `serde_json` escapes the C0 controls, and writes the C1 ones raw
/// unless told otherwise.
#[test]
fn json_search_output_escapes_every_control() -> Result<()> {
    let root = hostile_tree("json-control")?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "--json", "search", "quarterly"])
        .env_remove("CTX_DATA_DIR")
        .env_remove("CTX_EMBED_MODEL")
        .output()
        .context("running ctx search");
    remove_test_root(&root)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");
    assert_eq!(controls(&stdout), Vec::<char>::new(), "{stdout:?}");
    let value: Value = serde_json::from_str(&stdout)?;
    let line = value
        .pointer("/data/hits/0/line")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        ['\u{1b}', '\u{7}', '\u{9b}']
            .iter()
            .all(|control| line.contains(*control)),
        "the JSON line is not whole: {line:?}"
    );
    Ok(())
}

/// On a terminal the printers style their own text, and the source's controls still go: the
/// styling is added after the text is cleaned. The printer is driven directly, as the command
/// line drives it when standard output is a terminal.
#[test]
fn terminal_search_output_keeps_its_styling_and_drops_the_source_s_controls() -> Result<()> {
    let search = ctx_memory::search::Search {
        query: "quarterly".to_owned(),
        hits: vec![ctx_memory::search::Hit {
            path: "meetings/\u{1b}]0;owned\u{7}review.md".to_owned(),
            tier: Some("I2".to_owned()),
            title: "Quarterly \u{9b}2Jreview".to_owned(),
            line: HOSTILE_BODY.trim_end().to_owned(),
            matched_terms: 1,
            total_terms: 1,
            by_meaning: false,
        }],
        total_terms: 1,
        dropped_by_verbatim: 0,
        notice: None,
    };
    let mut out = Vec::new();
    ctx_memory::search::print(&search, &mut out)?;
    let printed = String::from_utf8(out)?;

    assert!(
        printed.contains("\u{1b}[1mI2\u{1b}[0m"),
        "the tier lost its styling: {printed:?}"
    );
    let unstyled = printed
        .replace("\u{1b}[1m", "")
        .replace("\u{1b}[2m", "")
        .replace("\u{1b}[0m", "");
    assert_eq!(controls(&unstyled), Vec::<char>::new(), "{printed:?}");
    Ok(())
}

#[test]
fn search_ranks_direct_sources_first() -> Result<()> {
    let root = test_root("tier-order")?;
    let record = |tier: u8| {
        format!(
            "---\n\
             type: decision\n\
             status: open\n\
             title: Renewal\n\
             tier: {tier}\n\
             quote: \"The vendor renewal was agreed.\"\n\
             source: \"example:planning-note\"\n\
             ---\n\
             Renewal terms are unchanged.\n"
        )
    };
    write(&root, "decisions/summary.md", &record(3))?;
    write(&root, "decisions/direct.md", &record(1))?;
    write(&root, "decisions/recollection.md", &record(4))?;
    write(&root, "decisions/internal.md", &record(2))?;

    let response = ctx(&root, &["search", "vendor renewal"]);
    remove_test_root(&root)?;

    assert_eq!(
        hit_paths(&response?)?,
        [
            "decisions/direct.md",
            "decisions/internal.md",
            "decisions/summary.md",
            "decisions/recollection.md",
        ]
    );
    Ok(())
}

#[test]
fn search_absence_only_when_no_document_matches() -> Result<()> {
    let root = test_root("absence")?;
    write(
        &root,
        "decisions/renewal.md",
        "---\n\
         type: decision\n\
         status: open\n\
         title: Renewal\n\
         tier: 3\n\
         quote: \"The vendor renewal was agreed.\"\n\
         source: \"example:planning-note\"\n\
         ---\n\
         Renewal terms are unchanged.\n",
    )?;
    let root_text = root
        .to_str()
        .context("test tree path is not valid UTF-8")?
        .to_owned();
    let run = |arguments: &[&str]| -> Result<String> {
        let output = binary()
            .args(["--memory", &root_text])
            .args(arguments)
            .output()
            .context("running ctx search")?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };

    let missing = run(&["search", "helium balloon"]);
    let verbatim = run(&["search", "--verbatim", "vendor renewal"]);
    let unlimited = run(&["search", "--limit", "0", "vendor renewal"]);
    remove_test_root(&root)?;

    let missing = missing?;
    assert!(
        missing.contains("none of the 2 query terms appear anywhere in the tree"),
        "{missing}"
    );
    let verbatim = verbatim?;
    assert!(
        !verbatim.contains("appear anywhere in the tree"),
        "a filtered-out match must not be reported as an absence: {verbatim}"
    );
    assert!(verbatim.contains('1'), "{verbatim}");
    let unlimited = unlimited?;
    assert!(
        !unlimited.contains("no hits"),
        "a matching document must not be reported as an absence under --limit 0: {unlimited}"
    );
    assert!(unlimited.contains("decisions/renewal.md"), "{unlimited}");
    Ok(())
}

#[test]
fn absence_is_not_claimed_for_terms_the_tree_holds_in_frontmatter() -> Result<()> {
    let root = test_root("absence-frontmatter")?;
    write(
        &root,
        "questions/source-coverage.md",
        "---\n\
         type: question\n\
         status: open\n\
         title: Source coverage for durable records\n\
         owner: Carol\n\
         tier: 2\n\
         quote: \"Which records need a source?\"\n\
         source: \"example:planning-note\"\n\
         ---\n\
         Resolve this question against the coverage metric.\n",
    )?;
    write(
        &root,
        "commitments/publish-guide.md",
        "---\n\
         type: commitment\n\
         status: open\n\
         title: Publish the contributor guide\n\
         owner: Bob\n\
         budget: 4200\n\
         confidential: true\n\
         tier: 2\n\
         quote: \"Publish the guide before the review.\"\n\
         source: \"example:planning-note\"\n\
         ---\n\
         This commitment supports the provenance decision.\n",
    )?;
    let root_text = root
        .to_str()
        .context("test tree path is not valid UTF-8")?
        .to_owned();
    let searched = binary()
        .args(["--memory", &root_text, "search", "Carol Bob"])
        .output()
        .context("running ctx search");
    let dossier = ctx(&root, &["dossier", "Carol"]);
    let numeric = ctx(&root, &["search", "4200 widgets"]);
    let boolean = ctx(&root, &["search", "true"]);
    remove_test_root(&root)?;

    let searched = searched?;
    assert!(
        searched.status.success(),
        "{}",
        String::from_utf8_lossy(&searched.stderr)
    );
    // `ctx dossier` reads the whole file, frontmatter included, so the tree does hold the term.
    let found = hit_paths(&dossier?)?;
    assert_eq!(found, ["questions/source-coverage.md"]);
    let searched = String::from_utf8_lossy(&searched.stdout).into_owned();
    assert!(
        !searched.contains("appear anywhere in the tree"),
        "a term the tree holds must not be reported as absent from it: {searched}"
    );
    // A number or a boolean in frontmatter is a term the tree holds, too.
    let numeric = numeric?;
    assert_eq!(hit_paths(&numeric)?, ["commitments/publish-guide.md"]);
    assert_eq!(
        numeric
            .get("data")
            .and_then(|data| data.get("hits"))
            .and_then(Value::as_array)
            .and_then(|hits| hits.first())
            .and_then(|hit| hit.get("matched_terms"))
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(hit_paths(&boolean?)?, ["commitments/publish-guide.md"]);
    Ok(())
}

#[test]
fn absence_is_not_claimed_for_a_term_only_in_unparsable_frontmatter() -> Result<()> {
    let root = test_root("absence-invalid-frontmatter")?;
    write(
        &root,
        "questions/pricing.md",
        "---\n\
         type: question\n\
         status: open\n\
         owner: Zelda\n\
         due: TBD\n\
         ---\n\
         What does a seat cost?\n",
    )?;

    let response = ctx(&root, &["search", "Zelda"]);
    remove_test_root(&root)?;

    assert_eq!(hit_paths(&response?)?, ["questions/pricing.md"]);
    Ok(())
}

#[test]
fn a_non_utf8_filename_does_not_stop_search() -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let root = test_root("non-utf8-filename")?;
    write(
        &root,
        "decisions/renewal.md",
        "The vendor renewal was agreed.\n",
    )?;
    let odd = root.join(std::ffi::OsStr::from_bytes(b"decisions/renewal-\xff.md"));
    std::fs::write(&odd, "A second note on the vendor renewal.\n")
        .with_context(|| format!("writing {}", odd.display()))?;

    let response = ctx(&root, &["search", "renewal"]);
    remove_test_root(&root)?;

    let paths = hit_paths(&response?)?;
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths.contains(&"decisions/renewal.md".to_owned()),
        "{paths:?}"
    );
    assert!(
        paths.contains(&"decisions/renewal-\u{fffd}.md".to_owned()),
        "{paths:?}"
    );
    Ok(())
}

#[test]
fn search_finds_a_record_by_owner_attendee_and_source_pointer() -> Result<()> {
    let root = test_root("frontmatter-fields")?;
    write(
        &root,
        "meetings/planning-review.md",
        "---\n\
         type: meeting\n\
         title: Planning review\n\
         date: 2026-10-25\n\
         attendees:\n\
         \x20 - Dana\n\
         sources:\n\
         \x20 - pointer: \"example:note-000123\"\n\
         \x20   tier: 2\n\
         ---\n\
         The review covered the renewal.\n",
    )?;
    write(
        &root,
        "commitments/publish-guide.md",
        "---\n\
         type: commitment\n\
         status: open\n\
         owner: Bob\n\
         what: Publish the contributor guide\n\
         title: Guide\n\
         tier: 2\n\
         quote: \"Publish the guide before the review.\"\n\
         source: \"example:note-000124\"\n\
         ---\n\
         Body text.\n",
    )?;

    let owner = ctx(&root, &["search", "Bob"]);
    let attendee = ctx(&root, &["search", "Dana"]);
    let pointer = ctx(&root, &["search", "note-000123"]);
    let nested_pointer = ctx(&root, &["search", "note-000124"]);
    let what = ctx(&root, &["search", "contributor"]);
    remove_test_root(&root)?;

    assert_eq!(hit_paths(&owner?)?, ["commitments/publish-guide.md"]);
    assert_eq!(hit_paths(&attendee?)?, ["meetings/planning-review.md"]);
    assert_eq!(hit_paths(&pointer?)?, ["meetings/planning-review.md"]);
    assert_eq!(
        hit_paths(&nested_pointer?)?,
        ["commitments/publish-guide.md"]
    );
    assert_eq!(hit_paths(&what?)?, ["commitments/publish-guide.md"]);
    Ok(())
}

#[test]
fn search_limit_bounds_hits_and_zero_returns_every_match() -> Result<()> {
    let root = test_root("limit")?;
    let record = |tier: u8| {
        format!(
            "---\n\
             type: decision\n\
             status: open\n\
             title: Renewal\n\
             tier: {tier}\n\
             quote: \"The vendor renewal was agreed.\"\n\
             source: \"example:planning-note\"\n\
             ---\n\
             Renewal terms are unchanged.\n"
        )
    };
    write(&root, "decisions/a.md", &record(1))?;
    write(&root, "decisions/b.md", &record(2))?;
    write(&root, "decisions/c.md", &record(1))?;
    write(&root, "decisions/summary.md", &record(3))?;

    let unlimited = ctx(&root, &["search", "--limit", "0", "renewal"]);
    let two = ctx(&root, &["search", "--limit", "2", "renewal"]);
    let verbatim_unlimited = ctx(&root, &["search", "--verbatim", "--limit", "0", "renewal"]);
    let verbatim_one = ctx(&root, &["search", "--verbatim", "--limit", "1", "renewal"]);
    remove_test_root(&root)?;

    let dropped = |response: &Value| {
        response
            .get("data")
            .and_then(|data| data.get("dropped_by_verbatim"))
            .and_then(Value::as_u64)
    };

    assert_eq!(hit_paths(&unlimited?)?.len(), 4);

    let two = hit_paths(&two?)?;
    assert_eq!(two.len(), 2);
    assert!(
        !two.contains(&"decisions/summary.md".to_owned()),
        "the limit keeps the best-ranked hits: {two:?}"
    );

    let verbatim_unlimited = verbatim_unlimited?;
    let mut direct = hit_paths(&verbatim_unlimited)?;
    direct.sort();
    assert_eq!(
        direct,
        ["decisions/a.md", "decisions/b.md", "decisions/c.md"]
    );
    assert_eq!(dropped(&verbatim_unlimited), Some(1));

    let verbatim_one = verbatim_one?;
    assert_eq!(hit_paths(&verbatim_one)?.len(), 1);
    assert_eq!(dropped(&verbatim_one), Some(1));
    Ok(())
}

/// The record a paraphrase means, and one that means something else.
fn write_paraphrase_tree(root: &Path) -> Result<()> {
    write(root, "ctx.toml", "version = 1\nid = \"paraphrase\"\n")?;
    write(
        root,
        "decisions/renewal.md",
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 2\n\
         ---\n\
         The Acme contract renewal is due in March.\n",
    )?;
    write(
        root,
        "people/panel.md",
        "---\n\
         type: person\n\
         status: active\n\
         ---\n\
         Interview the hiring candidate on Tuesday.\n",
    )?;
    Ok(())
}

#[test]
fn search_finds_paraphrase() -> Result<()> {
    let root = test_root("paraphrase")?;
    write_paraphrase_tree(&root)?;
    let config = TreeConfig::load(&root)?;
    let tree = Tree::load(&root, &config)?;
    let embedder: Arc<dyn Embed> = Arc::new(ConceptEmbedder::new("fake-embedding-1"));
    let index = Index::build(&tree, &Embedding::Ready(embedder))?;

    let paraphrase = index.search("supplier agreement extends spring", 10)?;
    let unrelated = index.search("zebra giraffe", 10)?;
    remove_test_root(&root)?;

    assert_eq!(paraphrase.notice, None);
    assert_eq!(
        paraphrase
            .hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        ["decisions/renewal.md"],
        "the query shares no word with the record it paraphrases"
    );
    assert!(
        paraphrase.hits.iter().all(|hit| hit.by_meaning),
        "a hit with no matching word is a meaning hit"
    );
    assert!(
        unrelated.hits.is_empty(),
        "meaning search must not return every record: {:?}",
        unrelated.hits
    );
    Ok(())
}

#[test]
fn no_credentials_falls_back_to_text_search_with_notice() -> Result<()> {
    let root = test_root("no-credentials")?;
    write_paraphrase_tree(&root)?;
    let data = root.join("data");

    // A project is configured, so meaning search is asked for, and the metadata server that
    // hands out the service identity is not there, which is what a missing credential is.
    let response = ctx_with_env(
        &root,
        &[
            ("GOOGLE_CLOUD_PROJECT", "example-project"),
            ("GOOGLE_CLOUD_LOCATION", "us-west1"),
            ("CTX_EMBED_MODEL", "text-embedding-005"),
            ("GCE_METADATA_HOST", "127.0.0.1:1"),
            ("CTX_DATA_DIR", &data.to_string_lossy()),
        ],
        &["search", "contract renewal"],
    );
    remove_test_root(&root)?;
    let response = response?;

    assert_eq!(hit_paths(&response)?, ["decisions/renewal.md"]);
    let notice = response
        .get("data")
        .and_then(|data| data.get("notice"))
        .and_then(Value::as_str)
        .context("a fallback to text search says so")?;
    assert!(
        notice.contains("meaning search is off"),
        "unexpected notice: {notice}"
    );
    Ok(())
}

#[test]
fn vertex_vectors_are_cached_by_content_hash() -> Result<()> {
    let root = test_root("vertex-cache")?;
    write_paraphrase_tree(&root)?;
    let data = root.join("data");
    let metadata = FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({"access_token": "ya29.test", "expires_in": 3599}).to_string(),
    )?;
    let vertex = FakeHttp::start()?;
    let predict = "/v1/projects/example-project/locations/us-west1/publishers/google/models/text-embedding-005:predict";
    vertex.json(
        predict,
        200,
        &predictions(&[
            "The Acme contract renewal is due in March.",
            "Interview the hiring candidate on Tuesday.",
        ]),
    )?;
    vertex.then_json(
        predict,
        200,
        &predictions(&["supplier agreement extends spring"]),
    )?;
    let environment = [
        ("GOOGLE_CLOUD_PROJECT", "example-project"),
        ("GOOGLE_CLOUD_LOCATION", "us-west1"),
        ("CTX_EMBED_MODEL", "text-embedding-005"),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
        ("CTX_VERTEX_API_BASE", &vertex.url("")),
        ("CTX_DATA_DIR", &data.to_string_lossy()),
    ];

    let first = ctx_with_env(
        &root,
        &environment,
        &["search", "supplier agreement extends spring"],
    );
    let asked_first = vertex.requests()?;
    let second = ctx_with_env(
        &root,
        &environment,
        &["search", "supplier agreement extends spring"],
    );
    let asked_second = vertex.requests()?;
    let cached = std::fs::read_dir(data.join("embeddings"))
        .context("the embedding cache directory exists")?
        .count();
    remove_test_root(&root)?;

    assert_eq!(hit_paths(&first?)?, ["decisions/renewal.md"]);
    assert_eq!(
        asked_first.len(),
        2,
        "one request for the records and one for the query: {asked_first:?}"
    );
    let body: Value = serde_json::from_str(
        &asked_first
            .first()
            .context("the first request embedded the records")?
            .body,
    )?;
    let first_content = body
        .pointer("/instances/0/content")
        .and_then(Value::as_str)
        .context("the first instance carried text")?;
    assert!(
        first_content.contains("The Acme contract renewal is due in March."),
        "the record's own text is what is embedded: {body}"
    );
    assert_eq!(
        body.pointer("/parameters/outputDimensionality")
            .and_then(Value::as_u64),
        Some(768)
    );

    assert_eq!(hit_paths(&second?)?, ["decisions/renewal.md"]);
    assert_eq!(
        asked_second.len(),
        3,
        "the second run reads the record vectors from the cache and asks only for the query's: {asked_second:?}"
    );
    assert_eq!(cached, 2, "one cache file per record, none for the query");
    Ok(())
}

/// A search query's vector is never kept. The cache under `embeddings/` holds the vectors of
/// records and messages only, so a caller that searches for one new string after another can
/// neither fill the shared disk nor leave the share a writer the layout does not name.
#[test]
fn a_search_writes_nothing_under_embeddings() -> Result<()> {
    let root = test_root("query-not-cached")?;
    write_paraphrase_tree(&root)?;
    let data = root.join("data");
    let metadata = FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({"access_token": "ya29.test", "expires_in": 3599}).to_string(),
    )?;
    let vertex = FakeHttp::start()?;
    let predict = "/v1/projects/example-project/locations/us-west1/publishers/google/models/text-embedding-005:predict";
    vertex.json(
        predict,
        200,
        &predictions(&[
            "The Acme contract renewal is due in March.",
            "Interview the hiring candidate on Tuesday.",
        ]),
    )?;
    vertex.then_json(
        predict,
        200,
        &predictions(&["supplier agreement extends spring"]),
    )?;
    vertex.then_json(predict, 200, &predictions(&["acme renewal"]))?;
    let environment = [
        ("GOOGLE_CLOUD_PROJECT", "example-project"),
        ("GOOGLE_CLOUD_LOCATION", "us-west1"),
        ("CTX_EMBED_MODEL", "text-embedding-005"),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
        ("CTX_VERTEX_API_BASE", &vertex.url("")),
        ("CTX_DATA_DIR", &data.to_string_lossy()),
    ];

    // The first search embeds the two records, whose vectors the cache keeps.
    let first = ctx_with_env(
        &root,
        &environment,
        &["search", "supplier agreement extends spring"],
    );
    let after_first = cache_files(&data);
    let second = ctx_with_env(&root, &environment, &["search", "acme renewal"]);
    let after_second = cache_files(&data);
    let asked = vertex.requests()?;
    remove_test_root(&root)?;
    let (after_first, after_second) = (after_first?, after_second?);

    assert_eq!(hit_paths(&first?)?, ["decisions/renewal.md"]);
    assert_eq!(hit_paths(&second?)?, ["decisions/renewal.md"]);
    assert_eq!(
        after_first.len(),
        2,
        "the cache holds the two records' vectors and no query's: {after_first:?}"
    );
    assert_eq!(
        after_second, after_first,
        "a search wrote under embeddings/"
    );
    assert_eq!(
        asked.len(),
        3,
        "the records once, then each query afresh: {asked:?}"
    );
    Ok(())
}

/// The names of the files in the embedding cache, sorted.
fn cache_files(data: &Path) -> Result<Vec<String>> {
    let dir = data.join("embeddings");
    let mut names = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// A Vertex answer carrying the fake embedder's vector for each text, in order.
fn predictions(texts: &[&str]) -> String {
    let predictions = texts
        .iter()
        .map(|text| json!({"embeddings": {"values": fake_embed::vector(text)}}))
        .collect::<Vec<_>>();
    json!({ "predictions": predictions }).to_string()
}
