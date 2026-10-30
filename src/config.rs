//! Tree configuration, user configuration, and state-directory resolution.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;

pub const TREE_CONFIG_FILE: &str = "ctx.toml";
pub const RULES_FILE: &str = "rules.md";

pub type TreeId = String;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    pub version: u32,
    pub id: Option<TreeId>,
    /// Directory holding the files shared between machines; see `data_dir`.
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub folder_notes: Vec<String>,
    #[serde(default)]
    pub dossier: DossierConfig,
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    /// Where a credential comes from on a machine without Secret Manager.
    #[serde(default)]
    pub credentials: crate::credentials::Config,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            version: 1,
            id: None,
            data_dir: None,
            folder_notes: Vec::new(),
            dossier: DossierConfig::default(),
            ingest: IngestConfig::default(),
            sources: Vec::new(),
            credentials: crate::credentials::Config::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DossierConfig {
    #[serde(default)]
    pub vitals: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestConfig {
    #[serde(default = "default_backfill_hours")]
    pub backfill_hours: u32,
    /// Pointer patterns of the messages this tree never ingests ([`crate::exclude`]).
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            backfill_hours: default_backfill_hours(),
            exclude: Vec::new(),
        }
    }
}

const fn default_backfill_hours() -> u32 {
    24
}

#[derive(Debug, Deserialize)]
pub struct SourceConfig {
    pub id: String,
    pub kind: String,
    pub scheme: Option<String>,
    pub discover_field: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(flatten)]
    pub settings: toml::Table,
}

/// Returns the pointer scheme assigned to a built-in source kind.
#[must_use]
pub fn default_scheme(kind: &str) -> Option<&'static str> {
    match kind {
        "slack" => Some("slack"),
        "gmail" => Some("gmail"),
        "drive" => Some("drive"),
        "calendar" => Some("gcal"),
        "github" => Some("github"),
        _ => None,
    }
}

impl TreeConfig {
    /// Loads the optional configuration at the root of a tree.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing configuration cannot be read, parsed, or validated.
    pub fn load(tree_root: &Path) -> Result<Self> {
        let path = tree_root.join(TREE_CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("ctx.toml version must be 1, got {}", self.version);
        }
        if let Some(id) = &self.id {
            validate_tree_id(id)?;
        }
        if !self.sources.is_empty() && self.id.is_none() {
            bail!("ctx.toml id is required when sources are configured");
        }

        let mut source_ids = HashSet::new();
        let mut source_schemes = HashMap::new();
        for source in &self.sources {
            validate_source_id(&source.id)?;
            if source.kind.is_empty() {
                bail!("source {} kind must not be empty", source.id);
            }
            let scheme = source
                .scheme
                .as_deref()
                .or_else(|| default_scheme(&source.kind))
                .ok_or_else(|| {
                    anyhow!(
                        "source {} kind {} requires an explicit scheme",
                        source.id,
                        source.kind
                    )
                })?;
            if !source_ids.insert(source.id.as_str()) {
                bail!("source id {} appears more than once", source.id);
            }
            if let Some(other_id) = source_schemes.insert(scheme, source.id.as_str()) {
                bail!(
                    "sources {other_id} and {} share pointer scheme {scheme}",
                    source.id
                );
            }
        }
        crate::exclude::Exclusion::new(&self.ingest.exclude).context("ingest.exclude")?;
        Ok(())
    }
}

fn validate_tree_id(id: &str) -> Result<()> {
    let mut characters = id.bytes();
    let Some(first) = characters.next() else {
        return Err(anyhow!("ctx.toml id must not be empty"));
    };
    if id.len() > 63
        || !first.is_ascii_lowercase() && !first.is_ascii_digit()
        || !characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'-'
        })
    {
        bail!("ctx.toml id must match [a-z0-9][a-z0-9-]{{0,62}}");
    }
    Ok(())
}

fn validate_source_id(id: &str) -> Result<()> {
    let mut characters = id.bytes();
    let Some(first) = characters.next() else {
        bail!("source id {id:?} must match [a-z0-9][a-z0-9_-]{{0,62}}");
    };
    if id.len() > 63
        || !first.is_ascii_lowercase() && !first.is_ascii_digit()
        || !characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == b'-'
                || character == b'_'
        })
    {
        bail!("source id {id:?} must match [a-z0-9][a-z0-9_-]{{0,62}}");
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub memory: Option<String>,
    pub state_dir: Option<PathBuf>,
    /// The ctx server this machine reads and writes through, when it has one.
    pub server: Option<String>,
}

impl UserConfig {
    /// Loads configuration from the conventional XDG user configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing configuration cannot be read or parsed.
    pub fn load() -> Result<Self> {
        user_config_path().map_or_else(|| Ok(Self::default()), |path| Self::load_from(&path))
    }

