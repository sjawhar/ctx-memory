//! Evidence models and operations shared by provenance and quote judgment.

use std::collections::BTreeSet;

use anyhow::Result;
use serde_json::Value;

use crate::pointer::{self, Pointer, Schemes};
use crate::record::Record;
use crate::source::slack::{Mentions, Spelled};
use crate::store::Store;
use crate::tree::{Doc, Tier};

use super::lookup::local_records;
use super::text_start;
/// What resolving one cited source pointer learned about the text behind it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceStatus {
    /// The source stores its records verbatim and this pointer's text is loaded, so a quote
    /// can be compared against it.
    Verbatim,
    /// The source stores its records verbatim, but this pointer resolved to nothing or its
    /// source failed. The pointer pass has already reported it as broken.
    Reported,
    /// The source has declared that its records are not its own words, so its text can never
    /// settle a quote: the built-in calendar renders an event, and a custom source may declare
    /// the same of itself.
    NotVerbatim,
    /// No hello has declared whether this source's records are its own words. Offline, "I have
    /// never heard of this source" cannot be told apart from "this source is not verbatim",
    /// and only the second makes skipping a quote safe.
    Undeclared,
}

/// A source pointer the record cites, with the byte offset where it cites it.
pub(super) struct CitedSource {
    pub(super) pointer: Pointer,
    pub(super) start: usize,
    pub(super) status: SourceStatus,
}

/// The provenance a record declares for itself: every string it wrote under `source` and
/// `sources`, and the pointers among them. These carry the record's single `quote`.
pub(super) struct RecordSources {
    pub(super) texts: Vec<String>,
    pub(super) pointers: Vec<Pointer>,
}

/// What one document's pointer pass produced, which the quote pass then judges.
pub(super) struct Evidence {
    /// Every source pointer the record cites, and what resolving it learned.
    pub(super) cited: Vec<CitedSource>,
    /// The loaded text of each source a quote may be checked against.
    pub(super) quote_sources: Vec<QuoteSource>,
    /// The provenance the record declares for itself.
    pub(super) record: RecordSources,
}

/// What a claim's declared provenance amounts to: text its quote can be compared against, or
/// the reason nothing can compare it.
pub(super) enum Provenance<'a> {
    /// Loaded verbatim text to compare the quote against.
    Checkable(Vec<&'a QuoteSource>),
    /// Every pointer the claim names was already reported as broken by the pointer pass.
    Reported,
    /// The claim names a source whose text can never settle it.
    Uncheckable {
        start: usize,
        pointer: String,
        why: &'static str,
    },
    /// The claim names no pointer at all, which only a summary or a secondhand account may do.
    Untraceable { start: usize, pointer: String },
}
/// The text a cited pointer resolves to, and every other text a quote may have been taken
/// from, as readings a quote is checked against: for each version of each of its records, the
/// stored text, the source's own text, the text ctx serves, and, for a Slack message, its
/// text with each mention spelled every way that names that one person ([`Mentions::spelled`]).
pub(super) struct QuoteSource {
    pub(super) pointer: Pointer,
    pub(super) start: usize,
    readings: Vec<Reading>,
}

impl QuoteSource {
    /// Reads the thread of a pointer as `ctx resolve --thread` serves it, with every earlier
    /// version of its records.
    pub(super) fn load(
        store: &Store,
        schemes: &Schemes,
        mentions: &Mentions<'_>,
        pointer: Pointer,
        start: usize,
    ) -> Result<Self> {
        let records = local_records(store, schemes, &pointer, true)?;
        let mut seen = BTreeSet::new();
        let mut thread = Vec::new();
        for (index, record) in records.iter().enumerate() {
            if index > 0 {
                thread.push(Form::Text("\n".to_owned()));
            }
            seen.insert(version_key(record));
            thread.push(Form::Either(readings_of(record, mentions)?));
        }
        let mut readings = vec![Reading::new(&thread)];
        for record in &records {
            let Some(parsed) = schemes.parse(&record.pointer) else {
                continue;
            };
            for (_, version) in store.versions(&parsed)? {
                if seen.insert(version_key(&version)) {
                    readings.push(Reading::new(&[Form::Either(readings_of(
                        &version, mentions,
                    )?)]));
                }
            }
        }
        Ok(Self {
            pointer,
            start,
            readings,
        })
    }

