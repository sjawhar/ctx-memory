//! The writes the server refuses, and what a refusal leaves behind.

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, put_body, within};
use crate::support::{fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

#[test]
fn put_with_sweep_finding_is_422_and_leaves_tree_clean() -> Result<()> {
    let tree = fixture_clone("write-sweep")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let broken = good_record() + "\nIt replaces [[a-record-that-does-not-exist]].\n";
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/broken.md", &broken, "session-8"),
    )?;
    drop(server);
    let status = git(&tree, &["status", "--porcelain"])?;
    let listing = git(&tree, &["ls-files"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    let findings = reply
        .field("findings")?
        .as_array()
        .context("findings is a list")?;
    assert_eq!(findings.len(), 1, "unexpected findings: {findings:?}");
    assert_eq!(
        findings
            .first()
            .and_then(|finding| finding.get("kind"))
            .and_then(Value::as_str),
        Some("gap")
    );
    assert_eq!(status, "", "the rejected write left the tree dirty");
    assert!(
        !listing.lines().any(|name| name == "decisions/broken.md"),
        "the rejected record was committed: {listing}"
    );
    Ok(())
}

/// A record that states an absence without naming what was searched is a claim nothing
/// checked. The sweep reports it per record and line, and the gate refuses the put.
#[test]
fn put_stating_an_unsourced_absence_is_422() -> Result<()> {
    let tree = fixture_clone("write-negative")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let record = good_record() + "\nThe vendor was never told about the cadence.\n";
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/absence.md", &record, "session-negative"),
    )?;
    drop(server);
    let status = git(&tree, &["status", "--porcelain"])?;
    let listing = git(&tree, &["ls-files"])?;
    let bare = remote_of(&tree)?;
    let remote_listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    let findings = reply
        .field("findings")?
        .as_array()
        .context("findings is a list")?;
    let kinds: Vec<&str> = findings
        .iter()
        .filter_map(|finding| finding.get("kind").and_then(Value::as_str))
        .collect();
    assert_eq!(kinds, ["negative"], "unexpected findings: {findings:?}");
    assert_eq!(status, "", "the rejected write left the tree dirty");
    assert!(
        !listing.lines().any(|name| name == "decisions/absence.md"),
        "the rejected record was committed: {listing}"
    );
    assert!(
        !remote_listing.contains("absence.md"),
        "the unsourced absence reached the remote: {remote_listing}"
    );
    Ok(())
}

#[test]
fn put_with_unverifiable_quote_is_422() -> Result<()> {
    let tree = fixture_clone("write-quote")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let record = good_record().replace("echo C0EXAMPLE01", "words the source never carried");
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/quote.md", &record, "session-9"),
    )?;
    drop(server);
    let status = git(&tree, &["status", "--porcelain"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    let findings = reply
        .field("findings")?
        .as_array()
        .context("findings is a list")?;
    assert!(
        findings
            .iter()
            .any(|finding| finding.get("kind").and_then(Value::as_str) == Some("quote")),
        "unexpected findings: {findings:?}"
    );
    assert_eq!(status, "", "the rejected write left the tree dirty");
    Ok(())
}

/// A record whose quote does not verify must be refused with 422, even when the server first
/// learned the cited source only after its store was built.
///
/// A long-running server builds its record store, then the fetch job adds a source and writes
/// that source's verbatim declaration (`sources.json`) and a window beside it. The follow loop
/// ingests the window, so the cited pointer resolves, but the running server never re-reads the
/// verbatim declaration. The write-time `ctx cite verify` check (`live = false`) then trusts the
/// store's source metadata alone: if it is stale, the check treats the source as non-verbatim
/// and silently skips the quote, admitting an unverifiable record that a restarted server would
/// refuse.
///
/// Observed on the real transport: a remote `ctx record put` whose quote the source never
/// carried was committed to `main` with HTTP 200 instead of 422, and became searchable.
#[test]
fn put_with_unverifiable_quote_is_422_when_source_verbatim_is_stale() -> Result<()> {
    let tree = fixture_clone("write-stale-verbatim")?;
    let root = tree.parent().context("the fixture has a root")?;
    // The server keeps its record store in its own state directory, as a separate Cloud Run
    // container does; the fetch job's store never warms the server's.
    let server_state = root.join("server-state");
    let server_state_text = server_state
        .to_str()
        .context("state path is not valid UTF-8")?;
    let server = Server::start_with_env(
        &tree,
        &["--pull-interval", "1"],
        &[("CTX_STATE_DIR", server_state_text)],
    )?;

    // Build the server's store before any source is known: the share is still empty, so the
    // store holds no verbatim declaration for the echo source. `cite verify` builds the record
    // store without executing the source, which the server does not have on its PATH.
    let built = server.command(&["cite", "verify"])?;
    assert_eq!(built.field("exit_code")?, &json!(0));

    // The fetch job now writes the echo source's window and its `sources.json` verbatim
    // declaration to the share. The follow loop ingests the window, so the pointer resolves.
    fetch_window(&tree)?;
    let resolved = within(10, || {
        let reply = server.command(&["resolve", "echo:C0EXAMPLE01/record"])?;
        Ok(reply
            .text("stdout")
            .map(str::to_owned)
            .ok()
            .filter(|stdout| stdout.contains("echo C0EXAMPLE01")))
    });
    resolved?;

    // A record citing that message with a quote the source never carried must be refused.
    let record = good_record().replace("echo C0EXAMPLE01", "words the source never carried");
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/stale-verbatim.md", &record, "session-stale"),
    )?;
    let bare = remote_of(&tree)?;
    let landed = git(&bare, &["ls-tree", "main", "decisions/stale-verbatim.md"])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(
        reply.status, 422,
        "an unverifiable quote was not refused: {} {}",
        reply.status, reply.body
    );
    assert!(
        landed.is_empty(),
        "the refused record reached main: {landed}"
    );
    Ok(())
}

/// A quote the server cannot check is refused, not committed.
///
/// Nothing has declared the cited source: no fetch has run, so the share carries neither its
/// window nor its verbatim declaration. "This source's records are not its own words" and
/// "nothing has said what this source's records are" are different facts, and only the first
/// makes skipping a quote right. The second leaves the record's evidence unchecked, which no
/// reader could tell apart from a checked one afterwards.
#[test]
fn put_citing_a_source_that_declared_nothing_is_422() -> Result<()> {
    let tree = fixture_clone("write-undeclared")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/undeclared.md", &good_record(), "session-u"),
    )?;
    let bare = remote_of(&tree)?;
    let landed = git(&bare, &["ls-tree", "main", "decisions/undeclared.md"])?;
    drop(server);
    let kinds = reply
        .field("findings")
        .ok()
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|finding| finding.get("kind").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    remove_fixture(&tree)?;

    assert_eq!(
        reply.status, 422,
        "an unchecked quote was not refused: {} {}",
        reply.status, reply.body
    );
    assert_eq!(kinds, vec!["unchecked".to_owned()]);
    assert!(
        landed.is_empty(),
        "the refused record reached main: {landed}"
    );
    Ok(())
}
