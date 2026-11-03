//! `GET /v1/windows/{id}/context`: the server runs the tree's context source for one window and
//! answers what it said, cleaned, or that it is unavailable — never an error status.

mod support;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::Value;

use support::{commit, fetch_window, fixture_clone, git, remove_fixture, write_executable, Server};

/// The one caller the fixture's `[context_source]` lets read a window's context.
const CYCLE: &str = "cycle@example.com";

/// A cloned fixture tree whose `[context_source]` runs `stub` (a shell script) and whose open
/// question is tracked by a link, with one fetched window. Returns the tree, the window id and
/// the file the stub writes its standard input to.
fn tree_with_source(label: &str, stub: &str, extra: &str) -> Result<(PathBuf, String, PathBuf)> {
    tree_with_source_for(label, stub, extra, &format!("'{CYCLE}'"))
}

/// [`tree_with_source`] whose `callers` are `callers`, a TOML list body.
fn tree_with_source_for(
    label: &str,
    stub: &str,
    extra: &str,
    callers: &str,
) -> Result<(PathBuf, String, PathBuf)> {
    let tree = fixture_clone(label)?;
    let root = tree
        .parent()
        .context("the fixture has a root")?
        .to_path_buf();
    let program = root.join("context-stub");
    let request = root.join("context-request.json");
    write_executable(
        &program,
        &format!("#!/bin/sh\ncat > '{}'\n{stub}\n", request.display()),
    )?;
    let mut config = std::fs::read_to_string(tree.join("ctx.toml"))?;
    std::fmt::Write::write_fmt(
        &mut config,
        format_args!(
        "\n[context_source]\ncommand = ['{}']\ncallers = [{callers}]\nkey_pattern = 'C0EXAMPLE[0-9]+'\nlink_prefixes = ['https://tracker.example/']\n{extra}",
        program.display()
        ),
    )?;
    std::fs::write(tree.join("ctx.toml"), config)?;
    // A clone holds no empty directory, so the fixture's `decisions/` is made here.
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(
        tree.join("decisions/tracked.md"),
        "---\ntype: question\nstatus: open\ntitle: Who owns the cadence\nowner: unassigned\ntracked_by:\n  - https://tracker.example/issues/7\n---\nNobody has said.\n",
    )?;
    git(&tree, &["add", "."])?;
    commit(&tree, "a context source and a tracked question")?;
    git(&tree, &["push", "--quiet", "origin", "main"])?;
    let window = fetch_window(&tree)?;
    Ok((tree, window, request))
}

fn ask(server: &Server, window: &str, caller: Option<&str>) -> Result<support::Reply> {
    let headers: Vec<(&str, &str)> = caller
        .map(|caller| vec![("Tailscale-User-Login", caller)])
        .unwrap_or_default();
    server.get(
        server.tailnet,
        &format!("/v1/windows/{window}/context"),
        &headers,
    )
}

fn items(reply: &support::Reply) -> Result<&Vec<Value>> {
    reply
        .field("items")?
        .as_array()
        .with_context(|| format!("items is not a list: {}", reply.body))
}

