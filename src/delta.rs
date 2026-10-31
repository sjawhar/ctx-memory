//! Per-window changes matched against tree records.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::config::SourceConfig;
use crate::embed::{cosine, Embed, MEANING_THRESHOLD};
use crate::exclude::Exclusion;
use crate::index;
use crate::output;
use crate::pointer::Schemes;
use crate::record::Record;
use crate::schema::{OPEN_KINDS, OPEN_STATES};
use crate::source::slack::Mentions;
use crate::source::{self, MAX_POINTER_BYTES};
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
    /// `pointer`, `alias`, `quote`, `owner`, `stream`, or `meaning`.
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
    /// How many new messages this copy leaves out to stay within one answer's size
    /// ([`Delta::fit`]): the newest ones, which the windows' own `delta.json` still list. A
    /// delta a fetch writes never leaves any out, and then the field is not written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_messages: Option<usize>,
    /// How many record status changes (`closed_since_previous`, then `opened_since_previous`),
    /// open flags, and record matches of a message listed alone this copy leaves out to stay
    /// within one answer's size; not written when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_changes: Option<usize>,
}

/// The most bytes the delta of one remote answer — `ctx pending`, `ctx delta` — takes on the
/// wire, as [`Delta::answer_len`] counts them.
///
/// A host may refuse a larger answer outright: Cloud Run refuses an HTTP/1 response over
/// 32 MiB. This leaves the rest of an answer, the window list of `ctx pending` included, room
/// under that.
pub const ANSWER_DELTA_BYTES: usize = 24 * 1024 * 1024;

/// What status changes may take of an answer: a quarter, so a tree with very many records
/// leaves room for the messages.
const CHANGES_SHARE: usize = 4;

impl Delta {
    /// Leaves out every message `exclusion` names, and every reply it names from the open
    /// flags, dropping a flag left with no reply.
    ///
    /// A delta written before a pattern existed still lists what it names; reading it through
    /// this is what lets an exclusion shrink a window that was already fetched, with its files
    /// as they are.
    pub fn exclude(&mut self, exclusion: &Exclusion) {
        if exclusion.is_empty() {
            return;
        }
        self.new_messages
            .retain(|message| !exclusion.matches(&message.pointer));
        self.retain_replies(|pointer| !exclusion.matches(pointer));
    }

    /// Holds a delta written before sources were held to [`MAX_POINTER_BYTES`] to it, as the
    /// protocol now holds every record: a message or reply whose own pointer is longer is left
    /// out, as no source may name it, and a thread pointer that is longer is dropped from the
    /// message that names it. Every answer quotes a pointer whole, so this is what bounds one.
    pub fn bound_pointers(&mut self) {
        self.new_messages
            .retain(|message| message.pointer.len() <= MAX_POINTER_BYTES);
        for message in &mut self.new_messages {
            if message
                .thread
                .as_ref()
                .is_some_and(|thread| thread.len() > MAX_POINTER_BYTES)
            {
                message.thread = None;
            }
        }
        self.retain_replies(|pointer| pointer.len() <= MAX_POINTER_BYTES);
    }

    /// Keeps the open flags' replies whose pointer `keep` admits, dropping a flag left with
    /// none.
    fn retain_replies(&mut self, keep: impl Fn(&str) -> bool) {
        for flag in &mut self.unreviewed {
            flag.replies.retain(|reply| keep(&reply.pointer));
        }
        self.unreviewed.retain(|flag| !flag.replies.is_empty());
    }

    /// The bytes this delta takes in a remote answer: printed the way `--json` prints it
    /// ([`output::write_json`]), and then escaped again into the JSON string the server answers
    /// with, where each `"` and `\` gains a backslash.
    #[must_use]
    pub fn answer_len(&self) -> usize {
        answer_len(self)
    }

    /// Leaves out the `count` oldest new messages, with the open flags' replies that are among
    /// them: what an earlier answer already listed (`ctx pending --offset`).
    pub fn skip(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.new_messages
            .drain(..count.min(self.new_messages.len()));
        self.keep_replies_to_listed();
    }

