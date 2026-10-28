//! Cursor persistence and advancement from a completed window.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::context::Context;
use crate::output::printable;
use crate::windows::{Manifest, Status, WindowId};

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
    load(&context.data()?.cursors())
}

/// Applies the successful stream cursors of a completed window.
///
/// The caller holds the fetch lock. `ctx fetch` moves a stream's cursor itself once that
/// stream's messages are stored, so an interrupted fetch re-reads the same material. A stream
/// that errored, proposed no cursor, or was already advanced by a newer window is skipped, and
/// skipping one stream never holds back another.
///
/// # Errors
///
/// Returns an error when cursor state cannot be read or persisted, or when a stored cursor names
/// a window identifier that is not canonical.
pub(crate) fn advance(
    path: &Path,
    window: &WindowId,
    manifest: &Manifest,
) -> Result<AdvanceOutcome> {
    let mut cursors = load(path)?;
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
            {
                let existing_window = WindowId::parse(&existing.window).with_context(|| {
                    format!("reading the stored cursor for source {source} stream {stream}")
                })?;
                if existing_window > *window {
                    skipped.push(Skipped {
                        source: source.clone(),
                        stream: stream.clone(),
                        why: format!("a newer window {existing_window} already advanced it"),
                    });
                    continue;
                }
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
                window: window.to_string(),
            },
        );
        advanced.push(Advanced {
            source,
            stream,
            cursor,
        });
    }
    if !advanced.is_empty() {
        write(path, &cursors)?;
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

/// Prints every stored cursor, one stream per line.
///
/// A stream name can come from a record (a discovered stream) and a cursor from a source's
/// answer, so each is printed without its control characters.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_cursors(cursors: &Cursors, out: &mut dyn Write) -> std::io::Result<()> {
    for (source, streams) in &cursors.sources {
        for (stream, cursor) in streams {
            writeln!(
                out,
                "{} {} {} {}",
                printable(source),
                printable(stream),
                printable(&cursor.cursor),
                printable(&cursor.window)
            )?;
        }
    }
    Ok(())
}

/// Prints which cursors one window moved and which it left, and why.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_advance(outcome: &AdvanceOutcome, out: &mut dyn Write) -> std::io::Result<()> {
    for advanced in &outcome.advanced {
        writeln!(
            out,
            "advanced {} {} {}",
            printable(&advanced.source),
            printable(&advanced.stream),
            printable(&advanced.cursor)
        )?;
    }
    for skipped in &outcome.skipped {
        writeln!(
            out,
            "skipped {} {} {}",
            printable(&skipped.source),
            printable(&skipped.stream),
            printable(&skipped.why)
        )?;
    }
    Ok(())
}
