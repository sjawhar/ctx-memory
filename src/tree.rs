//! Loading and modelling a markdown context tree.

use std::borrow::Cow;
use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_saphyr::Options;

use crate::config::{TreeConfig, RULES_FILE};

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

    fn from_value(value: &TierValue) -> Option<Self> {
        match value {
            TierValue::Numeric(number) => Self::from_numeric(*number),
            TierValue::Named(name) => Self::from_name(name),
        }
    }

    fn from_json_value(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Number(number) => number.as_u64().and_then(Self::from_numeric),
            serde_json::Value::String(name) => Self::from_name(name),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TierValue {
    Numeric(u64),
    Named(String),
}

impl<'de> Deserialize<'de> for Tier {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = TierValue::deserialize(deserializer)?;
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
    tier: Option<TierValue>,
    source_tier: Option<TierValue>,
    #[serde(default)]
    sources: Vec<BTreeMap<String, serde_json::Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
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
            .filter_map(|source| source.get("tier").and_then(Tier::from_json_value))
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
        self.extra.get(key).and_then(serde_json::Value::as_str)
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
        let (_, body) = self.raw.split_at(self.body_start);
        body
    }

    #[must_use]
    pub fn stem(&self) -> String {
        self.rel.with_extension("").to_string_lossy().into_owned()
    }

    fn is_folder_note(&self, folder_notes: &[String]) -> bool {
        self.rel.file_name().is_some_and(|file_name| {
            let file_name = file_name.to_string_lossy();
            folder_notes
                .iter()
                .any(|folder_note| folder_note == &file_name)
        })
    }

    /// Wikilink targets this document answers to: its relative stem and, for configured
    /// folder-note file names, its containing directory.
    #[must_use]
    pub fn link_names(&self, folder_notes: &[String]) -> Vec<String> {
        let mut names = vec![self.stem()];
        let is_folder_note = self.is_folder_note(folder_notes);
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

/// A parsed Obsidian-style wikilink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wikilink {
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
    pub start: usize,
}

/// Parses wikilinks from Markdown and frontmatter text.
#[must_use]
pub fn wikilinks(text: &str) -> Vec<Wikilink> {
    let mut links = Vec::new();
    let mut offset = 0;
    while let Some(relative_open) = text
        .get(offset..)
        .and_then(|remaining| remaining.find("[["))
    {
        let open = offset + relative_open;
        let after_open = open + "[[".len();
        let Some(relative_close) = text
            .get(after_open..)
            .and_then(|remaining| remaining.find("]]"))
        else {
            break;
        };
        let close = after_open + relative_close;
        offset = close + "]]".len();

        let Some(content) = text.get(after_open..close) else {
            continue;
        };
        if content.contains('[') || content.contains(']') {
            continue;
        }
        let (target_and_heading, alias) = match content.split_once('|') {
            Some((target, alias)) if !alias.is_empty() => (target, Some(alias.to_owned())),
            Some(_) => continue,
            None => (content, None),
        };
        let (target, heading) = match target_and_heading.split_once('#') {
            Some((target, heading)) if !heading.is_empty() => (target, Some(heading.to_owned())),
            Some(_) => continue,
            None => (target_and_heading, None),
        };
        let target = target.trim();
        let target = target.strip_suffix('\\').unwrap_or(target).trim();
        let target = strip_markdown_suffix(target).trim();
        if target.is_empty() {
            continue;
        }

        let embed = text
            .get(..open)
            .and_then(|before| before.chars().next_back())
            .is_some_and(|character| character == '!');
        links.push(Wikilink {
            target: target.to_owned(),
            heading,
            alias,
            embed,
            start: if embed {
                open.saturating_sub('!'.len_utf8())
            } else {
                open
            },
        });
    }
    links
}

/// The result of resolving a wikilink target within a tree.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    Found(usize),
    Missing,
    Ambiguous(Vec<usize>),
}

pub struct Tree {
    pub root: PathBuf,
    pub docs: Vec<Doc>,
    pub(crate) folder_notes: Vec<String>,
}

/// Split a file into normalized YAML frontmatter and the byte offset of its Markdown body.
fn split_front(text: &str) -> (Option<Cow<'_, str>>, usize) {
    let without_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
    let bom_len = text.len().saturating_sub(without_bom.len());
    let (frontmatter, opening_len) = if let Some(rest) = without_bom.strip_prefix("---\r\n") {
        (rest, "---\r\n".len())
    } else if let Some(rest) = without_bom.strip_prefix("---\n") {
        (rest, "---\n".len())
    } else {
        return (None, 0);
    };

    let mut frontmatter_len = 0;
    for line in frontmatter.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        let closing_candidate = line_without_newline
            .strip_suffix('\r')
            .unwrap_or(line_without_newline);
        if closing_candidate == "---" {
            let (front, _) = frontmatter.split_at(frontmatter_len);
            let front = if front.contains("\r\n") {
                Cow::Owned(front.replace("\r\n", "\n"))
            } else {
                Cow::Borrowed(front)
            };
            return (
                Some(front),
                bom_len + opening_len + frontmatter_len + line.len(),
            );
        }
        frontmatter_len += line.len();
    }
    (None, 0)
}

