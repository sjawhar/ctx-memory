//! Credential refusal tests.

use anyhow::{Context as _, Result};
use serde_json::json;

use super::finding_kinds;
use crate::fixtures::{good_record, put_body};
use crate::support::{commit, fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

/// The agent cycle's session can read its own model key, and an instruction it obeys could
/// put that key — or any other credential it can reach — into a record. The writer is the last
/// place that can stop it reaching the graph repository, so a put that holds a credential is
/// refused before it is written, and the refusal names the kind of credential, never the
/// credential.
#[test]
fn a_put_holding_a_credential_is_refused_without_repeating_it() -> Result<()> {
    let tree = fixture_clone("write-credential")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let record = good_record() + &format!("\nThe deploy used {token} for an hour.\n");
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/leaked.md", &record, "session-credential"),
    )?;
    drop(server);
    let listing = git(
        &remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    assert_eq!(finding_kinds(&reply), vec!["credential".to_owned()]);
    assert!(
        !reply.body.to_string().contains(&token),
        "the refusal repeats the credential: {}",
        reply.body
    );
    assert!(
        !listing.contains("leaked"),
        "the credential reached the remote: {listing}"
    );
    assert_eq!(status, "", "the refused write left the clone dirty");
    Ok(())
}

/// A credential reaches the graph repository through the two other things a put carries: the
/// record's path, which becomes a file name and the commit's subject, and the session, which
/// becomes the commit's `Session:` line. The agent cycle's session chooses the path as freely
/// as the content, so an instruction it obeys can put a key there as easily. Observed on the
/// real surface: from another box over the tailnet, `ctx record put decisions/ghp_….md` and a
/// put with `CTX_SESSION=ghp_…` both answered 200 and pushed the fake token into `main`'s tree
/// and history.
#[test]
fn a_put_whose_path_or_session_holds_a_credential_is_refused() -> Result<()> {
    let tree = fixture_clone("write-credential-path")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let in_path = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            &format!("decisions/{token}.md"),
            &good_record(),
            "session-ok",
        ),
    )?;
    let in_session = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/session-leak.md", &good_record(), &token),
    )?;
    drop(server);
    let bare = remote_of(&tree)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let history = git(&bare, &["log", "--format=%B", "main"])?;
    remove_fixture(&tree)?;

    assert!(
        !listing.contains(&token) && !history.contains(&token),
        "the credential reached the remote (path put {} {}, session put {} {}): {listing}\n{history}",
        in_path.status,
        in_path.body,
        in_session.status,
        in_session.body
    );
    assert_ne!(in_path.status, 200, "unexpected body: {}", in_path.body);
    assert_ne!(
        in_session.status, 200,
        "unexpected body: {}",
        in_session.body
    );
    Ok(())
}

/// A delete carries a path and a session too, and both go into the commit it makes: its
/// subject names the path, its `Session:` line the session. A record whose name holds a key
/// can reach `main` through a merge this server never saw; a delete of it through the server
/// would write the key into one more commit.
#[test]
fn a_delete_whose_path_or_session_holds_a_credential_is_refused() -> Result<()> {
    let tree = fixture_clone("write-credential-rm")?;
    fetch_window(&tree)?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let leaked = format!("decisions/{token}.md");
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(tree.join(&leaked), good_record())?;
    std::fs::write(tree.join("decisions/kept.md"), good_record())?;
    git(&tree, &["add", "decisions"])?;
    commit(&tree, "two records someone merged")?;
    git(&tree, &["push", "--quiet", "origin", "main"])?;
    let bare = remote_of(&tree)?;
    let before = git(&bare, &["rev-parse", "main"])?;

    let server = Server::start(&tree, &[])?;
    let by_path = server.post_as(
        "ada@example.com",
        "/v1/records/delete",
        &json!({ "path": leaked, "session": "session-ok" }),
    )?;
    let by_session = server.post_as(
        "ada@example.com",
        "/v1/records/delete",
        &json!({ "path": "decisions/kept.md", "session": token }),
    )?;
    drop(server);
    let after = git(&bare, &["rev-parse", "main"])?;
    let added = git(&bare, &["log", "--format=%B", &format!("{before}..main")])?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    for (what, reply) in [("path", &by_path), ("session", &by_session)] {
        assert_eq!(reply.status, 422, "{what}: unexpected body: {}", reply.body);
        assert_eq!(
            finding_kinds(reply),
            vec!["credential".to_owned()],
            "{what}"
        );
        assert!(
            !reply.body.to_string().contains(&token),
            "{what}: the refusal repeats the credential: {}",
            reply.body
        );
    }
    assert_eq!(
        after, before,
        "a delete carried the credential into a commit: {added}"
    );
    assert_eq!(status, "", "the refused delete left the clone dirty");
    Ok(())
}

