//! A full-text index over a loaded tree.
//!
//! Records carry their claims in frontmatter — the quote, the title, the statement, and the
//! people, dates and source pointers around them — so the index covers the whole of it beside
//! the markdown body. SQLite's FTS5 does the matching and BM25 the ranking; authority tier
//! breaks a tie, keeping a direct source ahead of a summary.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rusqlite::Connection;
use serde_json::Value;

use crate::embed::{cosine, Embed, Embedding, MEANING_THRESHOLD};
use crate::tree::{Doc, Tier, Tree};

/// The tier value stored for a record that declares none. It sorts after every real tier.
const UNTIERED: i64 = 9;

/// Frontmatter keys whose text already has a column of its own. Anything else a record
/// declares — `owner`, `attendees`, `status`, `source`, a tree's own fields — goes to
/// [`Fields::attributes`], so `ctx search` can never call a term absent that the tree holds.
const COLUMN_KEYS: [&str; 2] = ["statement", "quote"];

/// One document that matched a query.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The document's position in the tree it was built from. A path string cannot identify
    /// it: a file name is bytes, not text, and one undecodable name would break every lookup.
    pub position: usize,
    /// Tree-relative path, lossily decoded for display.
    pub path: String,
    /// BM25 relevance. The score is negative and a smaller number is a better match.
    pub score: f64,
    /// Authority tier 1–4, or `None` when the record declares none.
    pub tier: Option<u8>,
    /// Matching text, with the surrounding words FTS5 selected.
    pub snippet: String,
    /// This document matched only because its meaning is close, not because it holds a query
    /// word.
    pub by_meaning: bool,
}

/// What one query found: its hits, best first, and a notice when meaning search was asked for
/// but is unavailable, so the result is text alone.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub hits: Vec<Hit>,
    pub notice: Option<String>,
}

/// The indexed text of one document, column by column. Borrowed where the document already
/// holds the text, so indexing a large tree copies only what it joins.
struct Fields<'a> {
    title: String,
    quote: String,
    statement: &'a str,
    aliases: String,
    /// Every other frontmatter value: owner, attendees, status, type, dates, source pointers
    /// and whatever else the tree's own record types declare.
    attributes: String,
    body: &'a str,
}

/// Appends every scalar in a frontmatter value as its text, skipping the keys that have
/// their own column. A number or a boolean is a term a reader may search by, so it is
/// indexed like a string.
fn push_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Number(number) => out.push(number.to_string()),
        Value::Bool(flag) => out.push(flag.to_string()),
        Value::Array(values) => {
            for value in values {
                push_strings(value, out);
            }
        }
        Value::Object(entries) => {
            for (key, value) in entries {
                if !COLUMN_KEYS.contains(&key.as_str()) {
                    push_strings(value, out);
                }
            }
        }
        Value::Null => {}
    }
}

fn fields(doc: &Doc) -> Fields<'_> {
    let front = &doc.front;
    let title = [
        front.title.as_deref(),
        front.name.as_deref(),
        front.what.as_deref(),
        front.question.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");

    let mut other: Vec<String> = [
        front.kind.as_deref(),
        front.status.as_deref(),
        front.owner.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_owned)
    .collect();
    for date in [
        front.date,
        front.opened,
        front.due,
        front.revisit_by,
        front.last_reviewed,
    ]
    .into_iter()
    .flatten()
    {
        other.push(date.to_string());
    }
    for entry in &front.quotes {
        if let Some(source) = entry.source.as_deref() {
            other.push(source.to_owned());
        }
    }
    for source in front.source_entries() {
        for (key, value) in source {
            if !COLUMN_KEYS.contains(&key.as_str()) {
                push_strings(value, &mut other);
            }
        }
    }
    for (key, value) in &front.extra {
        if !COLUMN_KEYS.contains(&key.as_str()) {
            push_strings(value, &mut other);
        }
    }
    // A tree repeats a value across fields — one source pointer under `source`, `sources`
    // and every `quotes` entry — and one copy is all a search needs.
    let mut seen = HashSet::new();
    other.retain(|value| seen.insert(value.clone()));
    // Frontmatter that did not parse leaves every typed field empty, so index its text as it
    // stands: the tree holds those terms, and a search must not call them absent.
    if doc.front_error.is_some() {
        other.push(doc.frontmatter_text().into_owned());
    }

    Fields {
        title,
        quote: front.quote_texts().join("\n"),
        statement: front.extra_str("statement").unwrap_or_default(),
        aliases: front.aliases.join("\n"),
        attributes: other.join("\n"),
        body: doc.body(),
    }
}

/// Every text a search may match in one document, in the order the index stores it.
#[must_use]
pub fn document_text(doc: &Doc) -> String {
    let fields = fields(doc);
    [
        fields.title.as_str(),
        fields.quote.as_str(),
        fields.statement,
        fields.aliases.as_str(),
        fields.attributes.as_str(),
        fields.body,
    ]
    .join("\n")
}

