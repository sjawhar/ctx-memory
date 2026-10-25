//! Retrieval over a markdown context tree.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use owo_colors::OwoColorize as _;
use serde::Serialize;

use crate::config::TreeConfig;
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

/// Rank by term coverage, then authority tier. Partial matches are returned and labeled.
pub fn run(tree: &Tree, query: &str, verbatim_only: bool, limit: usize) -> Vec<Hit> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<(usize, u8, Hit)> = Vec::new();

    for doc in &tree.docs {
        let tier = doc.front.best_tier();
        if verbatim_only && !tier.is_some_and(Tier::is_verbatim) {
            continue;
        }
        let body = doc.body().to_lowercase();
        let matched = terms
            .iter()
            .filter(|term| body.contains(term.as_str()))
            .count();
        if matched == 0 {
            continue;
        }
        let mut best: Option<(usize, &str)> = None;
        for line in doc.body().lines() {
            let trimmed = line.trim();
            if trimmed.len() < 12 {
                continue;
            }
            let lower = trimmed.to_lowercase();
            let matched_in_line = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            if matched_in_line > 0 && best.is_none_or(|(count, _)| matched_in_line > count) {
                best = Some((matched_in_line, trimmed));
            }
        }
        let Some((_, line)) = best else { continue };
        hits.push((
            terms.len() - matched,
            tier_rank(tier),
            Hit {
                path: doc.rel.to_string_lossy().into_owned(),
                tier: tier.map(|tier| tier.label().to_owned()),
                title: doc.label(),
                line: line.to_owned(),
                matched_terms: matched,
                total_terms: terms.len(),
            },
        ));
    }

    hits.sort_by_key(|(missed, rank, _)| (*missed, *rank));
    hits.into_iter()
        .take(limit)
        .map(|(_, _, hit)| hit)
        .collect()
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

pub fn print(hits: &[Hit], total_terms: usize) {
    if hits.is_empty() {
        if total_terms > 1 {
            println!("no hits — none of the {total_terms} query terms appear anywhere in the tree");
        } else {
            println!("no hits");
        }
        return;
    }
    if hits
        .first()
        .is_some_and(|hit| hit.matched_terms < hit.total_terms)
    {
        let (matched, total) = hits
            .first()
            .map_or((0, 0), |hit| (hit.matched_terms, hit.total_terms));
        println!(
            "{}",
            format!(
                "no document matches all {total} terms — best match covers {matched}; narrow the query before relying on absence"
            )
            .yellow()
        );
        println!();
    }
    for hit in hits {
        let tier = hit.tier.as_deref().unwrap_or("--");
        if hit.matched_terms < hit.total_terms {
            println!(
                "[{}] {}  {}",
                tier.bold(),
                hit.title,
                format!("({}/{} terms)", hit.matched_terms, hit.total_terms).yellow()
            );
        } else {
            println!("[{}] {}", tier.bold(), hit.title);
        }
        println!("     {}", hit.line);
        println!("     {}\n", hit.path.dimmed());
    }
}

pub fn print_dossier(dossier: &Dossier) {
    println!(
        "{} — {} documents\n",
        dossier.entity.bold(),
        dossier.hits.len()
    );
    if !dossier.candidates.is_empty() {
        println!("ambiguous entity page; candidates:");
        for candidate in &dossier.candidates {
            println!("  {candidate}");
        }
        println!();
    }
    print(&dossier.hits, 1);
}
