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
fn external_command_passes_an_absolute_configured_state_path() -> Result<()> {
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
    let expected = std::env::current_dir()?.join("relative-state/example");
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
