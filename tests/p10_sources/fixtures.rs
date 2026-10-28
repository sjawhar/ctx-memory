//! Scratch directories, request builders and the conformance runner the Google checks share.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::{
    config::{SourceConfig, TreeConfig},
    source::{self, FetchRequest, Request, Session, Source, SourceOutcome, SourceRef},
};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

pub(super) fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-p10-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

pub(super) fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

pub(super) fn source_spec(kind: &str, settings: &str) -> Result<SourceConfig> {
    let config: TreeConfig = toml::from_str(&format!(
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"{kind}\"\nkind = \"{kind}\"\n{settings}"
    ))?;
    config
        .sources
        .into_iter()
        .next()
        .context("test configuration contains its source")
}

pub(super) fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

pub(super) fn fetch_request(
    root: &Path,
    kind: &str,
    scheme: &str,
    config: Value,
) -> Result<Request> {
    Ok(Request::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: kind.to_owned(),
            kind: kind.to_owned(),
            scheme: scheme.to_owned(),
        },
        config,
        window_end: timestamp("2026-10-25T18:00:00Z")?,
        backfill_hours: 24,
        cursors: BTreeMap::new(),
        discovered: Vec::new(),
        state_dir: root.join("state"),
    }))
}

pub(super) fn run_conformance(source: &dyn Source, request: &Request) -> SourceOutcome {
    let schemes = ctx_memory::pointer::Schemes::new([request.source().scheme.clone()]);
    let mut session = Session::new(request, &schemes);
    let run = source.run(request, &mut |message| session.accept(message).map(|_| ()));
    session.finish(run)
}

/// A fetch of one source's streams from `cursors`, at `window_end`, with `backfill_hours`.
pub(super) fn fetch_from(
    root: &Path,
    kind: &str,
    config: Value,
    cursors: &[(&str, &str)],
    window_end: &str,
    backfill_hours: u32,
) -> Result<Request> {
    let Request::Fetch(mut fetch) = fetch_request(root, kind, kind, config)? else {
        anyhow::bail!("fetch_request builds a fetch");
    };
    fetch.cursors = cursors
        .iter()
        .map(|(stream, cursor)| ((*stream).to_owned(), (*cursor).to_owned()))
        .collect();
    fetch.window_end = timestamp(window_end)?;
    fetch.backfill_hours = backfill_hours;
    Ok(Request::Fetch(fetch))
}