/// The program gets the window and its subjects on standard input — a key from the window's
/// messages and the link an open record is tracked by — and its items come back cleaned.
#[test]
fn the_context_source_is_sent_the_windows_subjects() -> Result<()> {
    let (tree, window, request) = tree_with_source(
        "context-subjects",
        r#"printf '%s' '{"items":[{"ref":"OPS-7","title":"Cadence\u001b[2J owner","status":"done","updated_at":"2026-11-01T00:00:00Z","links":["https://chat.example/archives/C1/p1","javascript:alert(1)"]}]}'"#,
        "",
    )?;
    let server = Server::start(&tree, &[])?;
    let reply = ask(&server, &window, Some(CYCLE))?;
    drop(server);
    let sent = std::fs::read_to_string(&request).unwrap_or_default();
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "{}", reply.body);
    let sent: Value = serde_json::from_str(&sent).context("the program got no JSON request")?;
    assert_eq!(
        sent.get("window").cloned().unwrap_or_default(),
        Value::from(window.as_str())
    );
    assert_eq!(
        sent.get("subjects").cloned().unwrap_or_default(),
        serde_json::json!(["C0EXAMPLE01", "https://tracker.example/issues/7"])
    );
    let items = items(&reply)?;
    assert_eq!(items.len(), 1, "{}", reply.body);
    let item = items.first().context("no item")?;
    assert_eq!(item.get("ref").cloned().unwrap_or_default(), "OPS-7");
    assert_eq!(
        item.get("title").cloned().unwrap_or_default(),
        "Cadence[2J owner"
    );
    assert_eq!(
        item.get("links").cloned().unwrap_or_default(),
        serde_json::json!(["https://chat.example/archives/C1/p1"])
    );
    assert!(reply.body.get("unavailable").is_none(), "{}", reply.body);
    Ok(())
}

/// A program that does not answer in its time is stopped, and the answer is 200 and
/// `unavailable`, well before the program would have finished.
#[test]
fn a_context_source_that_hangs_answers_unavailable() -> Result<()> {
    let (tree, window, _) =
        tree_with_source("context-hang", "exec sleep 60", "timeout_secs = 1\n")?;
    let server = Server::start(&tree, &[])?;
    let started = std::time::Instant::now();
    let reply = ask(&server, &window, Some(CYCLE))?;
    let took = started.elapsed();
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply
            .text("unavailable")?
            .contains("did not answer within 1s"),
        "{}",
        reply.body
    );
    assert_eq!(items(&reply)?.len(), 0, "{}", reply.body);
    assert!(took.as_secs() < 30, "the answer took {took:?}");
    Ok(())
}

/// A program that fails, prints something other than its JSON, or prints past the output cap
/// answers 200 and `unavailable`: context is optional, and its failure is no server error.
#[test]
fn a_context_source_that_fails_or_floods_answers_unavailable() -> Result<()> {
    for (label, stub, reason) in [
        ("context-exit", "exit 3", "exited with status 3"),
        (
            "context-garbage",
            "echo not json",
            "other than its JSON answer",
        ),
        (
            "context-flood",
            "head -c 400000 /dev/zero | tr '\\0' a",
            "more than",
        ),
    ] {
        let (tree, window, _) = tree_with_source(label, stub, "")?;
        let server = Server::start(&tree, &[])?;
        let reply = ask(&server, &window, Some(CYCLE))?;
        drop(server);
        remove_fixture(&tree)?;

        assert_eq!(reply.status, 200, "{label}: {}", reply.body);
        assert!(
            reply.text("unavailable")?.contains(reason),
            "{label}: {}",
            reply.body
        );
        assert_eq!(items(&reply)?.len(), 0, "{label}: {}", reply.body);
    }
    Ok(())
}

/// However much the program answers within its output cap, the items stay under the answer's
/// byte cap and the answer counts what it left out.
#[test]
fn the_answer_stays_under_its_byte_cap() -> Result<()> {
    let stub = r#"awk 'BEGIN { printf "{\"items\":["; for (i = 0; i < 400; i++) { if (i) printf ","; t = ""; for (j = 0; j < 300; j++) t = t "x"; printf "{\"ref\":\"OPS-%d\",\"title\":\"%s\",\"status\":\"open\",\"links\":[\"https://tracker.example/OPS-%d\"]}", i, t, i } printf "]}" }'"#;
    let (tree, window, _) = tree_with_source("context-cap", stub, "")?;
    let server = Server::start(&tree, &[])?;
    let reply = ask(&server, &window, Some(CYCLE))?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "{}", reply.body);
    let items = items(&reply)?;
    let bytes = serde_json::to_vec(items)?.len();
    assert!(bytes <= 48 * 1024, "the items take {bytes} bytes");
    assert_ne!(items.len(), 0, "{}", reply.body);
    for item in items {
        assert!(item
            .get("title")
            .cloned()
            .unwrap_or_default()
            .as_str()
            .is_some_and(|t| t.chars().count() <= 200));
    }
    let dropped = reply
        .field("dropped")?
        .as_u64()
        .context("dropped is a count")?;
    assert_eq!(usize::try_from(dropped)? + items.len(), 400);
    Ok(())
}

