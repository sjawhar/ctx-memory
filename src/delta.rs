//! Per-window changes matched against tree records.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::config::SourceConfig;
use crate::pointer::Schemes;
use crate::record::Record;
use crate::schema::{OPEN_KINDS, OPEN_STATES};
use crate::source;
use crate::tree::{Doc, Tree};
use crate::windows::{SourcedRecord, Window};

const MIN_QUOTE_WORDS: usize = 5;
const EXCERPT_CHARS: usize = 160;

/// The record that owns each configured source stream.
#[derive(Debug, Default)]
pub struct Owners(BTreeMap<(String, String), String>);

impl Owners {
    /// Discovers configured stream owners from the tree.
    #[must_use]
    pub fn discover(tree: &Tree, sources: &[SourceConfig]) -> Self {
        let mut owners = BTreeMap::new();
        for source_config in sources {
            let Some(field) = source_config.discover_field.as_deref() else {
                continue;
            };
            for discovered in source::discover_streams(tree, field) {
                owners.insert(
                    (source_config.id.clone(), discovered.stream),
                    discovered.owner,
                );
            }
        }
        Self(owners)
    }

    fn get(&self, source: &str, stream: &str) -> Option<&str> {
        self.0
            .get(&(source.to_owned(), stream.to_owned()))
            .map(String::as_str)
    }
}

/// One record match for a new source message.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub record: String,
    /// `pointer`, `alias`, `quote`, `owner`, or `stream`.
    pub how: String,
}

/// A new non-context source record and its tree matches.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewMessage {
    pub pointer: String,
    pub label: Option<String>,
    pub ts: DateTime<Utc>,
    pub author: Option<String>,
    pub is_reply: bool,
    pub thread: Option<String>,
    pub excerpt: String,
    pub matches: Vec<Match>,
}

/// One reply that makes an open record stale.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyRef {
    pub pointer: String,
    pub ts: DateTime<Utc>,
    pub author: Option<String>,
    pub how: String,
    pub excerpt: String,
}

/// An open record with messages after its last review.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenFlag {
    pub record: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub owner: Option<String>,
    pub last_reviewed: Option<NaiveDate>,
    pub replies: Vec<ReplyRef>,
}

/// A record whose open status changed since the prior window.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusChange {
    pub record: String,
    pub title: String,
    pub was: String,
    pub now: Option<String>,
}

/// The durable delta for one fetch window.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Delta {
    pub version: u32,
    pub window: String,
    pub window_end: DateTime<Utc>,
    pub generated_at: DateTime<Utc>,
    pub previous_window: Option<String>,
    pub new_messages: Vec<NewMessage>,
    pub unreviewed: Vec<OpenFlag>,
    pub closed_since_previous: Vec<StatusChange>,
    pub opened_since_previous: Vec<String>,
}

/// One open record captured when a fetch window was created.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRecord {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub last_reviewed: Option<NaiveDate>,
}

/// The open set as it stood when a fetch window was created.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSnapshot {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub open: Vec<OpenRecord>,
}

/// A reply that remains unreviewed at the current tree revision.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Staleness {
    pub pointer: String,
    pub ts: DateTime<Utc>,
    pub author: Option<String>,
    pub how: String,
    pub same_day: bool,
}

struct Cited {
    rel: String,
    pointers: BTreeSet<String>,
    tokens: BTreeSet<String>,
    quotes: Vec<String>,
}

struct Arrival<'a> {
    source: &'a str,
    record: &'a Record,
    anchor_texts: Vec<String>,
}

/// Captures the current open set from `tree`.
#[must_use]
pub fn open_snapshot(tree: &Tree, generated_at: DateTime<Utc>) -> OpenSnapshot {
    let mut open = tree
        .docs
        .iter()
        .filter(|document| is_open(document))
        .map(|document| OpenRecord {
            path: document.rel.to_string_lossy().into_owned(),
            kind: document.front.kind.clone().unwrap_or_default(),
            status: document.front.status.clone().unwrap_or_default(),
            title: document.label(),
            last_reviewed: document.front.last_reviewed,
        })
        .collect::<Vec<_>>();
    open.sort_by(|left, right| left.path.cmp(&right.path));
    OpenSnapshot {
        version: 1,
        generated_at,
        open,
    }
}

