//! Loading and modelling a markdown context tree.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

/// How much authority a source carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// An external source's own words, verbatim.
    External,
    /// An internal source's own words, verbatim.
    Internal,
    /// A generated or editorial summary.
    Summary,
    /// A secondhand paraphrase.
    Recollection,
}

impl Tier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::External => "E1",
            Self::Internal => "I2",
            Self::Summary => "s3",
            Self::Recollection => "r4",
        }
    }

    /// Verbatim tiers are the ones that settle a conflict.
    #[must_use]
    pub const fn is_verbatim(self) -> bool {
        matches!(self, Self::External | Self::Internal)
    }

    const fn from_numeric(n: u64) -> Option<Self> {
        match n {
            1 => Some(Self::External),
            2 => Some(Self::Internal),
            3 => Some(Self::Summary),
            4 => Some(Self::Recollection),
            _ => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "1" | "external" | "counterparty" => Some(Self::External),
            "2" | "internal" | "principal" => Some(Self::Internal),
            "3" | "summary" => Some(Self::Summary),
            "4" | "recollection" => Some(Self::Recollection),
            _ => None,
        }
    }

    fn from_value(value: &serde_yaml::Value) -> Option<Self> {
        match value {
            serde_yaml::Value::Number(number) => number.as_u64().and_then(Self::from_numeric),
            serde_yaml::Value::String(name) => Self::from_name(name),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Tier {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        Self::from_value(&value).ok_or_else(|| D::Error::custom("invalid authority tier"))
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Frontmatter fields the library reasons about. Other frontmatter remains available in
/// [`Self::extra`].
#[derive(Debug, Default, Deserialize)]
pub struct Front {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub what: Option<String>,
    pub question: Option<String>,
    pub quote: Option<String>,
    pub date: Option<NaiveDate>,
    pub opened: Option<NaiveDate>,
    pub due: Option<NaiveDate>,
    pub revisit_by: Option<NaiveDate>,
    pub last_reviewed: Option<NaiveDate>,
    #[serde(default)]
    pub aliases: Vec<String>,
    tier: Option<serde_yaml::Value>,
    source_tier: Option<serde_yaml::Value>,
    #[serde(default)]
    sources: Vec<BTreeMap<String, serde_yaml::Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl Front {
    /// The strongest tier this document carries, declared directly or by a source.
    pub fn best_tier(&self) -> Option<Tier> {
        let direct = self
            .tier
            .as_ref()
            .or(self.source_tier.as_ref())
            .and_then(Tier::from_value);
        let nested = self
            .sources
            .iter()
            .filter_map(|source| source.get("tier").and_then(Tier::from_value))
            .min();
        match (direct, nested) {
            (Some(first), Some(second)) => Some(first.min(second)),
            (first, second) => first.or(second),
        }
    }

    #[must_use]
    pub fn has_any_tier(&self) -> bool {
        self.best_tier().is_some()
    }

    /// The date an open item is due to be revisited, if any.
    #[must_use]
    pub fn deadline(&self) -> Option<NaiveDate> {
        self.revisit_by.or(self.due)
    }

    #[must_use]
    pub fn display_title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .or(self.name.as_deref())
            .or(self.what.as_deref())
            .or(self.question.as_deref())
    }

    /// Returns a string-valued frontmatter field that is not part of the generic schema.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(serde_yaml::Value::as_str)
    }
}

#[derive(Debug)]
pub struct Doc {
    /// Path relative to the tree root — the form used in wikilinks and output.
    pub rel: PathBuf,
    pub front: Front,
    /// The original markdown text, including YAML frontmatter.
    pub raw: String,
    body_start: usize,
}

impl Doc {
    #[must_use]
    pub fn body(&self) -> &str {
        self.raw.get(self.body_start..).unwrap_or_default()
    }

    #[must_use]
    pub fn stem(&self) -> String {
        self.rel.with_extension("").to_string_lossy().into_owned()
    }

    /// Wikilink targets this document answers to: its relative stem and, for configured
    /// folder-note file names, its containing directory.
    #[must_use]
    pub fn link_names(&self, folder_notes: &[String]) -> Vec<String> {
        let mut names = vec![self.stem()];
        let is_folder_note = self.rel.file_name().is_some_and(|file_name| {
            let file_name = file_name.to_string_lossy();
            folder_notes
                .iter()
                .any(|folder_note| folder_note == &file_name)
        });
        if is_folder_note {
            if let Some(directory) = self.rel.parent() {
                names.push(directory.to_string_lossy().into_owned());
            }
        }
        names
    }

    pub fn label(&self) -> String {
        self.front
            .display_title()
            .map_or_else(|| self.rel.to_string_lossy().into_owned(), str::to_owned)
    }

    /// Quotes are provenance anchors: frontmatter `quote:` or a markdown blockquote.
    #[must_use]
    pub fn has_quote(&self) -> bool {
        self.front.quote.is_some()
            || self
                .body()
                .lines()
                .any(|line| line.trim_start().starts_with("> "))
    }
}

pub struct Tree {
    pub root: PathBuf,
    pub docs: Vec<Doc>,
}

/// Split a file into its YAML frontmatter and the byte offset of its markdown body.
fn split_front(text: &str) -> (Option<&str>, usize) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, 0);
    };
    match rest.find("\n---") {
        Some(end) => {
            let body_start = "---\n".len() + end + "\n---".len();
            (Some(&rest[..end]), body_start)
        }
        None => (None, 0),
    }
}

