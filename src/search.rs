//! Retrieval over a markdown context tree.

use owo_colors::OwoColorize as _;
use serde::Serialize;

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

/// Every document that names an entity, links to its resolved page, plus the entity's own page.
#[must_use]
pub fn dossier(tree: &Tree, entity: &str) -> Vec<Hit> {
    let needle = entity.to_lowercase();
    let entity_position = tree
        .docs
        .iter()
        .find_map(|from| match tree.resolve(from, entity) {
            Resolution::Found(position) => Some(position),
            Resolution::Missing | Resolution::Ambiguous(_) => None,
        });
    let mut hits: Vec<(u8, Hit)> = tree
        .docs
        .iter()
        .enumerate()
        .filter_map(|(position, doc)| {
            let searchable =
                format!("{}\n{}", doc.rel.to_string_lossy(), doc.body()).to_lowercase();
            let linked = entity_position.is_some_and(|entity_position| {
                wikilinks(&doc.raw).iter().any(|link| {
                    matches!(
                        tree.resolve(doc, &link.target),
                        Resolution::Found(target) if target == entity_position
                    )
                })
            });
            let named = entity_position.is_some_and(|entity_position| position == entity_position)
                || linked
                || searchable.contains(&needle)
                || doc
                    .front
                    .aliases
                    .iter()
                    .any(|alias| alias.to_lowercase() == needle);
            if !named {
                return None;
            }
            let tier = doc.front.best_tier();
            Some((
                tier_rank(tier),
                Hit {
                    path: doc.rel.to_string_lossy().into_owned(),
                    tier: tier.map(|tier| tier.label().to_owned()),
                    title: doc.label(),
                    line: first_meaningful_line(doc),
                    matched_terms: 1,
                    total_terms: 1,
                },
            ))
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, hit)| hit).collect()
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

pub fn print_dossier(entity: &str, pages: &[Hit]) {
    println!("{} — {} documents\n", entity.bold(), pages.len());
    print(pages, 1);
}
