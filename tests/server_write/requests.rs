//! What the write routes and the command route do with a request, and how they answer.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, merge_during_the_push, put_body};
use crate::support::{commit, fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

/// The commit subject, author name and author email of the remote's newest commit.
fn remote_head(bare: &Path) -> Result<(String, String, String, String)> {
    let line = git(bare, &["log", "-1", "--format=%s%n%an%n%ae%n%H", "main"])?;
    let mut lines = line.lines();
    Ok((
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or_default().to_owned(),
    ))
}

#[test]
fn put_commits_and_pushes_as_caller() -> Result<()> {
    let tree = fixture_clone("write-put")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/cadence.md", &good_record(), "session-7"),
    )?;
    // A copy that reported itself unhealthy after each write would be restarted out from
    // under the next one: a published commit is one the remote has, so the clone can still
    // fast-forward.
    let health = server.get(server.tailnet, "/healthz", &[])?;
    drop(server);
    let bare = remote_of(&tree)?;
    let (subject, author, email, head) = remote_head(&bare)?;
    let message = git(&bare, &["log", "-1", "--format=%B", "main"])?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(reply.text("commit")?, head);
    assert_eq!(subject, "put decisions/cadence.md");
    assert_eq!(author, "ada");
    assert_eq!(email, "ada@example.com");
    assert!(
        message.contains("Session: session-7"),
        "commit message lost the session: {message}"
    );
    assert!(
        listing.lines().any(|name| name == "decisions/cadence.md"),
        "the record is not on the remote: {listing}"
    );
    assert_eq!(
        health.status, 200,
        "the copy reported itself unhealthy after publishing a record: {}",
        health.body
    );
    Ok(())
}

#[test]
fn concurrent_puts_are_serialized() -> Result<()> {
    let tree = fixture_clone("write-concurrent")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let statuses = std::thread::scope(|scope| -> Result<Vec<u16>> {
        let first = scope.spawn(|| {
            server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body("decisions/first.md", &good_record(), "session-a"),
            )
        });
        let second = scope.spawn(|| {
            server.post_as(
                "grace@example.com",
                "/v1/records/put",
                &put_body("decisions/second.md", &good_record(), "session-b"),
            )
        });
        let first = first
            .join()
            .map_err(|_| anyhow::anyhow!("put panicked"))??;
        let second = second
            .join()
            .map_err(|_| anyhow::anyhow!("put panicked"))??;
        Ok(vec![first.status, second.status])
    })?;
    drop(server);
    let bare = remote_of(&tree)?;
    let history = git(&bare, &["log", "--format=%s", "main"])?;
    let merges = git(&bare, &["log", "--merges", "--format=%s", "main"])?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(statuses, vec![200, 200]);
    assert!(
        history.contains("put decisions/first.md") && history.contains("put decisions/second.md"),
        "both writes should be on main: {history}"
    );
    assert_eq!(merges, "", "history should be linear");
    assert!(
        listing.lines().any(|name| name == "decisions/first.md")
            && listing.lines().any(|name| name == "decisions/second.md"),
        "both records should be on the remote: {listing}"
    );
    Ok(())
}