impl Tree {
    /// Loads all Markdown documents below `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be traversed, a document cannot be read, or
    /// its frontmatter cannot be parsed.
    pub fn load(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("no markdown tree at {}", root.display()))?;
        let mut docs = Vec::new();
        for entry in walkdir::WalkDir::new(&root).sort_by_file_name() {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (frontmatter, body_start) = split_front(&raw);
            let front = match frontmatter {
                Some(frontmatter) => serde_yaml::from_str(frontmatter)
                    .with_context(|| format!("parsing frontmatter in {}", path.display()))?,
                None => Front::default(),
            };
            docs.push(Doc {
                rel: path.strip_prefix(&root).unwrap_or(path).to_path_buf(),
                front,
                raw,
                body_start,
            });
        }
        Ok(Self { root, docs })
    }

    #[must_use]
    pub fn index_by_name(&self) -> HashMap<String, usize> {
        let mut index = HashMap::new();
        for (position, doc) in self.docs.iter().enumerate() {
            for name in doc.link_names(&[]) {
                index.insert(name, position);
            }
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{anyhow, Context as _, Result};

    use super::{Tier, Tree};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_root(label: &str) -> Result<PathBuf> {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-tree-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(root)
    }

    fn remove_test_root(root: &Path) -> Result<()> {
        std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
    }

    #[test]
    fn load_rejects_malformed_frontmatter_with_file_context() -> Result<()> {
        let root = test_root("malformed-frontmatter")?;
        std::fs::write(root.join("broken.md"), "---\ntype: [\n---\nbody\n")
            .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root);
        remove_test_root(&root)?;
        let error = result
            .err()
            .ok_or_else(|| anyhow!("malformed frontmatter unexpectedly loaded"))?;
        let rendered = format!("{error:#}");
        assert!(rendered.contains("parsing frontmatter"));
        assert!(rendered.contains("broken.md"));
        Ok(())
    }

    #[test]
    fn load_accepts_a_document_without_frontmatter() -> Result<()> {
        let root = test_root("no-frontmatter")?;
        std::fs::write(root.join("plain.md"), "ordinary markdown\n")
            .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root);
        remove_test_root(&root)?;
        let tree = result?;
        let doc = tree
            .docs
            .first()
            .ok_or_else(|| anyhow!("plain document was not loaded"))?;
        assert!(doc.front.kind.is_none());
        Ok(())
    }

    #[test]
    fn parses_canonical_and_legacy_tier_names() -> Result<()> {
        for (value, expected) in [
            ("external", Tier::External),
            ("counterparty", Tier::External),
            ("internal", Tier::Internal),
            ("principal", Tier::Internal),
            ("summary", Tier::Summary),
            ("recollection", Tier::Recollection),
            ("1", Tier::External),
            ("2", Tier::Internal),
            ("3", Tier::Summary),
            ("4", Tier::Recollection),
            ("\"1\"", Tier::External),
            ("\"2\"", Tier::Internal),
            ("\"3\"", Tier::Summary),
            ("\"4\"", Tier::Recollection),
        ] {
            let frontmatter = format!("tier: {value}\n");
            let tree = serde_yaml::from_str::<super::Front>(&frontmatter)?;
            assert_eq!(tree.best_tier(), Some(expected));
        }
        Ok(())
    }
}
