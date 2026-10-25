//! Durable whole-file replacement.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, Context as _, Result};

static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

fn open_temporary(path: &Path) -> Result<(PathBuf, fs::File)> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    for _ in 0..128 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary {}", temporary.display()));
            }
        }
    }
    Err(anyhow!(
        "could not allocate a temporary replacement for {}",
        path.display()
    ))
}

fn cleanup_temporary(temporary: &Path, error: anyhow::Error) -> anyhow::Error {
    match fs::remove_file(temporary) {
        Ok(()) => error,
        Err(cleanup) => error.context(format!(
            "removing temporary {} after failed replacement: {cleanup}",
            temporary.display()
        )),
    }
}

/// Replaces `path` only after its complete new contents are durable.
///
/// # Errors
///
/// Returns an error when its parent or temporary file cannot be created, bytes cannot be
/// written and synced, the replacement cannot be renamed, or the parent directory cannot be
/// synced.
pub fn write_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let (temporary, mut file) = open_temporary(path)?;
    let write_result: Result<()> = (|| {
        file.write_all(bytes)
            .with_context(|| format!("writing temporary {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary {}", temporary.display()))
    })();
    drop(file);
    if let Err(error) = write_result {
        return Err(cleanup_temporary(&temporary, error));
    }
    if let Err(error) =
        fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
    {
        return Err(cleanup_temporary(&temporary, error));
    }
    fs::File::open(parent)
        .with_context(|| format!("opening {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", parent.display()))
}
