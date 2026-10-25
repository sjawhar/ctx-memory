//! Local record resolution and citation verification.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::context::Context;
use crate::pointer::{Pointer, Schemes};
use crate::record::render;
use crate::store::Store;

/// The outcome for one `ctx resolve` input.
#[derive(Debug, Serialize)]
pub struct Resolved {
    pub input: String,
    pub pointer: Option<String>,
    pub text: Option<String>,
    pub error: Option<String>,
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
}

/// Resolves `pointers` from the local store only.
///
/// # Errors
///
/// Returns an error when the context has no state directory or the local store cannot be opened
/// or read.
pub fn resolve(context: &Context, pointers: &[String], thread: bool) -> Result<Vec<Resolved>> {
    let schemes = context_schemes(context);
    let store = Store::open(&context.state()?.store())?;
    pointers
        .iter()
        .map(|input| match schemes.parse(input) {
            Some(pointer) => resolve_one(&store, &schemes, &pointer, thread).map(|text| Resolved {
                input: input.clone(),
                pointer: Some(pointer.to_string()),
                text,
                error: None,
            }),
            None => Ok(Resolved {
                input: input.clone(),
                pointer: None,
                text: None,
                error: Some("invalid or unregistered pointer".to_owned()),
            }),
        })
        .collect()
}

/// Verifies local resolution and quote containment for pointers in selected tree documents.
///
/// # Errors
///
/// Returns an error when the context has no state directory or the local store cannot be opened
/// or read.
pub fn verify(context: &Context, files: &[PathBuf], _live: bool) -> Result<Verification> {
    let schemes = context_schemes(context);
    let store = Store::open(&context.state()?.store())?;
    let mut verification = Verification::default();
    for document in &context.tree.docs {
        if !files.is_empty() && !files.contains(&document.rel) {
            continue;
        }
        for found in schemes.find_all(&document.raw) {
            if !is_verbatim(&found.pointer.scheme) {
                *verification
                    .skipped
                    .entry(found.pointer.scheme.clone())
                    .or_default() += 1;
                continue;
            }
            verification.checked += 1;
            let text = resolve_one(&store, &schemes, &found.pointer, false)?;
            let why = match (text.as_deref(), document.front.quote.as_deref()) {
                (None, _) => Some("does not resolve in the local store"),
                (Some(text), Some(quote)) if !text.contains(quote) => {
                    Some("quote does not appear in the resolved text")
                }
                _ => None,
            };
            if let Some(why) = why {
                verification.broken.push(Broken {
                    file: document.rel.to_string_lossy().into_owned(),
                    line: line_number(&document.raw, found.start),
                    pointer: found.pointer.to_string(),
                    why: why.to_owned(),
                });
            }
        }
    }
    Ok(verification)
}

fn context_schemes(context: &Context) -> Schemes {
    Schemes::new(context.config.sources.iter().filter_map(|source| {
        source.scheme.clone().or_else(|| {
            matches!(
                source.kind.as_str(),
                "slack" | "gmail" | "drive" | "calendar"
            )
            .then(|| source.kind.clone())
        })
    }))
}

fn is_verbatim(scheme: &str) -> bool {
    matches!(scheme, "slack" | "gmail")
}

fn resolve_one(
    store: &Store,
    schemes: &Schemes,
    pointer: &Pointer,
    thread: bool,
) -> Result<Option<String>> {
    let is_gmail_thread = pointer.scheme == "gmail" && pointer.locator.starts_with("thread/");
    if !thread && !is_gmail_thread {
        return store
            .get(pointer)
            .map(|record| record.map(|record| render(&record)));
    }
    let mut records = store.thread(pointer)?;
    let pointer_text = pointer.to_string();
    let nested_thread = records
        .iter()
        .find(|record| record.pointer == pointer_text)
        .and_then(|record| record.thread.as_deref())
        .map(str::to_owned);
    if let Some(nested_thread) = nested_thread {
        let thread_pointer = schemes.parse(&nested_thread).ok_or_else(|| {
            anyhow!("stored thread pointer {nested_thread} is invalid for its configured scheme")
        })?;
        records = store.thread(&thread_pointer)?;
    }
    (!records.is_empty())
        .then(|| records.iter().map(render).collect::<Vec<_>>().join("\n\n"))
        .map_or(Ok(None), |text| Ok(Some(text)))
}

fn line_number(text: &str, offset: usize) -> usize {
    text.get(..offset).map_or(1, |prefix| {
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1
    })
}
