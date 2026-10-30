use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};

use super::{clone_into, Author, Push, Repo};

fn scratch(label: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("ctx-git-{label}-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Runs git in a directory and returns its trimmed standard output.
fn output(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Writes an executable script, from a child process.
///
/// Tests run on many threads, and a child another thread forks holds a copy of every file this
/// process has open until it execs. A script this process wrote itself could be held open for
/// writing by such a child when git runs it, which then fails with "Text file busy". Only the
/// short-lived `sh` below holds the file open, and it starts nothing else.
fn executable(path: &Path, script: &str) -> Result<()> {
    let mut writer = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    std::io::Write::write_all(
        &mut writer
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("the writer has no standard input"))?,
        script.as_bytes(),
    )?;
    let status = writer.wait()?;
    if !status.success() {
        bail!("writing {} failed: {status}", path.display());
    }
    Ok(())
}

/// A bare remote whose `main` holds one commit, and a clone of it in `tree` that tracks
/// `origin/main`.
fn published(label: &str) -> Result<(PathBuf, PathBuf)> {
    let root = scratch(label)?;
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "remote.git",
        ],
    )?;
    let remote = root.join("remote.git");
    let remote = remote.to_str().context("remote path is not UTF-8")?;
    git(&root, &["clone", "--quiet", remote, "tree"])?;
    let tree = root.join("tree");
    std::fs::write(tree.join("rules.md"), "# Rules\n")?;
    git(&tree, &["add", "rules.md"])?;
    git(&tree, &["commit", "--quiet", "--message", "rules"])?;
    git(&tree, &["branch", "--quiet", "--move", "main"])?;
    git(
        &tree,
        &["push", "--quiet", "--set-upstream", "origin", "main"],
    )?;
    Ok((root, tree))
}

/// Commits one new file in `tree`, which the remote does not have yet.
fn commit_one(tree: &Path, name: &str) -> Result<()> {
    std::fs::write(tree.join(name), "a record\n")?;
    git(tree, &["add", name])?;
    git(tree, &["commit", "--quiet", "--message", name])
}

/// Points the clone's push or fetch at a script that stands in for the remote's side.
fn stand_in(root: &Path, tree: &Path, key: &str, script: &str) -> Result<()> {
    let path = root.join(key);
    executable(&path, script)?;
    let path = path.to_str().context("script path is not UTF-8")?;
    git(tree, &["config", key, path])
}

/// A record path is a name, never a pattern: `decisions/*.md` names one file, and a
/// commit of it must not take every other file the pattern would match.
#[test]
fn a_path_is_never_read_as_a_pattern() -> Result<()> {
    let (root, tree) = published("literal-path")?;
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(tree.join("decisions/other.md"), "not part of this write\n")?;
    let repo = Repo::open(&tree)?;
    let author = Author {
        name: "Ada".to_owned(),
        email: "ada@example.com".to_owned(),
    };
    let committed = repo.commit(
        &["decisions/*.md".to_owned()],
        "put decisions/*.md",
        &author,
    );
    let tracked = output(&tree, &["ls-files"])?;
    std::fs::remove_dir_all(&root)?;

    assert!(
        !tracked.contains("decisions/other.md"),
        "the pattern took a file it matched: {tracked}"
    );
    assert!(committed.is_err(), "a commit of a pattern succeeded");
    Ok(())
}

/// A push can land and still end badly: the remote moved its branch, and then its own
/// process failed or the connection broke. The remote is asked before the push is called
/// failed, so a caller is never told a published write was not published.
#[test]
fn a_push_the_remote_took_is_pushed_however_it_ended() -> Result<()> {
    let (root, tree) = published("push-landed-badly")?;
    stand_in(
        &root,
        &tree,
        "remote.origin.receivepack",
        "#!/bin/sh\ngit-receive-pack \"$@\"\nexit 1\n",
    )?;
    commit_one(&tree, "record.md")?;
    let repo = Repo::open(&tree)?;
    let pushed = repo.push();
    let head = repo.head()?;
    let remote_head = output(&root.join("remote.git"), &["rev-parse", "main"])?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        head, remote_head,
        "the push did not land, so this test checks nothing"
    );
    assert!(matches!(pushed, Ok(Push::Pushed)), "{pushed:?}");
    Ok(())
}

/// A push that failed, followed by a look at the remote that failed too, says nothing
/// about where the commit is: the answer is "unknown", never "failed".
#[test]
fn a_failed_push_whose_remote_cannot_be_read_is_unknown() -> Result<()> {
    let (root, tree) = published("push-failed-unread")?;
    stand_in(
        &root,
        &tree,
        "remote.origin.receivepack",
        "#!/bin/sh\nexit 1\n",
    )?;
    stand_in(
        &root,
        &tree,
        "remote.origin.uploadpack",
        "#!/bin/sh\nexit 1\n",
    )?;
    commit_one(&tree, "record.md")?;
    let pushed = Repo::open(&tree)?.push();
    std::fs::remove_dir_all(&root)?;

    assert!(matches!(pushed, Ok(Push::Unknown(_))), "{pushed:?}");
    Ok(())
}

/// A push stopped at its time limit may have landed. When the look at the remote fails
/// for any reason — not only its own time limit — nobody can say, so the answer is
/// "unknown".
#[test]
fn a_stopped_push_whose_remote_cannot_be_read_is_unknown() -> Result<()> {
    let (root, tree) = published("push-stopped-unread")?;
    executable(
        &root.join("remote.git/hooks/pre-receive"),
        "#!/bin/sh\nsleep 30\n",
    )?;
    stand_in(
        &root,
        &tree,
        "remote.origin.uploadpack",
        "#!/bin/sh\nexit 1\n",
    )?;
    commit_one(&tree, "record.md")?;
    let mut repo = Repo::open(&tree)?;
    repo.network_timeout = Some(Duration::from_secs(2));
    let pushed = repo.push();
    std::fs::remove_dir_all(&root)?;

    assert!(matches!(pushed, Ok(Push::Unknown(_))), "{pushed:?}");
    Ok(())
}

