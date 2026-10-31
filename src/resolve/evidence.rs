//! Evidence models and operations shared by provenance and quote judgment.

use std::borrow::Cow;
use std::collections::BTreeSet;

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

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
/// from, as the readings a quote is checked against. For each version of each of its records:
/// the stored text and the source's own text; or, for a Slack message that mentions anyone,
/// its text with each mention one of the complete spellings that name that one person
/// ([`Mentions::spelled`]), which reads as the stored, the source's and the served text alike.
pub(super) struct QuoteSource {
    pub(super) pointer: Pointer,
    pub(super) start: usize,
    readings: Vec<Reading>,
}

impl QuoteSource {
    /// Reads the thread of a pointer as `ctx resolve --thread` serves it, with every earlier
    /// version of its records.
    ///
    /// Each message reads in the thread with its mentions spelled, which spells its stored
    /// text and the text ctx serves for it too wherever the stored text reads as Slack's. When
    /// one does not, the thread is read once more exactly as `ctx resolve --thread` prints it:
    /// that message as stored and every other one spelled, so a quote copied across messages
    /// from that output holds, and a rendered name in it is still held whole.
    pub(super) fn load(
        store: &Store,
        schemes: &Schemes,
        mentions: &Mentions<'_>,
        pointer: Pointer,
        start: usize,
    ) -> Result<Self> {
        let records = local_records(store, schemes, &pointer, true)?;
        let slack = mentions.is_slack_scheme(&pointer.scheme);
        let mut seen = BTreeSet::new();
        let mut each = Vec::with_capacity(records.len());
        for record in &records {
            seen.insert(version_key(record));
            each.push(record_parts(record, mentions)?);
        }
        let served_apart = each.iter().any(|parts| parts.served.is_some());
        let mut thread = Vec::new();
        let mut served = Vec::new();
        let mut others = Vec::new();
        for (index, parts) in each.into_iter().enumerate() {
            if index > 0 {
                thread.push(Part::Text(Cow::Borrowed("\n")));
                if served_apart {
                    served.push(Part::Text(Cow::Borrowed("\n")));
                }
            }
            if served_apart {
                match parts.served {
                    Some(as_served) => served.extend(as_served),
                    None => served.extend(parts.thread.iter().cloned()),
                }
            }
            thread.extend(parts.thread);
            others.extend(parts.others);
        }
        let mut readings = Vec::new();
        add_reading(&mut readings, thread, slack);
        if served_apart {
            add_reading(&mut readings, served, slack);
        }
        for parts in others {
            add_reading(&mut readings, parts, slack);
        }
        for record in &records {
            let Some(parsed) = schemes.parse(&record.pointer) else {
                continue;
            };
            for (_, version) in store.versions(&parsed)? {
                if !seen.insert(version_key(&version)) {
                    continue;
                }
                let parts = record_parts(&version, mentions)?;
                add_reading(&mut readings, parts.thread, slack);
                if let Some(as_served) = parts.served {
                    add_reading(&mut readings, as_served, slack);
                }
                for others in parts.others {
                    add_reading(&mut readings, others, slack);
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
    /// named that mention, and one taken before that person's name changed. A Slack mention
    /// in the quote holds only whole: as a whole spelling of the person the message mentions
    /// there, or, in a text read as it is stored, as the whole mention it writes.
    pub(super) fn holds(&self, quote: &str) -> bool {
        let quote = normalise_quote_text(quote);
        let fragments = fragments(&quote).collect::<Vec<_>>();
        !fragments.is_empty()
            && self
                .readings
                .iter()
                .any(|reading| reading.holds(&fragments))
    }
}

/// What tells one stored version of a record from another for a quote: a digest of its text
/// and of the source's own text, so a long document is not copied to tell.
fn version_key(record: &Record) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(record.text.as_bytes());
    if let Some(own) = source_text(record) {
        digest.update([0]);
        digest.update(own.as_bytes());
    }
    digest.finalize().into()
}

/// How one record reads for a quote.
struct RecordParts<'r> {
    /// As it reads in its thread: with its mentions spelled, or as its stored text.
    thread: Vec<Part<'r>>,
    /// As `ctx resolve` prints it, where `thread` does not spell that: the stored text of a
    /// Slack message that does not read as Slack's own text mention by mention.
    served: Option<Vec<Part<'r>>>,
    /// The other texts a quote of it may have been taken from, each read alone.
    others: Vec<Vec<Part<'r>>>,
}

/// How `record` reads for a quote. A Slack message that mentions anyone reads with its
/// mentions spelled, which spells its stored and served text where the stored text reads as
/// Slack's; Slack's own text, and the stored text where it does not read as Slack's, are read
/// as they are. Any other record reads as its stored text, and as the source's own text where
/// that differs.
fn record_parts<'r>(record: &'r Record, mentions: &Mentions<'_>) -> Result<RecordParts<'r>> {
    let (stored, mut others) = literal_parts(record);
    let Some(spelled) = mentions.spelled(record)? else {
        return Ok(RecordParts {
            thread: stored,
            served: None,
            others,
        });
    };
    let thread = spelled
        .parts
        .into_iter()
        .map(|part| match part {
            Spelled::Text(text) => Part::Text(Cow::Owned(text)),
            Spelled::OneOf(spellings) => Part::Mention(spellings),
        })
        .collect();
    if spelled.spells_stored {
        return Ok(RecordParts {
            thread,
            served: None,
            others,
        });
    }
    others.insert(0, stored);
    Ok(RecordParts {
        thread,
        served: Some(vec![Part::Text(Cow::Borrowed(&record.text))]),
        others,
    })
}

/// `record` as its stored text, with the source's own text as another reading where it is
/// not the same text.
fn literal_parts(record: &Record) -> (Vec<Part<'_>>, Vec<Vec<Part<'_>>>) {
    let others = source_text(record)
        .filter(|own| *own != record.text)
        .map(|own| vec![Part::Text(Cow::Borrowed(own))])
        .into_iter()
        .collect();
    (vec![Part::Text(Cow::Borrowed(&record.text))], others)
}

/// The source's own text of a record, as its `raw` keeps it: for Slack, the message as Slack
/// sent it, each mention still its id, where the record's text names the person.
fn source_text(record: &Record) -> Option<&str> {
    record.raw.get("text").and_then(Value::as_str)
}

/// Adds the reading `parts` make to `readings`, unless an identical one is there. `slack` says
/// the texts are a Slack source's, whose mentions a quote holds only whole.
fn add_reading(readings: &mut Vec<Reading>, parts: Vec<Part<'_>>, slack: bool) {
    let reading = Reading::new(parts, slack);
    if !readings.contains(&reading) {
        readings.push(reading);
    }
}

/// One part of a text a quote is compared against, before normalising: text, or a mention
/// a quote may spell any one of several ways.
#[derive(Clone)]
enum Part<'a> {
    Text(Cow<'a, str>),
    Mention(Vec<String>),
}

/// One text a quote may have been taken from, normalised as a quote is.
#[derive(Debug, PartialEq)]
enum Reading {
    /// A text with no mention to keep whole, compared as a string.
    Plain(String),
    /// A Slack message's text in segments: plain text, and each mention as its complete
    /// spellings, of which a quote must hold one whole.
    Spelled(Vec<Segment>),
}

/// One segment of a [`Reading::Spelled`].
#[derive(Debug, PartialEq)]
enum Segment {
    Text(String),
    Mention(Vec<String>),
}

impl Reading {
    /// The reading `parts` make. When `slack`, each mention written into a text part as Slack
    /// writes one (`<@U…>`, `<#C…>`, `<!…>`), or as `@` and a word (an id such as `@U…`, or a
    /// name a renderer wrote), is a mention a quote holds only whole, as it is written there.
    fn new(parts: Vec<Part<'_>>, slack: bool) -> Self {
        if parts.iter().all(|part| matches!(part, Part::Text(_))) {
            let text = match <[_; 1]>::try_from(parts) {
                Ok([Part::Text(text)]) => text,
                Ok([Part::Mention(_)]) => Cow::Borrowed(""),
                Err(parts) => Cow::Owned(
                    parts
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(text) => Some(text.as_ref()),
                            Part::Mention(_) => None,
                        })
                        .collect(),
                ),
            };
            let normalised = normalise_piece(&text);
            if !slack || mention_tokens(&normalised).is_empty() {
                return Self::Plain(normalised);
            }
            let mut segments = Vec::new();
            push_normalised(&mut segments, &normalised, slack);
            return Self::Spelled(segments);
        }
        let mut segments = Vec::new();
        let mut text = String::new();
        for part in parts {
            match part {
                Part::Text(more) => text.push_str(&more),
                Part::Mention(spellings) => {
                    push_text(&mut segments, &mut text, slack);
                    let mut spellings = spellings
                        .iter()
                        .map(|spelling| normalise_quote_text(spelling))
                        .filter(|spelling| !spelling.is_empty())
                        .collect::<Vec<_>>();
                    spellings.sort();
                    spellings.dedup();
                    segments.push(Segment::Mention(spellings));
                }
            }
        }
        push_text(&mut segments, &mut text, slack);
        Self::Spelled(segments)
    }

