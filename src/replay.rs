//! The cohort `ctx fetch --replay` delivers again, and how far its runs have got.
//!
//! A cohort file lists messages that windows already delivered and that the ingest never read,
//! one line each, `<origin window id>\t<pointer>`, sorted by window and then by pointer: the
//! window that first delivered the message, and the message. Each run of the replay takes whole
//! origin windows, in the file's order, and writes each again as a new window
//! ([`crate::fetch::replay`]). `replay/<cohort file name>.json` in the data directory, beside the
//! cursors, keeps the cohort's SHA-256 and the origin windows already replayed, so the next run
//! goes on after them, and a different cohort under the same name is refused.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::atomic::write_replace;
use crate::windows::WindowId;

/// The origin windows of a cohort file, in the file's order, and the file's SHA-256.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cohort {
    pub sha256: String,
    pub origins: Vec<Origin>,
}

/// One origin window of a cohort and the pointers the cohort lists for it, in the file's order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    pub window: WindowId,
    pub pointers: Vec<String>,
}

/// One origin window a run takes: the pointers it keeps, in the cohort's order, and how many the
/// deployment excludes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Taken<'a> {
    pub origin: &'a Origin,
    pub kept: Vec<&'a str>,
    pub excluded: usize,
}

impl Cohort {
    /// Reads the cohort file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, lists no message, or a line is not an
    /// origin window id, a tab and a pointer, or does not sort after the line before it by window
    /// and then by pointer: a file out of that order could split an origin window, or list a
    /// message twice. An empty file is refused rather than read as a cohort with nothing left,
    /// which a truncated file would otherwise pass for.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("reading the cohort {}", path.display()))?;
        let origins =
            parse(text).with_context(|| format!("reading the cohort {}", path.display()))?;
        Ok(Self {
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            origins,
        })
    }

    /// The origin windows one run takes, in the file's order, each whole: the first one
    /// `progress` has not replayed that keeps a message, however many it keeps, then each next
    /// one while the run's kept messages stay at most `max_messages` and the origin windows that
    /// keep one at most `max_windows`. `keep` says whether the deployment keeps a pointer. An
    /// origin window that keeps nothing writes no window, so it is taken without counting against
    /// either limit. An origin window is never split, so the progress file holds only whole ones.
    #[must_use]
    pub fn batch(
        &self,
        progress: &Progress,
        max_messages: usize,
        max_windows: usize,
        keep: impl Fn(&str) -> bool,
    ) -> Vec<Taken<'_>> {
        let mut batch = Vec::new();
        let mut messages = 0;
        let mut windows = 0;
        for origin in self.origins.iter().filter(|origin| progress.lacks(origin)) {
            let kept: Vec<&str> = origin
                .pointers
                .iter()
                .map(String::as_str)
                .filter(|pointer| keep(pointer))
                .collect();
            if !kept.is_empty() {
                if windows > 0 && (messages + kept.len() > max_messages || windows >= max_windows) {
                    break;
                }
                messages += kept.len();
                windows += 1;
            }
            batch.push(Taken {
                excluded: origin.pointers.len() - kept.len(),
                origin,
                kept,
            });
        }
        batch
    }

    /// How many origin windows of the cohort `progress` has not replayed.
    #[must_use]
    pub fn remaining(&self, progress: &Progress) -> usize {
        self.origins
            .iter()
            .filter(|origin| progress.lacks(origin))
            .count()
    }
}

fn parse(text: &str) -> Result<Vec<Origin>> {
    let mut origins: Vec<Origin> = Vec::new();
    let mut last: Option<(&str, &str)> = None;
    let mut listed = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let (window, pointer) = line.split_once('\t').ok_or_else(|| {
            anyhow!("line {number} is not an origin window id, a tab and a pointer")
        })?;
        let window_id = WindowId::parse(window).with_context(|| format!("line {number}"))?;
        if !pointer
            .split_once(':')
            .is_some_and(|(scheme, locator)| !scheme.is_empty() && !locator.is_empty())
            || pointer.contains(char::is_whitespace)
        {
            bail!("line {number}: {pointer:?} is not a pointer");
        }
        if last.is_some_and(|last| last >= (window, pointer)) {
            bail!("line {number} does not sort after the line before it by window, then pointer");
        }
        last = Some((window, pointer));
        // One message, one line: a pointer under two origin windows would be replayed twice.
        if !listed.insert(pointer) {
            bail!("line {number}: {pointer} is listed under an earlier origin window too");
        }
        match origins.last_mut() {
            Some(origin) if origin.window.as_str() == window => {
                origin.pointers.push(pointer.to_owned());
            }
            _ => origins.push(Origin {
                window: window_id,
                pointers: vec![pointer.to_owned()],
            }),
        }
    }
    if origins.is_empty() {
        bail!("it lists no message");
    }
    Ok(origins)
}

