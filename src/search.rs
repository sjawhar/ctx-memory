//! Retrieval over a markdown context tree.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{anyhow, bail, Result};
use owo_colors::OwoColorize as _;
use serde::Serialize;

use crate::config::TreeConfig;
use crate::index::{self, Index};
use crate::output::printable;
use crate::tree::{wikilinks, Doc, Resolution, Tier, Tree};

#[derive(Debug, Serialize)]
pub struct Hit {
    pub path: String,
    pub tier: Option<String>,
    pub title: String,
    pub line: String,
    /// How many query terms appear anywhere in this document.
    pub matched_terms: usize,
    pub total_terms: usize,
    /// This document was found because its meaning is close, not because it holds a query word.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_meaning: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EntityMatch {
    One(usize),
    Many(Vec<usize>),
    None,
}

#[derive(Debug, Serialize)]
pub struct Dossier {
    pub entity: String,
    pub page: Option<DossierPage>,
    pub candidates: Vec<String>,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Serialize)]
pub struct DossierPage {
    pub path: String,
    pub vitals: BTreeMap<String, serde_json::Value>,
}

fn tier_rank(tier: Option<Tier>) -> u8 {
    tier.map_or(9, |tier| tier as u8)
}

/// What one `ctx search` found.
#[derive(Debug, Serialize)]
pub struct Search {
    pub query: String,
    pub hits: Vec<Hit>,
    pub total_terms: usize,
    /// Documents that matched the query but were dropped by `--verbatim`.
    pub dropped_by_verbatim: usize,
    /// Set when meaning search was asked for but is unavailable, so the result is text alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Searches every frontmatter value — title, quotes, statement, aliases, owner, dates,
/// source pointers and whatever else a record declares — and the markdown body, ranking by
/// text relevance and then by authority tier, so a direct source precedes an equally
/// relevant summary.
///
/// `limit` of 0 returns every match, the meaning [`Index::search`] gives it.
///
/// The index is passed in, so one server process keeps a single index for every query.
///
/// # Errors
///
/// Returns an error when the index cannot be queried.
pub fn run(
    index: &Index,
    tree: &Tree,
    query: &str,
    verbatim_only: bool,
    limit: usize,
) -> Result<Search> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    // Without a filter the index applies the limit; with one it cannot, because the
    // dropped documents must still be counted to tell a filtered result from an absence.
    let found = index.search(query, if verbatim_only { 0 } else { limit })?;
    let matches = found.hits;
    // 0 is the index's "every match"; the hit loop must read it the same way, or a limit of
    // zero would drop every hit and print an absence the tree does not support.
    let keep = if limit == 0 { usize::MAX } else { limit };

    let mut hits = Vec::new();
    let mut dropped_by_verbatim = 0;
    for found in matches {
        let doc = tree
            .docs
            .get(found.position)
            .ok_or_else(|| anyhow!("search hit {} is not in the tree", found.path))?;
        // `--verbatim` keeps or drops the whole record by its strongest tier, never one quote:
        // a tier 3 `quotes` entry in a record that a tier 1 quote ranks at tier 1 is shown at
        // tier 1, beside it. The writer refuses a tier 1 or 2 record unless one of its tier 1
        // or 2 quotes was checked against its source, so the rank rests on that quote.
        let tier = doc.front.best_tier();
        if verbatim_only && !tier.is_some_and(Tier::is_verbatim) {
            dropped_by_verbatim += 1;
            continue;
        }
        if hits.len() >= keep {
            continue;
        }
        let haystack = index::document_text(doc).to_lowercase();
        hits.push(Hit {
            path: found.path,
            tier: tier.map(|tier| tier.label().to_owned()),
            title: doc.label(),
            line: one_line(&found.snippet),
            matched_terms: terms
                .iter()
                .filter(|term| haystack.contains(term.as_str()))
                .count(),
            total_terms: terms.len(),
            by_meaning: found.by_meaning,
        });
    }

    Ok(Search {
        query: query.to_owned(),
        hits,
        total_terms: terms.len(),
        dropped_by_verbatim,
        notice: found.notice,
    })
}