    /// Whether each of `fragments` reads in the text, in order, each starting where the one
    /// before it ended or later.
    fn holds(&self, fragments: &[&str]) -> bool {
        match self {
            Self::Plain(text) => {
                let mut cursor = 0;
                for fragment in fragments {
                    let Some(found) = text.get(cursor..).and_then(|rest| rest.find(fragment))
                    else {
                        return false;
                    };
                    cursor += found + fragment.len();
                }
                true
            }
            Self::Spelled(segments) => {
                let mut from = Position::default();
                for fragment in fragments {
                    match earliest_end(segments, from, fragment) {
                        Some(end) => from = end,
                        None => return false,
                    }
                }
                true
            }
        }
    }
}

/// Moves the text gathered so far into `segments`, normalised ([`push_normalised`]).
fn push_text(segments: &mut Vec<Segment>, text: &mut String, slack: bool) {
    if text.is_empty() {
        return;
    }
    let normalised = normalise_piece(&std::mem::take(text));
    push_normalised(segments, &normalised, slack);
}

/// Adds `normalised` text to `segments`: as text, and, when `slack`, each mention token in it
/// ([`mention_tokens`]) as a mention whose one spelling is itself, so a quote holds it whole.
fn push_normalised(segments: &mut Vec<Segment>, normalised: &str, slack: bool) {
    let tokens = if slack {
        mention_tokens(normalised)
    } else {
        Vec::new()
    };
    let mut written = 0;
    for (start, end) in tokens {
        if let Some(before) = normalised
            .get(written..start)
            .filter(|text| !text.is_empty())
        {
            segments.push(Segment::Text(before.to_owned()));
        }
        if let Some(token) = normalised.get(start..end) {
            segments.push(Segment::Mention(vec![token.to_owned()]));
        }
        written = end;
    }
    if let Some(rest) = normalised.get(written..).filter(|text| !text.is_empty()) {
        segments.push(Segment::Text(rest.to_owned()));
    }
}