/// Computes one window's record matches and open-state changes.
///
/// # Errors
///
/// Returns an error when the window or its prior snapshot cannot be read or decoded.
pub fn compute(
    tree: &Tree,
    schemes: &Schemes,
    owners: &Owners,
    window: &Window,
    previous: Option<&Window>,
    generated_at: DateTime<Utc>,
) -> Result<Delta> {
    let manifest = window.manifest()?;
    let sourced_records = window.records()?;
    let cited_records = tree
        .docs
        .iter()
        .map(|document| cited(document, schemes))
        .collect::<Vec<_>>();
    let docs = tree
        .docs
        .iter()
        .map(|document| (document.rel.to_string_lossy().into_owned(), document))
        .collect::<BTreeMap<_, _>>();
    let (new_messages, flagged) =
        match_arrivals(arrivals(&sourced_records), &cited_records, owners, &docs);
    let (previous_window, closed_since_previous, opened_since_previous) =
        state_changes(tree, &docs, previous)?;

    Ok(Delta {
        version: 1,
        window: window.id.to_string(),
        window_end: manifest.window_end,
        generated_at,
        previous_window,
        new_messages,
        unreviewed: open_flags(flagged, &docs),
        closed_since_previous,
        opened_since_previous,
    })
}

fn arrivals(sourced_records: &[SourcedRecord]) -> Vec<Arrival<'_>> {
    let texts = sourced_records
        .iter()
        .map(|sourced| {
            (
                sourced.record.pointer.as_str(),
                sourced.record.text.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut arrivals = sourced_records
        .iter()
        .filter(|sourced| !sourced.record.context)
        .map(|sourced| {
            let mut anchor_texts = vec![normalise(&sourced.record.text)];
            if let Some(thread) = sourced.record.thread.as_deref() {
                if let Some(parent) = texts.get(thread) {
                    anchor_texts.push(normalise(parent));
                }
            }
            Arrival {
                source: &sourced.source,
                record: &sourced.record,
                anchor_texts,
            }
        })
        .collect::<Vec<_>>();
    arrivals.sort_by(|left, right| {
        left.record
            .ts
            .cmp(&right.record.ts)
            .then_with(|| left.record.pointer.cmp(&right.record.pointer))
    });
    arrivals
}

fn match_arrivals(
    arrivals: Vec<Arrival<'_>>,
    cited_records: &[Cited],
    owners: &Owners,
    docs: &BTreeMap<String, &Doc>,
) -> (Vec<NewMessage>, BTreeMap<String, Vec<ReplyRef>>) {
    let mut new_messages = Vec::with_capacity(arrivals.len());
    let mut flagged = BTreeMap::<String, Vec<ReplyRef>>::new();
    for arrival in arrivals {
        let owner = owners.get(arrival.source, &arrival.record.stream);
        let mut matches = cited_records
            .iter()
            .filter_map(|cited| match_one(&arrival, cited, owner))
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.record.cmp(&right.record));
        flag_matches(&arrival, &matches, docs, &mut flagged);
        new_messages.push(NewMessage {
            pointer: arrival.record.pointer.clone(),
            label: arrival.record.label.clone(),
            ts: arrival.record.ts,
            author: arrival.record.author.clone(),
            is_reply: arrival.record.thread.is_some(),
            thread: arrival.record.thread.clone(),
            excerpt: excerpt(&arrival.record.text),
            matches,
        });
    }
    (new_messages, flagged)
}

fn flag_matches(
    arrival: &Arrival<'_>,
    matches: &[Match],
    docs: &BTreeMap<String, &Doc>,
    flagged: &mut BTreeMap<String, Vec<ReplyRef>>,
) {
    for matched in matches {
        let Some(document) = docs.get(&matched.record) else {
            continue;
        };
        if is_open(document)
            && is_after_review(arrival.record.ts.date_naive(), document.front.last_reviewed)
        {
            flagged
                .entry(matched.record.clone())
                .or_default()
                .push(ReplyRef {
                    pointer: arrival.record.pointer.clone(),
                    ts: arrival.record.ts,
                    author: arrival.record.author.clone(),
                    how: matched.how.clone(),
                    excerpt: excerpt(&arrival.record.text),
                });
        }
    }
}

fn open_flags(
    flagged: BTreeMap<String, Vec<ReplyRef>>,
    docs: &BTreeMap<String, &Doc>,
) -> Vec<OpenFlag> {
    flagged
        .into_iter()
        .filter_map(|(record, replies)| {
            let document = docs.get(&record)?;
            Some(OpenFlag {
                record,
                kind: document.front.kind.clone().unwrap_or_default(),
                status: document.front.status.clone().unwrap_or_default(),
                title: document.label(),
                owner: document.front.owner.clone(),
                last_reviewed: document.front.last_reviewed,
                replies,
            })
        })
        .collect()
}

fn cited(document: &Doc, schemes: &Schemes) -> Cited {
    let pointers = schemes
        .find_all(&document.raw)
        .into_iter()
        .flat_map(|found| {
            let mut pointers = vec![found.pointer.to_string()];
            if let Some(thread) = found.thread {
                pointers.push(thread.to_string());
            }
            pointers
        })
        .collect();
    Cited {
        rel: document.rel.to_string_lossy().into_owned(),
        pointers,
        tokens: tokens(&document.raw),
        quotes: quote_fragments(document.front.quote.as_deref(), document.body()),
    }
}

fn is_open(document: &Doc) -> bool {
    document
        .front
        .kind
        .as_deref()
        .is_some_and(|kind| OPEN_KINDS.contains(&kind))
        && document
            .front
            .status
            .as_deref()
            .is_some_and(|status| OPEN_STATES.contains(&status))
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|character: char| {
        !(character.is_ascii_alphanumeric()
            || matches!(character, '#' | '.' | '_' | '-' | '@' | '/'))
    })
    .filter_map(|token| {
        let token =
            token.trim_matches(|character| matches!(character, '.' | ',' | ';' | '!' | '?'));
        (!token.is_empty()).then(|| token.to_owned())
    })
    .collect()
}

