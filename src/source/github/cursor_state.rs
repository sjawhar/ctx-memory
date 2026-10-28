//! GitHub fetch cursor and persisted state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;

use super::super::{token_cursor, window_start, TokenCursor};

/// What one run leaves the next: where to start, and which streams the pre-pass may skip.
pub(super) const STATE_FILE: &str = "state.json";
/// Where an earlier build kept per-page `ETag`s. GitHub ties an `ETag` to the token that read
/// the page, and every fetch mints its own installation token, so no stored `ETag` was ever
/// answered `304` on a later run. The file is deleted on sight.
pub(super) const OLD_ETAGS_FILE: &str = "etags.json";

/// What one run leaves the next, in [`STATE_FILE`].
#[derive(Default, Deserialize, Serialize)]
pub(super) struct State {
    /// The stream the last run stopped at, which the next run reads first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) first: Option<String>,
    /// Each settled stream, with the cursor the run that settled it proposed: that run read it
    /// to the end and held nothing back, or the pre-pass found it unchanged. The pre-pass asks
    /// about a stream only while its cursor is still that value, which is true only once the
    /// window that run wrote was stored. A stream that held an item past its window end,
    /// stopped, or failed is read through REST, so nothing it listed waits on a later change.
    #[serde(default)]
    pub(super) settled: BTreeMap<String, String>,
}

impl State {
    /// Reads what the last run left, and deletes an earlier build's `ETag` file.
    ///
    /// Content this build cannot read is dropped rather than failing the run: losing it costs
    /// a REST read of every repository and no record. The reasons come back as warnings.
    pub(super) fn load(dir: &Path) -> Result<(Self, Vec<String>)> {
        let mut warnings = Vec::new();
        let old = dir.join(OLD_ETAGS_FILE);
        match fs::remove_file(&old) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => warnings.push(format!(
                "{} is no longer used and could not be deleted: {error}",
                old.display()
            )),
        }
        let path = dir.join(STATE_FILE);
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                warnings.push(format!(
                    "{} is not what this build reads ({error}); every repository is read through its REST listings this run",
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

    /// Marks `stream` settled at the cursor this run proposed for it.
    pub(super) fn settle(&mut self, stream: &str, cursor: &str) {
        self.settled.insert(stream.to_owned(), cursor.to_owned());
    }

    /// Marks `stream` as one the next run reads through REST.
    pub(super) fn unsettle(&mut self, stream: &str) {
        self.settled.remove(stream);
    }

    /// Forgets every stream this run did not plan, such as a repository the app has left.
    pub(super) fn retain(&mut self, streams: &[String]) {
        let planned: BTreeSet<&str> = streams.iter().map(String::as_str).collect();
        self.settled.retain(|stream, _| planned.contains(&**stream));
    }

    pub(super) fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(STATE_FILE);
        let bytes = serde_json::to_vec_pretty(self).context("serializing the GitHub state")?;
        write_replace(&path, &bytes)
    }
}

/// Where an issue stands in the feed: when it last changed, and its number.
pub(super) type FeedKey = (DateTime<Utc>, u64);

/// Where a repository's stream stands, as its cursor names it.
///
/// A run that finishes names `feed=<window end>,head=<sha>@<window end>`, or the window end
/// alone when the default branch has no commit. A run that stops names where the issues feed
/// resumes and the time the stream is stored through:
/// `feed=<time>,issue=<number>[,head=<sha>]@<stored>` when it finished issues, or
/// `feed=<window end>[,head=<sha>]@<stored>` when it finished every issue but not the commits.
/// Cursors an earlier build wrote still parse: a bare time, and `<resume>@<stored>`.
///
/// The feed's position cannot stall. Issues are finished in the order of `(updated_at,
/// number)`, and the position names the last one finished, so the next run skips exactly
/// those, even inside a second that hundreds of issues share: a position that named only a
/// time would send the next run back to the start of that second, and a second whose
/// follow-ups cost more than one run's budget would be read from its start on every run. Each
/// run lists from the position until its open issues span two seconds or the listing ends
/// (`feed_reserve`), so it has issues to finish whenever any remain; every issue it finishes
/// moves the position forward. A run finishes none only when the budget left after listing
/// does not cover one issue's follow-ups (three requests at most); listing a second's issues
/// costs one request a hundred, so at the default budget of 600 that takes about 59,000 issues
/// changed in one second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Position {
    /// The issues feed lists the issues changed at or after this time…
    pub(super) resume: DateTime<Utc>,
    /// …except, among those changed in that very second, the ones numbered up to this one,
    /// which an earlier run finished.
    pub(super) after: Option<u64>,
    /// The follow-ups changed since this time are listed again, and so are the commits since
    /// it when no head is known: a comment on an issue a run did not finish can be anywhere
    /// after it.
    pub(super) stored: DateTime<Utc>,
    /// The default branch head whose commits are stored.
    pub(super) head: Option<String>,
}