/// Where a normalised Slack text writes a mention itself, as byte ranges in order: a sequence
/// Slack writes, `<@…>`, `<#…>` or `<!…>`, and `@` where it starts a word, with the word after
/// it: letters and digits, joined by `.` or `-`. That word is a person's id, or a name a
/// renderer wrote, whose end a text read as it is shows nowhere else. A quote holds each whole.
fn mention_tokens(normalised: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut previous: Option<char> = None;
    let mut characters = normalised.char_indices().peekable();
    while let Some((start, character)) = characters.next() {
        let starts_word = previous.is_none_or(|before| !before.is_alphanumeric());
        let next = characters.peek().map(|(_, next)| *next);
        let end = match character {
            '<' if matches!(next, Some('@' | '#' | '!')) => normalised
                .get(start..)
                .and_then(|rest| rest.find('>'))
                .map(|close| start + close + 1),
            '@' if starts_word && next.is_some_and(char::is_alphanumeric) => {
                let word = start + 1;
                let mut end = word;
                let rest = normalised.get(word..).unwrap_or_default();
                for (offset, following) in rest.char_indices() {
                    if following.is_alphanumeric() {
                        end = word + offset + following.len_utf8();
                        continue;
                    }
                    // A `.` or `-` joins the word only to more of it.
                    let joins = matches!(following, '.' | '-')
                        && rest
                            .get(offset + 1..)
                            .and_then(|after| after.chars().next())
                            .is_some_and(char::is_alphanumeric);
                    if !joins {
                        break;
                    }
                }
                Some(end)
            }
            _ => None,
        };
        match end {
            Some(end) => {
                tokens.push((start, end));
                while characters.peek().is_some_and(|(at, _)| *at < end) {
                    previous = characters.next().map(|(_, skipped)| skipped);
                }
            }
            None => previous = Some(character),
        }
    }
    tokens
}

