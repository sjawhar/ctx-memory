use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-cli-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

fn fixture_tree() -> Result<PathBuf> {
    let tree = scratch()?;
    std::fs::write(tree.join("ctx.toml"), "version = 1\n")?;
    std::fs::write(tree.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
    std::fs::create_dir(tree.join("decisions"))?;
    std::fs::write(
        tree.join("decisions/open.md"),
        "---\ntype: decision\nstatus: open\ntitle: Example decision\n---\nOpen decision.\n",
    )?;
    Ok(tree)
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn ctx(tree: &Path, arguments: &[&str]) -> Result<Value> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree])
        .args(arguments)
        .output()
        .context("running ctx")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn make_executable(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

fn plugin(bin: &Path, script: &str) -> Result<()> {
    let executable = bin.join("ctx-hello");
    std::fs::write(&executable, script)?;
    make_executable(&executable)
}

#[test]
fn open_works_without_home_on_a_tree_without_sources() -> Result<()> {
    let tree = fixture_tree()?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let output = binary()
        .args(["--memory", tree_text, "--json", "open"])
        .env_clear()
        .env("PATH", path)
        .output()
        .context("running ctx open")?;
    remove_scratch(&tree)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value.get("version"), Some(&serde_json::json!(1)));
    assert_eq!(value.get("command"), Some(&serde_json::json!("open")));
    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[test]
fn rules_prints_the_rules_page() -> Result<()> {
    let tree = fixture_tree()?;
    let value = ctx(&tree, &["--json", "rules"])?;
    remove_scratch(&tree)?;

    assert_eq!(
        value.get("data").and_then(|data| data.get("path")),
        Some(&serde_json::json!("rules.md"))
    );
    assert!(value
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("# Rules")));
    Ok(())
}

#[test]
fn dossier_page_resolves_a_unique_basename() -> Result<()> {
    let tree = fixture_tree()?;
    let entities = tree.join("entities");
    std::fs::create_dir(&entities).with_context(|| format!("creating {}", entities.display()))?;
    std::fs::write(
        entities.join("alice.md"),
        "---\ntitle: Alice\n---\n# Alice\n",
    )
    .with_context(|| format!("writing {}", entities.display()))?;

    let value = ctx(&tree, &["--json", "dossier", "alice"])?;
    remove_scratch(&tree)?;

    assert_eq!(
        value
            .get("data")
            .and_then(|data| data.get("page"))
            .and_then(|page| page.get("path")),
        Some(&serde_json::json!("entities/alice.md"))
    );
    Ok(())
}

#[test]
fn unknown_command_runs_ctx_dash_name_from_path_with_context_env() -> Result<()> {
    let bin = scratch()?;
    let executable = bin.join("ctx-hello");
    std::fs::write(
        &executable,
        "#!/bin/sh\necho \"$CTX_MEMORY|$CTX_JSON|$1\"\n",
    )?;
    make_executable(&executable)?;
    let tree = fixture_tree()?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "--json", "hello", "world"])
        .env("PATH", format!("{}:{path}", bin.display()))
        .output()
        .context("running external ctx command")?;
    remove_scratch(&tree)?;
    remove_scratch(&bin)?;

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{}|1|world", tree.display())
    );
    Ok(())
}

#[test]
fn external_command_rejects_invalid_user_config() -> Result<()> {
    let bin = scratch()?;
    plugin(&bin, "#!/bin/sh\nexit 0\n")?;
    let config_home = scratch()?;
    std::fs::create_dir(config_home.join("ctx"))?;
    std::fs::write(config_home.join("ctx/config.toml"), "memory = 1\n")?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let output = binary()
        .arg("hello")
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .context("running external ctx command with invalid user config")?;
    remove_scratch(&config_home)?;
    remove_scratch(&bin)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("config.toml"));
    Ok(())
}

#[test]
fn external_command_removes_inherited_json_without_the_flag() -> Result<()> {
    let bin = scratch()?;
    plugin(&bin, "#!/bin/sh\nprintf '%s\\n' \"${CTX_JSON-unset}\"\n")?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let output = binary()
        .arg("hello")
        .env_clear()
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("CTX_JSON", "1")
        .output()
        .context("running external ctx command with inherited JSON")?;
    remove_scratch(&bin)?;

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "unset");
    Ok(())
}

#[test]
fn external_command_removes_inherited_state_without_a_tree() -> Result<()> {
    let bin = scratch()?;
    plugin(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"${CTX_STATE_DIR-unset}\"\n",
    )?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let output = binary()
        .arg("hello")
        .env_clear()
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("CTX_STATE_DIR", "/base")
        .output()
        .context("running external ctx command with inherited state")?;
    remove_scratch(&bin)?;

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "unset");
    Ok(())
}