impl Position {
    pub(super) fn read(
        cursor: Option<&str>,
        window_end: DateTime<Utc>,
        backfill_hours: u32,
    ) -> Result<Self> {
        let Some(cursor) = cursor else {
            let start = window_start(None, window_end, backfill_hours)?;
            return Ok(Self {
                resume: start,
                after: None,
                stored: start,
                head: None,
            });
        };
        match TokenCursor::parse(cursor) {
            TokenCursor::Time(time) => Ok(Self {
                resume: time,
                after: None,
                stored: time,
                head: None,
            }),
            TokenCursor::Token {
                token,
                stored_through: Some(stored),
            } => match token.strip_prefix("feed=") {
                Some(fields) => Self::fields(fields, stored)
                    .with_context(|| format!("parsing source cursor {cursor}")),
                None => Ok(Self {
                    resume: parse_time(token)
                        .with_context(|| format!("parsing source cursor {cursor}"))?,
                    after: None,
                    stored,
                    head: None,
                }),
            },
            TokenCursor::Token {
                stored_through: None,
                ..
            } => {
                bail!(
                    "parsing source cursor {cursor}: it is neither a time nor <position>@<stored>"
                )
            }
        }
    }

    /// Reads `<time>[,issue=<number>][,head=<sha>]`.
    fn fields(fields: &str, stored: DateTime<Utc>) -> Result<Self> {
        let mut parts = fields.split(',');
        let resume = parse_time(parts.next().unwrap_or_default())?;
        let mut position = Self {
            resume,
            after: None,
            stored,
            head: None,
        };
        for part in parts {
            match part.split_once('=') {
                Some(("issue", number)) => {
                    position.after =
                        Some(number.parse().with_context(|| format!("issue {number}"))?);
                }
                Some(("head", sha)) if is_sha(sha) => position.head = Some(sha.to_owned()),
                _ => bail!("unknown field {part}"),
            }
        }
        Ok(position)
    }

    /// The cursor that names this position.
    pub(super) fn cursor(&self) -> String {
        if self.after.is_none() && self.head.is_none() {
            return if self.resume == self.stored {
                rfc3339(self.stored)
            } else {
                token_cursor(&rfc3339(self.resume), self.stored)
            };
        }
        let mut fields = vec![format!("feed={}", rfc3339(self.resume))];
        if let Some(number) = self.after {
            fields.push(format!("issue={number}"));
        }
        if let Some(head) = &self.head {
            fields.push(format!("head={head}"));
        }
        token_cursor(&fields.join(","), self.stored)
    }

    /// Whether the issue at `key` is one the feed still has to finish.
    pub(super) fn open(&self, (time, number): FeedKey) -> bool {
        time > self.resume || (time == self.resume && self.after.is_none_or(|after| number > after))
    }

    /// Where the feed resumes, in words.
    pub(super) fn describe(&self) -> String {
        let time = rfc3339(self.resume);
        self.after.map_or_else(
            || format!("the issues changed since {time}"),
            |number| format!("the issues after #{number}, changed {time}"),
        )
    }
}

fn parse_time(raw: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("parsing time {raw}"))?
        .with_timezone(&Utc))
}

/// Whether `value` reads as a commit id, which goes into a URL path.
pub(super) fn is_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}
