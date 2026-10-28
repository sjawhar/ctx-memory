//! The serialized write transaction and rollback protocol.

use std::path::Path;

use anyhow::{anyhow, Context as _, Result};

use crate::config::RULES_FILE;
use crate::context::Context;
use crate::git::{Access, Author, Push, Repo};
use crate::redact::one_line;

use super::super::identity::Caller;
use super::validation::{
    check, credential_findings, in_the_way, inside_the_tree, instruction_file, is_rules_page,
    record_path, too_long, unloaded,
};
use super::{Applied, Change, Shared};

/// Why one write did not happen.
///
/// The answers differ. A caller that asked for something this server will never do is told
/// so once, with 400, and gains nothing by trying again; a server that could not finish a
/// write it accepted answers 500, which a caller and a probe both read as "come back", and
/// which no retry of the same request makes worse. A copy that lost the write lease part-way
/// through answers 503, as one that never held it does.
pub(super) enum ApplyError {
    /// The request cannot be applied, whatever the server's state is.
    Caller(anyhow::Error),
    /// The server could not carry out a request it accepted.
    Server(anyhow::Error),
    /// This copy no longer holds the write lease, so it published nothing.
    Unheld(anyhow::Error),
    /// A push that did not end cleanly, followed by a look at the remote that failed too, so
    /// nobody can say yet whether the commit landed; why. 503, not 500: the follow loop
    /// settles it.
    Unknown(String),
}

impl From<anyhow::Error> for ApplyError {
    fn from(error: anyhow::Error) -> Self {
        Self::Server(error)
    }
}
/// Applies one change: pull, write, check, commit, push, and reload what the server serves.
///
/// Every step after the first change on disk can fail, and a failure must leave the clone as
/// the remote has it. Otherwise the commit stays here unpushed and the next caller's push
/// publishes it — a record whose own author was told the write failed — or, when the remote
/// has moved, every later write and every follow pass fails on a clone that cannot
/// fast-forward. So the body of the write runs inside [`attempt`], and anything it leaves
/// behind is thrown away here before the caller is answered.
pub(super) fn apply(
    shared: &Shared,
    caller: &Caller,
    change: &Change,
    session: &str,
) -> Result<Applied, ApplyError> {
    // A write that carries a credential is refused before anything is written, so the
    // credential never reaches the clone's disk, let alone the graph repository. It is refused
    // before the path is read for anything else, because every other refusal quotes the path.
    let credentials = credential_findings(change, session)?;
    if !credentials.is_empty() {
        return Ok(Applied::Refused(credentials));
    }
    let path = record_path(change.path()).map_err(ApplyError::Caller)?;
    // The rules page binds every cycle, so a write that rewrote it would steer every later
    // one. It changes only through a reviewed pull request on GitHub, which the follow loop
    // pulls.
    if is_rules_page(&path) {
        return Err(ApplyError::Caller(anyhow!(
            "{RULES_FILE} is the rules page, which binds every cycle: it changes only through \
             a reviewed pull request on GitHub, never through this server"
        )));
    }
    // A person who clones the graph and runs an agent there hands it these files as its
    // instructions, from wherever they are in the tree. Like the rules page, they change only
    // through review.
    if let Some(name) = instruction_file(&path) {
        return Err(ApplyError::Caller(anyhow!(
            "{path} is named {name}, which an agent run in a clone of the graph reads as its \
             instructions: such a file changes only through a reviewed pull request on GitHub, \
             never through this server"
        )));
    }
    // A name the file system cannot hold fails at the disk with the server's own path in the
    // error; it is the caller's path that is wrong, so it is refused here, before the tree is
    // touched.
    if let Some(finding) = too_long(&path) {
        return Ok(Applied::Refused(vec![finding]));
    }
    let mut tree = shared.tree()?;
    // The lease is checked once the tree is this write's, and before the clone is touched: a
    // check made before the wait for the tree could be stale by the time the write starts, and
    // a copy that may not write leaves the tree and the remote exactly as it found them.
    if let Err(error) = shared.hold_lease() {
        return Ok(Applied::Unheld(format!("{error:#}")));
    }
    let root = tree.tree.root.clone();
    let repo = Repo::open_with_credential(&root, &tree.config.credentials, Access::Write)?;
    if repo.cloned_by_ctx() {
        // Outside a write, nothing in a clone `ctx` made is anybody's work. A commit there is
        // one whose caller was told the write failed — a rollback that itself failed leaves
        // it — and this write's push would publish it. A clone that cannot be put back the way
        // the remote has it takes no write until it can.
        let discarded = repo.discard_strays().context(
            "the clone holds what the remote does not, and putting it back the way the remote \
             has it failed, so it takes no write",
        )?;
        let tainted = shared.tainted();
        shared.untaint();
        if discarded || tainted {
            *tree = Context::open(&root)?;
        }
    } else {
        // In a tree somebody works in, what the remote does not have is theirs. A failed write
        // is undone by putting the tree back where the write found it, which would throw their
        // uncommitted work away, and a push would publish their commits with none of the
        // checks a write gets here.
        let work = repo.unpublished()?;
        if !work.is_empty() {
            return Ok(Applied::Conflict(format!(
                "{} holds work the remote does not have, which a write here would publish or \
                 throw away; commit and push it, stash it or remove it first: {}",
                root.display(),
                work.join(", ")
            )));
        }
        // What a failed rollback left is gone with the rest, so the tree may be read again.
        if shared.tainted() {
            shared.untaint();
            *tree = Context::open(&root)?;
        }
    }
    // Nothing has been written yet, so a failed pull needs no undoing.
    repo.pull_ff_only()?;
    // Where the clone stands before this write touches it. Every way out of `attempt` that is
    // not a published commit comes back here, which removes exactly what this write added.
    let before = repo.head()?;

    let outcome = attempt(
        shared, &repo, &mut tree, &root, &path, caller, change, &before, session,
    );
    if outcome.is_err() {
        // A clone that cannot be put back is wedged: `/healthz` says so, and the next write or
        // follow pass throws the commit away before anything else happens in a clone `ctx`
        // made, or refuses to write in anyone else's. Until then the tree served stays the one
        // from before this write: the disk holds what the write left, which was never
        // published and may have been refused, and reading it back would serve it. The mark
        // keeps the follow loop and panic recovery from reading it back too.
        match repo.reset_to(&before) {
            Ok(()) => *tree = Context::open(&root)?,
            Err(error) => {
                shared.taint();
                eprintln!(
                    "the clone could not be reset after a failed write: {}",
                    one_line(&format!("{error:#}"))
                );
            }
        }
    }
    outcome
}

