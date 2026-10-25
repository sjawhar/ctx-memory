use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::{record::Record, store::Store};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn fixture_path() -> Result<PathBuf> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let inherited = std::env::var("PATH").context("PATH is not set")?;
    Ok(PathBuf::from(format!("{}:{inherited}", fixtures.display())))
}

fn tree(root: &Path, document: Option<&str>) -> Result<PathBuf> {
    let tree = root.join("tree");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    if let Some(document) = document {
        std::fs::write(tree.join("decision.md"), document)?;
    }
    Ok(tree)
}

fn clear_echo_environment(command: &mut std::process::Command) {
    for name in [
        "ECHO_FAIL_STREAM",
        "ECHO_BAD_SCHEME",
        "ECHO_NO_END",
        "ECHO_LATE",
        "ECHO_NO_HELLO",
        "ECHO_SLEEP",
        "ECHO_REQUEST_OUT",
        "ECHO_EXIT",
    ] {
        command.env_remove(name);
    }
}

#[test]
fn live_resolve_source_failure_exits_one_and_is_per_pointer() -> Result<()> {
    let root = scratch("resolve-source-error")?;
    let tree = tree(&root, None)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--json", "--memory", tree_text, "resolve", "echo:x"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?)
        .env("ECHO_EXIT", "1");
    clear_echo_environment(&mut command);
    command.env("ECHO_EXIT", "1");
    let output = command.output().context("running failed live resolve")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let error = value
        .get("data")
        .and_then(|data| data.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("error"))
        .and_then(Value::as_str)
        .context("resolve response reports a source error")?;
    assert!(error.contains("exit 1"));
    Ok(())
}

#[test]
fn live_cite_source_failure_exits_one_and_is_reported_per_pointer() -> Result<()> {
    let root = scratch("cite-source-error")?;
    let tree = tree(
        &root,
        Some("---\ntype: decision\nquote: \"echo\"\n---\necho:x\n"),
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = binary();
    command
        .args(["--json", "--memory", tree_text, "cite", "verify", "--live"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("PATH", fixture_path()?)
        .env("ECHO_EXIT", "1");
    clear_echo_environment(&mut command);
    command.env("ECHO_EXIT", "1");
    let output = command
        .output()
        .context("running failed live cite verification")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let why = value
        .get("data")
        .and_then(|data| data.get("broken"))
        .and_then(Value::as_array)
        .and_then(|broken| broken.first())
        .and_then(|entry| entry.get("why"))
        .and_then(Value::as_str)
        .context("citation response reports a source error")?;
    assert!(why.contains("exit 1"));
    Ok(())
}

#[test]
fn store_only_verify_does_not_assume_external_builtin_named_schemes_are_verbatim() -> Result<()> {
    let root = scratch("external-slack-scheme")?;
    let tree = root.join("tree");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"slack\"\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nquote: \"stored text\"\n---\nslack:C0EXAMPLE01/1700000000.000001\n",
    )?;
    let timestamp = DateTime::parse_from_rfc3339("2023-11-14T22:13:20Z")?.with_timezone(&Utc);
    let store = Store::open(&root.join("state/example/store.sqlite3"))?;
    store.put_all(
        "echo",
        &[Record {
            pointer: "slack:C0EXAMPLE01/1700000000.000001".to_owned(),
            stream: "C0EXAMPLE01".to_owned(),
            ts: timestamp,
            label: None,
            author: None,
            author_id: None,
            thread: None,
            title: None,
            url: None,
            text: "stored text".to_owned(),
            aliases: Vec::new(),
            context: false,
            raw: Value::Null,
        }],
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--json", "--memory", tree_text, "cite", "verify"])
        .env("CTX_STATE_DIR", root.join("state"))
        .output()
        .context("running store-only citation verification")?;
    remove_scratch(&root)?;

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let data = value
        .get("data")
        .context("citation verification has data")?;
    assert_eq!(data.get("checked").and_then(Value::as_u64), Some(0));
    assert_eq!(
        data.get("skipped")
            .and_then(|skipped| skipped.get("slack"))
            .and_then(Value::as_u64),
        Some(1)
    );
    Ok(())
}
