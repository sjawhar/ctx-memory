//! Repository paths and scratch directories that more than one agent cycle check uses.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{ensure, Context as _, Result};

static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

pub(super) fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The text of `runner/<name>`, the ingest runner this repository ships.
pub(super) fn runner(name: &str) -> Result<String> {
    let path = repo().join("runner").join(name);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

/// The maintenance passes `runner/run-cycle.sh` runs, as its `MAINTENANCE_PASSES` names them.
pub(super) fn maintenance_passes() -> Result<Vec<String>> {
    let script = runner("run-cycle.sh")?;
    let line = script
        .lines()
        .find_map(|line| line.strip_prefix("MAINTENANCE_PASSES="))
        .context("run-cycle.sh names no MAINTENANCE_PASSES")?;
    Ok(line
        .trim_matches('"')
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

/// What the scheduled session is told to follow, as `(name, text)`: both skills, and the
/// instructions of every maintenance pass the ingest skill keeps.
pub(super) fn session_instructions() -> Result<Vec<(String, String)>> {
    let skills = repo().join("skills");
    let mut paths = vec![
        skills.join("ctx-memory-ingest/SKILL.md"),
        skills.join("ctx-memory/SKILL.md"),
    ];
    let maintenance = skills.join("ctx-memory-ingest/maintenance");
    let mut passes: Vec<PathBuf> = std::fs::read_dir(&maintenance)
        .with_context(|| format!("reading {}", maintenance.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    passes.sort();
    paths.extend(passes);
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let name = path
                .strip_prefix(&skills)
                .unwrap_or(&path)
                .display()
                .to_string();
            Ok((name, text))
        })
        .collect()
}

/// The model the tests tell `run-cycle.sh` to select: the one provider of
/// `runner/models.example.yml` and its one model, which omp sends as the request's model.
pub(super) const EXAMPLE_MODEL: &str = "gateway/example-model";
pub(super) const EXAMPLE_MODEL_ID: &str = "example-model";

pub(super) fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-agent-cycle-{label}-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

pub(super) fn remove_scratch(path: &Path) -> Result<()> {
    reopen_directories(path);
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// A test may have closed a directory; open every one again so the tree can be removed.
fn reopen_directories(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                reopen_directories(&entry.path());
            }
        }
    }
}

/// Writes `text` to `path` as an executable, from a child process.
///
/// Tests run on many threads, and each child another thread forks holds a copy of every file
/// this process has open until that child execs. A stand-in this process wrote itself could be
/// held open for writing by such a child at the moment a test runs it, and running it then
/// fails with "Text file busy": `run-cycle.sh` read that as an `id` that answered nothing and
/// refused to start as if it were not root. Here only the short-lived `sh` below holds the file
/// open, and it starts nothing else.
pub(super) fn write_executable(path: &Path, text: &str) -> Result<()> {
    let mut writer = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the writer of {}", path.display()))?;
    writer
        .stdin
        .take()
        .context("the writer has no standard input")?
        .write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    let status = writer.wait()?;
    ensure!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
    Ok(())
}
