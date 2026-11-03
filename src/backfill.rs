//! Where each source's backfill window starts.
//!
//! A stream with no cursor reads the backfill window, `ingest.backfill_hours` back from the
//! window end. A fetch that does not store that read — the stream fails, the source fails before
//! it names its streams, or the fetch stops before it publishes its window — leaves the stream
//! with no cursor, and the next fetch's window ends later: a backfill of the same length would
//! start later too, and step over what lies between the two starts.
//!
//! So `backfill.json` in the data directory keeps, for each source, the start of the earliest
//! backfill read it has not stored. A fetch writes it before it runs the source, asks the source
//! for a backfill that reaches back to it, and removes it once the window is published with every
//! stream that read the backfill stored. The protocol carries one backfill per source, so every
//! stream of that source with no cursor reads from that start, a stream new since that read
//! included. A stream that keeps failing holds the start until it stores a read or leaves the
//! source's configuration.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::windows::{Manifest, Status};

/// The start of the earliest backfill read each source has not stored.
///
/// A field this build does not know is ignored rather than refused: machines on different builds
/// share the data directory, and this file only says how far back a backfill reaches, so losing
/// a later build's field costs at most a read that build would have made.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Backfills {
    pub version: u32,
    pub sources: BTreeMap<String, DateTime<Utc>>,
}

impl Default for Backfills {
    fn default() -> Self {
        Self {
            version: 1,
            sources: BTreeMap::new(),
        }
    }
}

/// How far back one source's backfill reaches in one fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reach {
    /// The `backfill_hours` the source is asked for.
    pub hours: u32,
    /// Where a backfill read an earlier fetch did not store began, when that is earlier than the
    /// configured backfill.
    pub unstored_since: Option<DateTime<Utc>>,
}

impl Backfills {
    /// Notes that `source` reads its backfill in a fetch ending at `window_end`, and returns how
    /// far back it reaches: `backfill_hours`, or to the start of a read an earlier fetch did not
    /// store, rounded up to a whole hour, since the protocol names the backfill in hours.
    pub(crate) fn begin(
        &mut self,
        source: &str,
        window_end: DateTime<Utc>,
        backfill_hours: u32,
    ) -> Reach {
        let configured = window_end - Duration::hours(i64::from(backfill_hours));
        let unstored_since = self
            .sources
            .get(source)
            .copied()
            .filter(|start| *start < configured);
        let start = unstored_since.unwrap_or(configured);
        self.sources.insert(source.to_owned(), start);
        let span = window_end - start;
        let mut hours = span.num_hours();
        if Duration::hours(hours) < span {
            hours += 1;
        }
        Reach {
            hours: u32::try_from(hours).unwrap_or(u32::MAX),
            unstored_since,
        }
    }

    /// Forgets the backfill start of each source of a published window whose backfill reads
    /// were all stored: the source named its streams, and none of its streams with no cursor
    /// failed. Returns whether anything was forgotten.
    pub(crate) fn finish(&mut self, manifest: &Manifest) -> bool {
        let mut changed = false;
        for (source, outcome) in &manifest.sources {
            let unstored = outcome.error.is_some()
                || outcome
                    .streams
                    .values()
                    .any(|stream| stream.status == Status::Error && stream.cursor_used.is_none());
            if !unstored {
                changed |= self.sources.remove(source).is_some();
            }
        }
        changed
    }
}

pub(crate) fn load(path: &Path) -> Result<Backfills> {
    match fs::read(path) {
        Ok(bytes) => {
            let backfills: Backfills = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            if backfills.version != 1 {
                bail!(
                    "{} has unsupported backfill version {}",
                    path.display(),
                    backfills.version
                );
            }
            Ok(backfills)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Backfills::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub(crate) fn write(path: &Path, backfills: &Backfills) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(backfills).context("serializing backfill state")?;
    write_replace(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::{SourceManifest, StreamManifest};

    fn at(time: &str) -> Result<DateTime<Utc>> {
        Ok(DateTime::parse_from_rfc3339(time)?.with_timezone(&Utc))
    }

    fn stream(status: Status, cursor_used: Option<&str>) -> StreamManifest {
        StreamManifest {
            status,
            error: None,
            cursor_used: cursor_used.map(str::to_owned),
            cursor_next: None,
            records: 0,
            label: None,
            owner: None,
        }
    }

    fn manifest(source: &str, error: Option<&str>, streams: Vec<StreamManifest>) -> Manifest {
        Manifest {
            version: 1,
            window: "20261025T180000Z-0000".to_owned(),
            generated_at: Utc::now(),
            window_end: Utc::now(),
            sources: BTreeMap::from([(
                source.to_owned(),
                SourceManifest {
                    kind: "echo".to_owned(),
                    status: Status::Ok,
                    error: error.map(str::to_owned),
                    streams: streams
                        .into_iter()
                        .enumerate()
                        .map(|(index, stream)| (format!("S{index}"), stream))
                        .collect(),
                },
            )]),
            warnings: Vec::new(),
            exit_status: Status::Ok,
        }
    }

    #[test]
    fn a_backfill_rounds_up_to_the_hour_that_reaches_the_unstored_start() -> Result<()> {
        let mut backfills = Backfills::default();
        backfills.begin("echo", at("2026-10-25T18:00:00.250Z")?, 24);

        // The unstored read began 2026-10-24T18:00:00.250Z. From 19:00:00.100, 25 hours reach
        // back to 18:00:00.100, before it; from 19:00:00.400 they reach only 18:00:00.400, so
        // the backfill is 26 hours.
        let reach = backfills.begin("echo", at("2026-10-25T19:00:00.100Z")?, 24);
        assert_eq!(reach.hours, 25);
        let reach = backfills.begin("echo", at("2026-10-25T19:00:00.400Z")?, 24);
        assert_eq!(reach.hours, 26);
        assert_eq!(reach.unstored_since, Some(at("2026-10-24T18:00:00.250Z")?));
        Ok(())
    }

    #[test]
    fn an_older_window_end_moves_the_unstored_start_earlier() -> Result<()> {
        let mut backfills = Backfills::default();
        backfills.begin("echo", at("2026-10-25T18:00:00Z")?, 24);
        let reach = backfills.begin("echo", at("2026-10-25T12:00:00Z")?, 24);

        assert_eq!(
            reach,
            Reach {
                hours: 24,
                unstored_since: None
            }
        );
        assert_eq!(
            backfills.sources.get("echo"),
            Some(&at("2026-10-24T12:00:00Z")?)
        );
        Ok(())
    }

    #[test]
    fn a_source_is_forgotten_only_once_every_backfill_read_is_stored() -> Result<()> {
        for (error, streams, forgotten) in [
            (None, vec![stream(Status::Ok, None)], true),
            (
                None,
                vec![stream(Status::Error, Some("2026-10-25T17:00:00Z"))],
                true,
            ),
            (None, vec![stream(Status::Error, None)], false),
            (Some("source did not emit hello"), Vec::new(), false),
        ] {
            let mut backfills = Backfills::default();
            backfills.begin("echo", at("2026-10-25T18:00:00Z")?, 24);
            backfills.begin("other", at("2026-10-25T18:00:00Z")?, 24);
            let changed = backfills.finish(&manifest("echo", error, streams));

            assert_eq!(changed, forgotten);
            assert_eq!(backfills.sources.contains_key("echo"), !forgotten);
            // A source the fetch did not run keeps its start.
            assert!(backfills.sources.contains_key("other"));
        }
        Ok(())
    }
}
