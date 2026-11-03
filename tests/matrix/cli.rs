//! The Matrix source through the `ctx` binary: the conformance run, a live resolve, a quote
//! check against a message, and two fetches that store and move a room's cursor.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context as _, Result};
use chrono::{Duration, Utc};
use serde_json::Value;

use crate::fixtures::{homeserver, pointer, remove, scratch, ALICE, BOB, FAMILY};
use crate::support::fake_matrix::{FakeMatrix, TOKEN};

/// A tree whose one source is the Matrix account on `server`.
fn tree(root: &Path, server: &FakeMatrix) -> Result<PathBuf> {
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        format!(
            "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"matrix\"\nkind = \"matrix\"\nhomeserver = \"{}\"\n",
            server.url()
        ),
    )?;
    Ok(tree)
}

fn ctx(root: &Path, tree: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_ctx"))
        .arg("--memory")
        .arg(tree)
        .args(arguments)
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_DATA_DIR", root.join("data"))
        .env("CTX_CREDENTIAL_MATRIX", TOKEN)
        .env_remove("CTX_CREDENTIAL_MATRIX_FILE")
        .env_remove("CTX_SECRET_MATRIX")
        .env_remove("CTX_SERVER")
        .env_remove("CTX_EXCLUDE")
        .output()
        .context("running ctx")
}

/// Milliseconds since the epoch, `minutes` before now.
fn minutes_ago(minutes: i64) -> i64 {
    (Utc::now() - Duration::minutes(minutes)).timestamp_millis()
}

fn stdout_json(output: &Output) -> Result<Value> {
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "decoding ctx output: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn source_run_validates_a_fetch_and_a_resolve_against_the_homeserver() -> Result<()> {
    let server = homeserver()?;
    let news = server.text(FAMILY, ALICE, minutes_ago(120), "Family news")?;
    let root = scratch("cli-source-run")?;
    let tree = tree(&root, &server)?;

    let fetch = ctx(&root, &tree, &["--json", "source", "run", "matrix"])?;
    let resolve = ctx(
        &root,
        &tree,
        &["source", "run", "matrix", "--op", "resolve"],
    )?;
    remove(&root)?;

    assert_eq!(
        fetch.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let envelope = stdout_json(&fetch)?;
    let data = envelope.get("data").context("the envelope has data")?;
    assert_eq!(data.pointer("/outcome/error"), Some(&Value::Null));
    assert_eq!(
        data.pointer(&format!("/outcome/streams/{FAMILY}/status")),
        Some(&Value::from("ok"))
    );
    let pointers: Vec<&str> = data
        .pointer("/outcome/records")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|record| record.get("pointer").and_then(Value::as_str))
        .collect();
    assert_eq!(pointers, [pointer(FAMILY, &news).as_str()]);
    assert_eq!(data.pointer("/outcome/verbatim"), Some(&Value::Bool(true)));

    assert_eq!(
        resolve.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&resolve.stderr)
    );
    let hello = stdout_json(&resolve)?;
    assert_eq!(hello.get("type"), Some(&Value::from("hello")));
    assert!(hello
        .get("ops")
        .and_then(Value::as_array)
        .is_some_and(|ops| ops.contains(&Value::from("resolve"))));
    Ok(())
}

#[test]
fn a_live_resolve_and_a_quote_check_read_the_message_from_the_homeserver() -> Result<()> {
    let server = homeserver()?;
    let said = server.text(
        FAMILY,
        BOB,
        minutes_ago(90),
        "Yes, dinner at seven works for everyone here.",
    )?;
    let root = scratch("cli-resolve")?;
    let tree = tree(&root, &server)?;
    let cited = pointer(FAMILY, &said);
    std::fs::create_dir_all(tree.join("decisions"))?;
    let record = |quote: &str| {
        format!(
            "---\ntype: decision\nstatus: open\ntitle: Dinner time\nquote: \"{quote}\"\nsource: \"{cited}\"\ntier: 1\n---\nDinner is at seven.\n"
        )
    };
    std::fs::write(
        tree.join("decisions/dinner.md"),
        record("dinner at seven works for everyone"),
    )?;
    std::fs::write(
        tree.join("decisions/lunch.md"),
        record("lunch at noon works for everyone"),
    )?;

    let resolved = ctx(&root, &tree, &["--json", "resolve", &cited])?;
    let held = ctx(
        &root,
        &tree,
        &["cite", "verify", "--live", "decisions/dinner.md"],
    )?;
    let broken = ctx(
        &root,
        &tree,
        &["cite", "verify", "--live", "decisions/lunch.md"],
    )?;
    remove(&root)?;

    assert_eq!(
        resolved.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let printed = String::from_utf8_lossy(&resolved.stdout);
    assert!(
        printed.contains("Yes, dinner at seven works for everyone here."),
        "{printed}"
    );
    assert_eq!(
        held.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&held.stdout),
        String::from_utf8_lossy(&held.stderr)
    );
    assert_eq!(
        broken.status.code(),
        Some(1),
        "{}{}",
        String::from_utf8_lossy(&broken.stdout),
        String::from_utf8_lossy(&broken.stderr)
    );
    Ok(())
}

#[test]
fn two_fetches_store_a_room_cursor_and_move_it() -> Result<()> {
    let server = homeserver()?;
    server.text(FAMILY, ALICE, minutes_ago(120), "Family news")?;
    let root = scratch("cli-fetch")?;
    let tree = tree(&root, &server)?;

    let first = ctx(&root, &tree, &["--json", "fetch"])?;
    let after_first = ctx(&root, &tree, &["--json", "cursor", "show"])?;
    let fresh = server.text(FAMILY, BOB, minutes_ago(1), "More news")?;
    let second = ctx(&root, &tree, &["--json", "fetch"])?;
    let after_second = ctx(&root, &tree, &["--json", "cursor", "show"])?;
    let state = std::fs::read(root.join("data/sources/matrix/state.json"));
    remove(&root)?;

    for output in [&first, &second] {
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cursor = |output: &Output| -> Result<String> {
        let shown = stdout_json(output)?;
        let text = shown.to_string();
        let start = text
            .find("$ev")
            .with_context(|| format!("no Matrix cursor in {text}"))?;
        Ok(text
            .get(start..)
            .and_then(|rest| rest.split('@').next())
            .unwrap_or_default()
            .to_owned())
    };
    assert_eq!(cursor(&after_first)?, "$ev0001");
    assert_eq!(cursor(&after_second)?, fresh);
    assert!(
        state.is_ok(),
        "the fetch keeps its state in the data directory"
    );
    Ok(())
}
