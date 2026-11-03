//! What one Matrix fetch leaves the next, in `<state_dir>/state.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;

use super::SyncMode;

/// The file a fetch keeps its state in, under the source's state directory.
pub(super) const STATE_FILE: &str = "state.json";

/// What one run leaves the next.
#[derive(Default, Deserialize, Serialize)]
pub(super) struct State {
    /// The `next_batch` of the run's `/sync`, which the next run's `/sync` starts from, kept
    /// only with `sync = "incremental"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) since: Option<String>,
    /// Each settled room, with the cursor the run that settled it proposed: that run read the
    /// room to its end and held nothing back. A `/sync` skips a room only while its cursor is
    /// still that value, which holds only once the window that run wrote was stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) settled: BTreeMap<String, String>,
    /// The id of the newest event the full `/sync` before that run showed in each settled room.
    /// The next full `/sync` skips a settled room while its newest event is still this one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) newest: BTreeMap<String, String>,
    /// The message events a run read that made no record of their own — edits, and messages
    /// with nothing to show — each with the end of the window that read it. A homeserver serves
    /// a redacted event with its content gone, so only this tells a redaction of one of them
    /// from a redaction of a message a record holds. Each is kept for
    /// [`super::fetch::UNRECORDED_DAYS`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) unrecorded: BTreeMap<String, DateTime<Utc>>,
    /// The room the last run's request budget or the homeserver's rate limit stopped at,
    /// which the next run reads first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) first: Option<String>,
}

impl State {
    /// Reads what the last run left. Content this build cannot read is dropped rather than
    /// failing the run: losing it costs a read of every changed room and no record. The reason
    /// comes back as a warning.
    pub(super) fn load(dir: &Path) -> Result<(Self, Vec<String>)> {
        let path = dir.join(STATE_FILE);
        let mut warnings = Vec::new();
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                warnings.push(format!(
                    "{} is not what this build reads ({error}); every room is looked at afresh this run",
                    path.display()
                ));
                Self::default()
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Self::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()))
            }
        };
        Ok((state, warnings))
    }

    /// Marks `room` settled at the cursor this run proposed for it, after a `/sync` that showed
    /// `newest` as its newest event.
    pub(super) fn settle(&mut self, room: &str, cursor: &str, newest: Option<&str>) {
        self.settled.insert(room.to_owned(), cursor.to_owned());
        match newest {
            Some(newest) => {
                self.newest.insert(room.to_owned(), newest.to_owned());
            }
            None => {
                self.newest.remove(room);
            }
        }
    }

    /// Marks `room` as one the next run reads whatever `/sync` says.
    pub(super) fn unsettle(&mut self, room: &str) {
        self.settled.remove(room);
        self.newest.remove(room);
    }

    /// Forgets the settled position of every room this run did not plan — a room excluded for
    /// a while has had no read since, so when it is read again its old position says nothing —
    /// and every unrecorded event read before `forget_before`.
    pub(super) fn retain(&mut self, planned: &[String], forget_before: DateTime<Utc>) {
        let planned: BTreeSet<&str> = planned.iter().map(String::as_str).collect();
        self.settled
            .retain(|room, _| planned.contains(room.as_str()));
        self.newest
            .retain(|room, _| planned.contains(room.as_str()));
        self.unrecorded.retain(|_, read| *read >= forget_before);
    }

    /// Keeps what the next run's `/sync` reads. An incremental `/sync` keeps the token this
    /// run's gave, or, when it gave none because it stopped, the token it had, from which the
    /// next `/sync` names every room this one would have, and more. A full `/sync` keeps no
    /// token, and with no `/sync` at all nothing is skipped, so no room is kept settled.
    pub(super) fn keep_sync(&mut self, mode: SyncMode, next_batch: Option<String>) {
        match mode {
            SyncMode::Incremental => self.since = next_batch.or(self.since.take()),
            SyncMode::Full => self.since = None,
            SyncMode::Off => {
                self.since = None;
                self.settled.clear();
                self.newest.clear();
            }
        }
    }

    pub(super) fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let bytes = serde_json::to_vec_pretty(self).context("serializing the Matrix state")?;
        write_replace(&dir.join(STATE_FILE), &bytes)
    }
}
