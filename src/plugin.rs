//! External subcommands: `ctx <name>` runs a `ctx-<name>` executable from `PATH`, handing it
//! the tree, the state directory and the output format `ctx` itself resolved.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::cli::GlobalArgs;
use crate::config::{locate_tree, state_base, TreeConfig, UserConfig};

/// What an external subcommand is told about the tree it runs against.
pub struct PluginEnv {
    pub tree_root: Option<PathBuf>,
    pub state_base: Option<PathBuf>,
    pub json: bool,
}

impl PluginEnv {
    /// Resolves the tree and state directory the way a built-in command would, without
    /// failing when there is none: a plugin may not need a tree at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the user configuration cannot be read.
    pub fn resolve(global: &GlobalArgs) -> Result<Self> {
        let user = UserConfig::load()?;
        let tree_root = resolve_tree(global, &user);
        let state_base = tree_root.as_deref().and_then(|root| {
            let config = TreeConfig::load(root).ok()?;
            config.id?;
            absolute_path(state_base(&user).ok()?).ok()
        });
        Ok(Self {
            tree_root,
            state_base,
            json: global.json,
        })
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("reading the current directory for plugin state")?
        .join(path))
}

fn resolve_tree(global: &GlobalArgs, user: &UserConfig) -> Option<PathBuf> {
    if let Some(root) = global.memory.as_deref() {
        return root.canonicalize().ok();
    }
    let cwd = std::env::current_dir().ok()?;
    locate_tree(None, &cwd, user)
        .ok()
        .and_then(|root| root.canonicalize().ok())
}

/// Executes a `ctx-<name>` command from `PATH` with the resolved ctx environment.
///
/// # Errors
///
/// Returns an error when no command name is supplied, when the name is not a command name,
/// or when the executable cannot be started.
pub fn run_external(arguments: &[OsString], environment: &PluginEnv) -> Result<Infallible> {
    let (name, arguments) = arguments
        .split_first()
        .ok_or_else(|| anyhow!("external command name is missing"))?;
    let name = name
        .to_str()
        .ok_or_else(|| anyhow!("external command name is not valid UTF-8"))?;
    // `Command::new` reads a program name holding a `/` as a path, so `a/../../bin/sh` would
    // run `/bin/sh` from any directory holding a `ctx-a`. A name is checked before anything
    // is looked up: only a name that can be nothing but a file on `PATH` gets that far.
    if !is_command_name(name) {
        bail!(
            "`{name}` is not a command name: an external command's name holds only ASCII \
             letters, digits, `-` and `_`"
        );
    }
    let executable = format!("ctx-{name}");
    let mut command = Command::new(&executable);
    command
        .env_remove("CTX_MEMORY")
        .env_remove("CTX_STATE_DIR")
        .env_remove("CTX_JSON");
    command.args(arguments);
    if let Some(root) = &environment.tree_root {
        command.env("CTX_MEMORY", root);
    }
    if let Some(state_base) = &environment.state_base {
        command.env("CTX_STATE_DIR", state_base);
    }
    if environment.json {
        command.env("CTX_JSON", "1");
    }
    command.env("CTX_VERSION", env!("CARGO_PKG_VERSION"));
    command.env(
        "CTX_BIN",
        std::env::current_exe().context("locating the ctx executable")?,
    );

    let error = command.exec();
    if error.kind() == std::io::ErrorKind::NotFound {
        bail!("unknown command `{name}` (no {executable} on PATH)");
    }
    Err(error).with_context(|| format!("running {executable}"))
}

/// Whether `name` can only be a file name on `PATH`: one or more ASCII letters, digits, `-`
/// and `_`, so no `/`, no `.` and nothing a shell would read twice.
fn is_command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::is_command_name;

    #[test]
    fn a_command_name_is_a_bare_word() {
        for name in ["hello", "source-linear", "a_b", "x9"] {
            assert!(is_command_name(name), "{name} is a command name");
        }
        for name in [
            "",
            "a/../../bin/sh",
            "/bin/sh",
            "..",
            ".",
            "a.b",
            "a b",
            "é",
            "a\0",
        ] {
            assert!(!is_command_name(name), "{name:?} is not a command name");
        }
    }
}
