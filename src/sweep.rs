//! Structural checks for a markdown context tree.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use owo_colors::OwoColorize as _;
use regex::Regex;
use serde::Serialize;

use crate::config::RULES_FILE;
use crate::output::printable;
use crate::records_schema::{self, SchemaViolation};
use crate::schema::{today_utc, NEEDS_QUOTE, NEEDS_TIER, OPEN_KINDS, OPEN_STATES};
use crate::tree::{wikilinks, Doc, Resolution, Tree};

#[derive(Debug, Serialize)]
pub struct Gap {
    pub target: String,
    pub from: String,
}

#[derive(Debug, Serialize)]
pub struct Ambiguous {
    pub target: String,
    pub from: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Stale {
    pub path: String,
    pub status: String,
    pub why: String,
    pub owner: Option<String>,
    /// `Some(days)` when a date has passed; `None` when no date was set.
    pub overdue_days: Option<i64>,
}

/// A document whose frontmatter did not parse. Its body and links are still checked.
#[derive(Debug, Serialize)]
pub struct InvalidFront {
    /// The finding kind, for callers that read the JSON envelope.
    pub kind: &'static str,
    pub path: String,
    pub error: String,
}

/// A universal negative a record states without naming the source it checked, at the line
/// that states it.
///
/// The single writer refuses one record at a time, so the finding names its record: a line of
/// prose the writer cannot attribute to a path is a finding that never fires at the gate.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Negative {
    pub path: String,
    pub line: usize,
    pub snippet: String,
}

impl Negative {
    /// The finding as one line of report text.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{}:{} — \"{}\" (source it directly or identify the searched surface)",
            self.path, self.line, self.snippet
        )
    }
}

pub const FRONTMATTER_INVALID: &str = "frontmatter-invalid";

#[derive(Debug, Default, Serialize)]
pub struct Findings {
    pub files: usize,
    pub gaps: Vec<Gap>,
    pub ambiguous: Vec<Ambiguous>,
    pub stale: Vec<Stale>,
    pub untiered: Vec<String>,
    pub unquoted: Vec<String>,
    pub negatives: Vec<Negative>,
    pub orphans: Vec<String>,
    pub rules_missing: bool,
    pub rules_wrong_type: bool,
    pub rules_oversize: Option<usize>,
    pub schema_violations: Vec<SchemaViolation>,
    pub frontmatter_invalid: Vec<InvalidFront>,
}

impl Findings {
    /// Structural findings are defects in the graph. Stale records remain visible without
    /// making the command fail.
    #[must_use]
    pub const fn structural_clean(&self) -> bool {
        self.gaps.is_empty()
            && self.ambiguous.is_empty()
            && self.untiered.is_empty()
            && self.unquoted.is_empty()
            && self.negatives.is_empty()
            && self.orphans.is_empty()
            && !self.rules_missing
            && !self.rules_wrong_type
            && self.rules_oversize.is_none()
            && self.schema_violations.is_empty()
            && self.frontmatter_invalid.is_empty()
    }

    /// Nothing to report at all: structure is clean and no record is stale.
    #[must_use]
    pub const fn clean(&self) -> bool {
        self.structural_clean() && self.stale.is_empty()
    }
}

const RULES_MAX_BYTES: usize = 8_192;

fn negative_claim_re() -> Result<Regex> {
    Regex::new(
        r"(?i)\bnever\s+(told|informed|notified|received|heard|answered|replied|responded|communicated|paid|invoiced|asked)\b|\b(was|were|has|had)\s+never\s+(been\s+)?(told|informed|notified|paid|invoiced|asked|answered|sent|contacted|given)\b|\bno\s+evidence\s+\w+\s+(was\s+|were\s+)?ever\b|\b(nobody|no\s+one)\s+(ever\s+)?(told|asked|answered|informed)\b",
    )
    .map_err(Into::into)
}

fn paragraph_re() -> Result<Regex> {
    Regex::new(r"\r?\n\r?\n").map_err(Into::into)
}

