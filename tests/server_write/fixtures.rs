//! Helpers that more than one child of the server write suite uses.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::support;
use crate::support::{commit, git, remote_of};

pub(super) fn good_record() -> String {
    support::cited_record()
}

pub(super) fn put_body(path: &str, content: &str, session: &str) -> Value {
    json!({ "path": path, "content": content, "session": session })
}

/// Waits for something the server does on its own, or gives up after a few seconds.
pub(super) fn within<T>(seconds: u64, mut attempt: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if let Some(value) = attempt()? {
            return Ok(value);
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the server did not get there within {seconds}s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Installs one executable hook script in the bare remote.
pub(super) fn install_hook(bare: &Path, name: &str, script: &str) -> Result<()> {
    let hooks = bare.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    support::write_executable(&hooks.join(name), script)
}

/// Makes the bare remote refuse every push, the way a remote that is down or that refuses
/// this server's credential does. `main` does not move, so this is not the race the rebase
/// path handles.
pub(super) fn refuse_every_push(bare: &Path) -> Result<()> {
    install_hook(
        bare,
        "pre-receive",
        "#!/bin/sh\necho 'the remote refused this push' >&2\nexit 1\n",
    )
}

/// A record that fails the sweep, under a title nothing else in the fixture carries, so a
/// search tells whether the server serves it.
pub(super) fn broken_zeppelin_record() -> String {
    good_record().replace("Keep the echo cadence", "Zeppelin budget")
        + "\nIt replaces [[a-record-that-does-not-exist]].\n"
}

/// Stages a person's merge that lands on `main` the moment the server first pushes: the
/// remote refuses that push, and the server has to rebase onto the merge and push again.
pub(super) fn merge_during_the_push(tree: &Path, path: &str, content: &str) -> Result<()> {
    let bare = remote_of(tree)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let root = tree.parent().context("the fixture has a root")?;
    let other = root.join("other");
    git(root, &["clone", "--quiet", bare_text, "other"])?;
    let file = other.join(path);
    std::fs::create_dir_all(file.parent().context("a record has a directory")?)?;
    std::fs::write(file, content)?;
    git(&other, &["add", "."])?;
    commit(&other, "a pull request someone merged")?;
    git(
        &other,
        &["push", "--quiet", "origin", "HEAD:refs/heads/incoming"],
    )?;
    install_hook(
        &bare,
        "pre-receive",
        "#!/bin/sh\n\
         if [ -f \"$GIT_DIR/raced\" ]; then exit 0; fi\n\
         touch \"$GIT_DIR/raced\"\n\
         unset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\n\
         git update-ref refs/heads/main refs/heads/incoming\n\
         echo 'main moved on' >&2\n\
         exit 1\n",
    )
}