fn yaml_options() -> Options {
    let mut options = Options::default();
    options.strict_booleans = true;
    options
}

fn parse_front(raw: &str, path: &Path) -> Result<Front> {
    serde_saphyr::from_str_with_options(raw, yaml_options())
        .with_context(|| format!("parsing frontmatter in {}", path.display()))
}

fn strip_markdown_suffix(target: &str) -> &str {
    target.strip_suffix(".md").unwrap_or(target)
}

fn relative_target(from: &Doc, target: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in from.rel.parent().unwrap_or(Path::new("")).components() {
        if let Component::Normal(part) = component {
            relative.push(part);
        }
    }
    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !relative.pop() {
                    return None;
                }
            }
            Component::Normal(part) => relative.push(part),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

fn names_match(left: &str, right: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn document_link_name_matches(
    doc: &Doc,
    target: &str,
    folder_notes: &[String],
    case_insensitive: bool,
) -> bool {
    let relative_path = doc.rel.to_string_lossy();
    let stem = relative_path
        .strip_suffix(".md")
        .unwrap_or(relative_path.as_ref());
    if names_match(stem, target, case_insensitive) {
        return true;
    }
    doc.is_folder_note(folder_notes)
        && doc.rel.parent().is_some_and(|directory| {
            names_match(
                directory.to_string_lossy().as_ref(),
                target,
                case_insensitive,
            )
        })
}
fn has_path_suffix(path: &str, suffix: &str) -> bool {
    let Some(start) = path.len().checked_sub(suffix.len()) else {
        return false;
    };
    path.get(start..).is_some_and(|end| {
        end.eq_ignore_ascii_case(suffix)
            && (start == 0
                || path
                    .get(..start)
                    .is_some_and(|prefix| prefix.ends_with('/')))
    })
}

impl Tree {
    /// Loads all Markdown documents below `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be traversed, a document cannot be read, or
    /// its frontmatter cannot be parsed.
    pub fn load(root: &Path, config: &TreeConfig) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("no markdown tree at {}", root.display()))?;
        let mut docs = Vec::new();
        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .sort_by_file_name(std::cmp::Ord::cmp)
            .require_git(false)
            .hidden(true);
        let walker = builder.build();
        for entry in walker {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if entry
                .file_type()
                .is_none_or(|file_type| !file_type.is_file())
            {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (frontmatter, body_start) = split_front(&raw);
            let front = frontmatter.map_or_else(
                || Ok(Front::default()),
                |frontmatter| parse_front(frontmatter.as_ref(), path),
            )?;
            let rel = path
                .strip_prefix(&root)
                .with_context(|| format!("deriving a tree-relative path for {}", path.display()))?
                .to_path_buf();
            docs.push(Doc {
                rel,
                front,
                raw,
                body_start,
            });
        }
        Ok(Self {
            root,
            docs,
            folder_notes: config.folder_notes.clone(),
        })
    }

    fn resolve_matching(&self, predicate: impl Fn(&Doc) -> bool) -> Resolution {
        let mut positions = self
            .docs
            .iter()
            .enumerate()
            .filter_map(|(position, doc)| predicate(doc).then_some(position));
        let Some(first) = positions.next() else {
            return Resolution::Missing;
        };
        let Some(second) = positions.next() else {
            return Resolution::Found(first);
        };
        let mut candidates = vec![first, second];
        candidates.extend(positions);
        Resolution::Ambiguous(candidates)
    }

    /// Resolves a target using path, relative-path, case-insensitive, and unique-shortest forms.
    #[must_use]
    pub fn resolve(&self, from: &Doc, target: &str) -> Resolution {
        match self.resolve_matching(|doc| doc.rel == Path::new(target)) {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        let target_without_suffix = strip_markdown_suffix(target);
        match self.resolve_matching(|doc| {
            document_link_name_matches(doc, target_without_suffix, &self.folder_notes, false)
        }) {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        if let Some(relative) = relative_target(from, target) {
            let relative = relative.to_string_lossy();
            let relative_without_suffix = strip_markdown_suffix(&relative);
            match self.resolve_matching(|doc| {
                names_match(doc.rel.to_string_lossy().as_ref(), relative.as_ref(), false)
                    || document_link_name_matches(
                        doc,
                        relative_without_suffix,
                        &self.folder_notes,
                        false,
                    )
            }) {
                Resolution::Missing => {}
                resolution => return resolution,
            }
        }

        match self.resolve_matching(|doc| {
            names_match(doc.rel.to_string_lossy().as_ref(), target, true)
                || document_link_name_matches(doc, target_without_suffix, &self.folder_notes, true)
        }) {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        let target_basename = Path::new(target_without_suffix)
            .file_name()
            .and_then(|name| name.to_str());
        self.resolve_matching(|doc| {
            let relative_path = doc.rel.to_string_lossy();
            let stem = relative_path
                .strip_suffix(".md")
                .unwrap_or(relative_path.as_ref());
            target_basename.is_some_and(|basename| {
                Path::new(stem)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| names_match(name, basename, true))
            }) || has_path_suffix(stem, target_without_suffix)
        })
    }

    #[must_use]
    pub fn index_by_name(&self) -> HashMap<String, usize> {
        let mut index = HashMap::new();
        let mut duplicates = HashSet::new();
        for (position, doc) in self.docs.iter().enumerate() {
            for name in doc.link_names(&self.folder_notes) {
                if duplicates.contains(&name) {
                    continue;
                }
                match index.entry(name) {
                    Entry::Occupied(entry) => {
                        let (name, _) = entry.remove_entry();
                        duplicates.insert(name);
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(position);
                    }
                }
            }
        }
        index
    }

    #[must_use]
    pub fn rules(&self) -> Option<&Doc> {
        self.docs
            .iter()
            .find(|doc| doc.rel == Path::new(RULES_FILE))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{anyhow, Context as _, Result};

    use crate::config::TreeConfig;

    use super::{split_front, wikilinks, Front, Tier, Tree};

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

    fn front_and_body(text: &str) -> (Option<String>, &str) {
        let (front, body_start) = split_front(text);
        (
            front.map(std::borrow::Cow::into_owned),
            text.get(body_start..).unwrap_or_default(),
        )
    }

    fn parse_front(text: &str, path: &Path) -> Result<Front> {
        let root = test_root("parse-front")?;
        let document = root.join(path);
        let parent = document
            .parent()
            .ok_or_else(|| anyhow!("test document has no parent"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        std::fs::write(&document, format!("---\n{text}---\nbody\n"))
            .with_context(|| format!("writing {}", document.display()))?;
        let result = Tree::load(&root, &TreeConfig::default()).and_then(|tree| {
            tree.docs
                .into_iter()
                .next()
                .map(|doc| doc.front)
                .ok_or_else(|| anyhow!("frontmatter document was not loaded"))
        });
        remove_test_root(&root)?;
        result
    }

    #[test]
    fn load_rejects_malformed_frontmatter_with_file_context() -> Result<()> {
        let root = test_root("malformed-frontmatter")?;
        std::fs::write(root.join("broken.md"), "---\ntype: [\n---\nbody\n")
            .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root, &TreeConfig::default());
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
        let result = Tree::load(&root, &TreeConfig::default());
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
    fn crlf_frontmatter_is_parsed() {
        let (front, body) =
            front_and_body("---\r\ntype: decision\r\nstatus: open\r\n---\r\nbody\r\n");
        assert_eq!(front, Some("type: decision\nstatus: open\n".to_owned()));
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn four_dashes_do_not_close_frontmatter() {
        assert_eq!(front_and_body("---\ntype: decision\n----\nbody\n").0, None);
    }

    #[test]
    fn bom_does_not_hide_frontmatter() {
        assert!(front_and_body("\u{feff}---\ntype: decision\n---\nx\n")
            .0
            .is_some());
    }

    #[test]
    fn quoted_numeral_tier_is_a_tier() -> Result<()> {
        let front = parse_front("type: meeting\ntier: \"1\"\n", Path::new("m.md"))?;
        assert_eq!(front.best_tier(), Some(Tier::External));
        Ok(())
    }

    #[test]
    fn legacy_and_neutral_tier_names_agree() -> Result<()> {
        for (value, expected) in [
            ("counterparty", Tier::External),
            ("external", Tier::External),
            ("principal", Tier::Internal),
            ("internal", Tier::Internal),
        ] {
            let front = parse_front(&format!("tier: {value}\n"), Path::new("x.md"))?;
            assert_eq!(front.best_tier(), Some(expected));
        }
        Ok(())
    }

    #[test]
    fn yaml_1_1_booleans_stay_strings() -> Result<()> {
        let front = parse_front("status: no\n", Path::new("x.md"))?;
        assert_eq!(front.status.as_deref(), Some("no"));
        Ok(())
    }

    #[test]
    fn duplicate_keys_are_an_error_naming_the_file() -> Result<()> {
        let error = parse_front("type: a\ntype: b\n", Path::new("dup.md"))
            .err()
            .ok_or_else(|| anyhow!("duplicate frontmatter keys unexpectedly loaded"))?;
        assert!(format!("{error:#}").contains("dup.md"));
        Ok(())
    }

    #[test]
    fn wikilink_forms() {
        let got: Vec<_> = wikilinks("[[a]] [[b|B]] [[c#h]] [[d.md]] [[e\\|E]] ![[f]] [[ g ]]")
            .into_iter()
            .map(|link| (link.target, link.heading, link.alias.is_some(), link.embed))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".to_owned(), None, false, false),
                ("b".to_owned(), None, true, false),
                ("c".to_owned(), Some("h".to_owned()), false, false),
                ("d".to_owned(), None, false, false),
                ("e".to_owned(), None, true, false),
                ("f".to_owned(), None, false, true),
                ("g".to_owned(), None, false, false),
            ]
        );
    }
}
