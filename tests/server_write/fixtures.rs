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

/// The bare remote's `proc-receive` hook, which takes over every push to `main` and answers it
/// as `outcomes` in the remote's git directory says, one line per push, in order:
///
/// - `fail`: `main` stays where it is, and the push is refused for a failure of the remote's
///   own, as a hosting service's backend refused pushes during an outage. git prints
///   `remote: fatal error in commit_refs` and ` ! [remote rejected] HEAD -> main (failure)`.
/// - `land-and-fail`: `main` moves to the pushed commit, and the push is refused all the same.
/// - a push past the last line lands.
///
/// Each push the hook answers appends its outcome to `pushes` there. The hook speaks git's
/// proc-receive protocol (githooks(5)): a version line and a flush, then one `ng <ref>
/// <reason>` or `ok <ref>` packet per command and a flush, each packet four hex digits of
/// length and the text. Objects are already out of quarantine when it runs.
const PROC_RECEIVE: &str = r#"#!/bin/sh
read_pkt() {
  len=$(dd bs=1 count=4 2>/dev/null)
  [ "$len" != 0000 ] || return 1
  pkt=$(dd bs=1 count=$((0x$len - 4)) 2>/dev/null)
}
write_pkt() { printf '%04x%s' $((${#1} + 4)) "$1"; }
while read_pkt; do :; done
write_pkt version=1
printf 0000
commands=
while read_pkt; do commands="$commands$pkt
"; done
unset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
printf '%s' "$commands" | while read -r old new ref; do
  outcome=$(head -n 1 "$GIT_DIR/outcomes")
  tail -n +2 "$GIT_DIR/outcomes" > "$GIT_DIR/outcomes.next"
  mv "$GIT_DIR/outcomes.next" "$GIT_DIR/outcomes"
  echo "${outcome:-land}" >> "$GIT_DIR/pushes"
  case "$outcome" in
    fail)
      echo 'fatal error in commit_refs' >&2
      write_pkt "ng $ref failure" ;;
    land-and-fail)
      git update-ref "$ref" "$new" "$old"
      echo 'fatal error in commit_refs' >&2
      write_pkt "ng $ref failure" ;;
    *)
      if git update-ref "$ref" "$new" "$old"; then write_pkt "ok $ref"
      else write_pkt "ng $ref failed to update ref"; fi ;;
  esac
done
printf 0000
"#;

/// Makes the bare remote answer its next pushes to `main` as `outcomes` says, one per push,
/// as [`PROC_RECEIVE`] describes; every push after them lands.
pub(super) fn fail_pushes_on_the_remotes_side(bare: &Path, outcomes: &[&str]) -> Result<()> {
    git(
        bare,
        &["config", "receive.procReceiveRefs", "refs/heads/main"],
    )?;
    let mut listed = outcomes.join("\n");
    listed.push('\n');
    std::fs::write(bare.join("outcomes"), listed)?;
    install_hook(bare, "proc-receive", PROC_RECEIVE)
}

/// How many pushes the remote [`fail_pushes_on_the_remotes_side`] set up was asked to take.
pub(super) fn pushes_taken(bare: &Path) -> Result<usize> {
    match std::fs::read_to_string(bare.join("pushes")) {
        Ok(pushes) => Ok(pushes.lines().count()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// Stages a person's merge that lands on `main` right after the server commits a write and
/// before it pushes, so git itself refuses the push, `! [rejected] HEAD -> main (fetch
/// first)`, without asking the remote to move anything. Returns the merge's commit.
pub(super) fn merge_before_the_servers_push(
    tree: &Path,
    path: &str,
    content: &str,
) -> Result<String> {
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
    let merged = git(&other, &["rev-parse", "HEAD"])?;
    // The hook runs once: the rebase that follows commits again.
    install_hook(
        &tree.join(".git"),
        "post-commit",
        &format!(
            "#!/bin/sh\n\
             if [ -e '{raced}' ]; then exit 0; fi\n\
             touch '{raced}'\n\
             unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX\n\
             git -C '{other}' push --quiet origin HEAD:main\n",
            raced = root.join("raced").display(),
            other = other.display(),
        ),
    )?;
    Ok(merged)
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