#[test]
fn push_rejected_by_newer_main_rebases_once() -> Result<()> {
    let tree = fixture_clone("write-rebase")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;

    // A commit that reaches main only when the server's push is already under way.
    let root = tree.parent().context("the fixture has a root")?;
    let other = root.join("other");
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    git(root, &["clone", "--quiet", bare_text, "other"])?;
    std::fs::create_dir_all(other.join("decisions"))?;
    std::fs::write(
        other.join("decisions/merged.md"),
        "---\ntype: decision\nstatus: open\ntitle: Merged elsewhere\n---\nA person merged this.\n",
    )?;
    git(&other, &["add", "."])?;
    commit(&other, "a pull request someone merged")?;
    git(
        &other,
        &["push", "--quiet", "origin", "HEAD:refs/heads/incoming"],
    )?;

    // The first push to main finds that main has moved on, exactly as a merge between the
    // server's pull and its push would.
    let hooks = bare.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         if [ -f \"$GIT_DIR/raced\" ]; then exit 0; fi\n\
         touch \"$GIT_DIR/raced\"\n\
         unset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\n\
         git update-ref refs/heads/main refs/heads/incoming\n\
         echo 'main moved on' >&2\n\
         exit 1\n",
    )?;
    let mut permissions = std::fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&hook, permissions)?;

    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/raced.md", &good_record(), "session-c"),
    )?;
    drop(server);
    let history = git(&bare, &["log", "--format=%s", "main"])?;
    let merges = git(&bare, &["log", "--merges", "--format=%s", "main"])?;
    let head = git(&bare, &["rev-parse", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(reply.text("commit")?, head);
    let subjects: Vec<&str> = history.lines().collect();
    assert_eq!(
        subjects.first().copied(),
        Some("put decisions/raced.md"),
        "the write should land on top: {history}"
    );
    assert!(
        history.contains("a pull request someone merged"),
        "the merged commit should still be there: {history}"
    );
    assert_eq!(merges, "", "the rebase should keep history linear");
    Ok(())
}

#[test]
fn delete_removes_the_record_and_commits() -> Result<()> {
    let tree = fixture_clone("write-delete")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let created = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/gone.md", &good_record(), "session-d"),
    )?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/delete",
        &json!({ "path": "decisions/gone.md", "session": "session-e" }),
    )?;
    drop(server);
    let bare = remote_of(&tree)?;
    let (subject, _, _, head) = remote_head(&bare)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(created.status, 200, "unexpected body: {}", created.body);
    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(reply.text("commit")?, head);
    assert_eq!(subject, "delete decisions/gone.md");
    assert!(
        !listing.lines().any(|name| name == "decisions/gone.md"),
        "the record is still on the remote: {listing}"
    );
    Ok(())
}

#[test]
fn marker_advance_records_the_caller() -> Result<()> {
    let tree = fixture_clone("write-marker")?;
    let window = fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/marker/advance",
        &json!({ "window": window, "session": "session-f" }),
    )?;
    drop(server);
    let root = tree.parent().context("the fixture has a root")?;
    let marker = std::fs::read_to_string(root.join("data/marker.json"))?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(
        reply.field("marker")?.get("window").and_then(Value::as_str),
        Some(window.as_str())
    );
    assert!(
        marker.contains("ada@example.com"),
        "the marker should name the caller: {marker}"
    );
    // A local advance names the session that moved the marker, and the request names it too.
    assert!(
        marker.contains("session-f"),
        "the marker should name the caller's session: {marker}"
    );
    Ok(())
}

/// A window the caller names wrongly is the caller's mistake, and the answer names it by the
/// id the caller gave. It used to name the directory the server keeps it in, `window
/// /srv/…/data/windows/<id> does not exist`, which tells a caller where the server keeps its
/// files and nothing it can use.
#[test]
fn a_marker_advance_to_a_missing_window_names_the_window_not_the_servers_path() -> Result<()> {
    let tree = fixture_clone("write-marker-missing")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/marker/advance",
        &json!({ "window": "20990101T000000Z-0000", "session": "session-missing" }),
    )?;
    drop(server);
    let root = tree
        .parent()
        .context("the fixture has a root")?
        .display()
        .to_string();
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 400, "unexpected body: {}", reply.body);
    let error = reply.text("error")?;
    assert!(
        error.contains("window 20990101T000000Z-0000 does not exist"),
        "{error}"
    );
    assert!(
        !error.contains(&root),
        "the answer shows the server's path: {error}"
    );
    Ok(())
}

/// What a command run through `/v1/command` prints goes to a remote caller, and an error in it
/// quotes the paths it failed at. Those are the server's own: they go out as `<data>`,
/// `<tree>` and `<state>`. Here `ctx pending` meets a marker file it cannot decode, and its
/// error names that file.
#[test]
fn a_remote_read_commands_error_names_the_servers_directories_by_placeholder() -> Result<()> {
    let tree = fixture_clone("command-error-paths")?;
    let data = tree
        .parent()
        .context("the fixture has a root")?
        .join("data");
    std::fs::create_dir_all(&data)?;
    std::fs::write(data.join("marker.json"), "not a marker\n")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.command(&["pending"]);
    drop(server);
    let root = tree
        .parent()
        .context("the fixture has a root")?
        .display()
        .to_string();
    remove_fixture(&tree)?;

    let reply = reply?;
    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    let printed = format!("{}{}", reply.text("stdout")?, reply.text("stderr")?);
    assert!(
        printed.contains("<data>/marker.json"),
        "the error does not name the data directory by its placeholder: {printed}"
    );
    assert!(
        !printed.contains(&root),
        "the answer shows the server's path: {printed}"
    );
    Ok(())
}

