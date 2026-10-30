//! The writes the server refuses, and what a refusal leaves behind.

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, put_body, within};
use crate::support::fake_http::FakeHttp;
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

/// A decision whose one claim cites `address` at `tier`: the summary a tracker or a pull
/// request holds, at the address where a reader can open it.
fn web_sourced_record(address: &str, tier: u8) -> String {
    format!(
        "---\n\
         type: decision\n\
         status: open\n\
         title: Keep the quarterly cadence\n\
         quote: \"the cadence stays quarterly, as the tracker records\"\n\
         source: \"{address}\"\n\
         tier: {tier}\n\
         ---\n\
         The tracker records the answer.\n"
    )
}

/// What one put of a record citing `address` at `tier` answered, and whether it reached the
/// remote.
fn put_sourced(label: &str, address: &str, tier: u8) -> Result<(u16, Value, String)> {
    let tree = fixture_clone(label)?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/web.md",
            &web_sourced_record(address, tier),
            "session-web",
        ),
    );
    drop(server);
    let landed = git(&remote_of(&tree)?, &["ls-tree", "main", "decisions/web.md"]);
    remove_fixture(&tree)?;
    let reply = reply?;
    Ok((reply.status, reply.body, landed?))
}

/// What one put of a web-sourced record at `tier` answered, whether it reached the remote, and
/// every request the cited address received.
fn put_web_sourced(label: &str, tier: u8) -> Result<(u16, Value, String, usize)> {
    let site = FakeHttp::start()?;
    let (status, body, landed) = put_sourced(label, &site.url("/ask/1"), tier)?;
    Ok((status, body, landed, site.requests()?.len()))
}

/// The kinds of the findings a refusal carries.
fn finding_kinds(body: &Value) -> Result<Vec<&str>> {
    Ok(body
        .get("findings")
        .and_then(Value::as_array)
        .context("findings is a list")?
        .iter()
        .filter_map(|finding| finding.get("kind").and_then(Value::as_str))
        .collect())
}

/// A summary may cite the web address where it can be read, at tier 3 or 4. The server takes
/// the address as written and never fetches it.
#[test]
fn put_citing_a_web_address_at_tier_3_is_committed_unfetched() -> Result<()> {
    let (status, body, landed, fetched) = put_web_sourced("write-web-tier-3", 3)?;

    assert_eq!(status, 200, "unexpected body: {body}");
    assert!(
        landed.contains("decisions/web.md"),
        "the record did not reach main"
    );
    assert_eq!(fetched, 0, "the server fetched the cited address");
    Ok(())
}

/// Tier 1 and 2 are a source's own words, checked against the source; a web address is
/// checked against nothing, so the server refuses it as unchecked, and still fetches nothing.
#[test]
fn put_citing_a_web_address_at_tier_1_is_422_unchecked() -> Result<()> {
    let (status, body, landed, fetched) = put_web_sourced("write-web-tier-1", 1)?;

    assert_eq!(status, 422, "unexpected body: {body}");
    assert_eq!(
        finding_kinds(&body)?,
        ["unchecked"],
        "unexpected body: {body}"
    );
    assert!(
        landed.is_empty(),
        "the refused record reached main: {landed}"
    );
    assert_eq!(fetched, 0, "the server fetched the cited address");
    Ok(())
}

/// A link the URL parser reads as a Slack host in any form the scanner does not read as a
/// message pointer, and a source that holds a web address but is not that one valid address
/// alone, in any case, is no web address a summary may cite: the server refuses it at tiers 3
/// and 4 as unchecked.
#[test]
fn put_citing_a_permalink_variant_or_a_malformed_address_is_422_at_tiers_3_and_4() -> Result<()> {
    let port = "https://acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001";
    let path = "/archives/C0EXAMPLE01/p1700000000000001";
    for (label, address) in [
        ("write-web-permalink-port", port.to_owned()),
        (
            "write-web-permalink-case",
            format!("https://acme.SLACK.com{path}"),
        ),
        ("write-web-permalink-markdown", format!("[message]({port})")),
        ("write-web-permalink-assignment", format!("source={port}")),
        (
            "write-web-permalink-paren-user",
            format!("source=https://a)b@acme.slack.com:443{path}"),
        ),
        (
            "write-web-permalink-host-tab",
            format!("https://acme.sla\\tck.com{path}"),
        ),
        (
            "write-web-permalink-scheme-tab",
            format!("ht\\ttps://acme.slack.com:443{path}"),
        ),
        (
            "write-web-markdown",
            "[the answer](https://tracker.example.com/ask/1)".to_owned(),
        ),
        ("write-web-no-host", "https://:443/ask/1".to_owned()),
        ("write-web-upper-no-host", "HTTPS://:443/ask/1".to_owned()),
        ("write-web-control", "https://example.com/\\e[2J".to_owned()),
        (
            "write-web-host-space",
            "https://exa mple.com/ask/1".to_owned(),
        ),
    ] {
        for tier in [3, 4] {
            let (status, body, landed) = put_sourced(&format!("{label}-{tier}"), &address, tier)?;

            assert_eq!(
                status, 422,
                "{label} at tier {tier}: unexpected body: {body}"
            );
            assert_eq!(
                finding_kinds(&body)?,
                ["unchecked"],
                "{label} at tier {tier}: {body}"
            );
            assert!(
                landed.is_empty(),
                "{label} at tier {tier}: the refused record reached main: {landed}"
            );
        }
    }
    Ok(())
}

/// One checked quote does not carry another claim of the record: a `quotes` entry citing a
/// variant of a Slack permalink beside a tier 1 quote its source holds is still refused.
#[test]
fn put_with_a_permalink_variant_beside_a_checked_quote_is_422() -> Result<()> {
    let tree = fixture_clone("write-web-mixed-quotes")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let record = "---\n\
         type: decision\n\
         status: open\n\
         title: Keep the echo cadence\n\
         quotes:\n\
         \x20 - quote: \"echo C0EXAMPLE01\"\n\
         \x20   source: \"echo:C0EXAMPLE01/record\"\n\
         \x20   tier: 1\n\
         \x20 - quote: \"the cadence stays quarterly, as the thread records\"\n\
         \x20   source: \"source=https://a)b@acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001\"\n\
         \x20   tier: 3\n\
         ---\n\
         The stream confirmed the cadence.\n";
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/web.md", record, "session-web"),
    );
    drop(server);
    let landed = git(&remote_of(&tree)?, &["ls-tree", "main", "decisions/web.md"]);
    remove_fixture(&tree)?;
    let (reply, landed) = (reply?, landed?);

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    assert_eq!(finding_kinds(&reply.body)?, ["unchecked"], "{}", reply.body);
    assert!(
        landed.is_empty(),
        "the refused record reached main: {landed}"
    );
    Ok(())
}
