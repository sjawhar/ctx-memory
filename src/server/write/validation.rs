//! Validation of records, paths and credentials before a write commits.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::config::RULES_FILE;
use crate::context::Context;
use crate::protocol::Finding;
use crate::redact::{credentials_in, redacted};
use crate::resolve;
use crate::sweep;
use crate::tree::Doc;

use super::Change;

/// The absolute path a write touches, refused when any part of it is a symbolic link.
///
/// [`record_path`] keeps a path inside the tree by its text. That is not enough on its own: a
/// symbolic link committed to `main` under a `.md` name would send the write wherever it
/// points, and the server runs as root in its container. Every existing component is checked,
/// so neither the file nor a directory on the way to it can redirect the write.
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

/// Every reason this one path would fail `ctx sweep` or `ctx cite verify`.
///
/// Reachability is left out: a record and the page that links it are two writes, so neither
/// could ever be the first. `ctx sweep` still reports orphans over the whole tree.
///
/// Every check below matches findings by this one path, so a path the tree does not load as
/// a record would pass all of them by finding nothing — and be committed and pushed with no
/// check at all. A put is therefore refused unless the written file is in the loaded tree. A
/// delete removes a record instead of adding one, so it has nothing left to load.
pub(super) fn check(context: &Context, path: &str, change: &Change) -> Result<Vec<Finding>> {
    if matches!(change, Change::Put { .. })
        && !context
            .tree
            .docs
            .iter()
            .any(|document| document.rel == Path::new(path))
    {
        return Ok(vec![unloaded(path)]);
    }
    let mut findings = structural_findings(&sweep::run(&context.tree)?, path);
    let written: Vec<&Doc> = context
        .tree
        .docs
        .iter()
        .filter(|document| document.rel == Path::new(path))
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
/// file can be written there.
pub(super) fn in_the_way(root: &Path, path: &str) -> Option<Finding> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut reached = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        reached.push(part);
        let Ok(metadata) = std::fs::symlink_metadata(&reached) else {
            // Nothing is there yet, so the rest of the path is this write's to make.
            return None;
        };
        let last = index + 1 == parts.len();
        let blocked = if last {
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
        if let Some(why) = blocked {
            return Some(Finding {
                kind: "not-a-record".to_owned(),
                path: path.to_owned(),
                detail: format!("no record file can be written here: {why}"),
            });
        }
    }
    None
}

/// What `ctx sweep` found about this one path, and about the rules page every write is made
/// under.
fn structural_findings(sweep: &sweep::Findings, path: &str) -> Vec<Finding> {
    let about = |kind: &str, detail: String| Finding {
        kind: kind.to_owned(),
        path: path.to_owned(),
        detail,
    };
    let mut findings = rules_findings(sweep);
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

/// Checks the path names a record this tree loads, and returns it in the one spelling the
/// tree uses.
///
/// The rule is the tree's own: a record is a `.md` file under the tree root, named by its
/// components, none of them hidden. `Tree::load` walks the tree with `hidden(true)`, so a
/// component starting with `.` is never loaded and nothing would check such a file. The
/// components are joined with `/` so that `decisions//renewal.md` and `decisions/renewal.md`
/// are one path: every check in [`check`] matches findings by this string, and two spellings
/// of one file would let a write past all of them.
///
/// A record path is also plain text, because git and a commit message both read it. git's
/// pathspec magic is off for every call `ctx` makes, and a path still carries no `*`, `?`,
/// `[` or `]`, which git would otherwise read as a pattern, and no component starting with
/// `-` or `:`, which git reads as an option or as magic. No control character either: the
/// path goes into the commit's subject line, and a line break would start a line of its own.
///
/// This covers what a path can say about itself. Whether this tree in fact loads the written
/// file — `.gitignore` decides that too — is settled in [`check`], against the loaded tree.
pub(super) fn record_path(path: &str) -> Result<String> {
    let candidate = Path::new(path);
    if path.is_empty() || candidate.is_absolute() {
        bail!("a record path is relative to the tree root");
    }
    let mut parts = Vec::new();
    for part in candidate.components() {
        let std::path::Component::Normal(name) = part else {
            bail!("{path} is not a record path");
        };
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("{path} is not valid UTF-8"))?;
        if name.starts_with('.') {
            bail!("{path} is not a record path: a tree loads no hidden file as a record");
        }
        if name.starts_with(['-', ':']) {
            bail!("{path} is not a record path: git reads a name starting with - or : as an option or as magic");
        }
        if let Some(letter) = name
            .chars()
            .find(|letter| letter.is_control() || matches!(letter, '*' | '?' | '[' | ']'))
        {
            bail!(
                "{path} is not a record path: a record path is plain text, and holds no {letter:?}"
            );
        }
        parts.push(name);
    }
    let normalised = parts.join("/");
    if Path::new(&normalised)
        .extension()
        .is_none_or(|suffix| suffix != "md")
    {
        bail!("{path} is not a markdown record");
    }
    Ok(normalised)
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
/// store it: the record's path, which becomes a file name and the commit's subject; the
/// session, which becomes the commit's `Session:` line; and a put's content.
///
/// A finding names the kind of credential and where it is, never the credential: the answer
/// goes back to the caller, and through it into a transcript. So the path a finding names has
/// every credential taken out of it.
pub(super) fn credential_findings(change: &Change, session: &str) -> Result<Vec<Finding>> {
    let shown = redacted(change.path())?;
    let mut carried = vec![("the record path", change.path()), ("the session", session)];
    if let Change::Put { content, .. } = change {
        carried.push(("the record", content.as_str()));
    }
    let mut findings = Vec::new();
    for (place, text) in carried {
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
