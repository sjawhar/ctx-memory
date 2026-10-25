//! Structural checks for a markdown context tree.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use regex::Regex;
use serde::Serialize;

use crate::schema::{NEEDS_QUOTE, NEEDS_TIER, OPEN_KINDS, OPEN_STATES};
use crate::tree::{Doc, Tree};

#[derive(Debug, Serialize)]
pub struct Gap {
    pub target: String,
    pub from: String,
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

#[derive(Debug, Default, Serialize)]
pub struct Findings {
    pub files: usize,
    pub gaps: Vec<Gap>,
    pub stale: Vec<Stale>,
    pub untiered: Vec<String>,
    pub unquoted: Vec<String>,
    pub negatives: Vec<String>,
    pub orphans: Vec<String>,
}

impl Findings {
    /// Structural findings are defects in the graph. Stale records remain visible without
    /// making the command fail.
    #[must_use]
    pub const fn structural_clean(&self) -> bool {
        self.gaps.is_empty()
            && self.untiered.is_empty()
            && self.unquoted.is_empty()
            && self.negatives.is_empty()
            && self.orphans.is_empty()
    }

    /// Nothing to report at all: structure is clean and no record is stale.
    #[must_use]
    pub const fn clean(&self) -> bool {
        self.structural_clean() && self.stale.is_empty()
    }
}

pub(crate) fn wikilink_re() -> Result<Regex> {
    Regex::new(r"\[\[([^\]|#]+)").map_err(Into::into)
}

fn negative_claim_re() -> Result<Regex> {
    Regex::new(
        r"(?i)\bnever\s+(told|informed|notified|received|heard|answered|replied|responded|communicated|paid|invoiced|asked)\b|\b(was|were|has|had)\s+never\s+(been\s+)?(told|informed|notified|paid|invoiced|asked|answered|sent|contacted|given)\b|\bno\s+evidence\s+\w+\s+(was\s+|were\s+)?ever\b|\b(nobody|no\s+one)\s+(ever\s+)?(told|asked|answered|informed)\b",
    )
    .map_err(Into::into)
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
fn scan_negatives(doc: &Doc, negative_re: &Regex, negatives: &mut Vec<String>) {
    if doc.rel.starts_with("raw") || doc.front.kind.as_deref() == Some("meeting") {
        return;
    }
    let mut line_number = 1usize;
    for paragraph in doc.raw.split("\n\n") {
        if !has_tier1_marker(paragraph) {
            if let Some(match_) = negative_re
                .find_iter(paragraph)
                .find(|match_| paragraph[..match_.start()].matches('"').count() % 2 == 0)
            {
                let snippet: String = paragraph[match_.start()..]
                    .chars()
                    .take(80)
                    .collect::<String>()
                    .replace('\n', " ");
                negatives.push(format!(
                    "{}:{} — \"{}\" (source it directly or identify the searched surface)",
                    doc.rel.to_string_lossy(),
                    line_number,
                    snippet
                ));
            }
        }
        line_number += paragraph.matches('\n').count() + 2;
    }
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
    let today = Utc::now().date_naive();
    let wikilink_re = wikilink_re()?;
    let known: HashSet<String> = tree
        .docs
        .iter()
        .flat_map(|doc| doc.link_names(&tree.folder_notes))
        .collect();

    let negative_re = negative_claim_re()?;
    let mut gaps = Vec::new();
    let mut negatives = Vec::new();
    let mut linked: HashSet<&str> = HashSet::new();
    for doc in &tree.docs {
        for capture in wikilink_re.captures_iter(&doc.raw) {
            let Some(target) = capture.get(1).map(|match_| match_.as_str().trim()) else {
                continue;
            };
            if let Some(name) = known.get(target) {
                linked.insert(name);
            } else {
                gaps.push(Gap {
                    target: target.to_owned(),
                    from: doc.rel.to_string_lossy().into_owned(),
                });
            }
        }
        scan_negatives(doc, &negative_re, &mut negatives);
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
        .filter(|doc| {
            doc.rel != Path::new("index.md")
                && !doc
                    .link_names(&tree.folder_notes)
                    .iter()
                    .any(|name| linked.contains(name.as_str()))
        })
        .map(|doc| doc.rel.to_string_lossy().into_owned())
        .collect();

    Ok(Findings {
        files: tree.docs.len(),
        gaps,
        stale,
        untiered,
        unquoted,
        negatives,
        orphans,
    })
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
    fn wikilink_pattern_extracts_targets_ignoring_alias_and_heading() -> Result<()> {
        let re = wikilink_re()?;
        let body = "see [[people/alice]] and [[a/b|Alice]] and [[c/d#Section]]";
        let found: Vec<&str> = re
            .captures_iter(body)
            .filter_map(|capture| capture.get(1).map(|match_| match_.as_str()))
            .collect();
        assert_eq!(found, vec!["people/alice", "a/b", "c/d"]);
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
        for findings in [with_gap, with_untiered, with_unquoted, with_orphan] {
            assert!(!findings.structural_clean());
            assert!(!findings.clean());
        }
    }
}
