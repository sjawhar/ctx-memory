//! Filesystem-path admission tests.

use anyhow::{Context as _, Result};
use serde_json::json;

use super::finding_kinds;
use crate::fixtures::{broken_zeppelin_record, good_record, put_body};
use crate::support::{commit, fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

/// Every check the writer runs matches findings by the record's path, so a path the tree
/// never loads passes all of them by finding nothing. A hidden path is one way to get there:
/// `Tree::load` walks with `hidden(true)`, so `.drafts/x.md` would be committed and pushed
/// with no check at all.
#[test]
fn put_to_a_hidden_path_is_refused() -> Result<()> {
    let tree = fixture_clone("write-hidden")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let replies = [
        ".drafts/smuggled.md",
        "decisions/.smuggled.md",
        ".smuggled.md",
    ]
    .map(|path| {
        server.post_as(
            "ada@example.com",
            "/v1/records/put",
            &put_body(path, &good_record(), "session-hidden"),
        )
    });
    drop(server);
    let bare = remote_of(&tree)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let status = git(&tree, &["status", "--porcelain"])?;
    remove_fixture(&tree)?;

    for reply in replies {
        let reply = reply?;
        assert_eq!(reply.status, 400, "unexpected body: {}", reply.body);
    }
    assert!(
        !listing.contains("smuggled"),
        "a hidden record reached the remote: {listing}"
    );
    assert_eq!(status, "", "the refused write left the tree dirty");
    Ok(())
}

/// `.gitignore` decides what the tree loads just as much as the path's own text does, and a
/// put the tree would not load is refused rather than committed unchecked.
#[test]
fn put_to_an_ignored_path_is_refused() -> Result<()> {
    let tree = fixture_clone("write-ignored")?;
    fetch_window(&tree)?;
    std::fs::write(tree.join(".gitignore"), "scratch/\n")?;
    git(&tree, &["add", ".gitignore"])?;
    commit(&tree, "ignore the scratch directory")?;
    git(&tree, &["push", "--quiet", "origin", "main"])?;

    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("scratch/smuggled.md", &good_record(), "session-ignored"),
    )?;
    drop(server);
    let bare = remote_of(&tree)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let status = git(&tree, &["status", "--porcelain"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    assert_eq!(finding_kinds(&reply), vec!["not-a-record".to_owned()]);
    assert!(
        !listing.contains("smuggled"),
        "an ignored record reached the remote: {listing}"
    );
    assert_eq!(status, "", "the refused write left the tree dirty");
    Ok(())
}

/// A path no file can have is the caller's mistake, not the server's trouble. A put whose
/// file name was 404 characters long reached the disk, failed there, and was answered `500`
/// with the server's own path in the body (round-5 acceptance of d9cb9f7). Such a path — a
/// name over the file system's 255 bytes, or a path over 1,024 — is refused with `422` before
/// anything is written, and the answer names none of the server's paths.
#[test]
fn a_put_whose_path_no_file_can_have_is_refused_without_the_servers_path() -> Result<()> {
    let tree = fixture_clone("write-too-long")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let long_name = format!("decisions/{}.md", "x".repeat(400));
    let long_path = format!(
        "decisions/{}.md",
        std::iter::repeat_n("y".repeat(200), 6)
            .collect::<Vec<_>>()
            .join("/")
    );
    let replies = [&long_name, &long_path]
        .into_iter()
        .map(|path| {
            server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body(path, &good_record(), "session-too-long"),
            )
        })
        .collect::<Result<Vec<_>>>();
    drop(server);
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    let server_path = tree
        .parent()
        .context("the fixture has a root")?
        .display()
        .to_string();
    remove_fixture(&tree)?;

    for reply in replies? {
        assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
        assert_eq!(finding_kinds(&reply), vec!["not-a-record".to_owned()]);
        assert!(
            !reply.body.to_string().contains(&server_path),
            "the refusal shows the server's path: {}",
            reply.body
        );
    }
    assert_eq!(status, "", "the refused write left something on disk");
    Ok(())
}

/// A put below an existing record, or onto a directory, fails at the disk the same way, so it
/// is refused the same way before anything is written.
#[test]
fn a_put_below_a_file_or_onto_a_directory_is_refused_without_the_servers_path() -> Result<()> {
    let tree = fixture_clone("write-in-the-way")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let put = |path: &str| {
        server.post_as(
            "ada@example.com",
            "/v1/records/put",
            &put_body(path, &good_record(), "session-in-the-way"),
        )
    };
    let outcome = (|| -> Result<_> {
        let file = put("decisions/cadence.md")?;
        let folder = put("decisions/folder.md/inner.md")?;
        Ok((
            file,
            folder,
            put("decisions/cadence.md/below.md")?,
            put("decisions/folder.md")?,
        ))
    })();
    drop(server);
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    let server_path = tree
        .parent()
        .context("the fixture has a root")?
        .display()
        .to_string();
    remove_fixture(&tree)?;

    let (file, folder, below, onto) = outcome?;
    assert_eq!(file.status, 200, "unexpected body: {}", file.body);
    assert_eq!(folder.status, 200, "unexpected body: {}", folder.body);
    for reply in [below, onto] {
        assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
        assert_eq!(finding_kinds(&reply), vec!["not-a-record".to_owned()]);
        assert!(
            !reply.body.to_string().contains(&server_path),
            "the refusal shows the server's path: {}",
            reply.body
        );
    }
    assert_eq!(status, "", "the refused write left something on disk");
    Ok(())
}

/// `decisions//renewal.md` and `decisions/renewal.md` are one file. The checks match findings
/// by the path string, so the two spellings would be one record and no check: the write is
/// normalised to the spelling the tree uses, and checked under it.
#[test]
fn put_with_a_doubled_separator_is_still_checked() -> Result<()> {
    let tree = fixture_clone("write-doubled")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let broken = good_record() + "\nIt replaces [[a-record-that-does-not-exist]].\n";
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions//doubled.md", &broken, "session-doubled"),
    )?;
    let good = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions//plain.md", &good_record(), "session-doubled-ok"),
    )?;
    drop(server);
    let bare = remote_of(&tree)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let subject = git(&bare, &["log", "-1", "--format=%s", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    assert_eq!(finding_kinds(&reply), vec!["gap".to_owned()]);
    assert!(
        !listing.contains("doubled.md"),
        "the unchecked record reached the remote: {listing}"
    );
    assert_eq!(good.status, 200, "unexpected body: {}", good.body);
    assert_eq!(
        subject, "put decisions/plain.md",
        "the commit should name the path the tree uses"
    );
    Ok(())
}

/// `record_path` keeps a path inside the tree by its text, which a symbolic link committed to
/// `main` defeats: the write follows the link out of the tree, and the server runs as root in
/// its container.
#[test]
fn put_through_a_symbolic_link_is_refused() -> Result<()> {
    let tree = fixture_clone("write-symlink")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside)?;
    std::fs::create_dir_all(tree.join("decisions"))?;
    let outside_text = outside.to_str().context("path is not valid UTF-8")?;
    std::os::unix::fs::symlink(outside_text, tree.join("decisions/elsewhere"))?;
    std::os::unix::fs::symlink(
        outside.join("captured.md"),
        tree.join("decisions/captured.md"),
    )?;
    git(&tree, &["add", "decisions"])?;
    commit(&tree, "a link someone merged")?;
    git(&tree, &["push", "--quiet", "origin", "main"])?;

    let server = Server::start(&tree, &[])?;
    let through_a_directory = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/elsewhere/captured.md",
            &good_record(),
            "session-link",
        ),
    )?;
    let onto_a_file = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/captured.md", &good_record(), "session-link-2"),
    )?;
    drop(server);
    let escaped = outside.join("captured.md").exists();
    remove_fixture(&tree)?;

    assert_eq!(
        through_a_directory.status, 400,
        "unexpected body: {}",
        through_a_directory.body
    );
    assert_eq!(
        onto_a_file.status, 400,
        "unexpected body: {}",
        onto_a_file.body
    );
    assert!(!escaped, "the write followed a link out of the tree");
    Ok(())
}

