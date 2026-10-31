//! Validation of records, paths and credentials before a write commits.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::config::RULES_FILE;
use crate::context::Context;
use crate::protocol::Finding;
use crate::redact::{credentials_in, redacted};
use crate::resolve;
use crate::sweep;
use crate::tree::Doc;

use super::{Change, Edit};

/// The absolute path a write touches, refused when any part of it is a symbolic link.
///
/// [`record_path`](crate::tree::record_path) keeps a path inside the tree by its text. That is
/// not enough on its own: a symbolic link committed to `main` under a `.md` name would send the
/// write wherever it points, and the server runs as root in its container. Every existing
/// component is checked, so neither the file nor a directory on the way to it can redirect the
/// write.
pub(super) fn inside_the_tree(root: &Path, path: &str) -> Result<PathBuf> {
    let mut absolute = root.to_path_buf();
    for part in Path::new(path).components() {
        let std::path::Component::Normal(name) = part else {
            bail!("{path} is not a record path");
        };
        absolute.push(name);
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
            // Nothing is there yet, so nothing redirects the write. A directory this write
            // creates is created by this server and is not a link.
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!("{path} leads through a symbolic link, which is not part of this tree");
        }
    }
    Ok(absolute)
}

/// Every reason the files a write changes would fail `ctx sweep` or `ctx cite verify`.
///
/// Reachability is left out: a record and the page that links it are often two writes, so
/// neither could ever be the first. `ctx sweep` still reports orphans over the whole tree.
///
/// Every check below matches findings by the written paths, so a path the tree does not load
/// as a record would pass all of them by finding nothing — and be committed and pushed with
/// no check at all. A put is therefore refused unless each file it writes is in the loaded
/// tree. A delete removes a record instead of adding one, so it has nothing left to load.
///
/// The tree holds every file of the write, so a link from one record of a batch to another
/// finds its target, and the sweep runs once for the whole write.
pub(super) fn check(context: &Context, edits: &[Edit<'_>]) -> Result<Vec<Finding>> {
    let loaded = |path: &str| {
        context
            .tree
            .docs
            .iter()
            .any(|document| document.rel == Path::new(path))
    };
    let unloadable: Vec<Finding> = edits
        .iter()
        .filter(|edit| edit.content.is_some() && !loaded(&edit.path))
        .map(|edit| unloaded(&edit.path))
        .collect();
    if !unloadable.is_empty() {
        return Ok(unloadable);
    }
    let sweep = sweep::run(&context.tree)?;
    let mut findings = rules_findings(&sweep);
    for edit in edits {
        findings.extend(structural_findings(&sweep, &edit.path));
    }
    let written: Vec<&Doc> = context
        .tree
        .docs
        .iter()
        .filter(|document| {
            edits
                .iter()
                .any(|edit| document.rel == Path::new(&edit.path))
        })
        .collect();
    let verification = resolve::verify_documents(context, &written, false)?;
    for broken in verification.broken {
        findings.push(Finding {
            kind: "quote".to_owned(),
            path: broken.file,
            detail: format!("{} — {}", broken.pointer, broken.why),
        });
    }
    // The server is the one writer: a quote it cannot check must not be committed as if it
    // were. A read command only reports an unchecked quote, but a committed record whose quote
    // nothing verified is indistinguishable afterwards from one that was verified.
    for unchecked in verification.unchecked {
        findings.push(Finding {
            kind: "unchecked".to_owned(),
            path: unchecked.file,
            detail: format!("{} — {}", unchecked.pointer, unchecked.why),
        });
    }
    Ok(findings)
}

/// The finding for a put to a path this tree does not load as a record.
pub(super) fn unloaded(path: &str) -> Finding {
    Finding {
        kind: "not-a-record".to_owned(),
        path: path.to_owned(),
        detail: "this tree does not load the path as a record, so no check covers it".to_owned(),
    }
}

/// The longest file name, in bytes, that the file systems a clone lives on hold: `NAME_MAX` is
/// 255 on Linux's ext4, XFS, Btrfs, tmpfs and overlayfs alike.
const NAME_LIMIT: usize = 255;

/// The longest record path, in bytes. Linux takes a path of at most 4,096 bytes (`PATH_MAX`)
/// in one call, and the clone's own directory comes before the record path; this leaves it
/// room, and no record path is near it.
const PATH_LIMIT: usize = 1024;

/// The finding for a put or a delete whose path names a file no file system here can hold.
pub(super) fn too_long(path: &str) -> Option<Finding> {
    let why = if path.len() > PATH_LIMIT {
        format!(
            "the path is {} bytes long, and a record path is at most {PATH_LIMIT}",
            path.len()
        )
    } else {
        let longest = path.split('/').map(str::len).max().unwrap_or(0);
        if longest <= NAME_LIMIT {
            return None;
        }
        format!(
            "a name in the path is {longest} bytes long, and a file system holds names of at \
             most {NAME_LIMIT}"
        )
    };
    Some(Finding {
        kind: "not-a-record".to_owned(),
        path: path.to_owned(),
        detail: format!("no file can have this path: {why}"),
    })
}

/// The finding for a put whose path runs through a file, or ends at a directory: no record
/// file can be written there. Another record the same write puts counts as a file already
/// there, so `batch` is every path the write names.
pub(super) fn in_the_way(root: &Path, path: &str, batch: &[String]) -> Option<Finding> {
    let blocked = |why: String| Finding {
        kind: "not-a-record".to_owned(),
        path: path.to_owned(),
        detail: format!("no record file can be written here: {why}"),
    };
    if let Some(outer) = batch.iter().find(|other| {
        path.strip_prefix(other.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }) {
        return Some(blocked(format!(
            "{outer} is another record of this write, so nothing can be written below it"
        )));
    }
    let parts: Vec<&str> = path.split('/').collect();
    let mut reached = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        reached.push(part);
        let Ok(metadata) = std::fs::symlink_metadata(&reached) else {
            // Nothing is there yet, so the rest of the path is this write's to make.
            return None;
        };
        let last = index + 1 == parts.len();
        let why = if last {
            metadata
                .is_dir()
                .then(|| "a directory is at this path".to_owned())
        } else {
            (!metadata.is_dir()).then(|| {
                format!(
                    "{} is a file, so nothing can be written below it",
                    parts.get(..=index).unwrap_or_default().join("/")
                )
            })
        };
        if let Some(why) = why {
            return Some(blocked(why));
        }
    }
    None
}

/// What `ctx sweep` found about this one path.
fn structural_findings(sweep: &sweep::Findings, path: &str) -> Vec<Finding> {
    let about = |kind: &str, detail: String| Finding {
        kind: kind.to_owned(),
        path: path.to_owned(),
        detail,
    };
    let mut findings = Vec::new();
    for gap in sweep.gaps.iter().filter(|gap| gap.from == path) {
        findings.push(about("gap", format!("the link {} has no file", gap.target)));
    }
    for ambiguous in sweep.ambiguous.iter().filter(|entry| entry.from == path) {
        findings.push(about(
            "ambiguous",
            format!(
                "the link {} matches {}",
                ambiguous.target,
                ambiguous.candidates.join(", ")
            ),
        ));
    }
    for violation in sweep
        .schema_violations
        .iter()
        .filter(|violation| violation.path == path)
    {
        findings.push(about(
            "schema",
            format!("{}{}", violation.pointer, violation.message),
        ));
    }
    for invalid in sweep
        .frontmatter_invalid
        .iter()
        .filter(|invalid| invalid.path == path)
    {
        findings.push(about(sweep::FRONTMATTER_INVALID, invalid.error.clone()));
    }
    for (kind, detail, items) in [
        (
            "untiered",
            "the record declares no authority tier",
            &sweep.untiered,
        ),
        (
            "unquoted",
            "the record carries no verbatim quote",
            &sweep.unquoted,
        ),
    ] {
        if items.iter().any(|item| item == path) {
            findings.push(about(kind, detail.to_owned()));
        }
    }
    // A negative-claim finding names its own record and line, so it is matched by path like
    // the other per-record findings rather than by the text the sweep prints.
    for negative in sweep
        .negatives
        .iter()
        .filter(|negative| negative.path == path)
    {
        findings.push(about(
            "negative",
            format!(
                "line {}: the record states an absence without naming the source checked",
                negative.line
            ),
        ));
    }
    findings
}

/// What `ctx sweep` found wrong with the rules page.
///
/// The page binds every cycle and is never written here, so a broken one refuses every write
/// until a reviewed change on GitHub fixes it: a cycle writing under a rules page nobody can
/// read follows rules nobody set.
fn rules_findings(sweep: &sweep::Findings) -> Vec<Finding> {
    let rules = |kind: &str, detail: String| Finding {
        kind: kind.to_owned(),
        path: RULES_FILE.to_owned(),
        detail,
    };
    let mut findings = Vec::new();
    if sweep.rules_missing {
        findings.push(rules(
            "rules-missing",
            format!("the tree has no root {RULES_FILE}"),
        ));
    }
    if sweep.rules_wrong_type {
        findings.push(rules(
            "rules-wrong-type",
            format!("{RULES_FILE} must have type: rules"),
        ));
    }
    if let Some(bytes) = sweep.rules_oversize {
        findings.push(rules(
            "rules-oversize",
            format!("{RULES_FILE} is {bytes} bytes, over the limit `ctx sweep` sets"),
        ));
    }
    findings
}

/// The file names an agent reads as its instructions, from anywhere in the tree it runs in.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "GEMINI.md", "SKILL.md"];

/// Which of [`INSTRUCTION_FILES`] the path's file name is, compared without regard to case:
/// a case-insensitive file system, and an agent that looks up its file by any spelling, both
/// take `Agents.md` for `AGENTS.md`.
pub(super) fn instruction_file(path: &str) -> Option<&'static str> {
    let name = Path::new(path).file_name()?.to_str()?;
    INSTRUCTION_FILES
        .iter()
        .copied()
        .find(|instructions| instructions.eq_ignore_ascii_case(name))
}

