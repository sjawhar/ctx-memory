//! Local record resolution and citation verification.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context as _, Result};
use serde::Serialize;
use serde_json::Value;

use crate::config::SourceConfig;
use crate::context::Context;
use crate::pointer::{Pointer, Schemes};
use crate::record::render;
use crate::source::{self, Request as SourceRequest, ResolveRequest, Session, SourceRef};
use crate::store::Store;
use crate::tree::Doc;

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
    #[serde(skip)]
    pub source_errors: bool,
}

struct ResolvedText {
    text: Option<String>,
    verbatim: bool,
    source_error: Option<String>,
}

struct QuoteSource {
    pointer: Pointer,
    start: usize,
    text: String,
}

#[derive(Clone, Copy)]
enum ResolutionMode {
    Resolve,
    Verify { live: bool, refresh: bool },
}

impl ResolutionMode {
    const fn live(self) -> bool {
        match self {
            Self::Resolve => true,
            Self::Verify { live, .. } => live,
        }
    }

    const fn needs_verbatim(self) -> bool {
        matches!(self, Self::Verify { .. })
    }

    const fn force_live(self) -> bool {
        matches!(self, Self::Verify { refresh: true, .. })
    }
}

/// Resolves `pointers` from the local store, then through each owning source's `resolve` operation.
///
/// # Errors
///
/// Returns an error when the context has no state directory, a configured source cannot run, or
/// the local store cannot be opened, updated, or read.
pub fn resolve(context: &Context, pointers: &[String], thread: bool) -> Result<Vec<Resolved>> {
    let schemes = source::configured_schemes(&context.config.sources)?;
    let store = Store::open(&context.state()?.store())?;
    pointers
        .iter()
        .map(|input| match schemes.parse(input) {
            Some(pointer) => {
                let resolved = resolve_with_live(
                    context,
                    &store,
                    &schemes,
                    &pointer,
                    thread,
                    ResolutionMode::Resolve,
                )?;
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
/// # Errors
///
/// Returns an error when the context has no state directory, a requested live source cannot run,
/// or the local store cannot be opened, updated, or read.
pub fn verify(context: &Context, files: &[PathBuf], live: bool) -> Result<Verification> {
    let schemes = source::configured_schemes(&context.config.sources)?;
    let store = Store::open(&context.state()?.store())?;
    let mut verification = Verification::default();
    for document in &context.tree.docs {
        if files.is_empty() || files.contains(&document.rel) {
            verify_document(context, &store, &schemes, document, live, &mut verification)?;
        }
    }
    Ok(verification)
}

fn verify_document(
    context: &Context,
    store: &Store,
    schemes: &Schemes,
    document: &Doc,
    live: bool,
    verification: &mut Verification,
) -> Result<()> {
    let source_pointers = source_pointers(document, schemes)?;
    let mut has_verbatim_source = false;
    let mut quote_sources = Vec::new();
    for found in schemes.find_all(&document.raw) {
        let is_source_pointer = source_pointers.contains(&found.pointer);
        let resolved = resolve_with_live(
            context,
            store,
            schemes,
            &found.pointer,
            false,
            ResolutionMode::Verify {
                live,
                refresh: false,
            },
        )?;
        if is_source_pointer && resolved.verbatim {
            has_verbatim_source = true;
        }
        if let Some(error) = resolved.source_error {
            verification.source_errors = true;
            verification.broken.push(Broken {
                file: document.rel.to_string_lossy().into_owned(),
                line: line_number(&document.raw, found.start),
                pointer: found.pointer.to_string(),
                why: error,
            });
            continue;
        }
        if !resolved.verbatim {
            *verification
                .skipped
                .entry(found.pointer.scheme.clone())
                .or_default() += 1;
            continue;
        }
        verification.checked += 1;
        if resolved.text.is_none() {
            verification.broken.push(Broken {
                file: document.rel.to_string_lossy().into_owned(),
                line: line_number(&document.raw, found.start),
                pointer: found.pointer.to_string(),
                why: "does not resolve in the local store".to_owned(),
            });
            continue;
        }
        if is_source_pointer {
            let thread_resolved = resolve_with_live(
                context,
                store,
                schemes,
                &found.pointer,
                true,
                ResolutionMode::Verify {
                    live,
                    refresh: false,
                },
            )?;
            if let Some(error) = thread_resolved.source_error {
                verification.source_errors = true;
                verification.broken.push(Broken {
                    file: document.rel.to_string_lossy().into_owned(),
                    line: line_number(&document.raw, found.start),
                    pointer: found.pointer.to_string(),
                    why: error,
                });
                continue;
            }
            if !quote_sources
                .iter()
                .any(|source: &QuoteSource| source.pointer == found.pointer)
            {
                let text = local_records(store, schemes, &found.pointer, true)?
                    .into_iter()
                    .map(|record| record.text)
                    .collect::<Vec<_>>()
                    .join("\n");
                quote_sources.push(QuoteSource {
                    pointer: found.pointer,
                    start: found.start,
                    text,
                });
            }
        }
    }
    refresh_quote_sources(
        context,
        store,
        schemes,
        document,
        live,
        &mut quote_sources,
        verification,
    )?;
    verify_quote(document, &quote_sources, has_verbatim_source, verification);
    Ok(())
}

fn refresh_quote_sources(
    context: &Context,
    store: &Store,
    schemes: &Schemes,
    document: &Doc,
    live: bool,
    quote_sources: &mut [QuoteSource],
    verification: &mut Verification,
) -> Result<()> {
    let Some(quote) = document.front.quote.as_deref() else {
        return Ok(());
    };
    if !live
        || quote_sources
            .iter()
            .any(|source| quote_matches(&source.text, quote))
    {
        return Ok(());
    }
    for source in quote_sources {
        let pointer = thread_refresh_pointer(store, schemes, &source.pointer)?;
        let resolved = resolve_with_live(
            context,
            store,
            schemes,
            &pointer,
            true,
            ResolutionMode::Verify {
                live,
                refresh: true,
            },
        )?;
        if let Some(error) = resolved.source_error {
            verification.source_errors = true;
            verification.broken.push(Broken {
                file: document.rel.to_string_lossy().into_owned(),
                line: line_number(&document.raw, source.start),
                pointer: source.pointer.to_string(),
                why: error,
            });
            continue;
        }
        source.text = local_records(store, schemes, &source.pointer, true)?
            .into_iter()
            .map(|record| record.text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    Ok(())
}

fn thread_refresh_pointer(store: &Store, schemes: &Schemes, pointer: &Pointer) -> Result<Pointer> {
    let thread = store.get(pointer)?.and_then(|record| record.thread);
    Ok(thread
        .as_deref()
        .and_then(|thread| schemes.parse(thread))
        .unwrap_or_else(|| pointer.clone()))
}

fn verify_quote(
    document: &Doc,
    quote_sources: &[QuoteSource],
    has_verbatim_source: bool,
    verification: &mut Verification,
) {
    let Some(quote) = document.front.quote.as_deref() else {
        return;
    };
    if !has_verbatim_source {
        verification.quotes_unchecked += 1;
        return;
    }
    if quote_sources.is_empty()
        || quote_sources
            .iter()
            .any(|source| quote_matches(&source.text, quote))
    {
        return;
    }
    let Some(source) = quote_sources.first() else {
        return;
    };
    verification.broken.push(Broken {
        file: document.rel.to_string_lossy().into_owned(),
        line: line_number(&document.raw, source.start),
        pointer: source.pointer.to_string(),
        why: "quote does not appear in the resolved text".to_owned(),
    });
}

fn default_verbatim(source: Option<&SourceConfig>, scheme: &str) -> bool {
    source.map_or(matches!(scheme, "slack" | "gmail"), |source| {
        matches!(source.kind.as_str(), "slack" | "gmail")
    })
}

fn is_builtin_source(source: &SourceConfig) -> bool {
    matches!(
        source.kind.as_str(),
        "slack" | "gmail" | "drive" | "calendar"
    )
}

fn source_for_scheme<'a>(context: &'a Context, scheme: &str) -> Result<Option<&'a SourceConfig>> {
    for source in &context.config.sources {
        if source::scheme(source)? == scheme {
            return Ok(Some(source));
        }
    }
    Ok(None)
}

fn resolve_with_live(
    context: &Context,
    store: &Store,
    schemes: &Schemes,
    pointer: &Pointer,
    thread: bool,
    mode: ResolutionMode,
) -> Result<ResolvedText> {
    let text = resolve_local(store, schemes, pointer, thread)?;
    let live = mode.live();
    let needs_verbatim = mode.needs_verbatim();
    let force_live = mode.force_live();
    let source = (text.is_none() || needs_verbatim || force_live)
        .then(|| source_for_scheme(context, &pointer.scheme))
        .transpose()?
        .flatten();
    let source_verbatim = if needs_verbatim {
        match source {
            Some(source) => store.source_verbatim(&source.id)?,
            None => None,
        }
    } else {
        None
    };
    let default_verbatim = default_verbatim(source, &pointer.scheme);
    let needs_verbatim_hello = needs_verbatim
        && live
        && text.is_some()
        && source.is_some_and(|source| !is_builtin_source(source))
        && source_verbatim.is_none();
    if (text.is_some() && !needs_verbatim_hello && !force_live) || !live {
        return Ok(ResolvedText {
            text,
            verbatim: source_verbatim.unwrap_or(default_verbatim),
            source_error: None,
        });
    }
    let Some(source) = source else {
        return Ok(ResolvedText {
            text: None,
            verbatim: default_verbatim,
            source_error: None,
        });
    };
    let outcome = resolve_from_source(context, source, &pointer.scheme, pointer, schemes)?;
    if let Some(error) = outcome.error.as_deref() {
        return Ok(ResolvedText {
            text: None,
            verbatim: source_verbatim.unwrap_or(default_verbatim),
            source_error: Some(format!("source {} resolve failed: {error}", source.id)),
        });
    }
    let verbatim = outcome.verbatim.unwrap_or(default_verbatim);
    store.set_source_verbatim(&source.id, verbatim)?;
    store.put_all(&source.id, &outcome.records)?;
    Ok(ResolvedText {
        text: resolve_local(store, schemes, pointer, thread)?,
        verbatim,
        source_error: None,
    })
}

fn resolve_from_source(
    context: &Context,
    spec: &SourceConfig,
    scheme: &str,
    pointer: &Pointer,
    schemes: &Schemes,
) -> Result<crate::source::SourceOutcome> {
    let state_dir = context.state()?.source_state(&spec.id);
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let request = SourceRequest::Resolve(ResolveRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: spec.id.clone(),
            kind: spec.kind.clone(),
            scheme: scheme.to_owned(),
        },
        config: source::config_json(spec)?,
        pointers: vec![pointer.to_string()],
        state_dir,
    });
    let source = source::open(spec, scheme)?;
    let mut session = Session::new(&request, schemes);
    let run = source.run(&request, &mut |message| session.accept(message).map(|_| ()));
    Ok(session.finish(run))
}

fn resolve_local(
    store: &Store,
    schemes: &Schemes,
    pointer: &Pointer,
    thread: bool,
) -> Result<Option<String>> {
    let records = local_records(store, schemes, pointer, thread)?;
    (!records.is_empty())
        .then(|| records.iter().map(render).collect::<Vec<_>>().join("\n\n"))
        .map_or(Ok(None), |text| Ok(Some(text)))
}

fn local_records(
    store: &Store,
    schemes: &Schemes,
    pointer: &Pointer,
    thread: bool,
) -> Result<Vec<crate::record::Record>> {
    let is_gmail_thread = pointer.scheme == "gmail" && pointer.locator.starts_with("thread/");
    if !thread && !is_gmail_thread {
        return store
            .get(pointer)
            .map(|record| record.into_iter().collect());
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
    Ok(records)
}

fn source_pointers(document: &Doc, schemes: &Schemes) -> Result<Vec<Pointer>> {
    let frontmatter = document.frontmatter_json()?;
    let mut pointers = Vec::new();
    let Some(fields) = frontmatter.as_object() else {
        return Ok(pointers);
    };
    for field in ["source", "sources"] {
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
    for fragment in quote
        .split("...")
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
    {
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

fn line_number(text: &str, offset: usize) -> usize {
    text.get(..offset).map_or(1, |prefix| {
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1
    })
}