    /// Loads a specific user configuration file. A missing file has no configured values.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing configuration cannot be read or parsed.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|directory| directory.join("ctx").join("config.toml"))
}

/// Locates a tree from command-line configuration, the current directory, or user configuration.
///
/// # Errors
///
/// Returns an error when no configured tree can be located.
pub fn locate_tree(cli: Option<&Path>, cwd: &Path, user: &UserConfig) -> Result<PathBuf> {
    if let Some(root) = cli {
        return Ok(root.to_path_buf());
    }
    if let Some(root) = cwd
        .ancestors()
        .find(|ancestor| ancestor.join(TREE_CONFIG_FILE).is_file())
    {
        return Ok(root.to_path_buf());
    }
    if let Some(memory) = &user.memory {
        return Ok(expand_home(memory));
    }
    bail!(
        "could not locate a tree; provide --memory (or CTX_MEMORY), run from an ancestor containing ctx.toml, or set memory in user config"
    )
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Resolves the base directory that holds per-tree state.
///
/// # Errors
///
/// Returns an error when no state-directory location is available.
pub fn state_base(user: &UserConfig) -> Result<PathBuf> {
    let state_dir = std::env::var("CTX_STATE_DIR").ok();
    let xdg_state_home = std::env::var("XDG_STATE_HOME").ok();
    let home = std::env::var("HOME").ok();
    let mut environment = Vec::new();
    if let Some(value) = state_dir.as_deref() {
        environment.push(("CTX_STATE_DIR", value));
    }
    if let Some(value) = xdg_state_home.as_deref() {
        environment.push(("XDG_STATE_HOME", value));
    }
    if let Some(value) = home.as_deref() {
        environment.push(("HOME", value));
    }
    state_base_from(&environment, user)
}

/// Pure state-base resolution over explicit environment values.
///
/// # Errors
///
/// Returns an error when no state-directory location is available.
pub fn state_base_from(environment: &[(&str, &str)], user: &UserConfig) -> Result<PathBuf> {
    if let Some(path) = environment
        .iter()
        .find_map(|(name, value)| (*name == "CTX_STATE_DIR").then_some(*value))
    {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = &user.state_dir {
        return Ok(path.clone());
    }
    if let Some(path) = environment
        .iter()
        .find_map(|(name, value)| (*name == "XDG_STATE_HOME").then_some(*value))
    {
        return Ok(PathBuf::from(path).join("ctx"));
    }
    if let Some(path) = environment
        .iter()
        .find_map(|(name, value)| (*name == "HOME").then_some(*value))
    {
        return Ok(PathBuf::from(path).join(".local").join("state").join("ctx"));
    }
    bail!(
        "cannot resolve a state directory; set CTX_STATE_DIR, state_dir in user config, XDG_STATE_HOME, or HOME"
    )
}

#[derive(Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn store(&self) -> PathBuf {
        self.root.join("store.sqlite3")
    }

    #[must_use]
    pub fn fetch_lock(&self) -> PathBuf {
        self.root.join("fetch.lock")
    }

    /// The records live resolution obtained on this machine, which a rebuild replays.
    #[must_use]
    pub fn resolved(&self) -> PathBuf {
        self.root.join("resolved")
    }

    #[must_use]
    pub fn source_state(&self, source_id: &str) -> PathBuf {
        self.root.join("sources").join(source_id)
    }
}

/// Returns the configured directory for the files shared between machines.
///
/// `CTX_DATA_DIR` wins over `data_dir` in `ctx.toml`. When neither is set the caller keeps its
/// own default, which is the tree's state directory.
#[must_use]
pub fn data_dir(config: &TreeConfig) -> Option<PathBuf> {
    data_dir_from(std::env::var_os("CTX_DATA_DIR").map(PathBuf::from), config)
}

/// Pure data-directory resolution over an explicit `CTX_DATA_DIR` value.
#[must_use]
pub fn data_dir_from(environment: Option<PathBuf>, config: &TreeConfig) -> Option<PathBuf> {
    environment.or_else(|| config.data_dir.clone())
}