    /// Cuts the delta to at most `budget` answer bytes ([`Delta::answer_len`]), keeping the
    /// oldest new messages that fit, with the open flags' replies that are among them, and
    /// counts the messages it left out in `omitted_messages`. A flag left with no reply is
    /// dropped: the messages that made the record stale are among those left out. The record
    /// status changes keep at most a quarter of the budget, and `omitted_changes` counts any
    /// left out.
    ///
    /// Messages come oldest first, so what an answer leaves out is the newest: `ctx pending
    /// --offset` lists it next. An answer always lists at least one message when any remain,
    /// so following `next_offset` always moves on; a message that alone passes the budget is
    /// listed with the open flags and record matches that fit (`fit_alone`).
    pub fn fit(&mut self, budget: usize) {
        if self.answer_len() <= budget {
            return;
        }
        self.fit_changes(budget / CHANGES_SHARE);
        // What each message costs in replies, and which flags its replies open: a flag's own
        // fields are charged once, to the first message kept that replies under it.
        let mut replies = BTreeMap::<&str, (usize, Vec<usize>)>::new();
        let mut headers = Vec::with_capacity(self.unreviewed.len());
        for (index, flag) in self.unreviewed.iter().enumerate() {
            headers.push(
                answer_len(&OpenFlag {
                    record: flag.record.clone(),
                    kind: flag.kind.clone(),
                    status: flag.status.clone(),
                    title: flag.title.clone(),
                    owner: flag.owner.clone(),
                    last_reviewed: flag.last_reviewed,
                    replies: Vec::new(),
                }) + 1,
            );
            for reply in &flag.replies {
                let entry = replies.entry(reply.pointer.as_str()).or_default();
                entry.0 += answer_len(reply) + 1;
                entry.1.push(index);
            }
        }
        // Everything but the messages and the flags, with the counts' widest values, so the
        // counts this writes never push the answer past the budget.
        let frame = Self {
            version: self.version,
            window: self.window.clone(),
            window_end: self.window_end,
            generated_at: self.generated_at,
            previous_window: self.previous_window.clone(),
            new_messages: Vec::new(),
            unreviewed: Vec::new(),
            closed_since_previous: self.closed_since_previous.clone(),
            opened_since_previous: self.opened_since_previous.clone(),
            omitted_messages: Some(usize::MAX),
            omitted_changes: Some(usize::MAX),
        };
        let mut used = frame.answer_len();
        let mut opened = vec![false; headers.len()];
        let mut kept = 0;
        for message in &self.new_messages {
            let (reply_bytes, flags) = replies
                .get(message.pointer.as_str())
                .map_or((0, &[][..]), |(bytes, flags)| (*bytes, flags.as_slice()));
            let header_bytes: usize = flags
                .iter()
                .filter(|index| !opened.get(**index).copied().unwrap_or(true))
                .filter_map(|index| headers.get(*index))
                .sum();
            let cost = answer_len(message) + 1 + reply_bytes + header_bytes;
            if kept > 0 && used + cost > budget {
                break;
            }
            used += cost;
            kept += 1;
            for index in flags {
                if let Some(open) = opened.get_mut(*index) {
                    *open = true;
                }
            }
        }
        let left_out = self.new_messages.len() - kept;
        self.omitted_messages = Some(self.omitted_messages.unwrap_or_default() + left_out);
        self.new_messages.truncate(kept);
        self.keep_replies_to_listed();
        self.fit_alone(budget);
    }

    /// Holds a delta whose one message, listed alone, still passes `budget` to it: it keeps
    /// the open flags that fit, oldest first, then the message's record matches that fit, and
    /// counts what it leaves out in `omitted_changes`. Then, if the message's own text still
    /// passes it, its author and label are cut to an excerpt's length: a source can name either
    /// with any number of characters, and they only describe the message, which the window's
    /// files hold whole.
    fn fit_alone(&mut self, budget: usize) {
        let mut over = self.answer_len().saturating_sub(budget);
        if over == 0 {
            return;
        }
        let mut left_out = 0;
        while over > 0 {
            let Some(flag) = self.unreviewed.pop() else {
                break;
            };
            over = over.saturating_sub(answer_len(&flag) + 1);
            left_out += 1;
        }
        for message in &mut self.new_messages {
            while over > 0 {
                let Some(found) = message.matches.pop() else {
                    break;
                };
                over = over.saturating_sub(answer_len(&found) + 1);
                left_out += 1;
            }
        }
        self.omitted_changes = Some(self.omitted_changes.unwrap_or_default() + left_out);
        if self.answer_len() > budget {
            for message in &mut self.new_messages {
                message.author = message.author.as_deref().map(excerpt);
                message.label = message.label.as_deref().map(excerpt);
                message.excerpt = excerpt(&message.excerpt);
            }
        }
    }