/// The origin windows of one cohort that earlier runs replayed: each wrote its window, or kept
/// none of its messages. `cohort` is the SHA-256 of the cohort file the runs read. The ids are
/// kept as text, so a file another build wrote with an id this one cannot parse still says what
/// it replayed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub version: u32,
    pub cohort: String,
    pub replayed: BTreeSet<String>,
}

impl Progress {
    /// The progress of a cohort no run has replayed.
    #[must_use]
    pub fn new(cohort: &str) -> Self {
        Self {
            version: 1,
            cohort: cohort.to_owned(),
            replayed: BTreeSet::new(),
        }
    }

    /// Whether no run has replayed `origin` yet.
    fn lacks(&self, origin: &Origin) -> bool {
        !self.replayed.contains(origin.window.as_str())
    }
}

/// The name the progress file of the cohort at `cohort` takes its name from: the file's own name.
///
/// # Errors
///
/// Returns an error when the path names no file, or a name that is not UTF-8.
pub fn cohort_name(cohort: &Path) -> Result<&str> {
    cohort
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            anyhow!(
                "the cohort {} has no file name this build can read",
                cohort.display()
            )
        })
}

/// Reads the progress file at `path` of the cohort whose SHA-256 is `cohort`; a file that is not
/// there is a cohort no run has replayed.
///
/// # Errors
///
/// Returns an error when the file is there but cannot be read or decoded, has another version, or
/// is the progress of a cohort with another SHA-256: a cohort file changed after its first run,
/// or another cohort with the same file name, whose progress this is not.
pub fn load(path: &Path, cohort: &str) -> Result<Progress> {
    let progress: Progress = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Progress::new(cohort))
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if progress.version != 1 {
        bail!(
            "{} has unsupported replay version {}",
            path.display(),
            progress.version
        );
    }
    if progress.cohort != cohort {
        bail!(
            "{} is the progress of the cohort with SHA-256 {}, not of this one, whose SHA-256 is \
             {cohort}; a cohort file is not changed once replayed, and another cohort takes \
             another file name",
            path.display(),
            progress.cohort
        );
    }
    Ok(progress)
}