/// Whether `path` names the rules page, in any letter case: a filesystem that folds case
/// would take `RULES.md` for it, and a reader that looks for it by name would too.
pub(super) fn is_rules_page(path: &str) -> bool {
    path.eq_ignore_ascii_case(RULES_FILE)
}

/// One finding for each kind of credential a write carries, in each place the write would
/// store it: each record's path, which becomes a file name and part of the commit message; the
/// session, which becomes the commit's `Session:` line; and each record a put writes.
///
/// A finding names the kind of credential and where it is, never the credential: the answer
/// goes back to the caller, and through it into a transcript. So the path a finding names has
/// every credential taken out of it. A finding about the session names the write's first path.
pub(super) fn credential_findings(change: &Change, session: &str) -> Result<Vec<Finding>> {
    let mut carried = Vec::new();
    for (index, (path, content)) in change.files().into_iter().enumerate() {
        let shown = redacted(path)?;
        carried.push((shown.clone(), "the record path", path));
        if index == 0 {
            carried.push((shown.clone(), "the session", session));
        }
        if let Some(content) = content {
            carried.push((shown, "the record", content));
        }
    }
    let mut findings = Vec::new();
    for (shown, place, text) in carried {
        for name in credentials_in(text)? {
            findings.push(Finding {
                kind: "credential".to_owned(),
                path: shown.clone(),
                detail: format!(
                    "{place} holds what has the shape of {name}; a credential never goes into \
                     the graph: take it out, and revoke it if it is real"
                ),
            });
        }
    }
    Ok(findings)
}
