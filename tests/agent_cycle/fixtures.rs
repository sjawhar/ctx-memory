//! Repository paths and scratch directories that more than one agent cycle check uses.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};

static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

pub(super) fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

pub(super) fn deploy(name: &str) -> Result<String> {
    let path = repo().join("deploy/agent").join(name);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

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

pub(super) fn write_executable(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
