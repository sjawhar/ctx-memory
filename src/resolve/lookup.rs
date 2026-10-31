//! Local and live cited-pointer lookup.

use anyhow::{anyhow, Context as _, Result};
use chrono::Utc;

use crate::config::SourceConfig;
use crate::context::Context;
use crate::pointer::{Pointer, Schemes};
use crate::record::render;
use crate::source::slack::Mentions;
use crate::source::{self, Request as SourceRequest, ResolveRequest, Session, SourceRef};
use crate::store::Store;

pub(super) struct ResolvedText {
    pub(super) text: Option<String>,
    pub(super) verbatim: bool,
    pub(super) source_error: Option<String>,
}

/// What every lookup reads: the run's context, the local store, the configured schemes, and
/// the names a served Slack message's mentions read with.
#[derive(Clone, Copy)]
pub(super) struct Lookup<'a> {
    pub(super) context: &'a Context,
    pub(super) store: &'a Store,
    pub(super) schemes: &'a Schemes,
    pub(super) mentions: &'a Mentions<'a>,
}

#[derive(Clone, Copy)]
pub(super) enum ResolutionMode {
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

pub(super) fn thread_refresh_pointer(
    store: &Store,
    schemes: &Schemes,
    pointer: &Pointer,
) -> Result<Pointer> {
    let thread = store.get(pointer)?.and_then(|record| record.thread);
    Ok(thread
        .as_deref()
        .and_then(|thread| schemes.parse(thread))
        .unwrap_or_else(|| pointer.clone()))
}

/// What a source's records are taken to be when no hello has declared it.
///
/// The built-in sources declare themselves: Slack, Gmail, Drive and GitHub store the text
/// their API returned, and Calendar renders an event, so only the first four are verbatim.
fn default_verbatim(source: Option<&SourceConfig>, scheme: &str) -> bool {
    source.map_or(
        matches!(scheme, "slack" | "gmail" | "drive" | "github"),
        |source| matches!(source.kind.as_str(), "slack" | "gmail" | "drive" | "github"),
    )
}

fn is_builtin_source(source: &SourceConfig) -> bool {
    matches!(
        source.kind.as_str(),
        "slack" | "gmail" | "drive" | "calendar" | "github"
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

/// Reports whether an offline check cannot know if a cited source's records are its own words.
///
/// A built-in source has a known default (Slack, Gmail and Drive store verbatim text; Calendar
/// renders an event), so its status is never unknown. For any other source the fact lives only
/// in the verbatim declaration a fetch writes to `sources.json`: until one has, the offline
/// check treats the source as non-verbatim by default, which is a guess, not knowledge.
pub(super) fn source_verbatim_unknown(
    context: &Context,
    store: &Store,
    pointer: &Pointer,
) -> Result<bool> {
    let Some(source) = source_for_scheme(context, &pointer.scheme)? else {
        return Ok(false);
    };
    if is_builtin_source(source) {
        return Ok(false);
    }
    Ok(store.source_verbatim(&source.id)?.is_none())
}

pub(super) fn resolve_with_live(
    lookup: Lookup<'_>,
    pointer: &Pointer,
    thread: bool,
    mode: ResolutionMode,
) -> Result<ResolvedText> {
    let Lookup {
        context,
        store,
        schemes,
        mentions,
    } = lookup;
    let text = resolve_local(store, schemes, mentions, pointer, thread)?;
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
    // A read keeps its evidence on this machine: the shared data directory has one writer, the
    // fetch job, and a rebuild of this machine's index replays what is kept here.
    store.put_resolved(&source.id, Some(verbatim), &outcome.records, Utc::now())?;
    Ok(ResolvedText {
        text: resolve_local(store, schemes, mentions, pointer, thread)?,
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

/// The text `ctx resolve` prints for `pointer`, or its thread: each stored record as a reader
/// reads it, a Slack message with the people it mentions named ([`Mentions`]).
pub(super) fn resolve_local(
    store: &Store,
    schemes: &Schemes,
    mentions: &Mentions<'_>,
    pointer: &Pointer,
    thread: bool,
) -> Result<Option<String>> {
    let mut records = local_records(store, schemes, pointer, thread)?;
    if records.is_empty() {
        return Ok(None);
    }
    for record in &mut records {
        mentions.serve(record)?;
    }
    Ok(Some(
        records.iter().map(render).collect::<Vec<_>>().join("\n\n"),
    ))
}

pub(super) fn local_records(
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