    /// Reports whether the quote reads in the thread as served, or in one of the other texts
    /// it may have been taken from. A quote taken before an edit stays verifiable, and so do
    /// one taken while a Slack mention read as its id, one taken from what ctx served when it
    /// named that mention, and one taken before that person's name changed. A mention in the
    /// quote holds only as a spelling of the person the message mentions there.
    pub(super) fn holds(&self, quote: &str) -> bool {
        let quote = normalise_quote_text(quote);
        let fragments = fragments(&quote)
            .map(|fragment| fragment.chars().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        !fragments.is_empty()
            && self
                .readings
                .iter()
                .any(|reading| reading.holds(&fragments))
    }
}

/// What tells one stored version of a record from another for a quote: its text and the
/// source's own text.
fn version_key(record: &Record) -> (String, Option<String>) {
    (record.text.clone(), source_text(record).map(str::to_owned))
}

/// Each way a reader may have read `record`: its stored text, the source's own text, the text
/// ctx serves for it, and its mentions each spelled every way that names that person.
fn readings_of(record: &Record, mentions: &Mentions<'_>) -> Result<Vec<Vec<Form>>> {
    let mut readings = vec![vec![Form::Text(record.text.clone())]];
    if let Some(own) = source_text(record) {
        readings.push(vec![Form::Text(own.to_owned())]);
    }
    if let Some(served) = mentions.text(record)? {
        readings.push(vec![Form::Text(served)]);
    }
    if let Some(spelled) = mentions.spelled(record)? {
        readings.push(
            spelled
                .into_iter()
                .map(|part| match part {
                    Spelled::Text(text) => Form::Text(text),
                    Spelled::OneOf(spellings) => Form::Either(
                        spellings
                            .into_iter()
                            .map(|spelling| vec![Form::Text(spelling)])
                            .collect(),
                    ),
                })
                .collect(),
        );
    }
    Ok(readings)
}

/// The source's own text of a record, as its `raw` keeps it: for Slack, the message as Slack
/// sent it, each mention still its id, where the record's text names the person.
fn source_text(record: &Record) -> Option<&str> {
    record.raw.get("text").and_then(Value::as_str)
}

/// A text a quote is compared against: plain text, and places it reads one of several ways.
enum Form {
    Text(String),
    Either(Vec<Vec<Form>>),
}

/// The texts a [`Form`] reads as, normalised as a quote is, as one graph of characters: each
/// character leads to those that may follow it, and the index past the last character is the
/// end. A character only ever leads to a later one.
struct Reading {
    characters: Vec<char>,
    next: Vec<Vec<usize>>,
}

impl Reading {
    fn new(forms: &[Form]) -> Self {
        let mut reading = ReadingBuilder::default();
        reading.add(forms);
        reading.finish()
    }