/// The body of one write, from the first change on disk to the push that publishes it.
///
/// The tree with this write in it is a candidate until the write is published, and the server
/// goes on serving `tree` meanwhile: a write that is refused, or whose rollback fails, never
/// reaches a reader. `tree` is read again from disk only once the clone is back where the
/// write found it.
#[allow(
    clippy::too_many_arguments,
    reason = "one write's whole context; splitting it would hide the order of the steps"
)]
fn attempt(
    shared: &Shared,
    repo: &Repo,
    tree: &mut Context,
    root: &Path,
    path: &str,
    caller: &Caller,
    change: &Change,
    before: &str,
    session: &str,
) -> Result<Applied, ApplyError> {
    // A path through a symbolic link is refused before git is asked anything about it: git
    // will not answer for a path beyond a link.
    let absolute = inside_the_tree(root, path).map_err(ApplyError::Caller)?;
    // A put below a file, or onto a directory, would fail at the disk the same way. The parts
    // of the path that exist decide it, before anything is written.
    if let Change::Put { .. } = change {
        if let Some(finding) = in_the_way(root, path) {
            *tree = Context::open(root)?;
            return Ok(Applied::Refused(vec![finding]));
        }
    }
    // No commit takes a file at a path git ignores, no rollback removes it, and the tree does
    // not load it, so nothing would check it. Such a path is refused before it is written,
    // which also leaves alone any file somebody keeps there.
    if repo.ignores(path)? {
        return match change {
            Change::Put { .. } => {
                *tree = Context::open(root)?;
                Ok(Applied::Refused(vec![unloaded(path)]))
            }
            Change::Delete { .. } => Err(ApplyError::Caller(anyhow!(
                "{path} is not a record in this tree: git ignores it"
            ))),
        };
    }
    apply_to_disk(&absolute, path, change)?;
    let mut candidate = Context::open(root)?;

    let findings = check(&candidate, path, change)?;
    if !findings.is_empty() {
        repo.reset_to(before)?;
        *tree = Context::open(root)?;
        return Ok(Applied::Refused(findings));
    }
    // A put that leaves the file as `main` has it — the retry of a write whose answer was
    // lost, or was "unknown" — has nothing to commit: its record is already published, in
    // the commit the clone is on.
    if repo.uncommitted()?.is_empty() {
        *tree = candidate;
        return Ok(Applied::Committed(before.to_owned()));
    }

    let author = Author {
        name: caller.name.clone(),
        email: caller.email.clone(),
    };
    let message = format!("{} {path}\n\nSession: {session}", change.verb());
    let mut commit = repo.commit(&[path.to_owned()], &message, &author)?;

    // The lease is checked once more right before each push: another copy may take it over
    // 30 seconds after the last check this copy passed, and a push is bounded to less than
    // that (`git::NETWORK_TIMEOUT`), so a push that lands, lands while this copy still holds
    // it. A failed check is undone like any other failure, before the caller is answered.
    shared.hold_lease().map_err(ApplyError::Unheld)?;
    if let Some(why) = publish(repo)? {
        if let Err(error) = repo.pull_rebase(&author) {
            if !repo.rebase_in_progress() {
                // No rebase started: the pull never reached the remote, or failed before it
                // replayed anything. That is the server's trouble, not a race the caller lost,
                // and `apply` puts the clone back.
                return Err(ApplyError::Server(
                    error.context("rebasing the write onto the newer main"),
                ));
            }
            // A record a person edited on GitHub and this write touch the same lines. git
            // stopped part-way through the rebase and is waiting for a person, which no
            // caller of this server can be: the clone goes back to where this write found
            // it, and the caller is told it lost the race rather than that the server broke.
            let why = format!(
                "the write was rebased onto a newer main and conflicted ({why}): {error:#}"
            );
            repo.reset_to(before)?;
            *tree = Context::open(root)?;
            return Ok(Applied::Conflict(why));
        }
        candidate = Context::open(root)?;
        let findings = check(&candidate, path, change)?;
        if !findings.is_empty() {
            // The rebase put this write on a tree it no longer fits. Leave `main` as the
            // remote has it and tell the caller what broke.
            repo.reset_to(before)?;
            *tree = Context::open(root)?;
            return Ok(Applied::Refused(findings));
        }
        commit = repo.head()?;
        shared.hold_lease().map_err(ApplyError::Unheld)?;
        if let Some(why) = publish(repo)? {
            repo.reset_to(before)?;
            *tree = Context::open(root)?;
            return Ok(Applied::Conflict(why));
        }
    }
    // The candidate is the published commit's tree: the write changed one file, and the clone
    // held nothing else the remote lacks.
    *tree = candidate;
    Ok(Applied::Committed(commit))
}

/// Pushes, and returns the remote's refusal when it moved on.
///
/// A push whose outcome nobody can tell is an error, and goes back the way every failed
/// write does: the clone returns to where the write found it. If the commit did land, the
/// clone's next pull takes it back, so neither answer loses it.
fn publish(repo: &Repo) -> Result<Option<String>, ApplyError> {
    match repo.push()? {
        Push::Pushed => Ok(None),
        Push::Rejected(why) => Ok(Some(why)),
        Push::Unknown(why) => Err(ApplyError::Unknown(why)),
    }
}

/// Writes or removes one record file, at `absolute`, which [`inside_the_tree`] returned for
/// `path`.
fn apply_to_disk(absolute: &Path, path: &str, change: &Change) -> Result<(), ApplyError> {
    match change {
        Change::Put { content, .. } => {
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(absolute, content)
                .with_context(|| format!("writing {}", absolute.display()))?;
        }
        Change::Delete { .. } => {
            if !absolute.is_file() {
                return Err(ApplyError::Caller(anyhow!(
                    "{path} is not a record in this tree"
                )));
            }
            std::fs::remove_file(absolute)
                .with_context(|| format!("removing {}", absolute.display()))?;
        }
    }
    Ok(())
}