/// A negative is exempt when its paragraph identifies a direct source or search surface.
fn has_tier1_marker(paragraph: &str) -> bool {
    let lower = paragraph.to_lowercase();
    [
        "tier 1",
        "tier: 1",
        "quote:",
        "searched",
        "no record",
        "thread",
        "email",
        "inbox",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn describe_deadline(
    deadline: Option<NaiveDate>,
    today: NaiveDate,
) -> Option<(String, Option<i64>)> {
    match deadline {
        None => Some(("no revisit_by/due date".to_owned(), None)),
        Some(date) if date < today => {
            let days = (today - date).num_days();
            Some((format!("overdue {days}d"), Some(days)))
        }
        Some(_) => None,
    }
}

fn is_closeable(doc: &Doc) -> bool {
    doc.front
        .kind
        .as_deref()
        .is_some_and(|kind| OPEN_KINDS.contains(&kind))
        && doc
            .front
            .status
            .as_deref()
            .is_some_and(|status| OPEN_STATES.contains(&status))
}

/// Scan a document for universal negatives that do not name a direct source.
///
/// The frontmatter is not prose: the fields that state the record's claim are checked one by
/// one, and the rest of it (a question, a name, the quotes and sources that are the evidence)
/// claims no absence. The body is checked paragraph by paragraph, apart from the frontmatter,
/// so a body that opens on the line after the closing `---` is a paragraph of its own. Every
/// kind of record is scanned: a meeting's notes state absences in the record's own voice as
/// readily as any other record's prose.
fn scan_negatives(
    doc: &Doc,
    negative_re: &Regex,
    paragraph_re: &Regex,
    negatives: &mut Vec<Negative>,
) -> Result<()> {
    if doc.rel.starts_with("raw") {
        return Ok(());
    }
    scan_claim_fields(doc, negative_re, negatives)?;
    let body = doc.body();
    let body_start = doc.frontmatter_block().len();
    let breaks = paragraph_re
        .find_iter(body)
        .map(|separator| (separator.start(), separator.end()))
        .chain(std::iter::once((body.len(), body.len())));
    let mut paragraph_start = 0;
    for (paragraph_end, next_start) in breaks {
        let paragraph = body
            .get(paragraph_start..paragraph_end)
            .ok_or_else(|| anyhow!("a paragraph break was not a character boundary"))?;
        if let Some(start) = unsourced_negative(paragraph, negative_re) {
            negatives.push(Negative {
                path: doc.rel.to_string_lossy().into_owned(),
                line: doc.line_at(body_start + paragraph_start + start),
                snippet: snippet(paragraph, start)?,
            });
        }
        paragraph_start = next_start;
    }
    Ok(())
}

/// Checks each frontmatter field in which the record states its claim, on its own value.
///
/// A negative there is excused by a marker the body accepts, written in that value; by a
/// non-empty `searched:` field, which names the search as the method prescribes for a
/// `not_found` record; or by a quote the record makes at tier 1 or 2, which is then the
/// evidence for it. The words `quote:` or `tier: 1` elsewhere in the frontmatter excuse
/// nothing. The sweep does not check quotes: `ctx cite verify` does, and the single writer
/// refuses a record whose tier 1 or 2 quote did not hold, so a quote excuses the absence only
/// where it was checked against its source.
fn scan_claim_fields(doc: &Doc, negative_re: &Regex, negatives: &mut Vec<Negative>) -> Result<()> {
    let front = &doc.front;
    if front.makes_verbatim_quote() || front.extra.get("searched").is_some_and(names_something) {
        return Ok(());
    }
    for (field, value) in [
        ("title", front.title.as_deref()),
        ("what", front.what.as_deref()),
        ("statement", front.extra_str("statement")),
    ] {
        let Some(value) = value else {
            continue;
        };
        let Some(start) = unsourced_negative(value, negative_re) else {
            continue;
        };
        let field_start = doc
            .frontmatter_line_start(|line| {
                line.strip_prefix(field)
                    .is_some_and(|rest| rest.starts_with(':'))
            })
            .unwrap_or(0);
        negatives.push(Negative {
            path: doc.rel.to_string_lossy().into_owned(),
            line: doc.line_at(field_start),
            snippet: snippet(value, start)?,
        });
    }
    Ok(())
}

/// Whether a `searched:` value names any search at all.
fn names_something(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => !text.trim().is_empty(),
        serde_json::Value::Array(values) => values.iter().any(names_something),
        serde_json::Value::Object(values) => values.values().any(names_something),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

/// Where a text states a universal negative outside a quotation, unless the text identifies a
/// direct source or the surface it searched.
fn unsourced_negative(text: &str, negative_re: &Regex) -> Option<usize> {
    if has_tier1_marker(text) {
        return None;
    }
    negative_re
        .find_iter(text)
        .find(|match_| {
            text.get(..match_.start())
                .is_some_and(|before| before.matches('"').count() % 2 == 0)
        })
        .map(|match_| match_.start())
}

/// Up to 80 characters of `text` from `start`, on one line.
fn snippet(text: &str, start: usize) -> Result<String> {
    Ok(text
        .get(start..)
        .ok_or_else(|| anyhow!("a negative-claim match was not a character boundary"))?
        .chars()
        .take(80)
        .map(|character| {
            if matches!(character, '\r' | '\n') {
                ' '
            } else {
                character
            }
        })
        .collect())
}

/// Runs the structural sweep for a loaded tree.
///
/// # Errors
///
/// Returns an error when a regex cannot be constructed.
#[allow(
    clippy::too_many_lines,
    reason = "one tree scan gathers interdependent findings"
)]
pub fn run(tree: &Tree) -> Result<Findings> {
    let today = today_utc();
    let (rules_missing, rules_wrong_type, rules_oversize) = match tree.rules() {
        Some(rules) => (
            false,
            rules.front.kind.as_deref() != Some("rules"),
            (rules.raw.len() > RULES_MAX_BYTES).then_some(rules.raw.len()),
        ),
        None => (true, false, None),
    };
    let negative_re = negative_claim_re()?;
    let paragraph_re = paragraph_re()?;
    let mut gaps = Vec::new();
    let mut ambiguous = Vec::new();
    let mut negatives = Vec::new();
    let mut linked = HashSet::new();
    for (from_position, doc) in tree.docs.iter().enumerate() {
        for link in wikilinks(&doc.raw) {
            match tree.resolve(doc, &link.target) {
                Resolution::Found(position) if position != from_position => {
                    linked.insert(position);
                }
                Resolution::Found(_) => {}
                Resolution::Missing => gaps.push(Gap {
                    target: link.target,
                    from: doc.rel.to_string_lossy().into_owned(),
                }),
                Resolution::Ambiguous(positions) => {
                    let candidates = positions
                        .into_iter()
                        .map(|position| {
                            tree.docs
                                .get(position)
                                .map(|candidate| candidate.rel.to_string_lossy().into_owned())
                                .ok_or_else(|| {
                                    anyhow!("resolver returned an invalid document position")
                                })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    ambiguous.push(Ambiguous {
                        target: link.target,
                        from: doc.rel.to_string_lossy().into_owned(),
                        candidates,
                    });
                }
            }
        }
        scan_negatives(doc, &negative_re, &paragraph_re, &mut negatives)?;
    }

    let mut stale = Vec::new();
    let mut untiered = Vec::new();
    let mut unquoted = Vec::new();
    for doc in &tree.docs {
        let path = doc.rel.to_string_lossy().into_owned();
        if is_closeable(doc) {
            if let Some((why, overdue_days)) = describe_deadline(doc.front.deadline(), today) {
                stale.push(Stale {
                    path: path.clone(),
                    status: doc.front.status.clone().unwrap_or_default(),
                    why,
                    owner: doc.front.owner.clone(),
                    overdue_days,
                });
            }
        }
        let kind = doc.front.kind.as_deref().unwrap_or_default();
        if NEEDS_TIER.contains(&kind) && !doc.front.has_any_tier() {
            untiered.push(path.clone());
        }
        if NEEDS_QUOTE.contains(&kind) && !doc.has_quote() {
            unquoted.push(path);
        }
    }

    stale.sort_by_key(|item| {
        (
            item.overdue_days.is_none(),
            std::cmp::Reverse(item.overdue_days.unwrap_or(0)),
        )
    });

    let orphans = tree
        .docs
        .iter()
        .enumerate()
        .filter(|(position, doc)| doc.rel != Path::new(RULES_FILE) && !linked.contains(position))
        .map(|(_, doc)| doc.rel.to_string_lossy().into_owned())
        .collect();
    let schema_violations = records_schema::validate(tree)?;
    let frontmatter_invalid = tree
        .docs
        .iter()
        .filter_map(|doc| {
            doc.front_error.as_ref().map(|error| InvalidFront {
                kind: FRONTMATTER_INVALID,
                path: doc.rel.to_string_lossy().into_owned(),
                error: error.clone(),
            })
        })
        .collect();

    Ok(Findings {
        files: tree.docs.len(),
        gaps,
        ambiguous,
        stale,
        untiered,
        unquoted,
        negatives,
        orphans,
        rules_missing,
        rules_wrong_type,
        rules_oversize,
        schema_violations,
        frontmatter_invalid,
    })
}

/// Prints a sweep the way a person reads it: each kind of finding under its own heading.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(findings: &Findings, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "{} files\n", findings.files)?;

    if !findings.gaps.is_empty() {
        writeln!(
            out,
            "{}",
            format!("GAPS — links with no file ({})", findings.gaps.len()).bold()
        )?;
        // Link targets, paths, titles and the lines a finding quotes are the tree's text, and
        // so the sources' text: each is printed without its control characters.
        for gap in &findings.gaps {
            writeln!(
                out,
                "  {}  <- {}",
                printable(&gap.target).yellow(),
                printable(&gap.from).dimmed()
            )?;
        }
        writeln!(out)?;
    }

    if !findings.ambiguous.is_empty() {
        writeln!(
            out,
            "{}",
            format!(
                "AMBIGUOUS — links with multiple files ({})",
                findings.ambiguous.len()
            )
            .bold()
        )?;
        for ambiguous in &findings.ambiguous {
            writeln!(
                out,
                "  {}  <- {}  candidates={}",
                printable(&ambiguous.target).yellow(),
                printable(&ambiguous.from).dimmed(),
                printable(&ambiguous.candidates.join(", "))
            )?;
        }
        writeln!(out)?;
    }

    let (overdue, undated): (Vec<_>, Vec<_>) = findings
        .stale
        .iter()
        .partition(|stale| stale.overdue_days.is_some());
    for (title, items) in [
        ("STALE — open past its date", &overdue),
        ("UNDATED — open with no revisit_by/due date", &undated),
    ] {
        if items.is_empty() {
            continue;
        }
        writeln!(out, "{}", format!("{title} ({})", items.len()).bold())?;
        for item in items {
            let owner = item.owner.as_deref().unwrap_or("unassigned");
            writeln!(
                out,
                "  {}  [{}] {}  owner={}",
                printable(&item.path).red(),
                printable(&item.status),
                printable(&item.why).yellow(),
                printable(owner).dimmed()
            )?;
        }
        writeln!(out)?;
    }

    let negatives: Vec<String> = findings.negatives.iter().map(Negative::describe).collect();
    for (title, items) in [
        ("UNTIERED — no authority tier", &findings.untiered),
        ("UNQUOTED — no verbatim quote", &findings.unquoted),
        ("NEGATIVES — direct source not identified", &negatives),
        ("ORPHANS — no document links here", &findings.orphans),
    ] {
        if items.is_empty() {
            continue;
        }
        writeln!(out, "{}", format!("{title} ({})", items.len()).bold())?;
        for item in items {
            writeln!(out, "  {}", printable(item))?;
        }
        writeln!(out)?;
    }

    print_rules_findings(findings, out)?;

    if findings.clean() {
        writeln!(out, "{}", "no findings".green())?;
    } else if findings.structural_clean() {
        writeln!(
            out,
            "{}",
            format!(
                "structure clean — {} stale item(s) need review",
                findings.stale.len()
            )
            .green()
        )?;
    }
    Ok(())
}

fn print_rules_findings(findings: &Findings, out: &mut dyn Write) -> std::io::Result<()> {
    if findings.rules_missing {
        writeln!(out, "{}", "RULES — missing root rules.md".bold())?;
        writeln!(out)?;
    }
    if findings.rules_wrong_type {
        writeln!(out, "{}", "RULES — rules.md must have type: rules".bold())?;
        writeln!(out)?;
    }
    if let Some(bytes) = findings.rules_oversize {
        writeln!(
            out,
            "{}",
            format!("RULES — rules.md exceeds 8192 bytes ({bytes})").bold()
        )?;
        writeln!(out)?;
    }
    if !findings.schema_violations.is_empty() {
        writeln!(
            out,
            "{}",
            format!(
                "SCHEMAS — records that do not match their type ({})",
                findings.schema_violations.len()
            )
            .bold()
        )?;
        for violation in &findings.schema_violations {
            writeln!(
                out,
                "  {}{} — {}",
                printable(&violation.path),
                printable(&violation.pointer),
                printable(&violation.message)
            )?;
        }
        writeln!(out)?;
    }
    if !findings.frontmatter_invalid.is_empty() {
        writeln!(
            out,
            "{}",
            format!(
                "FRONTMATTER — records whose frontmatter does not parse ({})",
                findings.frontmatter_invalid.len()
            )
            .bold()
        )?;
        for invalid in &findings.frontmatter_invalid {
            let summary = invalid.error.lines().next().unwrap_or(&invalid.error);
            writeln!(
                out,
                "  {} — {}",
                printable(&invalid.path).yellow(),
                printable(summary)
            )?;
        }
        writeln!(out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_claims_flag_unsourced_and_exempt_bounded() -> Result<()> {
        let re = negative_claim_re()?;
        assert!(re.is_match("the record says Alice was never told an outcome"));
        assert!(re.is_match("nobody answered Alice"));
        assert!(re.is_match("no evidence Alice was ever paid"));
        assert!(!re.is_match("Alice was told yesterday"));
        assert!(!re.is_match("never mind the formatting"));
        assert!(has_tier1_marker("no reply in the thread (tier 1)"));
        assert!(has_tier1_marker("searched the email archive: nothing"));
        assert!(!has_tier1_marker("the payment table is silent"));
        Ok(())
    }

    #[test]
    fn deadline_missing_is_stale_and_future_is_not() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 15).unwrap_or_default();
        assert_eq!(
            describe_deadline(None, today),
            Some(("no revisit_by/due date".to_owned(), None))
        );
        let past = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap_or_default();
        assert_eq!(
            describe_deadline(Some(past), today),
            Some(("overdue 2d".to_owned(), Some(2)))
        );
        let future = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap_or_default();
        assert!(describe_deadline(Some(future), today).is_none());
    }

    #[test]
    fn empty_findings_are_structurally_clean_and_clean() {
        let findings = Findings {
            files: 1,
            ..Findings::default()
        };
        assert!(findings.structural_clean());
        assert!(findings.clean());
    }

    #[test]
    fn stale_only_findings_pass_the_structural_gate_but_are_not_clean() {
        let findings = Findings {
            files: 1,
            stale: vec![Stale {
                path: "decisions/example.md".to_owned(),
                status: "open".to_owned(),
                why: "overdue 3d".to_owned(),
                owner: None,
                overdue_days: Some(3),
            }],
            ..Findings::default()
        };
        assert!(findings.structural_clean());
        assert!(!findings.clean());
    }

    #[test]
    fn stale_items_do_not_mask_structural_findings() {
        let findings = Findings {
            files: 1,
            gaps: vec![Gap {
                target: "missing".to_owned(),
                from: "index.md".to_owned(),
            }],
            stale: vec![Stale {
                path: "decisions/example.md".to_owned(),
                status: "open".to_owned(),
                why: "overdue 3d".to_owned(),
                owner: None,
                overdue_days: Some(3),
            }],
            ..Findings::default()
        };
        assert!(!findings.structural_clean());
        assert!(!findings.clean());
    }

    #[test]
    fn each_structural_finding_kind_fails_the_gate() {
        let with_gap = Findings {
            gaps: vec![Gap {
                target: "missing".to_owned(),
                from: "index.md".to_owned(),
            }],
            ..Findings::default()
        };
        let with_ambiguous = Findings {
            ambiguous: vec![Ambiguous {
                target: "note".to_owned(),
                from: "reference.md".to_owned(),
                candidates: vec!["a/note.md".to_owned(), "b/note.md".to_owned()],
            }],
            ..Findings::default()
        };
        let with_untiered = Findings {
            untiered: vec!["meetings/example.md".to_owned()],
            ..Findings::default()
        };
        let with_unquoted = Findings {
            unquoted: vec!["decisions/example.md".to_owned()],
            ..Findings::default()
        };
        let with_orphan = Findings {
            orphans: vec!["notes/example.md".to_owned()],
            ..Findings::default()
        };
        for findings in [
            with_gap,
            with_ambiguous,
            with_untiered,
            with_unquoted,
            with_orphan,
        ] {
            assert!(!findings.structural_clean());
            assert!(!findings.clean());
        }
    }
}