    /// Whether each of `fragments` reads along the graph, each starting where the one before
    /// it ended or later, the first anywhere.
    fn holds(&self, fragments: &[Vec<char>]) -> bool {
        let end = self.characters.len();
        let mut from = vec![true; end + 1];
        for (index, fragment) in fragments.iter().enumerate() {
            let mut at = (0..=end)
                .filter(|position| from.get(*position) == Some(&true))
                .collect::<Vec<_>>();
            for character in fragment {
                let mut after = Vec::new();
                for position in at {
                    if self.characters.get(position) == Some(character) {
                        after.extend(self.next.get(position).into_iter().flatten().copied());
                    }
                }
                after.sort_unstable();
                after.dedup();
                if after.is_empty() {
                    return false;
                }
                at = after;
            }
            if index + 1 < fragments.len() {
                from = vec![false; end + 1];
                for position in at {
                    if let Some(reached) = from.get_mut(position) {
                        *reached = true;
                    }
                }
                for position in 0..end {
                    if from.get(position) != Some(&true) {
                        continue;
                    }
                    for following in self.next.get(position).into_iter().flatten() {
                        if let Some(reached) = from.get_mut(*following) {
                            *reached = true;
                        }
                    }
                }
            }
        }
        true
    }
}

/// Builds a [`Reading`]: `open` holds the characters that lead to whichever comes next.
#[derive(Default)]
struct ReadingBuilder {
    characters: Vec<char>,
    next: Vec<Vec<usize>>,
    open: Vec<usize>,
}

impl ReadingBuilder {
    fn add(&mut self, forms: &[Form]) {
        for form in forms {
            match form {
                Form::Text(text) => {
                    for character in normalise_piece(text).chars() {
                        self.push(character);
                    }
                }
                Form::Either(branches) => {
                    let incoming = std::mem::take(&mut self.open);
                    let mut outgoing = Vec::new();
                    for branch in branches {
                        self.open.clone_from(&incoming);
                        self.add(branch);
                        outgoing.append(&mut self.open);
                    }
                    outgoing.sort_unstable();
                    outgoing.dedup();
                    self.open = outgoing;
                }
            }
        }
    }

    fn push(&mut self, character: char) {
        let node = self.characters.len();
        self.characters.push(character);
        self.next.push(Vec::new());
        for open in std::mem::replace(&mut self.open, vec![node]) {
            if let Some(next) = self.next.get_mut(open) {
                next.push(node);
            }
        }
    }

    /// Ends the graph, and lets a space that leads to another space lead past it instead, so
    /// texts read on as one normalised text, with no run of two spaces.
    fn finish(mut self) -> Reading {
        let end = self.characters.len();
        for open in std::mem::take(&mut self.open) {
            if let Some(next) = self.next.get_mut(open) {
                next.push(end);
            }
        }
        for node in (0..end).rev() {
            if self.characters.get(node) != Some(&' ') {
                continue;
            }
            let Some(following) = self.next.get_mut(node).map(std::mem::take) else {
                continue;
            };
            let mut collapsed = Vec::with_capacity(following.len());
            for successor in following {
                if self.characters.get(successor) == Some(&' ') {
                    collapsed.extend(self.next.get(successor).into_iter().flatten().copied());
                } else {
                    collapsed.push(successor);
                }
            }
            collapsed.sort_unstable();
            collapsed.dedup();
            if let Some(next) = self.next.get_mut(node) {
                *next = collapsed;
            }
        }
        Reading {
            characters: self.characters,
            next: self.next,
        }
    }
}

/// One quoted claim to verify: the quote, the source text the record wrote beside it, and the
/// authority the claim declares.
pub(super) struct QuoteClaim<'a> {
    pub(super) quote: &'a str,
    /// The `source` of a `quotes` entry, as the record wrote it. `None` for the single
    /// `quote`, which the record's own `source` and `sources` carry.
    pub(super) text: Option<&'a str>,
    /// The tier this claim declares, or the record's own when the claim declares none.
    pub(super) tier: Option<Tier>,
}

/// Why a quote was not checked. A read command reports these; the single writer refuses every
/// one of them, because a committed record whose quote nothing verified is indistinguishable
/// afterwards from one that was verified.
pub(super) const UNDECLARED: &str = "the source has not declared whether its records are verbatim";
pub(super) const NOT_VERBATIM: &str =
    "the source does not store its records verbatim, so nothing can check this quote";
pub(super) const UNREGISTERED_SCHEME: &str =
    "no configured source owns this scheme, so nothing resolves it";
