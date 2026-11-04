//! A push the remote fails on its own side: tried again inside the write, and deferred when the
//! remote keeps failing, never answered as a fault of the server's.

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::fixtures::{
    fail_pushes_on_the_remotes_side, good_record, merge_before_the_servers_push, pushes_taken,
    put_body,
};
use crate::logging::Logged;
use crate::support::{fetch_window, fixture_clone, git, remote_of, remove_fixture};

/// What the line the server prints for each push it tries again starts with.
const RETRIED: &str = "push retried: ";

/// The lines of `logged` that start with `prefix`.
fn lines_starting<'log>(logged: &'log str, prefix: &str) -> Vec<&'log str> {
    logged
        .lines()
        .filter(|line| line.starts_with(prefix))
        .collect()
}

/// During a hosting outage the remote's backend failed to move `main` for some pushes and took
/// others minutes apart, and git said `[remote rejected] HEAD -> main (failure)`. The write was
/// answered 500, which the client does not send again. A push the remote fails on its own side
/// is tried again inside the write now: the record lands once, in one commit, and the retry is
/// one line on standard error.
#[test]
fn a_push_the_remote_fails_once_is_tried_again_and_lands_once() -> Result<()> {
    let tree = fixture_clone("write-remote-fails-once")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    fail_pushes_on_the_remotes_side(&bare, &["fail"])?;
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let (status, retry_after, body) = server.post_answer(
        Some("ada@example.com"),
        "/v1/records/put",
        &put_body("decisions/retried.md", &good_record(), "session-retried"),
    )?;
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let commits = git(
        &bare,
        &["log", "--format=%H", "main", "--", "decisions/retried.md"],
    )?;
    let pushes = pushes_taken(&bare)?;
    let local_head = git(&tree, &["rev-parse", "HEAD"])?;
    remove_fixture(&tree)?;

    assert_eq!(status, 200, "unexpected body: {body}");
    assert_eq!(retry_after, None);
    assert_eq!(
        body.get("commit").and_then(Value::as_str),
        Some(remote_head.as_str()),
        "the answer does not name the commit the remote holds: {body}"
    );
    assert_eq!(
        commits.lines().count(),
        1,
        "the record did not land in exactly one commit: {commits}"
    );
    assert_eq!(pushes, 2, "the remote was not asked twice");
    assert_eq!(
        local_head, remote_head,
        "the clone is not where the remote is"
    );
    let retries = lines_starting(&logged, RETRIED);
    assert_eq!(retries.len(), 1, "one retry, one line: {logged}");
    assert!(
        retries[0].contains("(failure)"),
        "the retry line does not say what the remote said: {logged}"
    );
    assert!(
        lines_starting(&logged, "write failed: ").is_empty()
            && lines_starting(&logged, "write deferred: ").is_empty(),
        "a write that landed was logged as one that did not: {logged}"
    );
    Ok(())
}

/// A remote that fails every push on its own side is down, not refusing this write. The write
/// is put back, `main` on the remote and in the clone stays where it was, and the caller is
/// told to ask again: `503` with `Retry-After`, which the client sends again on its own, and a
/// `write deferred:` line, not the `write failed:` line an alert pages on.
#[test]
fn a_push_the_remote_keeps_failing_is_deferred_and_put_back() -> Result<()> {
    let tree = fixture_clone("write-remote-keeps-failing")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    let start = git(&bare, &["rev-parse", "main"])?;
    fail_pushes_on_the_remotes_side(&bare, &["fail"; 10])?;
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let (status, retry_after, body) = server.post_answer(
        Some("ada@example.com"),
        "/v1/records/put",
        &put_body("decisions/deferred.md", &good_record(), "session-deferred"),
    )?;
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let pushes = pushes_taken(&bare)?;
    let local_head = git(&tree, &["rev-parse", "HEAD"])?;
    let dirt = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    assert_eq!(status, 503, "unexpected body: {body}");
    assert_eq!(
        retry_after.as_deref(),
        Some("30"),
        "unexpected body: {body}"
    );
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("(failure)")),
        "the answer does not say what the remote said: {body}"
    );
    assert_eq!(pushes, 3, "the push was not tried exactly twice more");
    assert_eq!(remote_head, start, "the remote's main moved");
    assert!(
        !listing.lines().any(|name| name == "decisions/deferred.md"),
        "the remote holds a write its caller was told to send again: {listing}"
    );
    assert_eq!(
        local_head, start,
        "the clone is not back on the start commit"
    );
    assert_eq!(dirt, "", "the deferred write left the clone dirty");
    assert_eq!(
        lines_starting(&logged, RETRIED).len(),
        2,
        "one line per retry: {logged}"
    );
    assert_eq!(
        lines_starting(&logged, "write deferred: /v1/records/put 503 ").len(),
        1,
        "the deferral is not one write deferred: line: {logged}"
    );
    assert!(
        lines_starting(&logged, "write failed: ").is_empty(),
        "a remote's outage paged as a failed write: {logged}"
    );
    Ok(())
}

