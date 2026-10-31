//! Quote matching, normalisation, and tier judgment.

use anyhow::Result;

use super::evidence::{
    claim_provenance, long_enough, Evidence, Provenance, QuoteClaim, QuoteSource,
    FRAGMENT_CHARACTERS, FRAGMENT_WORDS, NO_SOURCE, UNCHECKED_TIER, UNQUOTED_TIER,
};
use super::lookup::{resolve_with_live, thread_refresh_pointer, Lookup, ResolutionMode};
use super::{broken, finding, Verification};
use crate::pointer::Schemes;
use crate::tree::{Doc, Tier};

/// The quotes a record makes: the single `quote`, which the record's own sources carry, and
/// each `quotes` entry, which names its own source.
pub(super) fn quote_claims(document: &Doc) -> Vec<QuoteClaim<'_>> {
    let record_tier = document.front.best_tier();
    let single = document.front.quote.as_deref().map(|quote| QuoteClaim {
        quote,
        text: None,
        tier: record_tier,
    });
    let listed = document.front.quotes.iter().map(|entry| QuoteClaim {
        quote: entry.quote.as_str(),
        text: entry
            .source
            .as_deref()
            .filter(|text| !text.trim().is_empty()),
        tier: entry.tier_in(record_tier),
    });
    single.into_iter().chain(listed).collect()
}