/// A record path is a name. A refused put to `decisions/*.md` used to be undone with git's
/// pathspec magic on: git read the path as a pattern, restored the tracked records it matched,
/// and left the refused record itself on disk, where the server served it to every reader.
#[test]
fn a_put_whose_path_is_a_pattern_leaves_nothing_behind() -> Result<()> {
    let tree = fixture_clone("write-pattern")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let tracked = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/tracked.md", &good_record(), "session-tracked"),
    )?;
    let broken = broken_zeppelin_record();
    let replies = [
        "decisions/*.md",
        "decisions/?.md",
        "decisions/[a].md",
        "decisions/a].md",
        "-zeppelin.md",
        ":zeppelin.md",
    ]
    .map(|path| {
        server
            .post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body(path, &broken, "session-pattern"),
            )
            .map(|reply| (path, reply))
    });
    let found = server.command(&["search", "zeppelin"])?;
    drop(server);
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    let left = tree.join("decisions/*.md").exists();
    remove_fixture(&tree)?;

    assert_eq!(tracked.status, 200, "unexpected body: {}", tracked.body);
    for reply in replies {
        let (path, reply) = reply?;
        assert_eq!(reply.status, 400, "{path}: unexpected body: {}", reply.body);
    }
    assert!(!left, "the refused record stayed on disk");
    assert_eq!(status, "", "the refused write left the clone dirty");
    assert!(
        !found.text("stdout")?.contains("Zeppelin budget"),
        "the server serves the refused record: {}",
        found.body
    );
    Ok(())
}