/// A refusal goes back to the caller, and through it into a transcript, so no refusal repeats
/// a credential the request carried — whatever else is wrong with the request. A path that
/// was refused for its shape, or for naming no record, used to be quoted back whole.
#[test]
fn a_refusal_never_repeats_a_credential_in_the_path_or_session() -> Result<()> {
    let tree = fixture_clone("write-credential-echo")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let put = |path: &str, session: &str| {
        server.post_as(
            "ada@example.com",
            "/v1/records/put",
            &put_body(path, &good_record(), session),
        )
    };
    let replies = [
        ("a record path", put(&format!("decisions/{token}.md"), "s")?),
        (
            "a path to no record",
            put(&format!("decisions/{token}.txt"), "s")?,
        ),
        ("a hidden path", put(&format!(".drafts/{token}.md"), "s")?),
        ("a session", put("decisions/clean.md", &token)?),
        (
            "a delete of no record",
            server.post_as(
                "ada@example.com",
                "/v1/records/delete",
                &json!({ "path": format!("decisions/{token}.md"), "session": "s" }),
            )?,
        ),
        (
            "a marker advance",
            server.post_as(
                "ada@example.com",
                "/v1/marker/advance",
                &json!({ "window": "no-such-window", "session": token }),
            )?,
        ),
    ];
    drop(server);
    remove_fixture(&tree)?;

    for (what, reply) in &replies {
        assert_ne!(reply.status, 200, "{what}: unexpected body: {}", reply.body);
        assert!(
            !reply.body.to_string().contains(&token),
            "{what}: the refusal repeats the credential: {}",
            reply.body
        );
    }
    Ok(())
}

/// The marker names the session that moved it, on the share every copy and the fetch job
/// read. A session holding a credential would put the credential there, so the advance is
/// refused and the marker stays where it was.
#[test]
fn a_marker_advance_whose_session_holds_a_credential_is_refused() -> Result<()> {
    let tree = fixture_clone("write-marker-credential")?;
    let window = fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let reply = server.post_as(
        "ada@example.com",
        "/v1/marker/advance",
        &json!({ "window": window, "session": token }),
    )?;
    drop(server);
    let root = tree.parent().context("the fixture has a root")?;
    let marker = std::fs::read_to_string(root.join("data/marker.json")).unwrap_or_default();
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 400, "unexpected body: {}", reply.body);
    assert!(
        !reply.body.to_string().contains(&token),
        "the refusal repeats the credential: {}",
        reply.body
    );
    assert!(
        !marker.contains(&token),
        "the credential reached the marker: {marker}"
    );
    Ok(())
}

/// `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` and `SKILL.md` are read as instructions by an agent
/// that a person runs in a clone of the graph, wherever they are in the tree. Like the rules
/// page, they change only through a reviewed pull request, never through the server, in any
/// spelling of their names.
#[test]
fn files_agents_read_as_instructions_are_not_written_through_the_server() -> Result<()> {
    let tree = fixture_clone("write-agent-files")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let head = git(&bare, &["rev-parse", "main"])?;
    let server = Server::start(&tree, &[])?;
    let mut replies = Vec::new();
    for path in [
        "AGENTS.md",
        "decisions/CLAUDE.md",
        "decisions/gemini.md",
        "skills/review/SKILL.md",
        "skills/review/Skill.md",
        "decisions//Agents.md",
    ] {
        replies.push((
            format!("put {path}"),
            server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body(path, &good_record(), "session-agent-files"),
            )?,
        ));
    }
    replies.push((
        "delete AGENTS.md".to_owned(),
        server.post_as(
            "ada@example.com",
            "/v1/records/delete",
            &json!({ "path": "AGENTS.md", "session": "session-agent-files" }),
        )?,
    ));
    drop(server);
    let after = git(&bare, &["rev-parse", "main"])?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    for (what, reply) in &replies {
        assert_eq!(reply.status, 400, "{what}: unexpected body: {}", reply.body);
        assert!(
            reply.text("error")?.contains("instructions"),
            "{what}: unexpected body: {}",
            reply.body
        );
    }
    assert_eq!(after, head, "a write through the server changed the remote");
    assert_eq!(status, "", "the refused write left the tree dirty");
    Ok(())
}