/// A place in a [`Reading::Spelled`]: a byte offset in one of its segments, where the offset
/// in a mention is always 0, its start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    segment: usize,
    offset: usize,
}

/// Where `fragment` ends, at the earliest, when it reads in `segments` at or after `from`. It
/// starts in a text or where a mention starts, never inside a mention, and it holds each
/// mention it reaches as one whole spelling, so it ends where a mention ends or in a text.
fn earliest_end(segments: &[Segment], from: Position, fragment: &str) -> Option<Position> {
    let mut best: Option<Position> = None;
    for (index, segment) in segments.iter().enumerate().skip(from.segment) {
        // What starts here ends here or later.
        if best.is_some_and(|best| best.segment < index) {
            break;
        }
        let start = if index == from.segment {
            from.offset
        } else {
            0
        };
        let found = match segment {
            Segment::Text(text) => {
                let inside = text
                    .get(start..)
                    .and_then(|rest| rest.find(fragment))
                    .map(|found| Position {
                        segment: index,
                        offset: start + found + fragment.len(),
                    });
                // A start this close to the text's end runs on past it.
                let first = text
                    .len()
                    .saturating_sub(fragment.len().saturating_sub(1))
                    .max(start);
                let crossing = (first..text.len())
                    .filter(|offset| text.is_char_boundary(*offset))
                    .filter_map(|offset| {
                        let rest = fragment.strip_prefix(text.get(offset..)?)?;
                        from_boundary(segments, index + 1, rest)
                    })
                    .min();
                inside.into_iter().chain(crossing).min()
            }
            Segment::Mention(_) if start == 0 => from_boundary(segments, index, fragment),
            Segment::Mention(_) => None,
        };
        best = best.into_iter().chain(found).min();
    }
    best
}