/// Collapses a snippet to one line so a hit stays one entry.
fn one_line(snippet: &str) -> String {
    snippet.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finds a page by its canonical name before accepting an unambiguous partial match.
#[must_use]
pub fn find_entity(tree: &Tree, entity: &str) -> EntityMatch {
    let entity = entity.trim();
    if entity.is_empty() {
        return EntityMatch::None;
    }

    let entity_lower = entity.to_lowercase();
    let exact = matching_entities(tree, entity, &entity_lower, true);
    if !exact.is_empty() {
        return entity_match(exact);
    }
    entity_match(matching_entities(tree, entity, &entity_lower, false))
}

fn matching_entities(tree: &Tree, entity: &str, entity_lower: &str, exact: bool) -> Vec<usize> {
    tree.docs
        .iter()
        .enumerate()
        .filter_map(|(position, document)| {
            entity_matches(document, entity, entity_lower, exact).then_some(position)
        })
        .collect()
}

fn entity_matches(document: &Doc, entity: &str, entity_lower: &str, exact: bool) -> bool {
    let matches = |value: &str| {
        if exact {
            value.eq_ignore_ascii_case(entity)
        } else {
            value.to_lowercase().contains(entity_lower)
        }
    };

    matches(&document.stem())
        || document
            .rel
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(matches)
        || document.front.name.as_deref().is_some_and(matches)
        || document.front.title.as_deref().is_some_and(matches)
        || exact
            && document
                .front
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(entity))
}

fn entity_match(mut positions: Vec<usize>) -> EntityMatch {
    match positions.len() {
        0 => EntityMatch::None,
        1 => EntityMatch::One(positions.remove(0)),
        _ => EntityMatch::Many(positions),
    }
}

/// Returns every matching document, the selected entity page, and its configured vital fields.
///
/// # Errors
///
/// Returns an error when `entity` is empty or a matching page's frontmatter cannot be read.
pub fn dossier(tree: &Tree, entity: &str, config: &TreeConfig) -> Result<Dossier> {
    let entity = entity.trim();
    if entity.is_empty() {
        bail!("dossier entity must not be empty");
    }

    let matched = find_entity(tree, entity);
    let (page_position, page, candidates) = match matched {
        EntityMatch::One(position) => {
            let document = document_at(tree, position)?;
            (
                Some(position),
                Some(build_dossier_page(document, &config.dossier.vitals)?),
                Vec::new(),
            )
        }
        EntityMatch::Many(positions) => {
            let candidates = positions
                .iter()
                .map(|position| {
                    document_at(tree, *position)
                        .map(|document| document.rel.to_string_lossy().into_owned())
                })
                .collect::<Result<Vec<_>>>()?;
            (None, None, candidates)
        }
        EntityMatch::None => (None, None, Vec::new()),
    };

    let needle = entity.to_lowercase();
    let mut hits: Vec<(u8, Hit)> = tree
        .docs
        .iter()
        .enumerate()
        .filter_map(|(position, document)| {
            let linked = page_position.is_some_and(|page_position| {
                wikilinks(&document.raw).iter().any(|link| {
                    matches!(
                        tree.resolve(document, &link.target),
                        Resolution::Found(target) if target == page_position
                    )
                })
            });
            let path_matches = document
                .rel
                .to_string_lossy()
                .to_lowercase()
                .contains(&needle);
            let named = page_position == Some(position)
                || linked
                || path_matches
                || document.raw.to_lowercase().contains(&needle);
            if !named {
                return None;
            }
            let tier = document.front.best_tier();
            Some((
                tier_rank(tier),
                Hit {
                    path: document.rel.to_string_lossy().into_owned(),
                    tier: tier.map(|tier| tier.label().to_owned()),
                    title: document.label(),
                    line: first_meaningful_line(document),
                    matched_terms: 1,
                    total_terms: 1,
                    by_meaning: false,
                },
            ))
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);

    Ok(Dossier {
        entity: entity.to_owned(),
        page,
        candidates,
        hits: hits.into_iter().map(|(_, hit)| hit).collect(),
    })
}

fn document_at(tree: &Tree, position: usize) -> Result<&Doc> {
    tree.docs
        .get(position)
        .ok_or_else(|| anyhow!("entity match points outside the tree"))
}

fn build_dossier_page(document: &Doc, fields: &[String]) -> Result<DossierPage> {
    let frontmatter = document.frontmatter_json()?;
    let serde_json::Value::Object(frontmatter) = frontmatter else {
        bail!(
            "frontmatter in {} must be an object",
            document.rel.to_string_lossy()
        );
    };
    let vitals = fields
        .iter()
        .filter_map(|field| {
            frontmatter
                .get(field)
                .cloned()
                .map(|value| (field.to_owned(), value))
        })
        .collect();
    Ok(DossierPage {
        path: document.rel.to_string_lossy().into_owned(),
        vitals,
    })
}

fn first_meaningful_line(doc: &Doc) -> String {
    doc.body()
        .lines()
        .map(str::trim)
        .find(|line| line.len() > 20 && !line.starts_with('#'))
        .unwrap_or("")
        .to_owned()
}

/// Prints a search result, keeping an absence claim honest: the tree is only said to lack
/// every term when nothing matched at all.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(search: &Search, out: &mut dyn Write) -> std::io::Result<()> {
    if let Some(notice) = &search.notice {
        writeln!(out, "{}\n", printable(notice).yellow())?;
    }
    if search.hits.is_empty() {
        if search.dropped_by_verbatim > 0 {
            return writeln!(
                out,
                "no direct-source hits — {} document(s) match in lower tiers; drop --verbatim to see them",
                search.dropped_by_verbatim
            );
        } else if search.total_terms > 1 {
            return writeln!(
                out,
                "no hits — none of the {} query terms appear anywhere in the tree",
                search.total_terms
            );
        }
        return writeln!(out, "no hits");
    }
    print_hits(&search.hits, out)
}

