//! Loading and modelling a markdown context tree.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
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

    /// The canonical numeric tier, 1–4.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::External => 1,
            Self::Internal => 2,
            Self::Summary => 3,
            Self::Recollection => 4,
        }
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

/// One verbatim claim: the quote, the source it came from, and how direct that source is.
/// A record may carry several, beside or instead of the single `quote`/`source`/`tier`.
#[derive(Debug, Deserialize)]
pub struct QuoteEntry {
    pub quote: String,
    pub source: Option<String>,
    tier: Option<TierValue>,
}

impl QuoteEntry {
    /// The authority tier this claim declares.
    #[must_use]
    pub fn tier(&self) -> Option<Tier> {
        self.tier.as_ref().and_then(Tier::from_value)
    }

    /// The tier this claim is judged at in a record whose strongest tier is `record`: its own
    /// when it declares one, else the record's.
    #[must_use]
    pub fn tier_in(&self, record: Option<Tier>) -> Option<Tier> {
        self.tier().or(record)
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
    /// The people an open item waits on to answer or decide, named as the tree names an owner.
    #[serde(default)]
    pub waiting_on: Vec<String>,
    /// The addresses of the items that track this record elsewhere, such as a tracker's ask.
    #[serde(default)]
    pub tracked_by: Vec<String>,
    tier: Option<TierValue>,
    source_tier: Option<TierValue>,
    #[serde(default)]
    sources: Vec<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub quotes: Vec<QuoteEntry>,
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
            .chain(self.quotes.iter().filter_map(QuoteEntry::tier))
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

    /// Whether the record makes a quote at tier 1 or 2: its single `quote` at the record's
    /// strongest tier, or a `quotes` entry at the tier [`QuoteEntry::tier_in`] judges it at.
    #[must_use]
    pub fn makes_verbatim_quote(&self) -> bool {
        let record = self.best_tier();
        let verbatim = |tier: Option<Tier>| tier.is_some_and(Tier::is_verbatim);
        (self.quote.is_some() && verbatim(record))
            || self
                .quotes
                .iter()
                .any(|entry| verbatim(entry.tier_in(record)))
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

    /// Every verbatim quote this record carries, from `quote` and from `quotes`.
    #[must_use]
    pub fn quote_texts(&self) -> Vec<&str> {
        self.quote
            .as_deref()
            .into_iter()
            .chain(self.quotes.iter().map(|entry| entry.quote.as_str()))
            .collect()
    }

    /// The structured `sources:` entries, which `extra` does not carry because `sources` is
    /// a field of its own. The search index reads their pointers.
    #[must_use]
    pub fn source_entries(&self) -> &[BTreeMap<String, serde_json::Value>] {
        &self.sources
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
    /// Why this document's frontmatter did not parse, if it did not. Such a document keeps
    /// its body and links; every typed field is empty.
    pub front_error: Option<String>,
    /// The original markdown text, including YAML frontmatter.
    pub raw: String,
    body_start: usize,
}

impl Doc {
    /// A document at the tree-relative path `rel` whose markdown, frontmatter included, is
    /// `raw`. Frontmatter that does not parse leaves every typed field empty and says why in
    /// [`Self::front_error`].
    #[must_use]
    pub fn from_text(rel: PathBuf, raw: String) -> Self {
        let (frontmatter, body_start) = split_front(&raw);
        let (front, front_error) = match frontmatter {
            Some(frontmatter) => match parse_front(frontmatter.as_ref()) {
                Ok(front) => (front, None),
                Err(error) => (Front::default(), Some(format!("{error:#}"))),
            },
            None => (Front::default(), None),
        };
        Self {
            rel,
            front,
            front_error,
            raw,
            body_start,
        }
    }

    #[must_use]
    pub fn body(&self) -> &str {
        let (_, body) = self.raw.split_at(self.body_start);
        body
    }

    /// The frontmatter block exactly as the file writes it, both `---` lines included, so an
    /// offset into it is an offset into [`Self::raw`]. Empty when the document has none.
    #[must_use]
    pub fn frontmatter_block(&self) -> &str {
        let (frontmatter, _) = self.raw.split_at(self.body_start);
        frontmatter
    }

    /// The line of the file that a byte offset into [`Self::raw`] falls on, counting from 1.
    #[must_use]
    pub fn line_at(&self, offset: usize) -> usize {
        self.raw.get(..offset).map_or(1, |before| {
            before.bytes().filter(|byte| *byte == b'\n').count() + 1
        })
    }

    /// The offset into [`Self::raw`] of the first frontmatter line, as written, that `accepts`
    /// returns true for. `None` when no frontmatter line is accepted; the body is never
    /// searched.
    pub fn frontmatter_line_start(&self, accepts: impl Fn(&str) -> bool) -> Option<usize> {
        let mut start = 0;
        for line in self.frontmatter_block().split_inclusive('\n') {
            if accepts(line) {
                return Some(start);
            }
            start += line.len();
        }
        None
    }

    /// The document's YAML frontmatter as written, before typed parsing. Empty when the
    /// document has none. A reader of a document whose frontmatter did not parse has this
    /// text and nothing else.
    #[must_use]
    pub fn frontmatter_text(&self) -> Cow<'_, str> {
        split_front(&self.raw).0.unwrap_or(Cow::Borrowed(""))
    }

    pub(crate) fn frontmatter_json(&self) -> Result<serde_json::Value> {
        if self.front_error.is_some() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        let (frontmatter, _) = split_front(&self.raw);
        frontmatter.map_or_else(
            || Ok(serde_json::Value::Object(serde_json::Map::new())),
            |frontmatter| {
                serde_saphyr::from_str_with_options(frontmatter.as_ref(), yaml_options())
                    .with_context(|| {
                        format!("parsing frontmatter in {}", self.rel.to_string_lossy())
                    })
            },
        )
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

    /// Quotes are provenance anchors: frontmatter `quote:`, a `quotes:` entry, or a
    /// markdown blockquote.
    #[must_use]
    pub fn has_quote(&self) -> bool {
        !self.front.quote_texts().is_empty()
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
    links: LinkIndex,
}

/// Link targets indexed once as a tree is loaded.
#[derive(Default)]
struct LinkIndex {
    exact_paths: HashMap<PathBuf, usize>,
    names: HashMap<String, Vec<usize>>,
    lowercase_names: HashMap<String, Vec<usize>>,
    basenames: HashMap<String, Vec<usize>>,
    path_suffixes: HashMap<String, Vec<usize>>,
}

impl LinkIndex {
    fn from_docs(docs: &[Doc], folder_notes: &[String]) -> Self {
        let mut index = Self::default();
        for (position, doc) in docs.iter().enumerate() {
            index.exact_paths.insert(doc.rel.clone(), position);

            let relative_path = doc.rel.to_string_lossy();
            let stem = strip_markdown_suffix(&relative_path);
            Self::add_name(&mut index.names, stem.to_owned(), position);
            Self::add_name(
                &mut index.lowercase_names,
                relative_path.to_ascii_lowercase(),
                position,
            );
            Self::add_name(
                &mut index.lowercase_names,
                stem.to_ascii_lowercase(),
                position,
            );

            if doc.is_folder_note(folder_notes) {
                if let Some(directory) = doc.rel.parent() {
                    let directory = directory.to_string_lossy();
                    Self::add_name(&mut index.names, directory.to_string(), position);
                    Self::add_name(
                        &mut index.lowercase_names,
                        directory.to_ascii_lowercase(),
                        position,
                    );
                }
            }

            if let Some(basename) = Path::new(stem).file_name().and_then(|name| name.to_str()) {
                Self::add_name(
                    &mut index.basenames,
                    basename.to_ascii_lowercase(),
                    position,
                );
            }
            for start in
                std::iter::once(0).chain(stem.match_indices('/').map(|(offset, _)| offset + 1))
            {
                Self::add_name(
                    &mut index.path_suffixes,
                    stem[start..].to_ascii_lowercase(),
                    position,
                );
            }
        }
        index
    }

    fn add_name(candidates: &mut HashMap<String, Vec<usize>>, name: String, position: usize) {
        let positions = candidates.entry(name).or_default();
        if positions.last().copied() != Some(position) {
            positions.push(position);
        }
    }

    fn exact_path(&self, target: &Path) -> Resolution {
        self.exact_paths
            .get(target)
            .copied()
            .map_or(Resolution::Missing, Resolution::Found)
    }

    fn name(&self, target: &str) -> Resolution {
        Self::resolve_candidates(self.names.get(target).map(Vec::as_slice))
    }

    fn exact_path_or_name(&self, path: &Path, name: &str) -> Resolution {
        Self::resolve_position_or_candidates(
            self.exact_paths.get(path).copied(),
            self.names.get(name).map(Vec::as_slice),
        )
    }

    fn case_insensitive_name(&self, path: &str, name: &str) -> Resolution {
        let path = path.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        Self::resolve_union(
            self.lowercase_names.get(&path).map(Vec::as_slice),
            self.lowercase_names.get(&name).map(Vec::as_slice),
        )
    }

    fn basename_or_path_suffix(&self, target: &str) -> Resolution {
        let target = target.to_ascii_lowercase();
        let basename = Path::new(&target)
            .file_name()
            .and_then(|name| name.to_str());
        Self::resolve_union(
            basename.and_then(|name| self.basenames.get(name).map(Vec::as_slice)),
            self.path_suffixes.get(&target).map(Vec::as_slice),
        )
    }

    fn resolve_candidates(candidates: Option<&[usize]>) -> Resolution {
        match candidates {
            None | Some([]) => Resolution::Missing,
            Some([position]) => Resolution::Found(*position),
            Some(positions) => Resolution::Ambiguous(positions.to_vec()),
        }
    }

    fn resolve_position_or_candidates(
        position: Option<usize>,
        candidates: Option<&[usize]>,
    ) -> Resolution {
        let Some(position) = position else {
            return Self::resolve_candidates(candidates);
        };
        let Some(candidates) = candidates else {
            return Resolution::Found(position);
        };
        Self::resolve_union(Some(std::slice::from_ref(&position)), Some(candidates))
    }

    fn resolve_union(left: Option<&[usize]>, right: Option<&[usize]>) -> Resolution {
        let (Some(left), Some(right)) = (left, right) else {
            return Self::resolve_candidates(left.or(right));
        };

        let mut left_index = 0;
        let mut right_index = 0;
        let mut first = None;
        let mut ambiguous: Option<Vec<usize>> = None;
        while left_index < left.len() || right_index < right.len() {
            let position = match (left.get(left_index), right.get(right_index)) {
                (Some(left_position), Some(right_position)) => {
                    match left_position.cmp(right_position) {
                        std::cmp::Ordering::Less => {
                            left_index += 1;
                            *left_position
                        }
                        std::cmp::Ordering::Greater => {
                            right_index += 1;
                            *right_position
                        }
                        std::cmp::Ordering::Equal => {
                            left_index += 1;
                            right_index += 1;
                            *left_position
                        }
                    }
                }
                (Some(left_position), None) => {
                    left_index += 1;
                    *left_position
                }
                (None, Some(right_position)) => {
                    right_index += 1;
                    *right_position
                }
                (None, None) => break,
            };
            if let Some(candidates) = ambiguous.as_mut() {
                candidates.push(position);
            } else if let Some(first) = first {
                let mut candidates = Vec::with_capacity(left.len().saturating_add(right.len()));
                candidates.push(first);
                candidates.push(position);
                ambiguous = Some(candidates);
            } else {
                first = Some(position);
            }
        }

        ambiguous.map_or_else(
            || first.map_or(Resolution::Missing, Resolution::Found),
            Resolution::Ambiguous,
        )
    }
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

pub(crate) fn yaml_options() -> Options {
    let mut options = Options::default();
    options.strict_booleans = true;
    options
}

/// Parses typed frontmatter fields. The error describes the offending value; the caller
/// names the document.
fn parse_front(raw: &str) -> Result<Front> {
    serde_saphyr::from_str_with_options(raw, yaml_options()).map_err(Into::into)
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
            let rel = path
                .strip_prefix(&root)
                .with_context(|| format!("deriving a tree-relative path for {}", path.display()))?
                .to_path_buf();
            docs.push(Doc::from_text(rel, raw));
        }
        let links = LinkIndex::from_docs(&docs, &config.folder_notes);
        Ok(Self { root, docs, links })
    }

    /// Resolves a target using path, relative-path, case-insensitive, and unique-shortest forms.
    #[must_use]
    pub fn resolve(&self, from: &Doc, target: &str) -> Resolution {
        match self.links.exact_path(Path::new(target)) {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        let target_without_suffix = strip_markdown_suffix(target);
        match self.links.name(target_without_suffix) {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        if let Some(relative) = relative_target(from, target) {
            let relative_without_suffix = relative.to_string_lossy();
            match self
                .links
                .exact_path_or_name(&relative, strip_markdown_suffix(&relative_without_suffix))
            {
                Resolution::Missing => {}
                resolution => return resolution,
            }
        }

        match self
            .links
            .case_insensitive_name(target, target_without_suffix)
        {
            Resolution::Missing => {}
            resolution => return resolution,
        }

        self.links.basename_or_path_suffix(target_without_suffix)
    }

    #[must_use]
    pub fn rules(&self) -> Option<&Doc> {
        self.docs
            .iter()
            .find(|doc| doc.rel == Path::new(RULES_FILE))
    }

    /// The record `path` names: a [`record_path`] this tree loaded.
    ///
    /// The tree loads only regular `.md` files under its root and follows no symbolic link, so
    /// a record this returns is a file inside the tree, whatever the path's text says.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is not a record path, or when this tree loaded no record
    /// at it.
    pub fn record(&self, path: &str) -> Result<&Doc> {
        let normalised = record_path(path)?;
        self.docs
            .iter()
            .find(|doc| doc.rel == Path::new(&normalised))
            .ok_or_else(|| anyhow!("{path} names no record of this tree"))
    }
}

/// Checks the path names a record this tree loads, and returns it in the one spelling the
/// tree uses.
///
/// The rule is the tree's own: a record is a `.md` file under the tree root, named by its
/// components, none of them hidden. `Tree::load` walks the tree with `hidden(true)`, so a
/// component starting with `.` is never loaded and nothing would check such a file. The
/// components are joined with `/` so that `decisions//renewal.md` and `decisions/renewal.md`
/// are one path: the writer matches every finding by this string, and two spellings of one
/// file would let a write past all of them.
///
/// A record path is also plain text, because git and a commit message both read it. git's
/// pathspec magic is off for every call `ctx` makes, and a path still carries no `*`, `?`,
/// `[` or `]`, which git would otherwise read as a pattern, and no component starting with
/// `-` or `:`, which git reads as an option or as magic. No control character either: the
/// path goes into the commit's subject line, and a line break would start a line of its own.
///
/// This covers what a path can say about itself. Whether this tree in fact loads the file —
/// `.gitignore` decides that too — is settled against the loaded tree: by [`Tree::record`] for
/// a read, and by the writer's own checks for a write.
///
/// # Errors
///
/// Returns an error when `path` is not a record path.
pub fn record_path(path: &str) -> Result<String> {
    let candidate = Path::new(path);
    if path.is_empty() {
        bail!("a record path names a file under the tree root, and this one is empty");
    }
    if candidate.is_absolute() {
        bail!("{path} is not a record path: a record path is relative to the tree root");
    }
    let mut parts = Vec::new();
    for part in candidate.components() {
        let Component::Normal(name) = part else {
            bail!("{path} is not a record path");
        };
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("{path} is not valid UTF-8"))?;
        if name.starts_with('.') {
            bail!("{path} is not a record path: a tree loads no hidden file as a record");
        }
        if name.starts_with(['-', ':']) {
            bail!("{path} is not a record path: git reads a name starting with - or : as an option or as magic");
        }
        if let Some(letter) = name
            .chars()
            .find(|letter| letter.is_control() || matches!(letter, '*' | '?' | '[' | ']'))
        {
            bail!(
                "{path} is not a record path: a record path is plain text, and holds no {letter:?}"
            );
        }
        parts.push(name);
    }
    let normalised = parts.join("/");
    if Path::new(&normalised)
        .extension()
        .is_none_or(|suffix| suffix != "md")
    {
        bail!("{path} is not a markdown record");
    }
    Ok(normalised)
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

    /// Loads one document and returns its typed frontmatter with the parse error, if any.
    fn load_front(text: &str, path: &Path) -> Result<(Front, Option<String>)> {
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
                .map(|doc| (doc.front, doc.front_error))
                .ok_or_else(|| anyhow!("frontmatter document was not loaded"))
        });
        remove_test_root(&root)?;
        result
    }

    fn parse_front(text: &str, path: &Path) -> Result<Front> {
        let (front, error) = load_front(text, path)?;
        match error {
            Some(error) => Err(anyhow!(error)),
            None => Ok(front),
        }
    }

    #[test]
    fn malformed_frontmatter_keeps_the_document_and_records_the_error() -> Result<()> {
        let root = test_root("malformed-frontmatter")?;
        std::fs::write(root.join("broken.md"), "---\ntype: [\n---\nbody text\n")
            .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root, &TreeConfig::default());
        remove_test_root(&root)?;
        let tree = result?;
        let doc = tree
            .docs
            .first()
            .ok_or_else(|| anyhow!("malformed document was not loaded"))?;
        assert_eq!(doc.rel, Path::new("broken.md"));
        assert!(doc.front.kind.is_none());
        assert!(doc.front_error.is_some());
        assert_eq!(doc.body().trim(), "body text");
        Ok(())
    }

    /// The frontmatter block and the body are the file, split once: offsets into either one
    /// land on the same line of the file, and a document without frontmatter has an empty
    /// block and all of its text as body.
    #[test]
    fn frontmatter_block_and_body_split_the_file_as_written() -> Result<()> {
        let root = test_root("frontmatter-block")?;
        let with = "\u{feff}---\r\ntype: decision\r\n---\r\nbody\r\n";
        std::fs::write(root.join("with.md"), with)
            .with_context(|| format!("writing {}", root.display()))?;
        std::fs::write(root.join("without.md"), "plain body\n")
            .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root, &TreeConfig::default());
        remove_test_root(&root)?;
        let tree = result?;
        let [with_doc, without_doc] = tree.docs.as_slice() else {
            return Err(anyhow!(
                "expected two documents, loaded {}",
                tree.docs.len()
            ));
        };
        assert_eq!(
            with_doc.frontmatter_block(),
            "\u{feff}---\r\ntype: decision\r\n---\r\n"
        );
        assert_eq!(
            format!("{}{}", with_doc.frontmatter_block(), with_doc.body()),
            with
        );
        assert_eq!(with_doc.line_at(0), 1);
        assert_eq!(with_doc.line_at(with_doc.frontmatter_block().len()), 4);
        assert_eq!(with_doc.line_at(with.len() + 1), 1);
        assert_eq!(without_doc.frontmatter_block(), "");
        assert_eq!(without_doc.body(), "plain body\n");
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

    /// A quote is judged at its `quotes` entry's own tier, else at the record's strongest, so
    /// a record's `tier: 1` makes no tier 1 quote of an entry that declares tier 3.
    #[test]
    fn a_verbatim_quote_is_judged_at_each_quote_s_own_tier() -> Result<()> {
        let single = |tier: &str| format!("tier: {tier}\nquote: \"q\"\n");
        let entry = |record: &str, own: &str| {
            format!("tier: {record}\nquotes:\n  - quote: \"q\"\n    source: \"s\"\n{own}")
        };
        for (text, expected) in [
            (single("1"), true),
            (single("3"), false),
            (entry("1", "    tier: 3\n"), false),
            (entry("3", "    tier: 2\n"), true),
            (entry("2", ""), true),
            ("tier: 1\n".to_owned(), false),
        ] {
            let front = parse_front(&text, Path::new("x.md"))?;
            assert_eq!(front.makes_verbatim_quote(), expected, "{text}");
        }
        Ok(())
    }

    /// The first frontmatter line a predicate accepts, as an offset into the file; the body is
    /// never searched.
    #[test]
    fn frontmatter_line_start_searches_the_frontmatter_alone() -> Result<()> {
        let root = test_root("frontmatter-line")?;
        std::fs::write(
            root.join("doc.md"),
            "---\ntype: decision\nstatement: s\n---\nstatement: in the body\n",
        )
        .with_context(|| format!("writing {}", root.display()))?;
        let result = Tree::load(&root, &TreeConfig::default());
        remove_test_root(&root)?;
        let tree = result?;
        let doc = tree
            .docs
            .first()
            .ok_or_else(|| anyhow!("document was not loaded"))?;
        let statement = doc.frontmatter_line_start(|line| line.starts_with("statement:"));
        assert_eq!(statement.map(|start| doc.line_at(start)), Some(3));
        assert_eq!(
            doc.frontmatter_line_start(|line| line.starts_with("owner:")),
            None
        );
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
    fn duplicate_keys_are_a_frontmatter_error() -> Result<()> {
        let (front, error) = load_front("type: a\ntype: b\n", Path::new("dup.md"))?;
        assert!(front.kind.is_none());
        let error = error.ok_or_else(|| anyhow!("duplicate frontmatter keys were accepted"))?;
        assert!(error.contains("duplicate"), "{error}");
        Ok(())
    }

    /// Who an open item waits on and what tracks it are lists. A record without them waits on
    /// nobody and nothing tracks it.
    #[test]
    fn waiting_on_and_tracked_by_are_lists_that_default_to_empty() -> Result<()> {
        let front = parse_front(
            "waiting_on: [Ada Lovelace, Grace Hopper]\ntracked_by: [\"https://example.com/ask/1\"]\n",
            Path::new("q.md"),
        )?;
        assert_eq!(front.waiting_on, ["Ada Lovelace", "Grace Hopper"]);
        assert_eq!(front.tracked_by, ["https://example.com/ask/1"]);
        assert!(!front.extra.contains_key("waiting_on"));
        assert!(!front.extra.contains_key("tracked_by"));

        let bare = parse_front("type: question\n", Path::new("q.md"))?;
        assert_eq!(bare.waiting_on, Vec::<String>::new());
        assert_eq!(bare.tracked_by, Vec::<String>::new());
        Ok(())
    }

    /// A single name where a list belongs is a frontmatter error, as a scalar `aliases` is: the
    /// sweep reports it and the server refuses the write, rather than read it as nobody.
    #[test]
    fn a_scalar_waiting_on_or_tracked_by_is_a_frontmatter_error() -> Result<()> {
        for field in [
            "waiting_on: Ada Lovelace\n",
            "tracked_by: https://example.com/ask/1\n",
        ] {
            let (front, error) = load_front(field, Path::new("q.md"))?;
            assert_eq!(
                (front.waiting_on, front.tracked_by),
                (Vec::<String>::new(), Vec::<String>::new())
            );
            assert!(error.is_some(), "{field:?} was accepted");
        }
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