/// The directory holding fetch windows, cursors, and the other files shared between machines.
#[derive(Debug)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn windows(&self) -> PathBuf {
        self.root.join("windows")
    }

    #[must_use]
    pub fn cursors(&self) -> PathBuf {
        self.root.join("cursors.json")
    }

    #[must_use]
    pub fn marker(&self) -> PathBuf {
        self.root.join("marker.json")
    }

    /// The directory holding cached embedding vectors, one file per content hash (C1).
    #[must_use]
    pub fn embeddings(&self) -> PathBuf {
        self.root.join("embeddings")
    }

    #[must_use]
    pub fn sources(&self) -> PathBuf {
        self.root.join("sources.json")
    }

    /// What a source must keep between fetches, such as the Slack threads whose parent is older
    /// than the window being fetched. A fetch reaches that material only through this file, so
    /// it travels with the windows instead of staying on the machine that fetched last.
    #[must_use]
    pub fn source_state(&self, source_id: &str) -> PathBuf {
        self.root.join("sources").join(source_id)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context as _, Result};

    use super::{
        data_dir_from, locate_tree, state_base_from, DataDir, StateDir, TreeConfig, UserConfig,
    };

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn scratch() -> Result<PathBuf> {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-config-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(root)
    }

    fn remove_scratch(path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    }

    #[test]
    fn memory_key_of_the_wrong_type_is_an_error() -> Result<()> {
        let dir = scratch()?;
        std::fs::create_dir_all(dir.join("ctx"))?;
        let config_path = dir.join("ctx/config.toml");
        std::fs::write(&config_path, "memory = 1\n")?;

        let result = UserConfig::load_from(&config_path);
        remove_scratch(&dir)?;

        let error = result.err().context("integer memory unexpectedly loaded")?;
        assert!(format!("{error:#}").contains("memory"));
        Ok(())
    }

    #[test]
    fn locate_tree_has_no_implicit_memory_default() -> Result<()> {
        let cwd = scratch()?;
        std::fs::create_dir_all(cwd.join("memory"))?;

        let result = locate_tree(None, &cwd, &UserConfig::default());
        remove_scratch(&cwd)?;

        let error = result
            .err()
            .context("implicit memory directory was selected")?;
        assert!(format!("{error:#}").contains("ctx.toml"));
        Ok(())
    }

    #[test]
    fn locate_tree_finds_the_nearest_ctx_toml_ancestor() -> Result<()> {
        let root = scratch()?;
        std::fs::write(root.join("ctx.toml"), "version = 1\n")?;
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested)?;

        let found = locate_tree(None, &nested, &UserConfig::default())?;
        remove_scratch(&root)?;

        assert_eq!(found, root);
        Ok(())
    }

    #[test]
    fn unknown_tree_config_keys_are_errors() -> Result<()> {
        let root = scratch()?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nbogus = true\n")?;

        let result = TreeConfig::load(&root);
        remove_scratch(&root)?;

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn source_id_must_be_a_safe_state_key() -> Result<()> {
        let root = scratch()?;
        for source_id in ["../x", "/tmp/x", "source/id"] {
            std::fs::write(
                root.join("ctx.toml"),
                format!(
                    "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"{source_id}\"\nkind = \"echo\"\nscheme = \"echo\"\n"
                ),
            )?;

            let result = TreeConfig::load(&root);
            let error = result
                .err()
                .context("unsafe source id unexpectedly loaded")?;
            assert!(format!("{error:#}").contains(source_id));
        }
        remove_scratch(&root)
    }

    #[test]
    fn state_base_honours_xdg_state_home() -> Result<()> {
        let environment = [("XDG_STATE_HOME", "/x/state"), ("HOME", "/home/u")];
        assert_eq!(
            state_base_from(&environment, &UserConfig::default())?,
            PathBuf::from("/x/state/ctx")
        );
        Ok(())
    }

    #[test]
    fn state_dir_uses_the_tree_scoped_layout() {
        let state = StateDir::new(PathBuf::from("/var/state/ctx/example"));
        assert_eq!(
            state.store(),
            PathBuf::from("/var/state/ctx/example/store.sqlite3")
        );
        assert_eq!(
            state.fetch_lock(),
            PathBuf::from("/var/state/ctx/example/fetch.lock")
        );
        assert_eq!(
            state.source_state("source"),
            PathBuf::from("/var/state/ctx/example/sources/source")
        );
    }

    #[test]
    fn data_dir_holds_the_files_shared_between_machines() {
        let data = DataDir::new(PathBuf::from("/mnt/share/memory"));
        assert_eq!(data.windows(), PathBuf::from("/mnt/share/memory/windows"));
        assert_eq!(
            data.cursors(),
            PathBuf::from("/mnt/share/memory/cursors.json")
        );
        assert_eq!(
            data.marker(),
            PathBuf::from("/mnt/share/memory/marker.json")
        );
        assert_eq!(
            data.source_state("slack"),
            PathBuf::from("/mnt/share/memory/sources/slack")
        );
    }

    #[test]
    fn the_environment_data_dir_wins_over_tree_configuration() {
        let mut config = TreeConfig::default();
        assert_eq!(data_dir_from(None, &config), None);

        config.data_dir = Some(PathBuf::from("/mnt/share/memory"));
        assert_eq!(
            data_dir_from(None, &config),
            Some(PathBuf::from("/mnt/share/memory"))
        );
        assert_eq!(
            data_dir_from(Some(PathBuf::from("/mnt/other")), &config),
            Some(PathBuf::from("/mnt/other"))
        );
    }
}