fn quote_fragments(front_quote: Option<&str>, body: &str) -> Vec<String> {
    let mut spans = front_quote.map_or_else(Vec::new, |quote| vec![quote.to_owned()]);
    let mut current = String::new();
    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix('>') {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(rest.trim());
        } else if !current.is_empty() {
            spans.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        spans.push(current);
    }

    spans
        .into_iter()
        .flat_map(|span| {
            span.replace('\u{2026}', "...")
                .split("...")
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .map(|fragment| normalise(fragment.trim()))
        .map(|fragment| {
            fragment
                .trim_matches(|character: char| matches!(character, '"' | '\''))
                .to_owned()
        })
        .filter(|fragment| fragment.split_whitespace().count() >= MIN_QUOTE_WORDS)
        .collect()
}

fn normalise(text: &str) -> String {
    let cleaned = text
        .chars()
        .map(|character| match character {
            '\u{2018}' | '\u{2019}' | '\u{02bc}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2014}' | '\u{2013}' => '-',
            '*' | '_' => ' ',
            _ => character,
        })
        .collect::<String>();
    cleaned
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn match_one(arrival: &Arrival<'_>, cited: &Cited, owner: Option<&str>) -> Option<Match> {
    let is_pointer = cited.pointers.contains(&arrival.record.pointer)
        || arrival
            .record
            .thread
            .as_deref()
            .is_some_and(|thread| cited.pointers.contains(thread));
    let is_alias = arrival
        .record
        .aliases
        .iter()
        .any(|alias| cited.tokens.contains(alias));
    let is_quote = cited
        .quotes
        .iter()
        .any(|quote| arrival.anchor_texts.iter().any(|text| text.contains(quote)));
    let is_owner = owner == Some(cited.rel.as_str());
    let is_stream = owner.is_some()
        && arrival
            .record
            .label
            .as_deref()
            .is_some_and(|label| cited.tokens.contains(label));
    let how = if is_pointer {
        Some("pointer")
    } else if is_alias {
        Some("alias")
    } else if is_quote {
        Some("quote")
    } else if is_owner {
        Some("owner")
    } else if is_stream {
        Some("stream")
    } else {
        None
    };
    how.map(|how| Match {
        record: cited.rel.clone(),
        how: how.to_owned(),
    })
}

fn is_after_review(arrival: NaiveDate, last_reviewed: Option<NaiveDate>) -> bool {
    last_reviewed.is_none_or(|reviewed| arrival >= reviewed)
}

fn state_changes(
    tree: &Tree,
    docs: &BTreeMap<String, &Doc>,
    previous: Option<&Window>,
) -> Result<(Option<String>, Vec<StatusChange>, Vec<String>)> {
    let Some(previous) = previous else {
        return Ok((None, Vec::new(), Vec::new()));
    };
    let Some(value) = previous.snapshot()? else {
        return Ok((None, Vec::new(), Vec::new()));
    };
    let snapshot: OpenSnapshot =
        serde_json::from_value(value).context("decoding previous open snapshot")?;
    if snapshot.version != 1 {
        bail!(
            "previous open snapshot has unsupported version {}",
            snapshot.version
        );
    }
    let previously_open = snapshot
        .open
        .iter()
        .map(|record| record.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut closed = snapshot
        .open
        .iter()
        .filter_map(|was| {
            let now = docs.get(&was.path);
            (!now.is_some_and(|document| is_open(document))).then(|| StatusChange {
                record: was.path.clone(),
                title: was.title.clone(),
                was: was.status.clone(),
                now: now.and_then(|document| document.front.status.clone()),
            })
        })
        .collect::<Vec<_>>();
    closed.sort_by(|left, right| left.record.cmp(&right.record));
    let mut opened = tree
        .docs
        .iter()
        .filter(|document| is_open(document))
        .map(|document| document.rel.to_string_lossy().into_owned())
        .filter(|path| !previously_open.contains(path.as_str()))
        .collect::<Vec<_>>();
    opened.sort();
    Ok((Some(previous.id.to_string()), closed, opened))
}

/// Writes a snapshot and delta artifacts atomically into `window`.
///
/// # Errors
///
/// Returns an error when an artifact version is unsupported, or an artifact cannot be serialized
/// or atomically replaced.
pub fn write(window: &Window, snapshot: &OpenSnapshot, delta: &Delta) -> Result<()> {
    if snapshot.version != 1 {
        bail!("open snapshot has unsupported version {}", snapshot.version);
    }
    if delta.version != 1 {
        bail!("delta has unsupported version {}", delta.version);
    }
    let snapshot = serde_json::to_vec_pretty(snapshot).context("serializing open snapshot")?;
    write_replace(&window.dir.join("open-snapshot.json"), &snapshot)?;
    let json = serde_json::to_vec_pretty(delta).context("serializing delta")?;
    write_replace(&window.dir.join("delta.json"), &json)?;
    write_replace(&window.dir.join("delta.md"), render(delta).as_bytes())
}

/// Returns a concise description of a delta.
#[must_use]
pub fn summary(delta: &Delta) -> String {
    let matched = delta
        .new_messages
        .iter()
        .filter(|message| !message.matches.is_empty())
        .count();
    format!(
        "delta: {} new messages ({matched} matched to records), {} open items with replies after last review, {} closed since previous window, {} opened",
        delta.new_messages.len(),
        delta.unreviewed.len(),
        delta.closed_since_previous.len(),
        delta.opened_since_previous.len(),
    )
}

/// Renders a human-readable delta report.
#[must_use]
pub fn render(delta: &Delta) -> String {
    let mut lines = vec![
        format!(
            "# Delta: window {} (ends {}) against the tree at {}",
            delta.window, delta.window_end, delta.generated_at
        ),
        String::new(),
        "Mechanical, tree-evidence-only matching (pointer > alias > quote > owner > stream). A match is a lead to read, not a verdict; a message with no match is not necessarily new business.".to_owned(),
        String::new(),
    ];
    render_unreviewed(&mut lines, &delta.unreviewed);
    render_closed(&mut lines, delta);
    render_opened(&mut lines, &delta.opened_since_previous);
    render_messages(&mut lines, &delta.new_messages);
    lines.join("\n")
}

fn render_unreviewed(lines: &mut Vec<String>, flags: &[OpenFlag]) {
    lines.push(format!(
        "## Open items with replies after their last review ({})",
        flags.len()
    ));
    lines.push(String::new());
    if flags.is_empty() {
        lines.push("(none)".to_owned());
    }
    for flag in flags {
        let reviewed = flag
            .last_reviewed
            .map_or_else(|| "never".to_owned(), |date| date.to_string());
        lines.push(format!(
            "- [{}] `{}` — {} — last_reviewed {reviewed}, {} new message(s):",
            flag.kind,
            flag.record,
            flag.title,
            flag.replies.len()
        ));
        for reply in &flag.replies {
            let same_day = flag
                .last_reviewed
                .is_some_and(|reviewed| reply.ts.date_naive() == reviewed);
            let marker = if same_day {
                " (same day as the review — check)"
            } else {
                ""
            };
            let author = reply.author.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "  - {} {author} ({}) `{}`{marker}: {}",
                reply.ts, reply.how, reply.pointer, reply.excerpt
            ));
        }
    }
    lines.push(String::new());
}

fn render_closed(lines: &mut Vec<String>, delta: &Delta) {
    let previous = delta
        .previous_window
        .as_deref()
        .unwrap_or("(no previous snapshot)");
    lines.push(format!(
        "## Closed since previous window {previous} ({})",
        delta.closed_since_previous.len()
    ));
    lines.push(String::new());
    if delta.closed_since_previous.is_empty() {
        lines.push("(none)".to_owned());
    }
    for change in &delta.closed_since_previous {
        lines.push(format!(
            "- `{}` — {} — {} → {}",
            change.record,
            change.title,
            change.was,
            change.now.as_deref().unwrap_or("deleted")
        ));
    }
    lines.push(String::new());
}

fn render_opened(lines: &mut Vec<String>, opened: &[String]) {
    lines.push(format!(
        "## Opened since previous window ({})",
        opened.len()
    ));
    lines.push(String::new());
    if opened.is_empty() {
        lines.push("(none)".to_owned());
    }
    for path in opened {
        lines.push(format!("- `{path}`"));
    }
    lines.push(String::new());
}

fn render_messages(lines: &mut Vec<String>, messages: &[NewMessage]) {
    let unmatched = messages
        .iter()
        .filter(|message| message.matches.is_empty())
        .count();
    lines.push(format!(
        "## New messages and the records they belong to ({}; {unmatched} unmatched)",
        messages.len()
    ));
    lines.push(String::new());
    if messages.is_empty() {
        lines.push("(none)".to_owned());
    }
    for message in messages {
        let kind = if message.is_reply { "reply" } else { "message" };
        let targets = if message.matches.is_empty() {
            "(no record matched)".to_owned()
        } else {
            message
                .matches
                .iter()
                .map(|matched| format!("`{}` ({})", matched.record, matched.how))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let label = message.label.as_deref().unwrap_or("unlabeled");
        let author = message.author.as_deref().unwrap_or("unknown");
        lines.push(format!(
            "- {label} {} {author} {kind} → {targets}",
            message.ts
        ));
        lines.push(format!("  `{}` {}", message.pointer, message.excerpt));
    }
    lines.push(String::new());
}

/// Reads all durable deltas and returns replies still unreviewed by record path.
///
/// # Errors
///
/// Returns an error when a durable delta cannot be read or decoded.
pub fn staleness(windows: &Path, tree: &Tree) -> Result<BTreeMap<String, Vec<Staleness>>> {
    let mut stale = BTreeMap::new();
    if !windows.exists() {
        return Ok(stale);
    }
    let reviewed = tree
        .docs
        .iter()
        .filter(|document| is_open(document))
        .map(|document| {
            (
                document.rel.to_string_lossy().into_owned(),
                document.front.last_reviewed,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut completed = Vec::new();
    for entry in fs::read_dir(windows).with_context(|| format!("reading {}", windows.display()))? {
        let entry = entry.with_context(|| format!("reading {}", windows.display()))?;
        let Ok(window) = Window::open(entry.path()) else {
            continue;
        };
        if window.dir.join("delta.json").is_file() {
            completed.push(window);
        }
    }
    completed.sort_by(|left, right| left.id.cmp(&right.id));

    let mut seen = BTreeSet::new();
    for window in completed {
        let path = window.dir.join("delta.json");
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let delta: Delta = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        if delta.version != 1 {
            bail!(
                "{} has unsupported delta version {}",
                path.display(),
                delta.version
            );
        }
        for flag in delta.unreviewed {
            let Some(last_reviewed) = reviewed.get(&flag.record) else {
                continue;
            };
            for reply in flag.replies {
                if !seen.insert((flag.record.clone(), reply.pointer.clone())) {
                    continue;
                }
                let (keep, same_day) = match last_reviewed {
                    Some(reviewed) if reply.ts.date_naive() > *reviewed => (true, false),
                    Some(reviewed) if reply.ts.date_naive() == *reviewed => (true, true),
                    Some(_) => (false, false),
                    None => (true, false),
                };
                if keep {
                    stale
                        .entry(flag.record.clone())
                        .or_default()
                        .push(Staleness {
                            pointer: reply.pointer,
                            ts: reply.ts,
                            author: reply.author,
                            how: reply.how,
                            same_day,
                        });
                }
            }
        }
    }
    for replies in stale.values_mut() {
        replies.sort_by_key(|reply| reply.ts);
    }
    Ok(stale)
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT_CHARS {
        return flat;
    }
    let mut shortened = flat.chars().take(EXCERPT_CHARS).collect::<String>();
    shortened.push('\u{2026}');
    shortened
}
