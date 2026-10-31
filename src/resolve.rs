//! Local record resolution and citation verification.

mod evidence;
mod lookup;
mod provenance;
mod quotes;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;

use crate::context::Context;
use crate::output::printable;
use crate::pointer::Pointer;
use crate::source;
use crate::source::slack::{slack_schemes, Mentions};
use crate::store::Store;
use crate::tree::{Doc, Tree};

use lookup::{resolve_with_live, Lookup, ResolutionMode};
use provenance::verify_pointers;
use quotes::{quote_claims, refresh_quote_sources, verify_quotes};

/// The outcome for one `ctx resolve` input.
#[derive(Debug, Serialize)]
pub struct Resolved {
    pub input: String,
    pub pointer: Option<String>,
    pub text: Option<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub source_error: bool,
}

/// A citation whose target or quoted text cannot be verified locally.
#[derive(Debug, Serialize)]
pub struct Broken {
    pub file: String,
    pub line: usize,
    pub pointer: String,
    pub why: String,
}

/// The aggregate outcome from citation verification.
#[derive(Debug, Default, Serialize)]
pub struct Verification {
    pub checked: usize,
    pub broken: Vec<Broken>,
    pub skipped: BTreeMap<String, usize>,
    pub quotes_unchecked: usize,
    /// Quotes the verification could not check, with the reason each one defeated it: the
    /// cited source has never declared whether its records are its own words, it stores
    /// renderings rather than the words themselves, no configured source owns the scheme the
    /// claim names, or a claim at tier 1 or 2 names no resolvable pointer at all. A record
    /// that ranks at tier 1 or 2 with no quote at those tiers checked against its source is
    /// listed here too, as `tier 1` or `tier 2`, and so is a record whose frontmatter does not
    /// parse, whose quotes nothing could read, as `frontmatter`. `cite verify` exits 1 on any of
    /// these, and the single writer refuses them, since it cannot tell whether the quote it is
    /// about to commit should have been checked.
    pub unchecked: Vec<Broken>,
    #[serde(skip)]
    pub source_errors: bool,
}

/// Resolves `pointers` from the local store, then through each owning source's `resolve` operation.
///
/// # Errors
///
/// Returns an error when the context has no state directory, a configured source cannot run, or
/// the local store cannot be opened, updated, or read.
pub fn resolve(context: &Context, pointers: &[String], thread: bool) -> Result<Vec<Resolved>> {
    let schemes = source::configured_schemes(&context.config.sources)?;
    let store = Store::load(context.data()?, context.state()?)?;
    let mentions = Mentions::new(&store, slack_schemes(&context.config.sources)?);
    let lookup = Lookup {
        context,
        store: &store,
        schemes: &schemes,
        mentions: &mentions,
    };
    pointers
        .iter()
        .map(|input| match schemes.parse(input) {
            Some(pointer) => {
                let resolved =
                    resolve_with_live(lookup, &pointer, thread, ResolutionMode::Resolve)?;
                let source_error = resolved.source_error;
                Ok(Resolved {
                    input: input.clone(),
                    pointer: Some(pointer.to_string()),
                    text: resolved.text,
                    error: source_error.clone(),
                    source_error: source_error.is_some(),
                })
            }
            None => Ok(Resolved {
                input: input.clone(),
                pointer: None,
                text: None,
                error: Some("invalid or unregistered pointer".to_owned()),
                source_error: false,
            }),
        })
        .collect()
}

/// Verifies source resolution and quote containment for pointers in selected tree documents.
///
/// With no file named, every document of the tree is checked. A named file is checked however
/// the caller writes its path: relative to the working directory or to the tree's root, or
/// absolute. One the caller sent as a draft (`drafts`, by the path the caller named) is checked
/// as that text. A path that names no record of the tree is an error, never `checked 0`: the
/// caller asked for that file to be checked, and nothing was.
///
/// # Errors
///
/// Returns an error when a named file is no record of the tree, the context has no state
/// directory, a requested live source cannot run, or the local store cannot be opened,
/// updated, or read.
pub fn verify(
    context: &Context,
    files: &[PathBuf],
    drafts: &[Doc],
    live: bool,
) -> Result<Verification> {
    let mut documents: Vec<&Doc> = Vec::new();
    if files.is_empty() {
        documents.extend(&context.tree.docs);
    }
    for file in files {
        let document = match drafts.iter().find(|draft| draft.rel == *file) {
            Some(draft) => draft,
            None => named_document(&context.tree, file)?,
        };
        if !documents
            .iter()
            .any(|chosen| std::ptr::eq(*chosen, document))
        {
            documents.push(document);
        }
    }
    verify_documents(context, &documents, live)
}

