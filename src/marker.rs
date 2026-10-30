//! The agent marker: the newest window whose material an agent has committed.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::config::DataDir;
use crate::context::Context;
use crate::delta::{self, Delta, ANSWER_DELTA_BYTES};
use crate::exclude::Exclusion;
use crate::output::printable;
use crate::windows::{self, Status, Window, WindowId};

/// The newest window an agent has turned into committed records.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub version: u32,
    pub window: String,
    pub advanced_at: DateTime<Utc>,
    pub by: String,
}

/// One completed window the marker has not reached.
#[derive(Clone, Debug, Serialize)]
pub struct PendingWindow {
    pub window: String,
    /// The window's directory under the data directory, such as `windows/<window id>`.
    ///
    /// Relative, because the agent that reads the files and the server that lists them each
    /// mount the share where their own container puts it.
    pub dir: String,
    pub window_end: DateTime<Utc>,
    pub status: Status,
    /// The window's new messages, less those the deployment excludes.
    pub new_messages: usize,
    /// How many of them the answer's delta lists: all of them, unless the answer was cut to
    /// its size, and then the rest are in the window's `delta.json`.
    pub listed_messages: usize,
}

/// The windows an agent still has to handle, with their combined delta.
#[derive(Debug, Serialize)]
pub struct Pending {
    pub marker: Option<Marker>,
    pub windows: Vec<PendingWindow>,
    pub delta: Option<Delta>,
    /// Completed windows after these that this answer leaves for a later one, because their
    /// deltas would take it past one answer's size. Once the marker passes the windows listed
    /// here, the next answer starts with them.
    pub later_windows: usize,
    /// When the delta leaves messages out (`omitted_messages`), the `--offset` that lists the
    /// next of them; the marker should not pass the window until its last part is handled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// Reads the marker without changing it.
///
/// # Errors
///
/// Returns an error when the marker file exists but cannot be decoded or uses another version.
pub fn show(context: &Context) -> Result<Option<Marker>> {
    load(&context.data()?.marker())
}

/// A marker move the caller asked for and may not have: a window that does not name a
/// completed window, or one the marker has already passed.
///
/// Typed, so the server can tell the caller's mistake from its own trouble: anything else that
/// stops a move — a marker or a window it cannot read, a marker it cannot write — is the
/// machine's, not the caller's.
#[derive(Debug)]
pub struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(why: String) -> anyhow::Error {
    anyhow::Error::new(Refused(why))
}

/// Moves the marker to a completed window, after the commit that handled it.
///
/// `window` is a completed window identifier or `newest`. A window the marker has already
/// passed is refused, so a repeated or out-of-order cycle cannot hide material; moving the
/// marker to the window it already holds changes only who moved it and when.
///
/// # Errors
///
/// Returns a [`Refused`] error when the window is not complete or is older than the current
/// marker, and any other error when marker state cannot be read or persisted.
pub fn advance(context: &Context, window: &str, by: &str) -> Result<Marker> {
    let data = context.data()?;
    let window = windows::resolve_arg(window, &data.windows())
        .and_then(Window::open)
        .map_err(|error| refused(format!("{error:#}")))?;
    let path = data.marker();
    if let Some(current) = load(&path)? {
        let held = WindowId::parse(&current.window)
            .with_context(|| format!("reading the marker at {}", path.display()))?;
        if held > window.id {
            return Err(refused(format!(
                "window {} is older than the marker at {held}",
                window.id
            )));
        }
    }
    let marker = Marker {
        version: 1,
        window: window.id.to_string(),
        advanced_at: Utc::now(),
        by: by.to_owned(),
    };
    write(&path, &marker)?;
    Ok(marker)
}

/// Lists the completed windows after the marker with the delta they combine into.
///
/// `limit` caps how many of those windows, oldest first, the answer holds: a cycle handles
/// the capped list and leaves the rest for the next one, so one run never has to read the
/// whole of a backlog at once. `None` lists every pending window. The delta combines only the
/// windows the answer holds.
///
/// The answer leaves out every message the deployment excludes, and stays within one answer's
/// size ([`ANSWER_DELTA_BYTES`]): it takes windows oldest first while they fit, and counts the
/// rest, and those past `limit`, in `later_windows`. A window that does not fit is taken anyway
/// when the windows before it hold no message, so empty windows never hold a large one back;
/// its delta then lists the oldest messages that fit, `omitted_messages` counts the rest, and
/// `next_offset` is the `offset` that lists the next part. `offset` leaves out that many of the
/// oldest messages.
///
/// # Errors
///
/// Returns an error when the marker, a window manifest, or a window delta cannot be read.
pub fn pending(context: &Context, limit: Option<usize>, offset: usize) -> Result<Pending> {
    pending_in(
        context.data()?,
        limit,
        &Exclusion::configured(&context.config)?,
        ANSWER_DELTA_BYTES,
        offset,
    )
}

/// [`pending`] over a data directory, with the cap, exclusion and size it answers within, so
/// it is testable without a whole [`Context`].
fn pending_in(
    data: &DataDir,
    limit: Option<usize>,
    exclusion: &Exclusion,
    budget: usize,
    offset: usize,
) -> Result<Pending> {
    let path = data.marker();
    let marker = load(&path)?;
    let held = marker
        .as_ref()
        .map(|marker| {
            WindowId::parse(&marker.window)
                .with_context(|| format!("reading the marker at {}", path.display()))
        })
        .transpose()?;
    let mut after = windows::after(&data.windows(), held.as_ref())?;
    let waiting = after.len();
    if let Some(limit) = limit {
        after.truncate(limit);
    }
    let mut windows = Vec::new();
    // Each listed window's delta, by its place in `windows`; a window without one has none.
    let mut deltas = Vec::new();
    let mut delta_of = Vec::new();
    let mut size = 0_usize;
    let mut listing = 0_usize;
    let mut window_bytes = 0_usize;
    for window in after {
        let manifest = window.manifest()?;
        let delta = delta::read(&window, exclusion)?;
        let dir = window.dir.strip_prefix(data.root()).with_context(|| {
            format!(
                "{} is not under the data directory {}",
                window.dir.display(),
                data.root().display()
            )
        })?;
        let new_messages = delta.as_ref().map_or(0, |delta| delta.new_messages.len());
        let entry = PendingWindow {
            window: window.id.to_string(),
            dir: dir.to_string_lossy().into_owned(),
            window_end: manifest.window_end,
            status: manifest.exit_status,
            new_messages,
            listed_messages: new_messages,
        };
        let entry_bytes = delta::answer_len_of(&entry);
        // A window with no new message adds only its entry: its status changes are capped with
        // the combined delta's (`Delta::fit`), so a run of them never holds back a later window.
        let bytes = if new_messages == 0 {
            entry_bytes
        } else {
            delta.as_ref().map_or(0, Delta::answer_len) + entry_bytes
        };
        let fits = size + bytes <= budget;
        if !fits && listing > 0 {
            break;
        }
        size += bytes;
        window_bytes += entry_bytes;
        listing += new_messages;
        windows.push(entry);
        delta_of.push(delta.map(|delta| {
            deltas.push(delta);
            deltas.len() - 1
        }));
        if !fits {
            // Taken past the size because the windows before it hold nothing: it is cut below,
            // and nothing after it is taken.
            break;
        }
    }
    let mut combined = delta::combine(&deltas, Utc::now());
    let mut next_offset = None;
    if let Some(combined) = combined.as_mut() {
        combined.skip(offset);
        combined.fit(budget.saturating_sub(window_bytes));
        if offset > 0 || combined.omitted_messages.is_some() {
            let listed = combined
                .new_messages
                .iter()
                .map(|message| message.pointer.as_str())
                .collect::<BTreeSet<_>>();
            for (window, index) in windows.iter_mut().zip(&delta_of) {
                window.listed_messages =
                    index
                        .and_then(|index| deltas.get(index))
                        .map_or(0, |delta| {
                            delta
                                .new_messages
                                .iter()
                                .filter(|message| listed.contains(message.pointer.as_str()))
                                .count()
                        });
            }
        }
        if combined.omitted_messages.is_some_and(|omitted| omitted > 0) {
            next_offset = Some(offset + combined.new_messages.len());
        }
    }
    Ok(Pending {
        marker,
        later_windows: waiting - windows.len(),
        delta: combined,
        windows,
        next_offset,
    })
}

/// Names the caller that moved the marker: the agent session when one is set.
#[must_use]
pub fn caller() -> String {
    for name in ["CTX_SESSION", "USER"] {
        if let Some(value) = std::env::var_os(name) {
            let value = value.to_string_lossy().into_owned();
            if !value.is_empty() {
                return value;
            }
        }
    }
    "unknown".to_owned()
}

/// Prints the pending windows the way a person reads them.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_pending(pending: &Pending, out: &mut dyn Write) -> std::io::Result<()> {
    match pending.marker.as_ref() {
        Some(marker) => writeln!(
            out,
            "marker {} ({} {})",
            printable(&marker.window),
            printable(&marker.by),
            marker.advanced_at.to_rfc3339()
        )?,
        None => writeln!(out, "marker (none)")?,
    }
    if pending.windows.is_empty() {
        writeln!(out, "(no pending windows)")?;
    }
    for window in &pending.windows {
        if window.listed_messages < window.new_messages {
            writeln!(
                out,
                "{} {} {} new message(s), {} listed below",
                window.window,
                window.status.label(),
                window.new_messages,
                window.listed_messages
            )?;
        } else {
            writeln!(
                out,
                "{} {} {} new message(s)",
                window.window,
                window.status.label(),
                window.new_messages
            )?;
        }
    }
    if let Some(next) = pending.next_offset {
        writeln!(
            out,
            "more messages wait: `ctx pending --offset {next}` lists the next part"
        )?;
    }
    if pending.later_windows > 0 {
        writeln!(
            out,
            "{} later window(s) wait for the next answer, once the marker passes these",
            pending.later_windows
        )?;
    }
    if let Some(delta) = pending.delta.as_ref() {
        writeln!(out, "{}", delta::summary(delta))?;
    }
    Ok(())
}