pub(super) const UNRESOLVED_POINTER: &str =
    "this record resolved no text for the pointer the claim names";
pub(super) const UNREAD_SOURCE_LINK: &str =
    "this links a source's message in a form ctx does not read as a pointer, so nothing checks \
     it: cite the message's pointer or its permalink";
pub(super) const MALFORMED_WEB_ADDRESS: &str =
    "a source that holds a web address must be that address alone: one http or https URL with \
     a host, no user name, no whitespace and no control character";
pub(super) const NO_SOURCE: &str = "a tier 1 or 2 quote must name a source that resolves";
/// Why a record that ranks at tier 1 or 2 is refused when nothing bears that out.
pub(super) const UNQUOTED_TIER: &str =
    "a tier 1 or 2 record holds a source's own words, an outside \
     party's at tier 1 and a colleague's at tier 2, and a record without a quote holds neither";
pub(super) const UNCHECKED_TIER: &str =
    "a tier 1 or 2 record must carry a tier 1 or 2 quote that was \
     checked against its source, and none of its quotes is one";

/// The fewest words a quote fragment holds for its match to count as evidence.
///
/// A quote is compared with its source by substring search over normalised text, so a
/// fragment as short as `ok` matches inside `book` or `token`, and `a ... b` matches nearly any
/// message: such a match proves nothing about what the source said. Three words in order rarely
/// appear in one cited message by chance.
pub(super) const FRAGMENT_WORDS: usize = 3;

/// The fewest letters and digits a quote fragment holds, whatever its word count, for its match
/// to count as evidence: an identifier, a figure or a long word of this length is as unlikely
/// to appear by chance as three words are. Punctuation is not counted, so padding cannot reach
/// it.
pub(super) const FRAGMENT_CHARACTERS: usize = 12;

/// What a claim that writes no source at all is reported as.
pub(super) const NO_SOURCE_POINTER: &str = "(no source)";
/// The provenance the record declares for itself: every string under `source` and `sources`,
/// and the pointers among them. A record that writes its sources only inside its `quotes`
/// list still carries its single `quote`, so those pointers stand in when it declares none.
pub(super) fn record_sources(
    document: &Doc,
    schemes: &Schemes,
    source_pointers: Vec<Pointer>,
) -> Result<RecordSources> {
    let texts = record_source_texts(document)?;
    if texts.is_empty() {
        return Ok(RecordSources {
            texts,
            pointers: source_pointers,
        });
    }
    let mut pointers: Vec<Pointer> = Vec::new();
    for found in texts.iter().flat_map(|text| schemes.find_all(text)) {
        if !pointers.contains(&found.pointer) {
            pointers.push(found.pointer);
        }
    }
    Ok(RecordSources { texts, pointers })
}

/// Every non-empty string the record writes under `source` and `sources`.
fn record_source_texts(document: &Doc) -> Result<Vec<String>> {
    let frontmatter = document.frontmatter_json()?;
    let mut texts = Vec::new();
    let Some(fields) = frontmatter.as_object() else {
        return Ok(texts);
    };
    for field in ["source", "sources"] {
        if let Some(value) = fields.get(field) {
            collect_source_texts(value, &mut texts);
        }
    }
    Ok(texts)
}

fn collect_source_texts(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.trim().is_empty() {
                texts.push(text.clone());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_source_texts(value, texts);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_source_texts(value, texts);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
/// What one claim's declared provenance amounts to.
///
/// A `quotes` entry is judged on the source it names alone: another entry's verbatim citation
/// is not evidence for this one. The single `quote` is judged on what the record declares for
/// itself.
pub(super) fn claim_provenance<'a>(
    claim: &QuoteClaim<'_>,
    document: &Doc,
    schemes: &Schemes,
    evidence: &'a Evidence,
    record_texts: &[&str],
) -> Provenance<'a> {
    let Some(text) = claim.text else {
        return source_provenance(
            document,
            claim,
            record_texts,
            &evidence.record.pointers,
            evidence,
        );
    };
    let pointers: Vec<Pointer> = schemes
        .find_all(text)
        .into_iter()
        .map(|found| found.pointer)
        .collect();
    source_provenance(
        document,
        claim,
        std::slice::from_ref(&text),
        &pointers,
        evidence,
    )
}