fn print_hits(hits: &[Hit], out: &mut dyn Write) -> std::io::Result<()> {
    if hits
        .first()
        .is_some_and(|hit| !hit.by_meaning && hit.matched_terms < hit.total_terms)
    {
        let (matched, total) = hits
            .first()
            .map_or((0, 0), |hit| (hit.matched_terms, hit.total_terms));
        writeln!(
            out,
            "{}",
            format!(
                "no document matches all {total} terms — best match covers {matched}; narrow the query before relying on absence"
            )
            .yellow()
        )?;
        writeln!(out)?;
    }
    // Titles, lines and paths are the tree's text, and so the sources' text: each is printed
    // without its control characters.
    for hit in hits {
        let tier = printable(hit.tier.as_deref().unwrap_or("--"));
        let title = printable(&hit.title);
        if hit.by_meaning {
            writeln!(
                out,
                "[{}] {title}  {}",
                tier.bold(),
                "(by meaning)".yellow()
            )?;
        } else if hit.matched_terms < hit.total_terms {
            writeln!(
                out,
                "[{}] {title}  {}",
                tier.bold(),
                format!("({}/{} terms)", hit.matched_terms, hit.total_terms).yellow()
            )?;
        } else {
            writeln!(out, "[{}] {title}", tier.bold())?;
        }
        writeln!(out, "     {}", printable(&hit.line))?;
        writeln!(out, "     {}\n", printable(&hit.path).dimmed())?;
    }
    Ok(())
}

/// Prints a dossier.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_dossier(dossier: &Dossier, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "{} — {} documents\n",
        printable(&dossier.entity).bold(),
        dossier.hits.len()
    )?;
    if !dossier.candidates.is_empty() {
        writeln!(out, "ambiguous entity page; candidates:")?;
        for candidate in &dossier.candidates {
            writeln!(out, "  {}", printable(candidate))?;
        }
        writeln!(out)?;
    }
    if dossier.hits.is_empty() {
        return writeln!(out, "no hits");
    }
    print_hits(&dossier.hits, out)
}
