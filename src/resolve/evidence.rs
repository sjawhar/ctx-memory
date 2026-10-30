//! Evidence models and operations shared by provenance and quote judgment.

use anyhow::Result;
use serde_json::Value;

use crate::pointer::{self, Pointer, Schemes};
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
/// The text a cited pointer resolves to, with the earlier versions a quote may predate.
pub(super) struct QuoteSource {
    pub(super) pointer: Pointer,
    pub(super) start: usize,
    text: String,
    older: Vec<String>,
}

impl QuoteSource {
    /// Reads the newest text of a pointer's thread and every earlier version of its records.
    pub(super) fn load(
        store: &Store,
        schemes: &Schemes,
        pointer: Pointer,
        start: usize,
    ) -> Result<Self> {
        let records = local_records(store, schemes, &pointer, true)?;
        let text = records
            .iter()
            .map(|record| record.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let mut older = Vec::new();
        for record in &records {
            let Some(parsed) = schemes.parse(&record.pointer) else {
                continue;
            };
            for (_, version) in store.versions(&parsed)? {
                if version.text != record.text && !older.contains(&version.text) {
                    older.push(version.text);
                }
            }
        }
        Ok(Self {
            pointer,
            start,
            text,
            older,
        })
    }

    /// Reports whether the quote appears in the current text or in one earlier version of one
    /// of its records. A quote taken before an edit stays verifiable.
    pub(super) fn holds(&self, quote: &str) -> bool {
        quote_matches(&self.text, quote) || self.older.iter().any(|text| quote_matches(text, quote))
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

fn quote_matches(text: &str, quote: &str) -> bool {
    let text = normalise_quote_text(text);
    let quote = normalise_quote_text(quote);
    let mut cursor = 0;
    let mut matched_fragment = false;
    for fragment in fragments(&quote) {
        let Some(relative) = text
            .get(cursor..)
            .and_then(|remaining| remaining.find(fragment))
        else {
            return false;
        };
        matched_fragment = true;
        cursor += relative + fragment.len();
    }
    matched_fragment
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

fn normalise_quote_text(text: &str) -> String {
    let text = strip_mentions(&unwrap_slack_links(text));
    let mut normalised = String::new();
    let mut whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() {
            whitespace = !normalised.is_empty();
            continue;
        }
        if matches!(character, '*' | '_' | '~' | '`') {
            continue;
        }
        if whitespace {
            normalised.push(' ');
            whitespace = false;
        }
        match character {
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

fn strip_mentions(text: &str) -> String {
    let mut stripped = String::new();
    let mut remaining = text;
    while let Some(character) = remaining.chars().next() {
        if let Some(after) = remaining.strip_prefix("<@") {
            if let Some(close) = after.find('>') {
                let identifier = after.get(..close);
                if identifier.is_some_and(|identifier| identifier.starts_with('U')) {
                    let Some(after_mention) = after.get(close + '>'.len_utf8()..) else {
                        break;
                    };
                    remaining = after_mention;
                    continue;
                }
            }
        }
        let mention_boundary = stripped.chars().next_back().is_none_or(|previous| {
            !previous.is_alphanumeric() && previous != '_' && previous != '-'
        });
        if character == '@' && mention_boundary {
            let Some(after) = remaining.get(character.len_utf8()..) else {
                break;
            };
            let name_end = after
                .char_indices()
                .find_map(|(offset, character)| {
                    (!character.is_alphanumeric() && character != '_' && character != '-')
                        .then_some(offset)
                })
                .unwrap_or(after.len());
            if name_end > 0 {
                let Some(after_mention) = after.get(name_end..) else {
                    break;
                };
                remaining = after_mention;
                continue;
            }
        }
        stripped.push(character);
        let Some(after_character) = remaining.get(character.len_utf8()..) else {
            break;
        };
        remaining = after_character;
    }
    stripped
}