/// Judges one claim against the sources it names.
fn source_provenance<'a>(
    document: &Doc,
    claim: &QuoteClaim<'_>,
    texts: &[&str],
    pointers: &[Pointer],
    evidence: &'a Evidence,
) -> Provenance<'a> {
    if pointers.is_empty() {
        return untraceable(document, claim, texts);
    }
    let checkable: Vec<&QuoteSource> = pointers
        .iter()
        .filter_map(|pointer| {
            evidence
                .quote_sources
                .iter()
                .find(|source| &source.pointer == pointer)
        })
        .collect();
    if !checkable.is_empty() {
        return Provenance::Checkable(checkable);
    }
    let cited: Vec<&CitedSource> = pointers
        .iter()
        .filter_map(|pointer| {
            evidence
                .cited
                .iter()
                .find(|source| &source.pointer == pointer)
        })
        .collect();
    for (status, why) in [
        (SourceStatus::Undeclared, UNDECLARED),
        (SourceStatus::NotVerbatim, NOT_VERBATIM),
    ] {
        if let Some(source) = cited.iter().find(|source| source.status == status) {
            return Provenance::Uncheckable {
                start: source.start,
                pointer: source.pointer.to_string(),
                why,
            };
        }
    }
    if cited
        .iter()
        .any(|source| source.status == SourceStatus::Reported)
    {
        return Provenance::Reported;
    }
    // The claim names a registered pointer that this record's pointer pass never resolved, so
    // nothing read the text behind it.
    let pointer = pointers
        .first()
        .map_or_else(String::new, Pointer::to_string);
    Provenance::Uncheckable {
        start: text_start(document, &pointer),
        pointer,
        why: UNRESOLVED_POINTER,
    }
}