pub(super) fn refresh_quote_sources(
    lookup: Lookup<'_>,
    document: &Doc,
    live: bool,
    claims: &[QuoteClaim<'_>],
    evidence: &mut Evidence,
    verification: &mut Verification,
) -> Result<()> {
    let Lookup {
        store,
        schemes,
        mentions,
        ..
    } = lookup;
    let record_texts: Vec<&str> = evidence.record.texts.iter().map(String::as_str).collect();
    let every_claim_matches = claims.iter().all(|claim| {
        match claim_provenance(claim, document, schemes, evidence, &record_texts) {
            Provenance::Checkable(sources) => {
                sources.iter().any(|source| source.holds(claim.quote))
            }
            // Newer text cannot give a claim a source it never named.
            Provenance::Reported
            | Provenance::Uncheckable { .. }
            | Provenance::Untraceable { .. } => true,
        }
    });
    drop(record_texts);
    if !live || claims.is_empty() || every_claim_matches {
        return Ok(());
    }
    for source in &mut evidence.quote_sources {
        let pointer = thread_refresh_pointer(store, schemes, &source.pointer)?;
        let resolved = resolve_with_live(
            lookup,
            &pointer,
            true,
            ResolutionMode::Verify {
                live,
                refresh: true,
            },
        )?;
        if let Some(error) = resolved.source_error {
            verification.source_errors = true;
            verification
                .broken
                .push(broken(document, source.start, &source.pointer, error));
            continue;
        }
        *source = QuoteSource::load(
            store,
            schemes,
            mentions,
            source.pointer.clone(),
            source.start,
        )?;
    }
    Ok(())
}

/// Checks every quote the record makes against the source that claim names, then whether
/// the tier the record ranks at is borne out by one of them.
///
/// Each claim ends in one of three places. It is verified against a source that stores its
/// records verbatim and holds the quote; it is declared unverifiable, which the writer
/// refuses; or it names no pointer at all, which only a summary (tier 3) or a secondhand
/// account (tier 4) may do.
pub(super) fn verify_quotes(
    document: &Doc,
    schemes: &Schemes,
    claims: &[QuoteClaim<'_>],
    evidence: &Evidence,
    verification: &mut Verification,
) {
    let record_texts: Vec<&str> = evidence.record.texts.iter().map(String::as_str).collect();
    // Whether a quote at tier 1 or 2 held in its source, whether one that did not already
    // carries a finding of its own, and whether one held but was too short to count.
    let mut verbatim_held = false;
    let mut verbatim_refused = false;
    let mut verbatim_short = false;
    for claim in claims {
        let verbatim = claim.tier.is_some_and(Tier::is_verbatim);
        match claim_provenance(claim, document, schemes, evidence, &record_texts) {
            Provenance::Checkable(sources) => {
                if sources.iter().any(|source| source.holds(claim.quote)) {
                    if long_enough(claim.quote) {
                        verbatim_held |= verbatim;
                    } else {
                        // It appears in its source, but a match this short appears by chance
                        // in most messages, so nothing was checked. It is not refused for
                        // that alone: it bears out no tier, and the tier rule below decides.
                        verification.quotes_unchecked += 1;
                        verbatim_short |= verbatim;
                    }
                    continue;
                }
                let Some(source) = sources.first() else {
                    continue;
                };
                verification.broken.push(broken(
                    document,
                    source.start,
                    &source.pointer,
                    "quote does not appear in the resolved text".to_owned(),
                ));
                verbatim_refused |= verbatim;
            }
            // The pointer pass already named this one as broken.
            Provenance::Reported => {
                verification.quotes_unchecked += 1;
                verbatim_refused |= verbatim;
            }
            Provenance::Uncheckable {
                start,
                pointer,
                why,
            } => {
                verification.quotes_unchecked += 1;
                verification
                    .unchecked
                    .push(finding(document, start, pointer, why.to_owned()));
                verbatim_refused |= verbatim;
            }
            Provenance::Untraceable { start, pointer } => {
                verification.quotes_unchecked += 1;
                // Tiers 3 and 4 are a summary and a secondhand account: naming no source is
                // what they mean. Tiers 1 and 2 are a source's own words, so a claim that
                // makes one owes a pointer the tool can resolve. A claim that declares no
                // tier at all is read as making the stronger claim.
                if claim.tier.is_none_or(Tier::is_verbatim) {
                    verification.unchecked.push(finding(
                        document,
                        start,
                        pointer,
                        NO_SOURCE.to_owned(),
                    ));
                    verbatim_refused |= verbatim;
                }
            }
        }
    }
    // A tier 1 or 2 quote that did not hold is refused by its own finding, and naming the
    // record beside it would add nothing.
    if !verbatim_held && !verbatim_refused {
        verify_record_tier(document, claims, verbatim_short, verification);
    }
}

/// Names a record that ranks at tier 1 or 2 when no quote at those tiers bears that out.
///
/// Search ranks a record by the strongest tier it declares anywhere, and a quote's own tier
/// overrides the record's when its claim is judged. A record at tier 1 or 2 whose every quote
/// is a tier 3 or 4 account, is too short to be evidence, or that quotes nothing, would rank
/// as a source's own words that nothing checked; the single writer refuses it.
///
/// The tier is the record's, not each quote's: `search --verbatim` keeps or drops the whole
/// record by it, so a tier 3 `quotes` entry in a record that one checked tier 1 quote bears out
/// is shown at tier 1 beside that quote.
fn verify_record_tier(
    document: &Doc,
    claims: &[QuoteClaim<'_>],
    verbatim_short: bool,
    verification: &mut Verification,
) {
    let Some(tier) = document.front.best_tier().filter(|tier| tier.is_verbatim()) else {
        return;
    };
    let why = if claims.is_empty() {
        UNQUOTED_TIER.to_owned()
    } else if verbatim_short {
        format!(
            "{UNCHECKED_TIER}: a quote counts as checked only when each fragment between its \
             ellipses holds at least {FRAGMENT_WORDS} words or {FRAGMENT_CHARACTERS} letters \
             and digits"
        )
    } else {
        UNCHECKED_TIER.to_owned()
    };
    verification.unchecked.push(finding(
        document,
        tier_start(document),
        format!("tier {}", tier.number()),
        why,
    ));
}

/// Where the record declares a tier, for the line a finding about its tier names: the first
/// frontmatter line that sets `tier` or `source_tier`, at any depth.
fn tier_start(document: &Doc) -> usize {
    document
        .frontmatter_line_start(|line| {
            let key = line.trim_start_matches(|character: char| {
                character == '-' || character.is_whitespace()
            });
            key.starts_with("tier:") || key.starts_with("source_tier:")
        })
        .unwrap_or(0)
}
