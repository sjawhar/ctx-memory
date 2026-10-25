//! Cursor persistence and explicit advancement from completed windows.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::context::Context;
use crate::fetch::acquire_lock;
use crate::windows::{Status, Window};

/// The complete versioned cursor file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cursors {
    pub version: u32,
    pub sources: BTreeMap<String, BTreeMap<String, Cursor>>,
}

impl Default for Cursors {
    fn default() -> Self {
        Self {
            version: 1,
            sources: BTreeMap::new(),
        }
    }
}

/// One opaque source-stream cursor and the window that set it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub cursor: String,
    pub window: String,
}

/// A stream whose cursor was applied from a completed window.
#[derive(Clone, Debug, Serialize)]
pub struct Advanced {
    pub source: String,
    pub stream: String,
    pub cursor: String,
}

/// A stream that could not advance from a completed window.
#[derive(Clone, Debug, Serialize)]
pub struct Skipped {
    pub source: String,
    pub stream: String,
    pub why: String,
}

/// Results from one cursor advancement request.
#[derive(Clone, Debug, Serialize)]
pub struct AdvanceOutcome {
    pub advanced: Vec<Advanced>,
    pub skipped: Vec<Skipped>,
}

/// Reads the cursor state without changing it.
///
/// # Errors
///
/// Returns an error when the cursor file exists but cannot be decoded or uses another version.
pub fn show(context: &Context) -> Result<Cursors> {
    load(&context.state()?.cursors())
}

/// Applies successful stream cursors from `window` while holding the fetch lock.
///
/// # Errors
///
/// Returns an error when the state lock is busy, the window is stale for a stream, or cursor state
/// cannot be read or persisted.
pub fn advance(context: &Context, window: &Window) -> Result<AdvanceOutcome> {
    let state = context.state()?;
    let _lock = acquire_lock(state)?;
    let manifest = window.manifest()?;
    let mut cursors = load(&state.cursors())?;
    let mut updates = Vec::new();
    let mut skipped = Vec::new();

    for (source, source_manifest) in &manifest.sources {
        for (stream, stream_manifest) in &source_manifest.streams {
            if stream_manifest.status == Status::Error {
                skipped.push(Skipped {
                    source: source.clone(),
                    stream: stream.clone(),
                    why: "stream errored".to_owned(),
                });
                continue;
            }
            let Some(next) = stream_manifest.cursor_next.as_deref() else {
                skipped.push(Skipped {
                    source: source.clone(),
                    stream: stream.clone(),
                    why: "stream did not propose a cursor".to_owned(),
                });
                continue;
            };
            if let Some(existing) = cursors
                .sources
                .get(source)
                .and_then(|streams| streams.get(stream))
                .filter(|existing| existing.window.as_str() > window.id.as_str())
            {
                bail!(
                    "window {} is older than {} for source {} stream {}",
                    window.id,
                    existing.window,
                    source,
                    stream
                );
            }
            updates.push((source.clone(), stream.clone(), next.to_owned()));
        }
    }

    let mut advanced = Vec::new();
    for (source, stream, cursor) in updates {
        cursors.sources.entry(source.clone()).or_default().insert(
            stream.clone(),
            Cursor {
                cursor: cursor.clone(),
                window: window.id.to_string(),
            },
        );
        advanced.push(Advanced {
            source,
            stream,
            cursor,
        });
    }
    if !advanced.is_empty() {
        write(&state.cursors(), &cursors)?;
    }
    Ok(AdvanceOutcome { advanced, skipped })
}

pub(crate) fn load(path: &Path) -> Result<Cursors> {
    match fs::read(path) {
        Ok(bytes) => {
            let cursors: Cursors = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            if cursors.version != 1 {
                bail!(
                    "{} has unsupported cursor version {}",
                    path.display(),
                    cursors.version
                );
            }
            Ok(cursors)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Cursors::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write(path: &Path, cursors: &Cursors) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(cursors).context("serializing cursor state")?;
    write_replace(path, &bytes)
}
