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
use serde_json::Value;

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

fn run_ctx(root: &str, arguments: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--memory", root])
        .args(arguments)
        .output()
        .context("running ctx")
}

fn json_data(output: &std::process::Output) -> Result<Value> {
    let response: Value =
        serde_json::from_slice(&output.stdout).context("parsing ctx JSON output")?;
    response
        .get("data")
        .cloned()
        .context("ctx JSON response carries data")
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

/// The single writer refuses one record at a time, so a negative-claim finding has to say
/// which record states the absence. A finding it cannot attribute is a finding that never
/// fires at the write gate.
#[test]
fn negative_claim_finding_names_its_record_and_line() -> Result<()> {
    let root = test_root("negative-path")?;
    write(
        &root,
        "claims/absence.md",
        "---\ntype: note\n---\n\nThe vendor was never told about the renewal.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let findings = result?;
    let negative = findings
        .negatives
        .first()
        .context("the sweep reports the unsourced negative")?;
    assert_eq!(negative.path, "claims/absence.md");
    assert_eq!(negative.line, 5);
    assert!(
        negative.snippet.contains("never told"),
        "{}",
        negative.snippet
    );
    Ok(())
}

/// The frontmatter's `quote:` and `tier: 1` lines name where the record's quote came from,
/// not where a line of its prose did. A negative on the first body line, written straight
/// after the closing `---`, is a paragraph of its own and must name its own source.
#[test]
fn a_negative_on_the_first_body_line_is_not_exempted_by_the_frontmatter() -> Result<()> {
    let root = test_root("negative-first-body-line")?;
    write(
        &root,
        "decisions/absence.md",
        "---\n\
         type: decision\n\
         tier: 1\n\
         quote: \"Keep the cadence.\"\n\
         source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         ---\n\
         The vendor was never told about the renewal.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let findings = result?;
    let negative = findings
        .negatives
        .first()
        .context("the sweep reports the negative on the first body line")?;
    assert_eq!(negative.path, "decisions/absence.md");
    assert_eq!(negative.line, 7);
    Ok(())
}

/// A meeting record is prose about what people said. An absence it states in its own voice
/// needs a source as much as any other record's does.
#[test]
fn a_meeting_record_is_checked_for_unsourced_negatives() -> Result<()> {
    let root = test_root("negative-meeting")?;
    write(
        &root,
        "meetings/review.md",
        "---\n\
         type: meeting\n\
         title: Renewal review\n\
         ---\n\
         \n\
         The vendor was never told about the renewal.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let findings = result?;
    let negative = findings
        .negatives
        .first()
        .context("the sweep reports the meeting's unsourced negative")?;
    assert_eq!(negative.path, "meetings/review.md");
    assert_eq!(negative.line, 6);
    Ok(())
}

/// A verbatim quote that states an absence is the evidence, not an unsourced claim, and the
/// body after it states none, so the record is clean.
#[test]
fn a_quoted_absence_in_the_frontmatter_is_not_a_negative() -> Result<()> {
    let root = test_root("negative-quoted")?;
    write(
        &root,
        "decisions/quoted.md",
        "---\n\
         type: decision\n\
         tier: 1\n\
         quote: \"We were never told about the renewal.\"\n\
         source: \"slack:C0EXAMPLE01/1700000000.000001\"\n\
         ---\n\
         The vendor's words are quoted above.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.negatives, Vec::new());
    Ok(())
}

/// The paths and lines the sweep reports as unsourced negatives, in report order.
fn negative_lines(findings: &sweep::Findings) -> Vec<(&str, usize)> {
    findings
        .negatives
        .iter()
        .map(|negative| (negative.path.as_str(), negative.line))
        .collect()
}

/// A tier 3 record that states an absence in its `statement` and names no source for it is
/// the case the negative rule is for. A record's `tier: 1` does not excuse it either when no
/// quote it makes is at tier 1 or 2: the words `quote:` and `tier: 1` are not evidence.
#[test]
fn a_statement_of_absence_without_a_source_is_a_negative() -> Result<()> {
    let root = test_root("negative-statement")?;
    write(
        &root,
        "decisions/summary.md",
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 3\n\
         statement: The vendor was never told about the renewal.\n\
         quote: \"a paraphrase of the standup\"\n\
         source: \"overheard in standup\"\n\
         ---\n\
         The record body.\n",
    )?;
    write(
        &root,
        "decisions/tier-1-summary.md",
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 1\n\
         quotes:\n\
         \x20 - quote: \"a paraphrase of the standup\"\n\
         \x20   source: \"overheard in standup\"\n\
         \x20   tier: 3\n\
         statement: The vendor was never told about the renewal.\n\
         ---\n\
         The record body.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    let findings = result?;
    assert_eq!(
        negative_lines(&findings),
        [
            ("decisions/summary.md", 5),
            ("decisions/tier-1-summary.md", 9)
        ]
    );
    let snippet = findings
        .negatives
        .first()
        .map(|negative| negative.snippet.as_str())
        .unwrap_or_default();
    assert!(snippet.contains("never told"), "{snippet}");
    Ok(())
}

/// `title`, `what` and `statement` are each checked on their own value, so writing the value
/// in YAML double quotes does not hide the absence it states.
#[test]
fn each_claim_field_is_checked_whatever_its_yaml_quoting() -> Result<()> {
    let root = test_root("negative-claim-fields")?;
    write(
        &root,
        "commitments/payment.md",
        "---\n\
         type: commitment\n\
         status: open\n\
         title: \"Nobody told the vendor about the renewal\"\n\
         what: The vendor was never paid for March.\n\
         tier: 4\n\
         quote: \"someone mentioned it\"\n\
         source: \"overheard in standup\"\n\
         ---\n\
         The record body.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(
        negative_lines(&result?),
        [("commitments/payment.md", 4), ("commitments/payment.md", 5)]
    );
    Ok(())
}

/// A statement of absence that says what was searched is the honest form: in its own text,
/// with a marker the body accepts, or in the record's `searched:` field, which the method
/// prescribes for a `not_found` record.
#[test]
fn a_statement_of_absence_that_names_the_search_is_not_a_negative() -> Result<()> {
    let root = test_root("negative-searched")?;
    write(
        &root,
        "decisions/inline.md",
        "---\n\
         type: decision\n\
         status: open\n\
         tier: 3\n\
         statement: Nobody answered Alice; we searched the inbox for a reply.\n\
         ---\n\
         The record body.\n",
    )?;
    write(
        &root,
        "not-found/renewal.md",
        "---\n\
         type: not_found\n\
         statement: The vendor was never told about the renewal.\n\
         searched:\n\
         \x20 - \"gmail: from:vendor renewal\"\n\
         \x20 - \"slack: #vendor since 2026-09-01\"\n\
         ---\n\
         The record body.\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(negative_lines(&result?), Vec::<(&str, usize)>::new());
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
fn invalid_due_is_a_finding_not_a_failure() -> Result<()> {
    let root = test_root("invalid-frontmatter")?;
    write(
        &root,
        "rules.md",
        "---\ntype: rules\n---\n[[questions/pricing]]\n",
    )?;
    write(
        &root,
        "questions/pricing.md",
        "---\n\
         type: question\n\
         status: open\n\
         due: TBD\n\
         ---\n\
         What does a seat cost?\n",
    )?;
    let root_text = root
        .to_str()
        .context("test tree path is not valid UTF-8")?
        .to_owned();

    let search = run_ctx(&root_text, &["search", "anything"])?;
    let open = run_ctx(&root_text, &["--json", "open"])?;
    let swept = run_ctx(&root_text, &["--json", "sweep"])?;
    remove_test_root(&root)?;

    assert_eq!(
        search.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    assert_eq!(
        open.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let open_items = json_data(&open)?
        .get("items")
        .and_then(Value::as_array)
        .context("open response has items")?
        .clone();
    assert!(
        !open_items
            .iter()
            .any(|item| item.get("path")
                == Some(&Value::String("questions/pricing.md".to_owned()))),
        "a record with invalid frontmatter must not be reported as open"
    );

    assert_eq!(swept.status.code(), Some(1));
    let invalid = json_data(&swept)?
        .get("frontmatter_invalid")
        .and_then(Value::as_array)
        .context("sweep response lists invalid frontmatter")?
        .clone();
    assert_eq!(invalid.len(), 1);
    let finding = invalid.first().context("one frontmatter finding")?;
    assert_eq!(
        finding.get("kind").and_then(Value::as_str),
        Some("frontmatter-invalid")
    );
    assert_eq!(
        finding.get("path").and_then(Value::as_str),
        Some("questions/pricing.md")
    );
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

/// A link target is the record's text, which can quote anyone. `ctx sweep` prints a gap's
/// target without its control characters (`ESC c`, a full terminal reset; `BEL`; the C1 `CSI`).
#[test]
fn piped_sweep_output_carries_none_of_the_tree_s_controls() -> Result<()> {
    let root = test_root("no-control")?;
    write(&root, "ctx.toml", "version = 1\n")?;
    write(&root, "rules.md", "---\ntype: rules\n---\n# Rules\n")?;
    write(
        &root,
        "notes/review.md",
        "---\ntitle: Review\n---\nSee [[gap\u{1b}c\u{7}\u{9b}31mtarget]].\n",
    )?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = run_ctx(tree, &["sweep"]);
    remove_test_root(&root)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("gapc31mtarget"),
        "the gap is missing: {stdout:?}"
    );
    let controls: Vec<char> = stdout
        .chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect();
    assert_eq!(controls, Vec::<char>::new(), "{stdout:?}");
    Ok(())
}