/// Verifies source resolution and quote containment in `documents`.
///
/// # Errors
///
/// Returns an error when the context has no state directory, a requested live source cannot
/// run, or the local store cannot be opened, updated, or read.
pub fn verify_documents(context: &Context, documents: &[&Doc], live: bool) -> Result<Verification> {
    let schemes = source::configured_schemes(&context.config.sources)?;
    let store = Store::load(context.data()?, context.state()?)?;
    let mentions = Mentions::new(&store, slack_schemes(&context.config.sources)?);
    let mut verification = Verification::default();
    let lookup = Lookup {
        context,
        store: &store,
        schemes: &schemes,
        mentions: &mentions,
    };
    for document in documents {
        verify_document(lookup, document, live, &mut verification)?;
    }
    Ok(verification)
}

/// The record of `tree` that `named` names: a path relative to the working directory, or to
/// the tree's root, or an absolute one.
fn named_document<'t>(tree: &'t Tree, named: &Path) -> Result<&'t Doc> {
    let mut candidates = Vec::with_capacity(2);
    if named.is_absolute() {
        candidates.push(named.to_path_buf());
    } else {
        if let Ok(directory) = std::env::current_dir() {
            candidates.push(directory.join(named));
        }
        candidates.push(tree.root.join(named));
    }
    for candidate in candidates {
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        let Ok(relative) = real.strip_prefix(&tree.root) else {
            continue;
        };
        if let Some(document) = tree.docs.iter().find(|document| document.rel == relative) {
            return Ok(document);
        }
    }
    bail!(
        "{} names no record of this tree, so nothing checked it",
        printable(&named.to_string_lossy())
    )
}

/// A citation that cannot be verified, at the line where the document writes it.
fn finding(document: &Doc, start: usize, pointer: String, why: String) -> Broken {
    Broken {
        file: document.rel.to_string_lossy().into_owned(),
        line: document.line_at(start),
        pointer,
        why,
    }
}

/// A cited pointer that cannot be verified, at the line where the document cites it.
fn broken(document: &Doc, start: usize, pointer: &Pointer, why: String) -> Broken {
    finding(document, start, pointer.to_string(), why)
}

/// Where the record writes `text`, for the line a finding names.
fn text_start(document: &Doc, text: &str) -> usize {
    document.raw.find(text).unwrap_or(0)
}

fn verify_document(
    lookup: Lookup<'_>,
    document: &Doc,
    live: bool,
    verification: &mut Verification,
) -> Result<()> {
    // A record whose frontmatter does not parse has no claims this check can read: its quotes,
    // tiers and sources are in text nothing parsed. Its pointers are still resolved below, but
    // passing it would tell a cycle that verifies before it writes that the writer will take
    // it, and the writer refuses it as frontmatter-invalid.
    if let Some(error) = &document.front_error {
        verification.unchecked.push(finding(
            document,
            0,
            "frontmatter".to_owned(),
            format!(
                "the frontmatter does not parse, so no quote of this record was checked: {error}"
            ),
        ));
    }
    let claims = quote_claims(document);
    let mut evidence = verify_pointers(lookup, document, live, verification)?;
    refresh_quote_sources(lookup, document, live, &claims, &mut evidence, verification)?;
    verify_quotes(document, lookup.schemes, &claims, &evidence, verification);
    Ok(())
}

/// Prints each resolved pointer and the text it resolved to.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_resolved(results: &[Resolved], out: &mut dyn Write) -> std::io::Result<()> {
    // The resolved text is a source's, and an error may quote one: each is printed without
    // its control characters.
    for result in results {
        writeln!(out, "{}", printable(&result.input))?;
        if let Some(error) = result.error.as_deref() {
            writeln!(out, "  error: {}", printable(error))?;
        } else if let Some(text) = result.text.as_deref() {
            writeln!(out, "{}", printable(text))?;
        } else {
            writeln!(out, "  not found")?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Prints a verification, including the quotes nothing checked.
///
/// A reader that sees only `checked` and the broken list would read "checked 0" as success
/// for a record whose quote nothing compared to a source — which the single writer refuses.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_verification(verification: &Verification, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "checked {}", verification.checked)?;
    if verification.quotes_unchecked > 0 {
        writeln!(
            out,
            "quotes nothing checked {}",
            verification.quotes_unchecked
        )?;
    }
    for broken in &verification.broken {
        writeln!(
            out,
            "{}:{}: {} — {}",
            printable(&broken.file),
            broken.line,
            printable(&broken.pointer),
            printable(&broken.why)
        )?;
    }
    for unchecked in &verification.unchecked {
        writeln!(
            out,
            "unchecked {}:{}: {} — {}",
            printable(&unchecked.file),
            unchecked.line,
            printable(&unchecked.pointer),
            printable(&unchecked.why)
        )?;
    }
    for (scheme, skipped) in &verification.skipped {
        writeln!(out, "skipped {}: {skipped}", printable(scheme))?;
    }
    Ok(())
}