/// A write whose answer was lost — or answered "unknown" — is asked for again, and its record
/// is already on `main`. The retry used to write the same bytes, find nothing to commit, and
/// answer 500 with an empty reason, however often it was asked.
#[test]
fn a_put_that_changes_nothing_is_answered_with_the_commit_that_holds_it() -> Result<()> {
    let tree = fixture_clone("write-unchanged")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let first = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/again.md", &good_record(), "session-first"),
    )?;
    let again = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/again.md", &good_record(), "session-again"),
    )?;
    drop(server);
    let remote_head = git(&remote_of(&tree)?, &["rev-parse", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(first.status, 200, "unexpected body: {}", first.body);
    assert_eq!(again.status, 200, "unexpected body: {}", again.body);
    assert_eq!(again.text("commit")?, first.text("commit")?);
    assert_eq!(
        again.text("commit")?,
        remote_head,
        "an unchanged put made a commit"
    );
    Ok(())
}

/// A container has no git identity of its own. The rebase after a push that lost to a merge
/// writes a commit, and with no committer git stopped part-way through — so the caller was
/// told their write conflicted with a merge it never touched. The caller commits the rebased
/// write, as they commit the write itself.
#[test]
fn a_rebase_in_a_container_with_no_git_identity_lands() -> Result<()> {
    let tree = fixture_clone("write-rebase-no-identity")?;
    fetch_window(&tree)?;
    merge_during_the_push(
        &tree,
        "decisions/merged.md",
        "---\ntype: decision\nstatus: open\ntitle: Merged elsewhere\n---\nA person merged this.\n",
    )?;
    let root = tree.parent().context("the fixture has a root")?;
    let config = root.join("no-identity.gitconfig");
    std::fs::write(&config, "[user]\n\tuseConfigOnly = true\n")?;
    let config_text = config.to_str().context("config path is not valid UTF-8")?;
    let server = Server::start_with_env(
        &tree,
        &["--pull-interval", "3600"],
        &[
            ("GIT_CONFIG_GLOBAL", config_text),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("EMAIL", ""),
        ],
    )?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/raced.md", &good_record(), "session-no-identity"),
    )?;
    drop(server);
    let bare = remote_of(&tree)?;
    let committer = git(&bare, &["log", "-1", "--format=%ce", "main"])?;
    let history = git(&bare, &["log", "--format=%s", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(committer, "ada@example.com");
    assert!(
        history.contains("a pull request someone merged"),
        "the merged commit should still be there: {history}"
    );
    Ok(())
}

/// The caller names its session, and the name goes into the commit message. A newline in it
/// used to start a line of its own, so a caller could add any trailer it liked —
/// `Signed-off-by`, `Co-authored-by` — under a commit the server made.
#[test]
fn a_session_cannot_forge_a_commit_trailer() -> Result<()> {
    let tree = fixture_clone("write-session-trailer")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/signed.md",
            &good_record(),
            "session-9\n\nSigned-off-by: Mallory <mallory@example.com>",
        ),
    )?;
    drop(server);
    let message = git(&remote_of(&tree)?, &["log", "-1", "--format=%B", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert!(
        !message
            .lines()
            .any(|line| line.starts_with("Signed-off-by:")),
        "the session forged a trailer: {message}"
    );
    assert!(
        message
            .lines()
            .any(|line| line.starts_with("Session: session-9")),
        "the commit lost the session: {message}"
    );
    Ok(())
}

/// A window the caller names wrongly is the caller's mistake, answered 400. A marker the
/// server cannot write is the server's own trouble, answered 500, which a caller may ask
/// again; it used to be answered 400 as well.
#[test]
fn marker_advance_tells_a_bad_window_from_a_failed_write() -> Result<()> {
    let tree = fixture_clone("write-marker-failed")?;
    let window = fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let unknown = server.post_as(
        "ada@example.com",
        "/v1/marker/advance",
        &json!({ "window": "20990101T000000Z-0001", "session": "session-g" }),
    )?;
    let root = tree.parent().context("the fixture has a root")?;
    std::fs::create_dir_all(root.join("data/marker.json"))?;
    let failed = server.post_as(
        "ada@example.com",
        "/v1/marker/advance",
        &json!({ "window": window, "session": "session-g" }),
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(unknown.status, 400, "unexpected body: {}", unknown.body);
    assert_eq!(failed.status, 500, "unexpected body: {}", failed.body);
    Ok(())
}
