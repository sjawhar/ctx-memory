//! Declared source and quote-claim provenance.

use anyhow::Result;

use super::evidence::{
    record_sources, source_pointers, CitedSource, Evidence, QuoteSource, SourceStatus,
};
use super::lookup::{resolve_with_live, source_verbatim_unknown, Lookup, ResolutionMode};
use super::{broken, Verification};
use crate::pointer::Found;
use crate::tree::Doc;

/// Resolves every pointer the document cites and reports the ones that do not resolve.
///
/// Returns what each cited source pointer turned out to be, the stored text of each source a
/// quote may be checked against, and the provenance the record declares for itself.
pub(super) fn verify_pointers(
    lookup: Lookup<'_>,
    document: &Doc,
    live: bool,
    verification: &mut Verification,
) -> Result<Evidence> {
    let Lookup {
        context,
        store,
        schemes,
        mentions,
    } = lookup;
    let source_pointers = source_pointers(document, schemes)?;
    let mode = ResolutionMode::Verify {
        live,
        refresh: false,
    };
    let mut cited: Vec<CitedSource> = Vec::new();
    let mut quote_sources: Vec<QuoteSource> = Vec::new();
    for found in schemes.find_all(&document.raw) {
        let is_source_pointer = source_pointers.contains(&found.pointer);
        let resolved = resolve_with_live(lookup, &found.pointer, false, mode)?;
        if let Some(error) = resolved.source_error {
            verification.source_errors = true;
            verification
                .broken
                .push(broken(document, found.start, &found.pointer, error));
            keep_cited(
                &mut cited,
                is_source_pointer,
                &found,
                SourceStatus::Reported,
            );
            continue;
        }
        if !resolved.verbatim {
            *verification
                .skipped
                .entry(found.pointer.scheme.clone())
                .or_default() += 1;
            let status =
                if is_source_pointer && source_verbatim_unknown(context, store, &found.pointer)? {
                    SourceStatus::Undeclared
                } else {
                    SourceStatus::NotVerbatim
                };
            keep_cited(&mut cited, is_source_pointer, &found, status);
            continue;
        }
        verification.checked += 1;
        if resolved.text.is_none() {
            verification.broken.push(broken(
                document,
                found.start,
                &found.pointer,
                "does not resolve in the local store".to_owned(),
            ));
            keep_cited(
                &mut cited,
                is_source_pointer,
                &found,
                SourceStatus::Reported,
            );
            continue;
        }
        if !is_source_pointer {
            continue;
        }
        let thread_resolved = resolve_with_live(lookup, &found.pointer, true, mode)?;
        if let Some(error) = thread_resolved.source_error {
            verification.source_errors = true;
            verification
                .broken
                .push(broken(document, found.start, &found.pointer, error));
            keep_cited(&mut cited, true, &found, SourceStatus::Reported);
            continue;
        }
        keep_cited(&mut cited, true, &found, SourceStatus::Verbatim);
        if quote_sources
            .iter()
            .any(|source| source.pointer == found.pointer)
        {
            continue;
        }
        quote_sources.push(QuoteSource::load(
            store,
            schemes,
            mentions,
            found.pointer,
            found.start,
        )?);
    }
    let record = record_sources(document, schemes, source_pointers)?;
    Ok(Evidence {
        cited,
        quote_sources,
        record,
    })
}

/// Keeps what resolving a pointer learned, once per pointer. A pointer the record does not
/// declare as a source never carries one of its quotes, so it is not kept.
fn keep_cited(
    cited: &mut Vec<CitedSource>,
    is_source_pointer: bool,
    found: &Found,
    status: SourceStatus,
) {
    if !is_source_pointer || cited.iter().any(|source| source.pointer == found.pointer) {
        return;
    }
    cited.push(CitedSource {
        pointer: found.pointer.clone(),
        start: found.start,
        status,
    });
}