/// The remote can move `main` and still refuse the push: its backend fails after the ref
/// moved. A retry that finds the remote holding the commit is answered as the write it is,
/// stored, and never makes or pushes a second commit.
#[test]
fn a_retry_the_remote_took_though_it_said_failure_is_committed_once() -> Result<()> {
    let tree = fixture_clone("write-remote-landed-failing")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    fail_pushes_on_the_remotes_side(&bare, &["fail", "land-and-fail"])?;
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let (status, _, body) = server.post_answer(
        Some("ada@example.com"),
        "/v1/records/put",
        &put_body("decisions/landed.md", &good_record(), "session-landed"),
    )?;
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let commits = git(
        &bare,
        &["log", "--format=%H", "main", "--", "decisions/landed.md"],
    )?;
    let pushes = pushes_taken(&bare)?;
    let local_head = git(&tree, &["rev-parse", "HEAD"])?;
    remove_fixture(&tree)?;

    assert_eq!(status, 200, "unexpected body: {body}");
    assert_eq!(
        body.get("commit").and_then(Value::as_str),
        Some(remote_head.as_str()),
        "the answer does not name the commit the remote holds: {body}"
    );
    assert_eq!(
        commits.lines().count(),
        1,
        "the record did not land in exactly one commit: {commits}"
    );
    assert_eq!(
        pushes, 2,
        "the server pushed again after the remote took it"
    );
    assert_eq!(
        local_head, remote_head,
        "the clone is not where the remote is"
    );
    assert_eq!(
        lines_starting(&logged, RETRIED).len(),
        1,
        "one line per retry: {logged}"
    );
    assert!(
        lines_starting(&logged, "write failed: ").is_empty(),
        "a write that landed was logged as failed: {logged}"
    );
    Ok(())
}

/// A push that lost a race with a merge is refused by git itself, `[rejected] HEAD -> main
/// (fetch first)`. That is no failure of the remote's: the write is rebased onto the merge and
/// pushed once more, as before, and no push is tried again as one the remote failed.
#[test]
fn a_non_fast_forward_is_rebased_as_before_and_not_retried() -> Result<()> {
    let tree = fixture_clone("write-non-fast-forward")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    let merged = merge_before_the_servers_push(&tree, "decisions/merged.md", &good_record())?;
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let (status, retry_after, body) = server.post_answer(
        Some("ada@example.com"),
        "/v1/records/put",
        &put_body("decisions/raced.md", &good_record(), "session-raced"),
    )?;
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let parent = git(&bare, &["rev-parse", "main^"])?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(status, 200, "unexpected body: {body}");
    assert_eq!(retry_after, None);
    assert_eq!(
        body.get("commit").and_then(Value::as_str),
        Some(remote_head.as_str()),
        "the answer does not name the commit the remote holds: {body}"
    );
    assert_eq!(parent, merged, "the write was not rebased onto the merge");
    for path in ["decisions/merged.md", "decisions/raced.md"] {
        assert!(
            listing.lines().any(|name| name == path),
            "{path} is not on the remote: {listing}"
        );
    }
    assert!(
        lines_starting(&logged, RETRIED).is_empty(),
        "a race git refused was tried again as a remote's failure: {logged}"
    );
    assert!(
        lines_starting(&logged, "write failed: ").is_empty()
            && lines_starting(&logged, "write deferred: ").is_empty(),
        "a write that landed was logged as one that did not: {logged}"
    );
    Ok(())
}
