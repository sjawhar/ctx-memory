use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{NaiveDate, Utc};
use ctx_memory::{
    config::TreeConfig,
    open::{open_items, open_items_at},
    schema::today_utc,
    sweep,
    tree::Tree,
};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-sweep-{label}-{}-{suffix}",
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

fn findings(root: &Path) -> Result<sweep::Findings> {
    let tree = Tree::load(root, &TreeConfig::default())?;
    sweep::run(&tree)
}

#[test]
fn md_suffix_relative_and_case_variants_resolve() -> Result<()> {
    let root = test_root("link-variants")?;
    write(&root, "targets/Foo.md", "# Target\n")?;
    write(
        &root,
        "sub/x.md",
        "[[targets/Foo.md]]\n[[../targets/Foo]]\n[[targets/foo]]\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert!(result?.gaps.is_empty());
    Ok(())
}

#[test]
fn unique_basename_resolves_and_shared_basename_is_ambiguous() -> Result<()> {
    let root = test_root("basename-links")?;
    write(&root, "a/note.md", "# Note\n")?;
    write(&root, "reference.md", "[[note]]\n")?;
    let unique = findings(&root)?;
    assert!(unique.gaps.is_empty());

    write(&root, "b/note.md", "# Another note\n")?;
    let duplicate = findings(&root);
    remove_test_root(&root)?;

    let duplicate = duplicate?;
    assert!(duplicate.gaps.is_empty());
    let ambiguity = duplicate
        .ambiguous
        .first()
        .context("shared basename was not reported as ambiguous")?;
    assert_eq!(ambiguity.target, "note");
    assert_eq!(ambiguity.from, "reference.md");
    assert_eq!(ambiguity.candidates, vec!["a/note.md", "b/note.md"]);
    Ok(())
}

#[test]
fn self_link_does_not_save_an_orphan() -> Result<()> {
    let root = test_root("self-link")?;
    write(&root, "self.md", "[[self]]\n")?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.orphans, vec!["self.md"]);
    Ok(())
}

#[test]
fn gitignored_and_hidden_files_are_not_records() -> Result<()> {
    let root = test_root("ignored-files")?;
    write(&root, ".gitignore", "build/\n")?;
    write(&root, ".obsidian/hidden.md", "# Hidden\n")?;
    write(&root, "build/generated.md", "# Generated\n")?;
    write(&root, "visible.md", "# Visible\n")?;
    let tree = Tree::load(&root, &TreeConfig::default());
    remove_test_root(&root)?;

    let tree = tree?;
    assert_eq!(tree.docs.len(), 1);
    let document = tree
        .docs
        .first()
        .context("visible Markdown document was not loaded")?;
    assert_eq!(document.rel, Path::new("visible.md"));
    Ok(())
}

