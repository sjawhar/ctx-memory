//! Completed fetch-window storage and lookup.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::record::Record;

static NEXT_WINDOW_SUFFIX: AtomicU16 = AtomicU16::new(0);

/// A collision-resistant identifier for one bounded fetch window.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WindowId(String);

impl WindowId {
    fn generate(window_end: DateTime<Utc>) -> Self {
        let suffix = NEXT_WINDOW_SUFFIX.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "{}-{suffix:04x}",
            window_end.format("%Y%m%dT%H%M%SZ")
        ))
    }

    /// Parses a completed window identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a timestamp followed by four hexadecimal digits.
    pub fn parse(value: &str) -> Result<Self> {
        let (timestamp, suffix) = value
            .rsplit_once('-')
            .ok_or_else(|| anyhow!("window id {value:?} is missing its suffix"))?;
        if suffix.len() != 4 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("window id {value:?} must end in four hexadecimal digits");
        }
        let parsed = NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ")
            .with_context(|| format!("parsing window id {value:?}"))?;
        if parsed.format("%Y%m%dT%H%M%SZ").to_string() != timestamp {
            bail!("window id {value:?} has a non-canonical timestamp");
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the identifier text used for the completed window directory.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A completed or staged fetch window.
#[derive(Clone, Debug)]
pub struct Window {
    pub id: WindowId,
    pub dir: PathBuf,
}

impl Window {
    /// Loads this window's manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be read, decoded, or uses another format version.
    pub fn manifest(&self) -> Result<Manifest> {
        let path = self.dir.join("manifest.json");
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        if manifest.version != 1 {
            bail!(
                "{} has unsupported manifest version {}",
                path.display(),
                manifest.version
            );
        }
        Ok(manifest)
    }

    /// Loads the normalized records captured in this window.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSONL file cannot be read or one line is invalid.
    pub fn records(&self) -> Result<Vec<SourcedRecord>> {
        let path = self.dir.join("records.jsonl");
        let file = fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut records = Vec::new();
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let line =
                line.with_context(|| format!("reading {} line {}", path.display(), number + 1))?;
            let record = serde_json::from_str(&line)
                .with_context(|| format!("decoding {} line {}", path.display(), number + 1))?;
            records.push(record);
        }
        Ok(records)
    }

    /// Loads the optional open-item snapshot written by the delta engine.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot exists but cannot be read or decoded.
    pub fn snapshot(&self) -> Result<Option<serde_json::Value>> {
        let path = self.dir.join("open-snapshot.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Opens a completed window directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not a completed window directory.
    pub fn open(dir: PathBuf) -> Result<Self> {
        if !dir.is_dir() {
            bail!("window {} does not exist", dir.display());
        }
        let name = dir
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| anyhow!("window {} has no valid name", dir.display()))?;
        let id = WindowId::parse(name)?;
        for required in ["manifest.json", "records.jsonl", "rendered.md"] {
            if !dir.join(required).is_file() {
                bail!("window {} is incomplete", dir.display());
            }
        }
        Ok(Self { id, dir })
    }
}

/// The status of an entire source or one source stream in a manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Error,
}

/// One source record with the source configuration that emitted it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourcedRecord {
    pub source: String,
    pub record: Record,
}

/// Persisted details of one completed fetch window.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub window: String,
    pub generated_at: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub sources: BTreeMap<String, SourceManifest>,
    pub warnings: Vec<String>,
    pub exit_status: Status,
}

/// Persisted status of one configured source in a fetch window.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub kind: String,
    pub status: Status,
    pub error: Option<String>,
    pub streams: BTreeMap<String, StreamManifest>,
}

/// Persisted status of one source stream in a fetch window.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamManifest {
    pub status: Status,
    pub error: Option<String>,
    pub cursor_used: Option<String>,
    pub cursor_next: Option<String>,
    pub records: usize,
    pub label: Option<String>,
    pub owner: Option<String>,
}

/// Returns the newest completed window under `windows`.
///
/// # Errors
///
/// Returns an error when no completed window exists or a directory cannot be inspected.
pub fn newest(windows: &Path) -> Result<Window> {
    completed(windows)?
        .into_iter()
        .max_by(|left, right| left.id.cmp(&right.id))
        .ok_or_else(|| anyhow!("no completed fetch windows"))
}

/// Returns the completed window immediately before `before`, if one exists.
///
/// # Errors
///
/// Returns an error when the window directory cannot be inspected.
pub fn previous(windows: &Path, before: &WindowId) -> Result<Option<Window>> {
    Ok(completed(windows)?
        .into_iter()
        .filter(|window| window.id < *before)
        .max_by(|left, right| left.id.cmp(&right.id)))
}

/// Resolves `newest` or a completed window identifier into its directory.
///
/// # Errors
///
/// Returns an error when the value does not name a completed window.
pub fn resolve_arg(value: &str, windows: &Path) -> Result<PathBuf> {
    if value == "newest" {
        return newest(windows).map(|window| window.dir);
    }
    let id = WindowId::parse(value)?;
    Window::open(windows.join(id.as_str())).map(|window| window.dir)
}

pub(crate) fn stage(windows: &Path, window_end: DateTime<Utc>) -> Result<Window> {
    fs::create_dir_all(windows).with_context(|| format!("creating {}", windows.display()))?;
    for _ in 0..=u16::MAX {
        let id = WindowId::generate(window_end);
        let completed = windows.join(id.as_str());
        if completed.exists() {
            continue;
        }
        let dir = windows.join(format!(".tmp-{id}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(Window { id, dir }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("creating {}", dir.display())),
        }
    }
    bail!(
        "could not allocate a unique fetch window in {}",
        windows.display()
    )
}

pub(crate) fn complete(staged: Window, windows: &Path) -> Result<Window> {
    let completed = windows.join(staged.id.as_str());
    if completed.exists() {
        bail!("window {} already exists", completed.display());
    }
    fs::File::open(&staged.dir)
        .with_context(|| format!("opening {} for sync", staged.dir.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", staged.dir.display()))?;
    fs::rename(&staged.dir, &completed).with_context(|| {
        format!(
            "moving staged window {} to {}",
            staged.dir.display(),
            completed.display()
        )
    })?;
    fs::File::open(windows)
        .with_context(|| format!("opening {} for sync", windows.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", windows.display()))?;
    Ok(Window {
        id: staged.id,
        dir: completed,
    })
}

fn completed(windows: &Path) -> Result<Vec<Window>> {
    if !windows.is_dir() {
        return Err(anyhow!("no completed fetch windows"));
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(windows).with_context(|| format!("reading {}", windows.display()))? {
        let entry = entry.with_context(|| format!("reading {}", windows.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspecting {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(id) = WindowId::parse(name) else {
            continue;
        };
        let dir = entry.path();
        if ["manifest.json", "records.jsonl", "rendered.md"]
            .iter()
            .all(|required| dir.join(required).is_file())
        {
            found.push(Window { id, dir });
        }
    }
    Ok(found)
}