/// Builds an FTS5 query from free text: every word, any of which may match.
///
/// Each word becomes an FTS5 string literal, so query punctuation and the operator words
/// `AND`, `OR` and `NOT` are searched for rather than obeyed.
fn match_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|term| term.chars().any(char::is_alphanumeric))
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

pub struct Index {
    connection: Connection,
    meaning: Option<Meaning>,
    /// Set when meaning search was asked for but is unavailable, so a result is text alone.
    notice: Option<String>,
}

/// The record vectors one index holds, and the embedder it asks for a query's vector.
struct Meaning {
    embedder: Arc<dyn Embed>,
    records: Vec<Vectored>,
}

/// One document's embedding, beside what a meaning-only hit needs to stand on its own.
struct Vectored {
    position: usize,
    path: String,
    tier: Option<u8>,
    preview: String,
    vector: Vec<f32>,
}

/// Standard reciprocal-rank-fusion constant: a rank's weight is `1/(K + rank)`.
const RRF_K: f64 = 60.0;

impl Index {
    /// Indexes every document in `tree`, and embeds each one when `embedding` is ready.
    ///
    /// When meaning search is configured but its embeddings cannot be computed — a dev box
    /// with no Google credential — the index keeps a text-only form and a notice, rather than
    /// failing, so `ctx search` still answers.
    ///
    /// # Errors
    ///
    /// Returns an error when the in-memory database cannot be created or written.
    pub fn build(tree: &Tree, embedding: &Embedding) -> Result<Self> {
        let mut connection = Connection::open_in_memory().context("opening the search index")?;
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE documents USING fts5(
                     title, quote, statement, aliases, attributes, body,
                     path UNINDEXED, tier UNINDEXED, position UNINDEXED,
                     tokenize = 'unicode61'
                 );",
            )
            .context("creating the search index")?;
        let transaction = connection
            .transaction()
            .context("indexing the tree in one transaction")?;
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO documents
                         (title, quote, statement, aliases, attributes, body, path, tier,
                          position)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .context("preparing the index insert")?;
            for (position, doc) in tree.docs.iter().enumerate() {
                let fields = fields(doc);
                let path = doc.rel.to_string_lossy();
                let tier = doc
                    .front
                    .best_tier()
                    .map_or(UNTIERED, |tier| i64::from(tier.number()));
                insert
                    .execute(rusqlite::params![
                        fields.title,
                        fields.quote,
                        fields.statement,
                        fields.aliases,
                        fields.attributes,
                        fields.body,
                        path,
                        tier,
                        i64::try_from(position).unwrap_or(i64::MAX),
                    ])
                    .with_context(|| format!("indexing {path}"))?;
            }
        }
        transaction
            .commit()
            .context("committing the search index")?;
        let (meaning, notice) = match embedding {
            Embedding::Off => (None, None),
            Embedding::Ready(embedder) => match embed_records(tree, embedder.as_ref()) {
                Ok(records) => (
                    Some(Meaning {
                        embedder: Arc::clone(embedder),
                        records,
                    }),
                    None,
                ),
                Err(error) => (None, Some(format!("meaning search is off; {error:#}"))),
            },
        };
        Ok(Self {
            connection,
            meaning,
            notice,
        })
    }

    /// Returns the documents that match `query`, best match first.
    ///
    /// With meaning search off, the result is the text match alone, ranked by BM25 and then by
    /// authority tier. With it on, a record whose meaning is close is found even when it shares
    /// no word with the query, and the two rankings are fused by reciprocal rank.
    ///
    /// `limit` of 0 returns every match.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be queried.
    pub fn search(&self, query: &str, limit: usize) -> Result<Hits> {
        let lexical = self.lexical(query)?;
        let keep = if limit == 0 { usize::MAX } else { limit };
        let Some(meaning) = &self.meaning else {
            return Ok(Hits {
                hits: lexical.into_iter().take(keep).collect(),
                notice: self.notice.clone(),
            });
        };
        let query_vector = match meaning.embedder.embed_queries(&[query]) {
            Ok(vectors) => vectors.into_iter().next(),
            Err(error) => {
                return Ok(Hits {
                    hits: lexical.into_iter().take(keep).collect(),
                    notice: Some(format!("meaning search is off for this query; {error:#}")),
                })
            }
        };
        let Some(query_vector) = query_vector else {
            return Ok(Hits {
                hits: lexical.into_iter().take(keep).collect(),
                notice: self.notice.clone(),
            });
        };
        let hits = fuse(&lexical, meaning, &query_vector, keep);
        Ok(Hits {
            hits,
            notice: self.notice.clone(),
        })
    }

    /// The text matches for `query`, ranked by BM25 then tier, every match returned.
    fn lexical(&self, query: &str) -> Result<Vec<Hit>> {
        let Some(match_query) = match_query(query) else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT path,
                        CAST(tier AS INTEGER) AS tier_number,
                        CAST(position AS INTEGER) AS doc_position,
                        bm25(documents, 4.0, 4.0, 3.0, 2.0, 2.0, 1.0) AS score,
                        snippet(documents, -1, '', '', '…', 12) AS snippet
                 FROM documents
                 WHERE documents MATCH ?1
                 ORDER BY score, tier_number",
            )
            .context("preparing the search query")?;
        let hits = statement
            .query_map(rusqlite::params![match_query], |row| {
                let tier: i64 = row.get("tier_number")?;
                let position: i64 = row.get("doc_position")?;
                Ok(Hit {
                    position: usize::try_from(position).unwrap_or(usize::MAX),
                    path: row.get("path")?,
                    score: row.get("score")?,
                    tier: u8::try_from(tier).ok().filter(|tier| *tier < 5),
                    snippet: row.get("snippet")?,
                    by_meaning: false,
                })
            })
            .context("running the search query")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading search results")?;
        Ok(hits)
    }
}