#[test]
fn crlf_records_are_checked() -> Result<()> {
    let root = test_root("crlf-record")?;
    write(
        &root,
        "decision.md",
        "---\r\ntype: decision\r\ntier: external\r\n---\r\nNo quote.\r\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.unquoted, vec!["decision.md"]);
    Ok(())
}

#[test]
fn crlf_paragraphs_do_not_hide_an_unsourced_negative() -> Result<()> {
    let crlf_root = test_root("negative-crlf")?;
    write(
        &crlf_root,
        "claim.md",
        "They were never told.\r\n\r\nThis paragraph mentions email but not a source.\r\n",
    )?;
    let crlf = findings(&crlf_root);
    remove_test_root(&crlf_root)?;

    let lf_root = test_root("negative-lf")?;
    write(
        &lf_root,
        "claim.md",
        "They were never told.\n\nThis paragraph mentions email but not a source.\n",
    )?;
    let lf = findings(&lf_root);
    remove_test_root(&lf_root)?;

    assert_eq!(crlf?.negatives, lf?.negatives);
    Ok(())
}

#[test]
fn missing_rules_page_is_structural() -> Result<()> {
    let root = test_root("missing-rules")?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert!(result.rules_missing);
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn wrong_type_rules_page_is_structural() -> Result<()> {
    let root = test_root("wrong-type-rules")?;
    write(&root, "rules.md", "---\ntype: note\n---\n# Rules\n")?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert!(result.rules_wrong_type);
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn oversize_rules_page_is_structural() -> Result<()> {
    let root = test_root("oversize-rules")?;
    let prefix = "---\ntype: rules\n---\n";
    let content = format!("{prefix}{}", "x".repeat(8_193 - prefix.len()));
    write(&root, "rules.md", &content)?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert_eq!(result.rules_oversize, Some(8_193));
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn rules_page_at_the_byte_limit_is_not_oversize() -> Result<()> {
    let root = test_root("rules-byte-limit")?;
    let prefix = "---\ntype: rules\n---\n";
    let content = format!("{prefix}{}", "x".repeat(8_192 - prefix.len()));
    write(&root, "rules.md", &content)?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert_eq!(result.rules_oversize, None);
    assert!(result.structural_clean());
    Ok(())
}

#[test]
fn nested_rules_page_is_still_an_orphan() -> Result<()> {
    let root = test_root("nested-rules")?;
    write(&root, "rules.md", "---\ntype: rules\n---\n# Rules\n")?;
    write(
        &root,
        "sub/rules.md",
        "---\ntype: note\n---\n# Nested rules\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.orphans, vec!["sub/rules.md"]);
    Ok(())
}

#[test]
fn nested_memory_md_is_an_orphan() -> Result<()> {
    let root = test_root("nested-memory")?;
    write(&root, "rules.md", "---\ntype: rules\n---\n# Rules\n")?;
    write(&root, "sub/memory.md", "# Nested memory\n")?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.orphans, vec!["sub/memory.md"]);
    Ok(())
}

#[test]
fn schema_violation_is_structural_and_names_the_field() -> Result<()> {
    let root = test_root("schema-violation")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[decisions/x]]\n",
    )?;
    write(
        &root,
        "schemas/decision.yaml",
        r"$schema: https://json-schema.org/draft/2020-12/schema
type: object
required: [status, what]
properties:
  status:
    enum: [open, decided]
",
    )?;
    write(
        &root,
        "decisions/x.md",
        "---\n\
         type: decision\n\
         status: maybe\n\
         what: Example decision\n\
         tier: external\n\
         quote: quoted\n\
         ---\n\
         # Example\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert_eq!(result.schema_violations.len(), 1);
    let violation = result
        .schema_violations
        .first()
        .context("schema violation was not reported")?;
    assert_eq!(violation.path, "decisions/x.md");
    assert_eq!(violation.pointer, "/status");
    assert!(violation.message.contains("maybe"));
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn schema_required_field_is_reported_when_frontmatter_omits_it() -> Result<()> {
    let root = test_root("schema-required")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[decisions/x]]\n",
    )?;
    write(
        &root,
        "schemas/decision.yaml",
        "type: object\nrequired: [what]\n",
    )?;
    write(
        &root,
        "decisions/x.md",
        "---\ntype: decision\nstatus: open\ntier: external\nquote: quoted\n---\n# Example\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert_eq!(result.schema_violations.len(), 1);
    let violation = result
        .schema_violations
        .first()
        .context("missing required field was not reported")?;
    assert_eq!(violation.path, "decisions/x.md");
    assert!(violation.message.contains("what"));
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn schema_date_format_violation_is_structural() -> Result<()> {
    let root = test_root("schema-date-format")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[decisions/x]]\n",
    )?;
    write(
        &root,
        "schemas/decision.yaml",
        "type: object\nproperties:\n  review_date:\n    type: string\n    format: date\n",
    )?;
    write(
        &root,
        "decisions/x.md",
        "---\n\
         type: decision\n\
         status: open\n\
         review_date: not-a-date\n\
         tier: external\n\
         quote: quoted\n\
         ---\n\
         # Example\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let result = result?;
    assert_eq!(result.schema_violations.len(), 1);
    let violation = result
        .schema_violations
        .first()
        .context("date format violation was not reported")?;
    assert_eq!(violation.pointer, "/review_date");
    assert!(violation.message.contains("date"));
    assert!(!result.structural_clean());
    Ok(())
}

#[test]
fn clean_sweep_summary_says_no_findings() -> Result<()> {
    let root = test_root("clean-summary")?;
    write(&root, "rules.md", "---\ntype: rules\n---\n# Rules\n")?;
    let root_text = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--memory", root_text, "sweep"])
        .output()
        .context("running clean ctx sweep")?;
    remove_test_root(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no findings"));
    Ok(())
}

#[test]
fn summary_never_claims_nothing_stale_when_items_are_listed() -> Result<()> {
    let root = test_root("stale-summary")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[decisions/example]]\n",
    )?;
    write(
        &root,
        "decisions/example.md",
        "---\n\
         type: decision\n\
         status: open\n\
         tier: external\n\
         quote: quoted\n\
         ---\n\
         # Example\n",
    )?;
    let root_text = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--memory", root_text, "sweep"])
        .output()
        .context("running stale ctx sweep")?;
    remove_test_root(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("structure clean — 1 stale item(s) need review"));
    assert!(!stdout.contains("nothing stale"));
    Ok(())
}

#[test]
fn overdue_days_use_the_utc_date() -> Result<()> {
    let root = test_root("utc-date")?;
    write(
        &root,
        "decision.md",
        "---\n\
         type: decision\n\
         status: open\n\
         revisit_by: 2026-10-23\n\
         ---\n\
         # Example\n",
    )?;
    let tree = Tree::load(&root, &TreeConfig::default());
    remove_test_root(&root)?;

    let tree = tree?;
    let fixed_today =
        NaiveDate::from_ymd_opt(2026, 10, 25).context("fixed UTC date was invalid")?;
    let fixed_items = open_items_at(&tree, fixed_today);
    assert_eq!(
        fixed_items.first().and_then(|item| item.days_overdue),
        Some(2)
    );

    assert_eq!(today_utc(), Utc::now().date_naive());
    let current_items = open_items(&tree);
    let expected_items = open_items_at(&tree, today_utc());
    assert_eq!(
        current_items.first().and_then(|item| item.days_overdue),
        expected_items.first().and_then(|item| item.days_overdue)
    );
    Ok(())
}