/// A tree with no context source answers an empty list, and nothing is wrong.
#[test]
fn a_tree_with_no_context_source_answers_an_empty_list() -> Result<()> {
    let tree = fixture_clone("context-none")?;
    let window = fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    let reply = ask(&server, &window, Some("ada@example.com"))?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(items(&reply)?.len(), 0, "{}", reply.body);
    assert!(reply.body.get("unavailable").is_none(), "{}", reply.body);
    Ok(())
}

/// Only the callers `context_source.callers` names read a window's context: anyone else the
/// server admits is refused 403, and a request with no caller 401, before the program runs.
#[test]
fn a_caller_the_context_source_does_not_name_is_refused() -> Result<()> {
    let (tree, window, request) =
        tree_with_source("context-callers", r#"printf '%s' '{"items":[]}'"#, "")?;
    let server = Server::start(&tree, &[])?;
    let stranger = ask(&server, &window, Some("ada@example.com"))?;
    let nobody = ask(&server, &window, None)?;
    let ran_for_others = request.exists();
    let cycle = ask(&server, &window, Some(CYCLE))?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(stranger.status, 403, "{}", stranger.body);
    assert_eq!(nobody.status, 401, "{}", nobody.body);
    assert!(
        !ran_for_others,
        "the program ran for a caller it does not serve"
    );
    assert_eq!(cycle.status, 200, "{}", cycle.body);
    Ok(())
}

/// A tagged ingest cycle has no login header; `context_source.callers` names it by its tag,
/// which the server admits through `--tailnet-allow-tag`, and only that tag reads the context.
#[test]
fn a_tagged_cycle_named_by_its_tag_reads_the_context() -> Result<()> {
    let (tree, window, _) = tree_with_source_for(
        "context-tagged",
        r#"printf '%s' '{"items":[]}'"#,
        "",
        "'tag:ctx-ingest'",
    )?;
    let root = tree.parent().context("the fixture has a root")?.to_owned();
    let api = support::fake_tailscaled::FakeTailscaled::start(
        &root.join("tailscaled"),
        &[
            ("100.64.0.7", &["tag:ctx-ingest"]),
            ("100.64.0.8", &["tag:ctx-legion"]),
        ],
    )?;
    let socket = api.socket.to_str().context("socket path is not UTF-8")?;
    let server = Server::start(
        &tree,
        &[
            "--tailnet-allow-tag",
            "tag:ctx-ingest,tag:ctx-legion",
            "--tailscale-socket",
            socket,
        ],
    )?;
    let path = format!("/v1/windows/{window}/context");
    let ingest = server.get(server.tailnet, &path, &[("X-Forwarded-For", "100.64.0.7")])?;
    let legion = server.get(server.tailnet, &path, &[("X-Forwarded-For", "100.64.0.8")])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(ingest.status, 200, "{}", ingest.body);
    assert_eq!(legion.status, 403, "{}", legion.body);
    Ok(())
}

/// A window id that is not one is 400, and one with no window behind it 404.
#[test]
fn a_window_that_is_not_there_is_refused() -> Result<()> {
    let (tree, _, _) = tree_with_source("context-window", r#"printf '%s' '{"items":[]}'"#, "")?;
    let server = Server::start(&tree, &[])?;
    let malformed = ask(&server, "newest", Some(CYCLE))?;
    let missing = ask(&server, "20200101T000000Z-0000", Some(CYCLE))?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(malformed.status, 400, "{}", malformed.body);
    assert_eq!(missing.status, 404, "{}", missing.body);
    Ok(())
}