/// Replaces the progress file at `path`.
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn write(path: &Path, progress: &Progress) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(progress).context("serializing replay progress")?;
    write_replace(path, &bytes)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::{parse, Cohort, Progress, Taken};

    const A: &str = "20261108T182845Z-0000";
    const B: &str = "20261109T000000Z-0001";
    const C: &str = "20261110T120000Z-0000";

    fn cohort(lines: &[(&str, &str)]) -> Result<Cohort> {
        let text = lines
            .iter()
            .map(|(window, pointer)| [*window, "\t", *pointer, "\n"].concat())
            .collect::<String>();
        Ok(Cohort {
            sha256: String::new(),
            origins: parse(&text)?,
        })
    }

    fn windows(batch: &[Taken<'_>]) -> Vec<String> {
        batch
            .iter()
            .map(|taken| taken.origin.window.to_string())
            .collect()
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn a_cohort_groups_its_lines_by_origin_window() -> Result<()> {
        let cohort = cohort(&[(A, "gmail:a"), (A, "gmail:b"), (B, "matrix:c")])?;
        let grouped: Vec<(String, Vec<String>)> = cohort
            .origins
            .iter()
            .map(|origin| (origin.window.to_string(), origin.pointers.clone()))
            .collect();
        assert_eq!(
            grouped,
            vec![
                (
                    A.to_owned(),
                    vec!["gmail:a".to_owned(), "gmail:b".to_owned()]
                ),
                (B.to_owned(), vec!["matrix:c".to_owned()]),
            ]
        );
        Ok(())
    }

    #[test]
    fn a_cohort_out_of_order_or_with_a_line_twice_is_refused() {
        for lines in [
            &[(B, "gmail:a"), (A, "gmail:b")][..],
            &[(A, "gmail:b"), (A, "gmail:a")][..],
            &[(A, "gmail:a"), (A, "gmail:a")][..],
            &[(A, "gmail:a"), (B, "gmail:b"), (A, "gmail:c")][..],
            &[(A, "gmail:a"), (B, "gmail:a")][..],
        ] {
            assert!(cohort(lines).is_err(), "{lines:?} was taken");
        }
    }

    #[test]
    fn a_cohort_with_no_line_or_a_line_not_a_window_and_a_pointer_is_refused() {
        for text in [
            "20261108T182845Z-0000 gmail:a\n",
            "20261108T182845Z\tgmail:a\n",
            "20261108T182845Z-0000\tgmail\n",
            "20261108T182845Z-0000\t:a\n",
            "20261108T182845Z-0000\tgmail:\n",
            "20261108T182845Z-0000\tgmail:a b\n",
            "\n",
            "",
        ] {
            assert!(parse(text).is_err(), "{text:?} was taken");
        }
    }

    #[test]
    fn a_run_takes_whole_windows_within_its_limits() -> Result<()> {
        let cohort = cohort(&[
            (A, "gmail:a1"),
            (A, "gmail:a2"),
            (B, "gmail:b1"),
            (B, "gmail:b2"),
            (C, "gmail:c1"),
        ])?;
        let progress = Progress::new("");
        assert_eq!(windows(&cohort.batch(&progress, 4, 25, all)), vec![A, B]);
        assert_eq!(windows(&cohort.batch(&progress, 3, 25, all)), vec![A]);
        assert_eq!(windows(&cohort.batch(&progress, 350, 2, all)), vec![A, B]);
        assert_eq!(windows(&cohort.batch(&progress, 350, 1, all)), vec![A]);
        // The first window not yet replayed is taken whole even when it holds more than the
        // run may write.
        assert_eq!(windows(&cohort.batch(&progress, 1, 25, all)), vec![A]);
        Ok(())
    }

    #[test]
    fn a_run_counts_only_the_messages_it_keeps() -> Result<()> {
        let cohort = cohort(&[
            (A, "gmail:a1"),
            (A, "slack:a2"),
            (A, "slack:a3"),
            (B, "slack:b1"),
            (C, "gmail:c1"),
        ])?;
        let progress = Progress::new("");
        let gmail = |pointer: &str| pointer.starts_with("gmail:");
        let batch = cohort.batch(&progress, 2, 1, gmail);
        // A keeps one message and B none, so B is taken free; C would be a second window.
        assert_eq!(windows(&batch), vec![A, B]);
        assert_eq!(
            batch
                .iter()
                .map(|taken| (taken.kept.clone(), taken.excluded))
                .collect::<Vec<_>>(),
            vec![(vec!["gmail:a1"], 2), (vec![], 1)]
        );
        assert_eq!(
            windows(&cohort.batch(&progress, 2, 25, gmail)),
            vec![A, B, C]
        );
        Ok(())
    }

    #[test]
    fn a_run_goes_on_after_the_windows_already_replayed() -> Result<()> {
        let cohort = cohort(&[(A, "gmail:a1"), (B, "gmail:b1"), (C, "gmail:c1")])?;
        let mut progress = Progress::new("");
        assert_eq!(cohort.remaining(&progress), 3);
        progress.replayed.insert(A.to_owned());
        assert_eq!(windows(&cohort.batch(&progress, 1, 25, all)), vec![B]);
        assert_eq!(cohort.remaining(&progress), 2);
        progress.replayed.insert(B.to_owned());
        progress.replayed.insert(C.to_owned());
        assert_eq!(
            windows(&cohort.batch(&progress, 350, 25, all)),
            Vec::<String>::new()
        );
        assert_eq!(cohort.remaining(&progress), 0);
        Ok(())
    }
}