#[test]
fn external_command_passes_an_absolute_configured_state_base() -> Result<()> {
    let bin = scratch()?;
    plugin(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"${CTX_STATE_DIR-unset}\"\n",
    )?;
    let tree = fixture_tree()?;
    std::fs::write(tree.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
    let config_home = scratch()?;
    std::fs::create_dir(config_home.join("ctx"))?;
    std::fs::write(
        config_home.join("ctx/config.toml"),
        "state_dir = \"relative-state\"\n",
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let path = std::env::var("PATH").context("PATH is not set")?;
    let output = binary()
        .args(["--memory", tree_text, "hello"])
        .env_clear()
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .context("running external ctx command with relative state")?;
    let expected = std::env::current_dir()?.join("relative-state");
    remove_scratch(&config_home)?;
    remove_scratch(&tree)?;
    remove_scratch(&bin)?;

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        expected
            .to_str()
            .context("expected path is not valid UTF-8")?
    );
    Ok(())
}

#[test]
fn unknown_command_with_no_executable_exits_two() -> Result<()> {
    let output = binary()
        .arg("nonesuch")
        .output()
        .context("running unknown ctx command")?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ctx-nonesuch"));
    Ok(())
}

#[test]
fn json_errors_use_the_error_envelope() -> Result<()> {
    let output = binary()
        .args(["--memory", "/nonexistent", "--json", "sweep"])
        .output()
        .context("running failing ctx sweep")?;

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(value
        .get("error")
        .and_then(|error| error.get("message"))
        .is_some_and(Value::is_string));
    Ok(())
}

/// The writer refuses a record whose quote nothing could check, and `cite verify` is the
/// check a writer runs first. It used to pass such a record with exit 0, so a cycle that
/// read the exit code took it for a checked one.
#[test]
fn cite_verify_fails_on_a_quote_nothing_could_check() -> Result<()> {
    let tree = fixture_tree()?;
    let state = scratch()?;
    // A tree resolves its state directory by its id.
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"cite-unchecked\"\n",
    )?;
    std::fs::write(
        tree.join("decisions/overheard.md"),
        "---\ntype: decision\nstatus: open\ntitle: Overheard\ntier: 1\nquote: \"invented words\"\nsource: \"overheard in standup\"\n---\nA claim nothing checked.\n",
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "--json", "cite", "verify"])
        .env("CTX_STATE_DIR", &state)
        .output()
        .context("running ctx cite verify")?;
    let help = binary()
        .args(["cite", "verify", "--help"])
        .output()
        .context("running ctx cite verify --help")?;
    remove_scratch(&tree)?;
    remove_scratch(&state)?;

    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        value
            .pointer("/data/unchecked")
            .and_then(Value::as_array)
            .is_some_and(|unchecked| !unchecked.is_empty()),
        "the quote was not reported unchecked: {value}"
    );
    assert_eq!(output.status.code(), Some(1), "{value}");
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("nothing could check"),
        "the help does not say an unchecked quote fails: {}",
        String::from_utf8_lossy(&help.stdout)
    );
    Ok(())
}

#[test]
fn rules_without_a_rules_page_exits_two() -> Result<()> {
    let tree = scratch()?;
    std::fs::write(tree.join("ctx.toml"), "version = 1\n")?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "rules"])
        .output()
        .context("running ctx rules")?;
    remove_scratch(&tree)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rules.md"));
    Ok(())
}

/// Every control character in `text` but a tab or a newline.
fn controls(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect()
}

/// The rules page is the tree's text: `ctx rules` prints it without the controls that would
/// drive a terminal (an `OSC` title ended by `BEL`, a C1 `CSI`), and what followed each stays.
#[test]
fn rules_are_printed_without_control_characters() -> Result<()> {
    let tree = scratch()?;
    std::fs::write(tree.join("ctx.toml"), "version = 1\n")?;
    std::fs::write(
        tree.join("rules.md"),
        "---\ntype: rules\n---\n# Rules\n\nCite \u{1b}]0;owned\u{7}every \u{9b}31mclaim.\n",
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "rules"])
        .output()
        .context("running ctx rules");
    remove_scratch(&tree)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("Cite ]0;ownedevery 31mclaim."),
        "the rules text is missing: {stdout:?}"
    );
    assert_eq!(controls(&stdout), Vec::<char>::new(), "{stdout:?}");
    Ok(())
}