/// Embeds every document, so a query can be matched to it by meaning.
fn embed_records(tree: &Tree, embedder: &dyn Embed) -> Result<Vec<Vectored>> {
    let texts = tree.docs.iter().map(document_text).collect::<Vec<_>>();
    let refs = texts.iter().map(String::as_str).collect::<Vec<_>>();
    let vectors = embedder.embed(&refs)?;
    if vectors.len() != tree.docs.len() {
        anyhow::bail!(
            "the embedder returned {} vectors for {} documents",
            vectors.len(),
            tree.docs.len()
        );
    }
    Ok(tree
        .docs
        .iter()
        .enumerate()
        .zip(vectors)
        .map(|((position, doc), vector)| Vectored {
            position,
            path: doc.rel.to_string_lossy().into_owned(),
            tier: doc.front.best_tier().map(Tier::number),
            preview: preview(doc),
            vector,
        })
        .collect())
}

/// Fuses the text ranking with the meaning ranking by reciprocal rank, then keeps the best.
fn fuse(lexical: &[Hit], meaning: &Meaning, query_vector: &[f32], keep: usize) -> Vec<Hit> {
    let mut meaning_hits = meaning
        .records
        .iter()
        .filter_map(|record| {
            let score = cosine(query_vector, &record.vector);
            (score >= MEANING_THRESHOLD).then_some((record.position, score))
        })
        .collect::<Vec<_>>();
    meaning_hits.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut fused: BTreeMap<usize, f64> = BTreeMap::new();
    for (rank, hit) in lexical.iter().enumerate() {
        *fused.entry(hit.position).or_default() += rrf_weight(rank);
    }
    for (rank, (position, _)) in meaning_hits.iter().enumerate() {
        *fused.entry(*position).or_default() += rrf_weight(rank);
    }

    let lexical_by_position = lexical
        .iter()
        .map(|hit| (hit.position, hit))
        .collect::<BTreeMap<_, _>>();

    let mut ranked = fused.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                tier_rank(
                    lexical_by_position.get(&left.0),
                    meaning.records.get(left.0),
                )
                .cmp(&tier_rank(
                    lexical_by_position.get(&right.0),
                    meaning.records.get(right.0),
                ))
            })
    });

    ranked
        .into_iter()
        .take(keep)
        .filter_map(|(position, score)| {
            if let Some(hit) = lexical_by_position.get(&position) {
                let mut hit = (*hit).clone();
                hit.score = score;
                hit.by_meaning = false;
                Some(hit)
            } else {
                let record = meaning.records.get(position)?;
                Some(Hit {
                    position,
                    path: record.path.clone(),
                    score,
                    tier: record.tier,
                    snippet: record.preview.clone(),
                    by_meaning: true,
                })
            }
        })
        .collect()
}

/// The reciprocal-rank weight of a zero-based rank: `1/(K + rank + 1)`.
fn rrf_weight(rank: usize) -> f64 {
    let rank = u32::try_from(rank.saturating_add(1)).unwrap_or(u32::MAX);
    1.0 / (RRF_K + f64::from(rank))
}

/// The tier value used to break a fusion tie: a direct source precedes a summary.
fn tier_rank(lexical: Option<&&Hit>, meaning: Option<&Vectored>) -> i64 {
    lexical
        .map(|hit| hit.tier)
        .or_else(|| meaning.map(|record| record.tier))
        .flatten()
        .map_or(UNTIERED, i64::from)
}

/// A short line of a document, shown for a hit that matched only by meaning.
fn preview(doc: &Doc) -> String {
    doc.body()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .or(doc.front.title.as_deref())
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::match_query;

    #[test]
    fn query_words_are_literals_joined_by_or() {
        assert_eq!(
            match_query("vendor renewal").as_deref(),
            Some("\"vendor\" OR \"renewal\"")
        );
    }

    #[test]
    fn an_operator_word_is_searched_for_not_obeyed() {
        assert_eq!(match_query("AND").as_deref(), Some("\"AND\""));
    }

    #[test]
    fn a_quoted_term_cannot_escape_its_literal() {
        assert_eq!(
            match_query("say \"hi\"").as_deref(),
            Some("\"say\" OR \"\"\"hi\"\"\"")
        );
    }

    #[test]
    fn punctuation_only_queries_match_nothing() {
        assert_eq!(match_query("  -- ").as_deref(), None);
    }
}