/// Prints the marker the way a person reads it.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(marker: Option<&Marker>, out: &mut dyn Write) -> std::io::Result<()> {
    match marker {
        Some(marker) => writeln!(
            out,
            "{} {} {}",
            printable(&marker.window),
            printable(&marker.by),
            marker.advanced_at.to_rfc3339()
        ),
        None => writeln!(out, "(none)"),
    }
}

fn load(path: &Path) -> Result<Option<Marker>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let marker: Marker =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    if marker.version != 1 {
        bail!(
            "{} has unsupported marker version {}",
            path.display(),
            marker.version
        );
    }
    Ok(Some(marker))
}

fn write(path: &Path, marker: &Marker) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(marker).context("serializing the marker")?;
    write_replace(path, &bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use anyhow::Result;
    use chrono::{TimeZone as _, Utc};

    use super::{pending_in, write, Marker};
    use crate::config::DataDir;
    use crate::delta::Delta;
    use crate::windows::{Manifest, Status};

    /// Writes one completed window `id` under `data`, carrying `new_messages` new messages.
    ///
    /// A window counts as completed only when it holds `manifest.json`, `records.jsonl`,
    /// `rendered.md` and `delta.json` (`windows::completed`), so this writes all four.
    fn write_window(data: &DataDir, id: &str, new_messages: usize) -> Result<()> {
        let end = Utc.with_ymd_and_hms(2026, 10, 30, 2, 0, 0).unwrap();
        let dir = data.windows().join(id);
        std::fs::create_dir_all(&dir)?;
        let manifest = Manifest {
            version: 1,
            window: id.to_owned(),
            generated_at: end,
            window_end: end,
            sources: BTreeMap::new(),
            warnings: Vec::new(),
            exit_status: Status::Ok,
        };
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
        std::fs::write(dir.join("records.jsonl"), b"")?;
        std::fs::write(dir.join("rendered.md"), b"# window\n")?;
        let delta = Delta {
            version: 1,
            window: id.to_owned(),
            window_end: end,
            generated_at: end,
            previous_window: None,
            new_messages: (0..new_messages)
                .map(|n| crate::delta::NewMessage {
                    pointer: format!("echo:{id}/{n}"),
                    label: None,
                    ts: end,
                    author: None,
                    is_reply: false,
                    thread: None,
                    excerpt: String::new(),
                    matches: Vec::new(),
                })
                .collect(),
            unreviewed: Vec::new(),
            closed_since_previous: Vec::new(),
            opened_since_previous: Vec::new(),
            omitted_messages: None,
            omitted_changes: None,
        };
        std::fs::write(dir.join("delta.json"), serde_json::to_vec(&delta)?)?;
        Ok(())
    }

    fn set_marker(data: &DataDir, window: &str) -> Result<()> {
        write(
            &data.marker(),
            &Marker {
                version: 1,
                window: window.to_owned(),
                advanced_at: Utc.with_ymd_and_hms(2026, 10, 30, 2, 0, 0).unwrap(),
                by: "test".to_owned(),
            },
        )
    }

    fn ids(pending: &super::Pending) -> Vec<String> {
        pending.windows.iter().map(|w| w.window.clone()).collect()
    }

    /// A session that reads `ctx pending --limit N` sees exactly the first N windows, oldest
    /// first; the rest stay pending and the next cycle, after the marker advances past the
    /// cap, reads them. This is the mechanism that keeps one failed cycle from handing the next
    /// an ever-larger backlog: each run handles at most the cap, never the whole backlog.
    #[test]
    fn a_cap_leaves_the_windows_past_it_pending() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-cap-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let data = DataDir::new(root.clone());
        // Five completed windows after no marker, oldest first by id.
        let windows = [
            "20261030T020000Z-0000",
            "20261030T021500Z-0000",
            "20261030T023000Z-0000",
            "20261030T024500Z-0000",
            "20261030T030000Z-0000",
        ];
        for (n, id) in windows.iter().enumerate() {
            write_window(&data, id, n + 1)?;
        }

        // No cap lists every pending window.
        let all = |data: &DataDir, limit| {
            pending_in(
                data,
                limit,
                &crate::exclude::Exclusion::default(),
                usize::MAX,
                0,
            )
        };
        assert_eq!(ids(&all(&data, None)?), windows);

        // A cap of two lists only the two oldest, and the delta combines only those.
        let capped = all(&data, Some(2))?;
        assert_eq!(ids(&capped), &windows[..2]);
        let combined = capped.delta.as_ref().map_or(0, |d| d.new_messages.len());
        assert_eq!(
            combined, 3,
            "delta combines windows 1 (1 msg) and 2 (2 msgs) only"
        );

        // The session handles the cap and advances the marker to the newest it handled; the
        // next cycle's capped read is the remainder, never the windows already done.
        set_marker(&data, windows[1])?;
        let next = all(&data, Some(2))?;
        assert_eq!(ids(&next), &windows[2..4]);

        // A cap larger than the backlog lists all of it and nothing waits.
        set_marker(&data, windows[3])?;
        assert_eq!(ids(&all(&data, Some(10))?), &windows[4..]);

        std::fs::remove_dir_all(&root).ok();
        let _: &Path = root.as_path();
        Ok(())
    }
}

#[cfg(test)]
mod pending_size;
