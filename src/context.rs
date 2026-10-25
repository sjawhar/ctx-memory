//! Loaded command context shared by built-in commands and external plugins.

use std::cell::OnceCell;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::cli::GlobalArgs;
use crate::config::{locate_tree, state_base, StateDir, TreeConfig, UserConfig};
use crate::tree::Tree;

pub struct Context {
    pub config: TreeConfig,
    pub tree: Tree,
    pub json: bool,
    user: UserConfig,
    state: OnceCell<StateDir>,
}

impl Context {
    /// Loads the selected tree and configuration for a built-in command.
    ///
    /// # Errors
    ///
    /// Returns an error when user configuration, tree location, tree configuration, or the tree
    /// itself cannot be loaded.
    pub fn load(global: &GlobalArgs) -> Result<Self> {
        let user = UserConfig::load()?;
        let cwd = std::env::current_dir().context("reading the current directory")?;
        let root = locate_tree(global.memory.as_deref(), &cwd, &user)?;
        Self::load_at(&root, global.json, user, None)
    }

    fn load_at(
        root: &std::path::Path,
        json: bool,
        user: UserConfig,
        initial_state: Option<StateDir>,
    ) -> Result<Self> {
        let config = TreeConfig::load(root)?;
        let tree = Tree::load(root, &config)?;
        let state = OnceCell::new();
        if let Some(initial_state) = initial_state {
            state
                .set(initial_state)
                .map_err(|_| anyhow!("plugin state directory was not retained"))?;
        }
        Ok(Self {
            config,
            tree,
            json,
            user,
            state,
        })
    }

    /// Resolves the state directory for this tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree has no identifier or no state base can be resolved.
    pub fn state(&self) -> Result<&StateDir> {
        if let Some(state) = self.state.get() {
            return Ok(state);
        }
        let id = self
            .config
            .id
            .as_deref()
            .ok_or_else(|| anyhow!("ctx.toml id is required to resolve state"))?;
        let state = StateDir::new(state_base(&self.user)?.join(id));
        self.state
            .set(state)
            .map_err(|_| anyhow!("state directory was initialized concurrently"))?;
        self.state
            .get()
            .ok_or_else(|| anyhow!("state directory was not retained"))
    }

    /// Loads a context passed from a `ctx` external subcommand invocation.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent did not supply a tree or uses an incompatible version.
    pub fn from_plugin_env() -> Result<Self> {
        let root = std::env::var_os("CTX_MEMORY")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("CTX_MEMORY is not set"))?;
        validate_parent_version()?;
        let user = UserConfig::load()?;
        let json = std::env::var_os("CTX_JSON").is_some_and(|value| value == "1");
        let state = std::env::var_os("CTX_STATE_DIR")
            .map(PathBuf::from)
            .map(StateDir::new);
        Self::load_at(&root, json, user, state)
    }
}

fn validate_parent_version() -> Result<()> {
    let parent = std::env::var("CTX_VERSION").context("CTX_VERSION is not set")?;
    let expected = env!("CARGO_PKG_VERSION");
    if major_minor(&parent)? != major_minor(expected)? {
        bail!(
            "CTX_VERSION {parent} is incompatible with ctx-memory {expected}; major.minor must match"
        );
    }
    Ok(())
}

fn major_minor(version: &str) -> Result<(&str, &str)> {
    let mut components = version.split('.');
    let major = components
        .next()
        .filter(|component| !component.is_empty())
        .ok_or_else(|| anyhow!("CTX_VERSION must include major.minor"))?;
    let minor = components
        .next()
        .filter(|component| !component.is_empty())
        .ok_or_else(|| anyhow!("CTX_VERSION must include major.minor"))?;
    Ok((major, minor))
}

#[cfg(test)]
mod tests {
    use std::cell::OnceCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context as _, Result};

    use crate::config::{StateDir, TreeConfig, UserConfig};
    use crate::tree::Tree;

    use super::Context;

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn scratch() -> Result<PathBuf> {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-context-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(root)
    }

    #[test]
    fn state_requires_a_tree_identifier() -> Result<()> {
        let root = scratch()?;
        let config = TreeConfig::default();
        let tree = Tree::load(&root, &config)?;
        let context = Context {
            config,
            tree,
            json: false,
            user: UserConfig::default(),
            state: OnceCell::new(),
        };

        let result = context.state();
        std::fs::remove_dir_all(&root)?;

        let error = result
            .err()
            .context("state resolved without a tree identifier")?;
        assert!(format!("{error:#}").contains("ctx.toml"));
        Ok(())
    }

    #[test]
    fn supplied_plugin_state_is_returned_without_appending_the_tree_identifier() -> Result<()> {
        let root = scratch()?;
        let state_path = PathBuf::from("/var/state/ctx/example");
        let context = Context::load_at(
            &root,
            false,
            UserConfig::default(),
            Some(StateDir::new(state_path.clone())),
        )?;

        assert_eq!(context.state()?.root(), state_path);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}