    /// Keeps the status changes, closed ones first, within `budget` answer bytes, and counts
    /// the rest in `omitted_changes`.
    fn fit_changes(&mut self, budget: usize) {
        let mut used = 0;
        let mut left_out = 0;
        self.closed_since_previous.retain(|change| {
            used += answer_len(change) + 1;
            let keep = used <= budget;
            left_out += usize::from(!keep);
            keep
        });
        self.opened_since_previous.retain(|path| {
            used += answer_len(path) + 1;
            let keep = used <= budget;
            left_out += usize::from(!keep);
            keep
        });
        if left_out > 0 {
            self.omitted_changes = Some(self.omitted_changes.unwrap_or_default() + left_out);
        }
    }

    /// Keeps the open flags' replies to the messages listed, dropping a flag left with none.
    fn keep_replies_to_listed(&mut self) {
        let listed = self
            .new_messages
            .iter()
            .map(|message| message.pointer.as_str())
            .collect::<BTreeSet<_>>();
        for flag in &mut self.unreviewed {
            flag.replies
                .retain(|reply| listed.contains(reply.pointer.as_str()));
        }
        self.unreviewed.retain(|flag| !flag.replies.is_empty());
    }

    /// Shows each message's excerpt as cut from the text ctx serves for the message now
    /// ([`Mentions`]): a Slack message's mentions read with the names the store holds, even in
    /// a window fetched before mentions were named. The excerpt is cut from the stored version
    /// the window's delta was written from; a message the store holds in no such version
    /// keeps the excerpt the delta wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn serve(&mut self, mentions: &Mentions<'_>) -> Result<()> {
        for message in &mut self.new_messages {
            serve_excerpt(&message.pointer, &mut message.excerpt, mentions)?;
        }
        for flag in &mut self.unreviewed {
            for reply in &mut flag.replies {
                serve_excerpt(&reply.pointer, &mut reply.excerpt, mentions)?;
            }
        }
        Ok(())
    }
}

/// Counts the bytes written to it, and how many of them are `"` or `\`, and keeps none.
#[derive(Default)]
struct AnswerCount {
    bytes: usize,
    escaped: usize,
}

impl std::io::Write for AnswerCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes += bytes.len();
        self.escaped += bytes
            .iter()
            .filter(|byte| matches!(byte, b'"' | b'\\'))
            .count();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes `value` takes in a remote answer, counted without building them: printed as
/// `--json` prints it, then with a backslash more for each `"` and `\` once the server wraps
/// the printed text in a JSON string. Printed JSON holds no raw control character, so nothing
/// else grows in that second escaping.
pub fn answer_len_of(value: &impl Serialize) -> usize {
    let mut count = AnswerCount::default();
    // Writing to a counter cannot fail, and every type here serializes; a failure would only
    // make the count short.
    let _ = output::write_json(&mut count, value);
    count.bytes + count.escaped
}