/// Outside a write, a clone `ctx` made holds nothing of its own: a commit the remote does
/// not have is one whose caller was told the write failed, and a stray file would be
/// served. Both go. A clone somebody keeps is theirs, and keeps both.
#[test]
fn only_a_clone_ctx_made_has_its_strays_thrown_away() -> Result<()> {
    let (root, theirs) = published("strays")?;
    let remote = root.join("remote.git");
    let remote = remote.to_str().context("remote path is not UTF-8")?;
    let ours = root.join("ours");
    clone_into(remote, &ours, None, Duration::from_secs(120))?;
    for tree in [&ours, &theirs] {
        commit_one(tree, "stranded.md")?;
        std::fs::write(tree.join("stray.md"), "left behind\n")?;
    }
    let ours_discarded = Repo::open(&ours)?.discard_strays()?;
    let theirs_discarded = Repo::open(&theirs)?.discard_strays()?;
    let ours_head = output(&ours, &["rev-parse", "HEAD"])?;
    let ours_status = output(&ours, &["status", "--porcelain"])?;
    let theirs_head = output(&theirs, &["log", "-1", "--format=%s"])?;
    let theirs_stray = theirs.join("stray.md").exists();
    let remote_head = output(&root.join("remote.git"), &["rev-parse", "main"])?;
    std::fs::remove_dir_all(&root)?;

    assert!(ours_discarded, "the clone ctx made kept its strays");
    assert_eq!(ours_head, remote_head, "the stranded commit survived");
    assert_eq!(ours_status, "", "a stray file survived");
    assert!(!theirs_discarded, "somebody's own clone was reset");
    assert_eq!(
        theirs_head, "stranded.md",
        "somebody's own commit was thrown away"
    );
    assert!(theirs_stray, "somebody's own file was thrown away");
    Ok(())
}

/// A remote that accepts the connection and never answers held a clone, and with it a
/// container's start, for as long as the connection stayed open.
#[test]
fn a_clone_from_a_remote_that_never_answers_is_stopped() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    let root = scratch("clone-silent")?;
    let started = Instant::now();
    let cloned = clone_into(
        &format!("http://{address}/graph.git"),
        &root.join("tree"),
        None,
        Duration::from_secs(2),
    );
    let elapsed = started.elapsed();
    std::fs::remove_dir_all(&root)?;

    let error = cloned
        .err()
        .context("a clone from a silent remote succeeded")?;
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    Ok(())
}

/// A push the remote holds open is stopped at its bound, so a write under the lease
/// cannot land after another copy has taken the lease over.
#[test]
fn a_push_the_remote_holds_open_is_stopped() -> Result<()> {
    let root = scratch("push-held")?;
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "remote.git",
        ],
    )?;
    let hook = root.join("remote.git/hooks/pre-receive");
    executable(&hook, "#!/bin/sh\nsleep 30\n")?;
    let remote = root.join("remote.git");
    let remote = remote.to_str().context("remote path is not UTF-8")?;
    git(&root, &["clone", "--quiet", remote, "tree"])?;
    let tree = root.join("tree");
    std::fs::write(tree.join("rules.md"), "# Rules\n")?;
    git(&tree, &["add", "rules.md"])?;
    git(&tree, &["commit", "--quiet", "--message", "rules"])?;
    git(&tree, &["branch", "--quiet", "--move", "main"])?;

    let mut repo = Repo::open(&tree)?;
    repo.network_timeout = Some(Duration::from_secs(2));
    let started = Instant::now();
    let pushed = repo.push();
    let elapsed = started.elapsed();
    std::fs::remove_dir_all(&root)?;

    let error = pushed
        .err()
        .context("a push the remote held open succeeded")?;
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    Ok(())
}

/// A branch that has an upstream whose ref git cannot resolve — here the remote-tracking
/// ref is gone — was read as having none, so `unpublished` named none of the commits the
/// remote lacks and a write could publish them (round-4 review of d5655fc, older items).
#[test]
fn an_upstream_git_cannot_resolve_is_not_read_as_none() -> Result<()> {
    let root = scratch("upstream-unresolved")?;
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "remote.git",
        ],
    )?;
    let remote = root.join("remote.git");
    let remote = remote.to_str().context("remote path is not UTF-8")?;
    git(&root, &["clone", "--quiet", remote, "tree"])?;
    let tree = root.join("tree");
    std::fs::write(tree.join("rules.md"), "# Rules\n")?;
    git(&tree, &["add", "rules.md"])?;
    git(&tree, &["commit", "--quiet", "--message", "rules"])?;
    git(&tree, &["branch", "--quiet", "--move", "main"])?;
    git(
        &tree,
        &["push", "--quiet", "--set-upstream", "origin", "main"],
    )?;
    std::fs::write(tree.join("notes.md"), "# Notes\n")?;
    git(&tree, &["add", "notes.md"])?;
    git(&tree, &["commit", "--quiet", "--message", "not pushed"])?;
    git(&tree, &["update-ref", "-d", "refs/remotes/origin/main"])?;

    let work = Repo::open(&tree)?.unpublished();
    std::fs::remove_dir_all(&root)?;

    assert!(
        !matches!(&work, Ok(work) if work.is_empty()),
        "an upstream git could not resolve was read as none: {work:?}"
    );
    Ok(())
}