/// A refused put leaves the clone as the write found it, as every other failed write does —
/// the directories it made for the record included.
#[test]
fn a_refused_put_leaves_no_directory_behind() -> Result<()> {
    let tree = fixture_clone("write-refused-directory")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "fresh/deeper/zeppelin.md",
            &broken_zeppelin_record(),
            "session-fresh",
        ),
    )?;
    drop(server);
    let made = tree.join("fresh").exists();
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    assert!(!made, "the refused write left its directories behind");
    Ok(())
}

/// `.gitignore` decides what the tree loads, and git's clean leaves an ignored file where it
/// is. A refused put to an ignored path stayed on disk, and a put over a file somebody keeps
/// there, ignored, replaced their file; a delete removed it and answered 200. None of it was
/// ever a record, so the server touches none of it.
#[test]
fn a_write_to_an_ignored_path_never_touches_the_file() -> Result<()> {
    let tree = fixture_clone("write-ignored-untouched")?;
    fetch_window(&tree)?;
    std::fs::write(tree.join(".gitignore"), "scratch/\n")?;
    git(&tree, &["add", ".gitignore"])?;
    commit(&tree, "ignore the scratch directory")?;
    git(&tree, &["push", "--quiet", "origin", "main"])?;
    std::fs::create_dir_all(tree.join("scratch"))?;
    let notes = "my own notes, which git ignores\n";
    std::fs::write(tree.join("scratch/notes.md"), notes)?;
    let bare = remote_of(&tree)?;
    let before = git(&bare, &["rev-parse", "main"])?;

    let server = Server::start(&tree, &[])?;
    let fresh = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("scratch/smuggled.md", &good_record(), "session-ignored"),
    )?;
    let over = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("scratch/notes.md", &good_record(), "session-ignored"),
    )?;
    let removed = server.post_as(
        "ada@example.com",
        "/v1/records/delete",
        &json!({ "path": "scratch/notes.md", "session": "session-ignored" }),
    )?;
    drop(server);
    let smuggled = tree.join("scratch/smuggled.md").exists();
    let kept = std::fs::read_to_string(tree.join("scratch/notes.md")).ok();
    let after = git(&bare, &["rev-parse", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(fresh.status, 422, "unexpected body: {}", fresh.body);
    assert_eq!(finding_kinds(&fresh), vec!["not-a-record".to_owned()]);
    assert!(!smuggled, "the refused record stayed on disk");
    assert_eq!(over.status, 422, "unexpected body: {}", over.body);
    assert_eq!(removed.status, 400, "unexpected body: {}", removed.body);
    assert_eq!(
        kept.as_deref(),
        Some(notes),
        "a file somebody keeps, ignored, was replaced or removed"
    );
    assert_eq!(
        after, before,
        "a write to an ignored path changed the remote"
    );
    Ok(())
}