/// Judges a claim whose source text names no registered pointer. A pointer-shaped text claims
/// a provenance no configured source can check, and so do a text that holds a web address but
/// is not that one valid address alone, and an address whose own host is a source's, in a form
/// the scanner could not read as a pointer. Free text claims none at all, and neither does a
/// web address: it names where a reader can open the claim, which nothing here fetches.
fn untraceable(document: &Doc, claim: &QuoteClaim<'_>, texts: &[&str]) -> Provenance<'static> {
    let uncheckable = |text: &str, why| Provenance::Uncheckable {
        start: text_start(document, text),
        pointer: text.to_owned(),
        why,
    };
    if let Some(text) = texts
        .iter()
        .find(|text| pointer::holds_web_scheme(text) && !pointer::is_web_address(text))
    {
        return uncheckable(text, MALFORMED_WEB_ADDRESS);
    }
    if let Some(text) = texts
        .iter()
        .find(|text| pointer::is_web_address(text) && pointer::is_source_link(text))
    {
        return uncheckable(text, UNREAD_SOURCE_LINK);
    }
    if let Some(text) = texts
        .iter()
        .find(|text| pointer::is_pointer_shaped(text) && !pointer::is_web_address(text))
    {
        return uncheckable(text, UNREGISTERED_SCHEME);
    }
    match texts.first().copied() {
        Some(text) => Provenance::Untraceable {
            start: text_start(document, text),
            pointer: text.to_owned(),
        },
        None => Provenance::Untraceable {
            start: text_start(document, claim.quote),
            pointer: NO_SOURCE_POINTER.to_owned(),
        },
    }
}
pub(super) fn source_pointers(document: &Doc, schemes: &Schemes) -> Result<Vec<Pointer>> {
    let frontmatter = document.frontmatter_json()?;
    let mut pointers = Vec::new();
    let Some(fields) = frontmatter.as_object() else {
        return Ok(pointers);
    };
    for field in ["source", "sources", "quotes"] {
        if let Some(value) = fields.get(field) {
            collect_source_pointers(value, schemes, &mut pointers);
        }
    }
    Ok(pointers)
}

fn collect_source_pointers(value: &Value, schemes: &Schemes, pointers: &mut Vec<Pointer>) {
    match value {
        Value::String(text) => {
            for found in schemes.find_all(text) {
                if !pointers.contains(&found.pointer) {
                    pointers.push(found.pointer);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_source_pointers(value, schemes, pointers);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_source_pointers(value, schemes, pointers);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The parts of a normalised quote between its ellipses, each of which must appear in the
/// source, in order.
fn fragments(normalised: &str) -> impl Iterator<Item = &str> {
    normalised
        .split("...")
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
}

/// Whether every fragment of `quote` is long enough that its match is evidence: at least
/// [`FRAGMENT_WORDS`] words, or [`FRAGMENT_CHARACTERS`] letters and digits. A word is a run
/// between spaces that holds a letter or a digit.
pub(super) fn long_enough(quote: &str) -> bool {
    let quote = normalise_quote_text(quote);
    let long = fragments(&quote).all(|fragment| {
        let words = fragment
            .split_whitespace()
            .filter(|word| word.chars().any(char::is_alphanumeric))
            .count();
        let characters = fragment
            .chars()
            .filter(|character| character.is_alphanumeric())
            .count();
        words >= FRAGMENT_WORDS || characters >= FRAGMENT_CHARACTERS
    });
    long
}

/// `text` as a quote is compared: trimmed, and read as [`normalise_piece`] reads it.
fn normalise_quote_text(text: &str) -> String {
    normalise_piece(text).trim().to_owned()
}

/// `text` as a quote and its source are compared: a Slack link reads as its label, emphasis
/// marks drop out, curly quotes, dashes and the ellipsis read as plain ones, letters read in
/// lower case, and each run of whitespace reads as one space, kept at either end so that the
/// pieces of a [`Reading`] read on together. A mention reads as it is written: whom it names
/// is a matter for the reading's spellings, never for dropping it.
fn normalise_piece(text: &str) -> String {
    let text = unwrap_slack_links(text);
    let mut normalised = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_whitespace() {
            if !normalised.ends_with(' ') {
                normalised.push(' ');
            }
            continue;
        }
        match character {
            '*' | '_' | '~' | '`' => {}
            '‘' | '’' | 'ʼ' => normalised.push('\''),
            '“' | '”' => normalised.push('"'),
            '–' | '—' | '−' => normalised.push('-'),
            '…' => normalised.push_str("..."),
            character => normalised.extend(character.to_lowercase()),
        }
    }
    normalised
}

fn unwrap_slack_links(text: &str) -> String {
    let mut unwrapped = String::new();
    let mut remaining = text;
    while let Some(open) = remaining.find('<') {
        let Some(before) = remaining.get(..open) else {
            break;
        };
        unwrapped.push_str(before);
        let Some(after_open) = remaining.get(open + '<'.len_utf8()..) else {
            break;
        };
        let Some(close) = after_open.find('>') else {
            unwrapped.push('<');
            unwrapped.push_str(after_open);
            return unwrapped;
        };
        let Some(content) = after_open.get(..close) else {
            break;
        };
        let Some(after_close) = after_open.get(close + '>'.len_utf8()..) else {
            break;
        };
        if content.starts_with("http://") || content.starts_with("https://") {
            unwrapped.push_str(content.split_once('|').map_or(content, |(_, label)| label));
        } else {
            unwrapped.push('<');
            unwrapped.push_str(content);
            unwrapped.push('>');
        }
        remaining = after_close;
    }
    unwrapped.push_str(remaining);
    unwrapped
}
