//! Loaded command context shared by built-in commands and external plugins.

use std::cell::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::cli::GlobalArgs;
use crate::config::{data_dir, locate_tree, state_base, DataDir, StateDir, TreeConfig, UserConfig};
use crate::embed::{CachedEmbedder, Embedder, Embedding};
use crate::index::Index;
use crate::tree::Tree;

pub struct Context {
    pub config: TreeConfig,
    pub tree: Tree,
    pub json: bool,
    user: UserConfig,
    state: OnceCell<StateDir>,
    data: OnceCell<DataDir>,
    index: OnceCell<Index>,
    embedding: OnceCell<Embedding>,
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

    /// Loads a tree from an explicit root, as `ctx serve` does for the clone it answers from.
    ///
    /// # Errors
    ///
    /// Returns an error when user configuration, tree configuration, or the tree cannot be
    /// loaded.
    pub fn open(root: &std::path::Path) -> Result<Self> {
        let user = UserConfig::load()?;
        Self::load_at(root, false, user, None)
    }

    fn load_at(
        root: &std::path::Path,
        json: bool,
        user: UserConfig,
        initial_state_base: Option<PathBuf>,
    ) -> Result<Self> {
        let config = TreeConfig::load(root)?;
        let tree = Tree::load(root, &config)?;
        let state = OnceCell::new();
        if let Some(initial_state_base) = initial_state_base {
            let id = config
                .id
                .as_deref()
                .ok_or_else(|| anyhow!("ctx.toml id is required to resolve plugin state"))?;
            state
                .set(StateDir::new(initial_state_base.join(id)))
                .map_err(|_| anyhow!("plugin state directory was not retained"))?;
        }
        Ok(Self {
            config,
            tree,
            json,
            user,
            state,
            data: OnceCell::new(),
            index: OnceCell::new(),
            embedding: OnceCell::new(),
        })
    }

    /// Returns this tree's full-text index, building it on first use.
    ///
    /// One process keeps one index: `ctx serve` answers every search from it.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be built.
    pub fn index(&self) -> Result<&Index> {
        if let Some(index) = self.index.get() {
            return Ok(index);
        }
        let index = Index::build(&self.tree, self.embedding()?)?;
        self.index
            .set(index)
            .map_err(|_| anyhow!("index was initialized concurrently"))?;
        self.index
            .get()
            .ok_or_else(|| anyhow!("index was not retained"))
    }

    /// Returns this tree's embedder for meaning search, building it on first use.
    ///
    /// Meaning search is on when `CTX_EMBED_MODEL`, `GOOGLE_CLOUD_PROJECT` and
    /// `GOOGLE_CLOUD_LOCATION` are all set; otherwise `ctx` matches by text alone. The vectors
    /// are cached under the data directory, so one process embeds each record once.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be resolved.
    pub fn embedding(&self) -> Result<&Embedding> {
        if let Some(embedding) = self.embedding.get() {
            return Ok(embedding);
        }
        let resolved = self.resolve_embedding()?;
        self.embedding
            .set(resolved)
            .map_err(|_| anyhow!("embedding was initialized concurrently"))?;
        self.embedding
            .get()
            .ok_or_else(|| anyhow!("embedding was not retained"))
    }

    fn resolve_embedding(&self) -> Result<Embedding> {
        let Some(model) = std::env::var_os("CTX_EMBED_MODEL") else {
            return Ok(Embedding::Off);
        };
        let model = model
            .to_str()
            .ok_or_else(|| anyhow!("CTX_EMBED_MODEL is not valid UTF-8"))?;
        if model.is_empty() {
            return Ok(Embedding::Off);
        }
        // A configured model with no project or region is a misconfiguration, not meaning
        // search; `ctx` keeps matching by text rather than refusing to answer.
        let Ok(vertex) = Embedder::vertex(model) else {
            return Ok(Embedding::Off);
        };
        let cache = self.data()?.embeddings();
        let embedder = CachedEmbedder::new(Arc::new(vertex), cache);
        Ok(Embedding::Ready(Arc::new(embedder)))
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

    /// Resolves the directory holding this tree's fetch windows and cursors.
    ///
    /// It is the configured `data_dir` when one is set, and otherwise the state directory, so a
    /// tree with no configuration keeps every file in one place.
    ///
    /// # Errors
    ///
    /// Returns an error when no `data_dir` is configured and the state directory cannot be
    /// resolved.
    pub fn data(&self) -> Result<&DataDir> {
        if let Some(data) = self.data.get() {
            return Ok(data);
        }
        let root = match data_dir(&self.config) {
            Some(root) => root,
            None => self.state()?.root().to_path_buf(),
        };
        self.data
            .set(DataDir::new(root))
            .map_err(|_| anyhow!("data directory was initialized concurrently"))?;
        self.data
            .get()
            .ok_or_else(|| anyhow!("data directory was not retained"))
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
        let state_base = std::env::var_os("CTX_STATE_DIR").map(PathBuf::from);
        Self::load_at(&root, json, user, state_base)
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
    use std::sync::{LazyLock, Mutex};

    use anyhow::{anyhow, Context as _, Result};

    use crate::config::{TreeConfig, UserConfig};
    use crate::tree::Tree;

    use super::Context;

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
            data: OnceCell::new(),
            index: OnceCell::new(),
            embedding: OnceCell::new(),
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
    fn supplied_plugin_state_base_is_appended_with_the_tree_identifier() -> Result<()> {
        let root = scratch()?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
        let state_base = PathBuf::from("/var/state/ctx");
        let context = Context::load_at(
            &root,
            false,
            UserConfig::default(),
            Some(state_base.clone()),
        )?;

        assert_eq!(context.state()?.root(), state_base.join("example"));
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn plugin_environment_appends_the_tree_identifier_to_the_state_base() -> Result<()> {
        let _environment = ENVIRONMENT_LOCK
            .lock()
            .map_err(|error| anyhow!("locking the test environment: {error}"))?;
        let root = scratch()?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
        let state_base = root.join("state-base");
        let config_home = root.join("config-home");
        let previous_memory = std::env::var_os("CTX_MEMORY");
        let previous_state = std::env::var_os("CTX_STATE_DIR");
        let previous_version = std::env::var_os("CTX_VERSION");
        let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("CTX_MEMORY", &root);
        std::env::set_var("CTX_STATE_DIR", &state_base);
        std::env::set_var("CTX_VERSION", env!("CARGO_PKG_VERSION"));
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        let state = Context::from_plugin_env()
            .and_then(|context| context.state().map(|state| state.root().to_path_buf()));

        restore_environment("CTX_MEMORY", previous_memory);
        restore_environment("CTX_STATE_DIR", previous_state);
        restore_environment("CTX_VERSION", previous_version);
        restore_environment("XDG_CONFIG_HOME", previous_config_home);
        std::fs::remove_dir_all(&root)?;

        assert_eq!(state?, state_base.join("example"));
        Ok(())
    }

    fn restore_environment(name: &str, value: Option<std::ffi::OsString>) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