/// Where `rest`, which is not empty, ends at the earliest when it reads from the start of the
/// segment at `index`: through each text, and through each mention as one whole spelling.
fn from_boundary(segments: &[Segment], index: usize, rest: &str) -> Option<Position> {
    match segments.get(index)? {
        Segment::Text(text) if rest.len() <= text.len() => {
            text.starts_with(rest).then_some(Position {
                segment: index,
                offset: rest.len(),
            })
        }
        Segment::Text(text) => {
            from_boundary(segments, index + 1, rest.strip_prefix(text.as_str())?)
        }
        Segment::Mention(spellings) => spellings
            .iter()
            .filter_map(|spelling| {
                if rest == spelling {
                    Some(Position {
                        segment: index + 1,
                        offset: 0,
                    })
                } else {
                    from_boundary(segments, index + 1, rest.strip_prefix(spelling.as_str())?)
                }
            })
            .min(),
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use chrono::DateTime;
    use serde_json::json;

    use super::{add_reading, literal_parts, normalise_quote_text, Part, Reading};
    use crate::record::Record;

    /// Whether `quote` holds in `reading`, as [`super::QuoteSource::holds`] checks it.
    fn holds(reading: &Reading, quote: &str) -> bool {
        let quote = normalise_quote_text(quote);
        let fragments = super::fragments(&quote).collect::<Vec<_>>();
        !fragments.is_empty() && reading.holds(&fragments)
    }

    /// A Slack message that asks one person, with that person's spellings.
    fn asked() -> Reading {
        Reading::new(
            vec![
                Part::Text(Cow::Borrowed("Please ask cc ")),
                Part::Mention(vec![
                    "@U0EXAMPLEA".to_owned(),
                    "<@U0EXAMPLEA>".to_owned(),
                    "@Kim Lee".to_owned(),
                ]),
                Part::Text(Cow::Borrowed(" to review this proposal.")),
            ],
            true,
        )
    }

    /// A quote that includes any part of a mention holds only with one of its spellings
    /// whole; it may end before a mention or start after one.
    #[test]
    fn a_quote_holds_a_mention_only_whole() {
        let reading = asked();
        for (quote, expected) in [
            ("Please ask cc @U0EXAMPLEA to review", true),
            ("Please ask cc <@U0EXAMPLEA> to review", true),
            ("ask cc @Kim Lee to review this proposal", true),
            ("Please ask cc", true),
            ("to review this proposal.", true),
            ("Please ask ... @Kim Lee ... proposal", true),
            ("Please ask cc @U0EXAMPLE", false),
            ("Please ask cc <@U0EXAMPLE", false),
            ("Please ask cc @Kim", false),
            ("Lee to review this proposal", false),
            ("EXAMPLEA to review this proposal", false),
            ("Please ask cc @U0EXAMPLED to review", false),
        ] {
            assert_eq!(holds(&reading, quote), expected, "{quote}");
        }
    }

    /// A Slack message's stored text and Slack's own text, read as they are, hold any quote
    /// copied from them, escapes included, but a quote that starts or stops inside a mention
    /// they write, `@U…`, `<@U…>` or a rendered `@Name`, does not hold.
    #[test]
    fn a_literal_slack_text_holds_its_mentions_only_whole() {
        let stored = Reading::new(
            vec![Part::Text(Cow::Borrowed(
                "Please ask cc @U0EXAMPLEA to review the new plan",
            ))],
            true,
        );
        let own = Reading::new(
            vec![Part::Text(Cow::Borrowed(
                "Please ask cc <@U0EXAMPLEA> to check A &amp; B",
            ))],
            true,
        );
        let named = Reading::new(
            vec![Part::Text(Cow::Borrowed(
                "Please ask cc @Kimberly to review the plan. Ask @kim.lee-park too.",
            ))],
            true,
        );
        for (reading, quote, expected) in [
            (
                &stored,
                "Please ask cc @U0EXAMPLEA to review the new plan",
                true,
            ),
            (&stored, "Please ask cc", true),
            (&stored, "to review the new plan", true),
            (&stored, "Please ask cc @U0EXAMPLE", false),
            (&stored, "0EXAMPLEA to review the new plan", false),
            (&own, "Please ask cc <@U0EXAMPLEA> to check A &amp; B", true),
            (&own, "<@U0EXAMPLEA> to check A &amp; B", true),
            (&own, "Please ask cc <@U0EXAMPLE", false),
            (&own, "Please ask cc <@U0EXAMPLEA", false),
            (&own, "U0EXAMPLEA> to check A &amp; B", false),
            (&named, "Please ask cc @Kimberly to review the plan", true),
            (&named, "Please ask cc @Kim", false),
            (&named, "Please ask cc @Kim ... to review the plan", false),
            (&named, "berly to review the plan", false),
            (&named, "Ask @kim.lee-park too", true),
            (&named, "Ask @kim.lee", false),
            (&named, "the plan. Ask", true),
        ] {
            assert_eq!(holds(reading, quote), expected, "{quote}");
        }
    }

    /// A text that is no Slack source's is compared as a string, `@` words and all.
    #[test]
    fn a_text_of_another_source_keeps_no_mention_whole() {
        let reading = Reading::new(
            vec![Part::Text(Cow::Borrowed("Thanks @user123 for the review"))],
            false,
        );

        assert!(matches!(reading, Reading::Plain(_)));
        assert!(holds(&reading, "Thanks @user123 for the review"));
    }

    /// A large text with no mention is one plain string, compared as a string, and a record
    /// whose source text is the same text reads once: no graph and no second copy.
    #[test]
    fn a_large_plain_text_reads_once_as_a_string() -> anyhow::Result<()> {
        let mut text = "Some words that fill a long document. ".repeat(140_000);
        text.push_str("At last the vendor signed the renewal.");
        let record = Record {
            pointer: "drive:1LargeExampleDocument000000".to_owned(),
            stream: "drive".to_owned(),
            ts: DateTime::UNIX_EPOCH,
            label: None,
            author: None,
            author_id: None,
            thread: None,
            title: None,
            url: None,
            text: text.clone(),
            aliases: Vec::new(),
            context: false,
            raw: json!({ "text": text }),
        };
        let (primary, others) = literal_parts(&record);
        let mut readings = Vec::new();
        add_reading(&mut readings, primary, false);
        for parts in others {
            add_reading(&mut readings, parts, false);
        }
        add_reading(
            &mut readings,
            vec![Part::Text(Cow::Borrowed(&record.text))],
            false,
        );

        assert_eq!(readings.len(), 1, "identical texts read once");
        let [reading @ Reading::Plain(plain)] = readings.as_slice() else {
            anyhow::bail!("a text with no mention reads as one plain string: {readings:?}");
        };
        assert!(record.text.len() > 5 * 1024 * 1024);
        assert!(plain.len() <= record.text.len(), "one normalised copy");
        assert!(holds(reading, "the vendor signed the renewal"));
        assert!(!holds(reading, "the vendor never signed"));
        Ok(())
    }
}