/// A cursor is what a source answered, and `ctx cursor show` prints it without its controls.
#[test]
fn cursors_are_printed_without_control_characters() -> Result<()> {
    let tree = fixture_tree()?;
    let data = tree.join("data");
    std::fs::create_dir(&data)?;
    std::fs::write(
        data.join("cursors.json"),
        "{\"version\":1,\"sources\":{\"echo\":{\"C0EXAMPLE01\":{\"cursor\":\"1700000000.1\\u001b]0;owned\\u0007\\u009b2J\",\"window\":\"20261025T120000Z-0000\"}}}}",
    )?;
    let tree_text = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text, "cursor", "show"])
        .env("CTX_DATA_DIR", &data)
        .env("CTX_STATE_DIR", tree.join("state"))
        .output()
        .context("running ctx cursor show");
    remove_scratch(&tree)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("echo C0EXAMPLE01 1700000000.1]0;owned2J 20261025T120000Z-0000"),
        "the cursor is missing: {stdout:?}"
    );
    assert_eq!(controls(&stdout), Vec::<char>::new(), "{stdout:?}");
    Ok(())
}

/// Runs `ctx` on a tree, whatever its exit code, with no server configured.
fn ctx_output(tree: &Path, arguments: &[&str]) -> Result<std::process::Output> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    binary()
        .args(["--memory", tree])
        .args(arguments)
        .env_remove("CTX_SERVER")
        .output()
        .context("running ctx")
}

/// An update to a record starts from what the record holds now, so `ctx record show` prints it
/// whole, frontmatter and body, as the tree holds it, and names it in the tree's own spelling.
#[test]
fn record_show_prints_the_whole_record() -> Result<()> {
    let tree = fixture_tree()?;
    let text = ctx_output(&tree, &["record", "show", "decisions/open.md"]);
    let json = ctx_output(&tree, &["--json", "record", "show", "decisions//open.md"]);
    remove_scratch(&tree)?;
    let (text, json) = (text?, json?);

    let record =
        "---\ntype: decision\nstatus: open\ntitle: Example decision\n---\nOpen decision.\n";
    assert_eq!(
        text.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&text.stdout), record);
    assert_eq!(
        json.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(
        value.get("command"),
        Some(&serde_json::json!("record show"))
    );
    assert_eq!(
        value.pointer("/data/path"),
        Some(&serde_json::json!("decisions/open.md"))
    );
    assert_eq!(
        value.pointer("/data/text"),
        Some(&serde_json::json!(record))
    );
    Ok(())
}

/// A path that names no record of the tree is an error, never empty text: a cycle that read
/// nothing must not take it for an empty record.
#[test]
fn record_show_refuses_a_path_that_names_no_record() -> Result<()> {
    let tree = fixture_tree()?;
    let missing = ctx_output(&tree, &["record", "show", "decisions/missing.md"]);
    let not_markdown = ctx_output(&tree, &["record", "show", "ctx.toml"]);
    remove_scratch(&tree)?;

    for (path, output) in [
        ("decisions/missing.md", missing?),
        ("ctx.toml", not_markdown?),
    ] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{path}: {stderr}");
        assert!(output.stdout.is_empty(), "{path}: {:?}", output.stdout);
        assert!(
            stderr.contains(path),
            "the refusal does not name {path}: {stderr}"
        );
    }
    Ok(())
}

/// `ctx record show` reads the tree and nothing else: a path that climbs out of it, an absolute
/// path, and a symbolic link in the tree that points out of it are each refused, and the file
/// outside is never printed.
#[test]
fn record_show_refuses_a_path_outside_the_tree() -> Result<()> {
    let tree = fixture_tree()?;
    let outside = scratch()?;
    let outcome = (|| -> Result<Vec<(String, std::process::Output)>> {
        let secret = outside.join("secret.md");
        std::fs::write(&secret, "---\ntype: decision\n---\nOUTSIDE THE TREE\n")?;
        std::os::unix::fs::symlink(&secret, tree.join("decisions/linked.md"))?;
        let name = outside
            .file_name()
            .and_then(|name| name.to_str())
            .context("scratch name is not valid UTF-8")?;
        let climbing = format!("../{name}/secret.md");
        let absolute = secret
            .to_str()
            .context("scratch path is not valid UTF-8")?
            .to_owned();
        let mut outputs = Vec::new();
        for path in [climbing, absolute, "decisions/linked.md".to_owned()] {
            let output = ctx_output(&tree, &["record", "show", &path])?;
            outputs.push((path, output));
        }
        Ok(outputs)
    })();
    remove_scratch(&tree)?;
    remove_scratch(&outside)?;

    for (path, output) in outcome? {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{path}: {stdout}{stderr}");
        assert!(
            !stdout.contains("OUTSIDE") && !stderr.contains("OUTSIDE"),
            "{path} printed the file outside the tree: {stdout}{stderr}"
        );
        assert!(
            stderr.contains(&path),
            "the refusal does not name {path}: {stderr}"
        );
    }
    Ok(())
}