fn answer_len(value: &impl Serialize) -> usize {
    answer_len_of(value)
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
/// A message `exclusion` names is no arrival: it is not listed, matched, or counted against an
/// open record.
///
/// # Errors
///
/// Returns an error when the window or its prior snapshot cannot be read or decoded.
#[allow(clippy::too_many_arguments)]
pub fn compute(
    tree: &Tree,
    schemes: &Schemes,
    owners: &Owners,
    exclusion: &Exclusion,
    window: &Window,
    previous: Option<&Window>,
    generated_at: DateTime<Utc>,
    embedder: Option<&dyn Embed>,
) -> Result<Delta> {
    let manifest = window.manifest()?;
    let mut sourced_records = window.records()?;
    sourced_records.retain(|sourced| !exclusion.matches(&sourced.record.pointer));
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
    let arrivals = arrivals(&sourced_records);
    // Meaning matching is best-effort: a window still yields its text matches when the
    // embedder cannot be reached, rather than failing the fetch.
    let meaning = embedder.and_then(|embedder| meaning_vectors(embedder, &arrivals, &docs).ok());
    let (new_messages, flagged) =
        match_arrivals(arrivals, &cited_records, owners, &docs, meaning.as_ref());
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
        omitted_messages: None,
        omitted_changes: None,
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
    meaning: Option<&MeaningVectors>,
) -> (Vec<NewMessage>, BTreeMap<String, Vec<ReplyRef>>) {
    let mut new_messages = Vec::with_capacity(arrivals.len());
    let mut flagged = BTreeMap::<String, Vec<ReplyRef>>::new();
    for (index, arrival) in arrivals.into_iter().enumerate() {
        let owner = owners.get(arrival.source, &arrival.record.stream);
        let mut matches = cited_records
            .iter()
            .filter_map(|cited| match_one(&arrival, cited, owner))
            .collect::<Vec<_>>();
        if let Some(meaning) = meaning {
            append_meaning_matches(index, meaning, &mut matches);
        }
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

/// The embeddings a window's meaning matching needs: one vector per arrival, in arrival order,
/// and one vector per tree record, keyed by its path.
struct MeaningVectors {
    arrivals: Vec<Vec<f32>>,
    records: Vec<(String, Vec<f32>)>,
}

/// Embeds every arrival and every record, so an arrival can be matched to the record it means
/// even when the two share no word. Both sets are embedded in one batch each, and the embedder
/// caches by content hash, so a record embedded for the index is not embedded again here.
fn meaning_vectors(
    embedder: &dyn Embed,
    arrivals: &[Arrival<'_>],
    docs: &BTreeMap<String, &Doc>,
) -> Result<MeaningVectors> {
    let record_texts = docs
        .values()
        .map(|document| index::document_text(document))
        .collect::<Vec<_>>();
    let record_refs = record_texts.iter().map(String::as_str).collect::<Vec<_>>();
    let record_vectors = embedder.embed(&record_refs)?;
    let arrival_refs = arrivals
        .iter()
        .map(|arrival| arrival.record.text.as_str())
        .collect::<Vec<_>>();
    let arrival_vectors = embedder.embed(&arrival_refs)?;
    let records = docs.keys().cloned().zip(record_vectors).collect::<Vec<_>>();
    Ok(MeaningVectors {
        arrivals: arrival_vectors,
        records,
    })
}

/// Adds a `meaning` match for every record whose vector is close to this arrival's and that no
/// text rule has already matched, so a paraphrase is never listed twice.
fn append_meaning_matches(index: usize, meaning: &MeaningVectors, matches: &mut Vec<Match>) {
    let Some(arrival_vector) = meaning.arrivals.get(index) else {
        return;
    };
    for (record, record_vector) in &meaning.records {
        if matches.iter().any(|matched| &matched.record == record) {
            continue;
        }
        if cosine(arrival_vector, record_vector) >= MEANING_THRESHOLD {
            matches.push(Match {
                record: record.clone(),
                how: "meaning".to_owned(),
            });
        }
    }
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

/// Reads one window's durable delta, if it has been written, without the messages `exclusion`
/// names, for an answer that hands its messages to an agent (`ctx pending`, `ctx delta`).
///
/// A message whose pointer is longer than [`MAX_POINTER_BYTES`], which only a delta written
/// before sources were held to it can carry, is refused rather than left out: an answer
/// cannot quote it whole, and leaving it out would let the marker pass a message no agent saw.
///
/// # Errors
///
/// Returns an error when the delta cannot be read, decoded, or uses another format version,
/// or lists a message whose pointer is past the bound.
pub fn read(window: &Window, exclusion: &Exclusion) -> Result<Option<Delta>> {
    let Some(mut delta) = read_file(window)? else {
        return Ok(None);
    };
    // What the deployment excludes is not handed to an agent, so it is not refused either.
    delta.exclude(exclusion);
    if let Some(long) = delta
        .new_messages
        .iter()
        .find(|message| message.pointer.len() > MAX_POINTER_BYTES)
    {
        bail!(
            "window {} lists a message whose pointer ({}…) is longer than {MAX_POINTER_BYTES} bytes, which no source may name now and no answer can quote whole; remove that record from the window's records.jsonl and run `ctx delta --window {} --write`",
            window.id,
            long.pointer.chars().take(64).collect::<String>(),
            window.id
        );
    }
    delta.bound_pointers();
    Ok(Some(delta))
}

/// Reads one window's durable delta, if it has been written, as the file holds it.
fn read_file(window: &Window) -> Result<Option<Delta>> {
    let path = window.dir.join("delta.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let delta: Delta =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    if delta.version != 1 {
        bail!(
            "{} has unsupported delta version {}",
            path.display(),
            delta.version
        );
    }
    Ok(Some(delta))
}

/// Combines the deltas of consecutive windows, oldest first, into one delta.
///
/// A message, open flag, or status change that several windows carry appears once, as the
/// newest window saw it, so an agent handling a backlog of windows reads one list.
#[must_use]
pub fn combine(deltas: &[Delta], generated_at: DateTime<Utc>) -> Option<Delta> {
    let newest = deltas.last()?;
    let oldest = deltas.first()?;
    let mut new_messages = BTreeMap::new();
    let mut unreviewed: BTreeMap<String, OpenFlag> = BTreeMap::new();
    let mut closed = BTreeMap::new();
    let mut opened = BTreeSet::new();
    for delta in deltas {
        for message in &delta.new_messages {
            new_messages.insert(message.pointer.clone(), message.clone());
        }
        for flag in &delta.unreviewed {
            match unreviewed.get_mut(&flag.record) {
                Some(existing) => {
                    for reply in &flag.replies {
                        if !existing
                            .replies
                            .iter()
                            .any(|kept| kept.pointer == reply.pointer)
                        {
                            existing.replies.push(reply.clone());
                        }
                    }
                    existing.replies.sort_by(|left, right| {
                        left.ts
                            .cmp(&right.ts)
                            .then_with(|| left.pointer.cmp(&right.pointer))
                    });
                    existing.last_reviewed = flag.last_reviewed;
                    existing.status.clone_from(&flag.status);
                }
                None => {
                    unreviewed.insert(flag.record.clone(), flag.clone());
                }
            }
        }
        for change in &delta.closed_since_previous {
            closed.insert(change.record.clone(), change.clone());
        }
        opened.extend(delta.opened_since_previous.iter().cloned());
    }
    let mut new_messages = new_messages.into_values().collect::<Vec<_>>();
    new_messages.sort_by(|left, right| {
        left.ts
            .cmp(&right.ts)
            .then_with(|| left.pointer.cmp(&right.pointer))
    });
    Some(Delta {
        version: 1,
        window: newest.window.clone(),
        window_end: newest.window_end,
        generated_at,
        previous_window: oldest.previous_window.clone(),
        new_messages,
        unreviewed: unreviewed.into_values().collect(),
        closed_since_previous: closed.into_values().collect(),
        opened_since_previous: opened.into_iter().collect(),
        omitted_messages: None,
        omitted_changes: None,
    })
}

/// Reads all durable deltas and returns replies still unreviewed by record path, leaving out
/// every reply `exclusion` names.
///
/// # Errors
///
/// Returns an error when a durable delta cannot be read or decoded.
pub fn staleness(
    windows: &Path,
    tree: &Tree,
    exclusion: &Exclusion,
) -> Result<BTreeMap<String, Vec<Staleness>>> {
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
        // Staleness only reports replies, so a pointer past the bound is left out here rather
        // than failing `ctx open` for the whole tree; `read` refuses it where messages are
        // handed to an agent.
        let Some(mut delta) = read_file(&window)? else {
            continue;
        };
        delta.bound_pointers();
        delta.exclude(exclusion);
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

/// Replaces `written`, the excerpt a window's delta holds for `pointer`, with the excerpt of
/// the text ctx serves for the stored version it was cut from, when that differs.
fn serve_excerpt(pointer: &str, written: &mut String, mentions: &Mentions<'_>) -> Result<()> {
    // Every mention reads with `@` or `#`, and one an earlier renderer left as Slack wrote it
    // keeps its `<`: an excerpt with none of them shows no mention, so no name changes it.
    if !written.contains(['@', '#', '<']) {
        return Ok(());
    }
    let cut = written.as_str();
    if let Some(text) = mentions.version_text(pointer, |stored| excerpt(stored) == cut)? {
        *written = excerpt(&text);
    }
    Ok(())
}
