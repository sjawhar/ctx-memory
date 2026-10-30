//! Write rollback, push and rebase recovery tests.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{
    broken_zeppelin_record, good_record, install_hook, merge_during_the_push, put_body,
    refuse_every_push, within,
};
use crate::support;
use crate::support::{commit, fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

/// A push stopped at its time limit may already have landed: the remote moves its branch
/// before it runs `post-receive`, and the push waits for that hook. The server looks at the
/// remote before it answers, so a caller is never told that a published write failed.
#[test]
fn a_push_stopped_after_it_landed_is_reported_committed() -> Result<()> {
    let tree = fixture_clone("write-push-landed")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    install_hook(&bare, "post-receive", "#!/bin/sh\nsleep 60\n")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/landed.md", &good_record(), "session-landed"),
    )?;
    drop(server);
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let status = git(&tree, &["status", "--porcelain"])?;
    remove_fixture(&tree)?;

    assert!(
        listing.lines().any(|name| name == "decisions/landed.md"),
        "the push did not land, so this test checks nothing: {listing}"
    );
    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(reply.text("commit")?, remote_head);
    assert_eq!(status, "", "the write left the clone dirty");
    Ok(())
}

/// A push that fails for a reason other than the remote moving on used to leave the commit
/// in this clone. The next caller's push would then publish it — a record whose own author
/// was told the write failed — and if the remote had moved meanwhile, every later write and
/// every follow pass would fail on a clone that can no longer fast-forward.
#[test]
fn a_failed_push_leaves_no_commit_behind() -> Result<()> {
    let tree = fixture_clone("write-push-fails")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let remote_head_before = git(&bare, &["rev-parse", "main"])?;
    refuse_every_push(&bare)?;

    let server = Server::start(&tree, &[])?;
    let refused = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/lost.md", &good_record(), "session-lost"),
    )?;
    let health = server.get(server.tailnet, "/healthz", &[])?;
    let local_head = git(&tree, &["rev-parse", "HEAD"])?;
    let status = git(&tree, &["status", "--porcelain"])?;

    // The remote starts accepting pushes again, and a later write must publish its own
    // record and nothing else.
    std::fs::remove_file(bare.join("hooks/pre-receive"))?;
    let later = server.post_as(
        "grace@example.com",
        "/v1/records/put",
        &put_body("decisions/later.md", &good_record(), "session-later"),
    )?;
    drop(server);
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    assert_eq!(
        refused.status, 500,
        "a failed push is the server's trouble, not a bad request: {}",
        refused.body
    );
    assert_eq!(
        local_head, remote_head_before,
        "the clone kept a commit the remote never took"
    );
    assert_eq!(status, "", "the failed write left the clone dirty");
    assert_eq!(health.status, 200, "unexpected body: {}", health.body);
    assert_eq!(later.status, 200, "unexpected body: {}", later.body);
    assert!(
        listing.lines().any(|name| name == "decisions/later.md"),
        "the later write did not land: {listing}"
    );
    assert!(
        !listing.lines().any(|name| name == "decisions/lost.md"),
        "a write whose author was told it failed was published: {listing}"
    );
    Ok(())
}

/// The plan's own race, in its hard form: a person edits the same record on GitHub between
/// this server's pull and its push, so the rebase conflicts. git stops part-way through and
/// waits for a person, which no caller of this server can be. The clone goes back to where
/// the write found it, the caller is told it lost the race, and the next write still works.
#[test]
fn a_conflicting_rebase_is_409_and_leaves_no_rebase_behind() -> Result<()> {
    let tree = fixture_clone("write-conflict")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let root = tree.parent().context("the fixture has a root")?;

    // A person's edit of the very record this write creates, staged on another branch and
    // moved onto main by the hook below the moment the server pushes.
    let other = root.join("other");
    git(root, &["clone", "--quiet", bare_text, "other"])?;
    std::fs::create_dir_all(other.join("decisions"))?;
    std::fs::write(
        other.join("decisions/contested.md"),
        good_record().replace("The stream confirmed", "A person wrote"),
    )?;
    git(&other, &["add", "."])?;
    commit(&other, "a person edited the same record")?;
    git(
        &other,
        &["push", "--quiet", "origin", "HEAD:refs/heads/incoming"],
    )?;

    let hooks = bare.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    crate::support::write_executable(
        &hooks.join("pre-receive"),
        "#!/bin/sh\n\
         if [ -f \"$GIT_DIR/raced\" ]; then exit 0; fi\n\
         touch \"$GIT_DIR/raced\"\n\
         unset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\n\
         git update-ref refs/heads/main refs/heads/incoming\n\
         echo 'main moved on' >&2\n\
         exit 1\n",
    )?;

    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/contested.md", &good_record(), "session-raced"),
    )?;
    let health = server.get(server.tailnet, "/healthz", &[])?;
    let rebasing =
        tree.join(".git/rebase-merge").exists() || tree.join(".git/rebase-apply").exists();
    let status = git(&tree, &["status", "--porcelain"])?;

    // The clone is usable again: a write to another record still lands.
    let later = server.post_as(
        "grace@example.com",
        "/v1/records/put",
        &put_body("decisions/after.md", &good_record(), "session-after"),
    )?;
    drop(server);
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let body = git(&bare, &["show", "main:decisions/contested.md"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 409, "unexpected body: {}", reply.body);
    assert!(!rebasing, "a half-finished rebase was left in the clone");
    assert_eq!(status, "", "the conflicting write left the clone dirty");
    assert_eq!(health.status, 200, "unexpected body: {}", health.body);
    assert!(
        body.contains("A person wrote"),
        "the person's version was overwritten: {body}"
    );
    assert_eq!(later.status, 200, "unexpected body: {}", later.body);
    assert!(
        listing.lines().any(|name| name == "decisions/after.md"),
        "the clone did not recover: {listing}"
    );
    Ok(())
}

/// A copy whose clone and remote have each moved on from the other can publish nothing and
/// follow nothing: `pull --ff-only` fails for every write and for every follow pass, while
/// the process is alive and answering. The probe has to see that, or the copy serves a tree
/// that goes stale and refuses every write.
///
/// A clone that is merely ahead of the remote is not that state — its pull is a no-op and its
/// next push publishes what is here — and a copy that called itself unhealthy there would be
/// restarted for no reason.
#[test]
fn healthz_reports_a_clone_that_cannot_fast_forward() -> Result<()> {
    let tree = fixture_clone("health-wedged")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let root = tree.parent().context("the fixture has a root")?;
    let server = Server::start(&tree, &["--pull-interval", "600"])?;
    let healthy = server.get(server.tailnet, "/healthz", &[])?;

    // A commit here that the remote never took, which on its own still fast-forwards.
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(tree.join("decisions/stranded.md"), good_record())?;
    git(&tree, &["add", "decisions/stranded.md"])?;
    commit(&tree, "a commit the remote never took")?;
    let ahead = server.get(server.tailnet, "/healthz", &[])?;

    // A person merges something, and now the two have each moved on from the other.
    let other = root.join("other");
    git(root, &["clone", "--quiet", bare_text, "other"])?;
    std::fs::create_dir_all(other.join("decisions"))?;
    std::fs::write(
        other.join("decisions/merged.md"),
        "---\ntype: decision\nstatus: open\ntitle: Merged elsewhere\n---\nA person merged this.\n",
    )?;
    git(&other, &["add", "."])?;
    commit(&other, "a record merged on GitHub")?;
    git(&other, &["push", "--quiet", "origin", "main"])?;
    git(&tree, &["fetch", "--quiet", "origin"])?;

    let wedged = server.get(server.tailnet, "/healthz", &[])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(healthy.status, 200, "unexpected body: {}", healthy.body);
    assert_eq!(
        ahead.status, 200,
        "a clone that is only ahead reported itself unhealthy: {}",
        ahead.body
    );
    assert_eq!(
        wedged.status, 503,
        "a wedged clone reported healthy: {}",
        wedged.body
    );
    assert_eq!(wedged.field("ok")?, &json!(false));
    Ok(())
}

/// A server started on an empty directory clones `CTX_MEMORY_REMOTE` and marks the clone its
/// own. Returns the fixture's seed tree (for `remove_fixture`), the bare remote and the
/// server's clone.
fn clone_ctx_makes(
    label: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let seed = fixture_clone(label)?;
    fetch_window(&seed)?;
    let bare = remote_of(&seed)?;
    let clone = seed
        .parent()
        .context("the fixture has a root")?
        .join("served");
    std::fs::create_dir_all(&clone)?;
    Ok((seed, bare, clone))
}

/// Commits a record straight into a clone, as a write whose rollback failed leaves it.
fn strand_a_commit(clone: &Path) -> Result<()> {
    std::fs::create_dir_all(clone.join("decisions"))?;
    std::fs::write(clone.join("decisions/stranded.md"), good_record())?;
    git(clone, &["add", "decisions/stranded.md"])?;
    commit(clone, "a write whose caller was told it failed")?;
    Ok(())
}

/// Outside a write, a commit in a clone `ctx` made that the remote does not have is one
/// whose caller was told the write failed: a rollback that failed itself leaves it there. The
/// copy calls itself wedged, and the next write throws the commit away rather than publish it
/// under somebody else's write.
#[test]
fn a_stranded_commit_in_a_clone_ctx_made_is_never_published() -> Result<()> {
    let (seed, bare, clone) = clone_ctx_makes("write-stranded")?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let server = Server::start_with_env(
        &clone,
        &["--pull-interval", "3600"],
        &[("CTX_MEMORY_REMOTE", bare_text)],
    )?;
    strand_a_commit(&clone)?;
    let wedged = server.get(server.tailnet, "/healthz", &[])?;
    let later = server.post_as(
        "grace@example.com",
        "/v1/records/put",
        &put_body("decisions/later.md", &good_record(), "session-later"),
    )?;
    let healthy = server.get(server.tailnet, "/healthz", &[])?;
    drop(server);
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&seed)?;

    assert_eq!(
        wedged.status, 503,
        "a clone holding a stranded commit reported healthy: {}",
        wedged.body
    );
    assert_eq!(later.status, 200, "unexpected body: {}", later.body);
    assert!(
        listing.lines().any(|name| name == "decisions/later.md"),
        "the later write did not land: {listing}"
    );
    assert!(
        !listing.lines().any(|name| name == "decisions/stranded.md"),
        "a write whose caller was told it failed was published: {listing}"
    );
    assert_eq!(healthy.status, 200, "unexpected body: {}", healthy.body);
    Ok(())
}

/// A copy that writes nothing still serves its tree, so the follow loop puts a clone `ctx`
/// made back the way the remote has it rather than wait for the next write.
#[test]
fn the_follow_loop_throws_away_a_stranded_commit() -> Result<()> {
    let (seed, bare, clone) = clone_ctx_makes("follow-stranded")?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let server = Server::start_with_env(
        &clone,
        &["--pull-interval", "1"],
        &[("CTX_MEMORY_REMOTE", bare_text)],
    )?;
    strand_a_commit(&clone)?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let healed = within(60, || {
        let head = git(&clone, &["rev-parse", "HEAD"])?;
        let health = server.get(server.tailnet, "/healthz", &[])?;
        Ok((head == remote_head && health.status == 200).then_some(()))
    });
    drop(server);
    remove_fixture(&seed)?;
    healed
}

/// Only a rebase git stopped part-way through is a race the caller lost. A pull that never
/// reached the remote started no rebase, and is the server's own trouble.
#[test]
fn a_rebase_that_cannot_reach_the_remote_is_not_a_conflict() -> Result<()> {
    let tree = fixture_clone("write-rebase-unreachable")?;
    fetch_window(&tree)?;
    merge_during_the_push(
        &tree,
        "decisions/merged.md",
        "---\ntype: decision\nstatus: open\ntitle: Merged elsewhere\n---\nA person merged this.\n",
    )?;
    let root = tree.parent().context("the fixture has a root")?;
    // The write's own pull and the look at the remote after the refused push reach it; the
    // pull that would rebase does not.
    let wrapper = root.join("upload-pack");
    let count = root.join("upload-pack.count");
    crate::support::write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\n\
             n=$(cat '{count}' 2>/dev/null || echo 0)\n\
             n=$((n + 1))\n\
             echo \"$n\" > '{count}'\n\
             if [ \"$n\" -ge 3 ]; then echo 'the remote is unreachable' >&2; exit 1; fi\n\
             exec git-upload-pack \"$@\"\n",
            count = count.display()
        ),
    )?;
    let wrapper_text = wrapper
        .to_str()
        .context("wrapper path is not valid UTF-8")?;
    git(&tree, &["config", "remote.origin.uploadpack", wrapper_text])?;
    let before = git(&tree, &["rev-parse", "HEAD"])?;

    let server = Server::start(&tree, &["--pull-interval", "3600"])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/raced.md", &good_record(), "session-unreachable"),
    )?;
    drop(server);
    let after = git(&tree, &["rev-parse", "HEAD"])?;
    let status = git(&tree, &["status", "--porcelain"])?;
    let rebasing =
        tree.join(".git/rebase-merge").exists() || tree.join(".git/rebase-apply").exists();
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 500, "unexpected body: {}", reply.body);
    assert!(!rebasing, "a rebase was left in the clone");
    assert_eq!(after, before, "the clone kept the failed write");
    assert_eq!(status, "", "the failed write left the clone dirty");
    Ok(())
}

/// A follow pass that cannot mint the token its pull needs has followed nothing. It used to
/// take the failure for "this tree is not a clone" and pass in silence, so a copy that had
/// stopped seeing merges answered every probe as if nothing were wrong.
///
/// The probe still answers 200: a restart cannot mint a token GitHub will not give, and a
/// copy that cannot reach the remote at start cannot clone at all, so restarting it would
/// stop it serving the tree it has. It says the loop is failing. `/healthz` is open to anyone
/// who can reach the port, so it says so in fixed words: why the loop fails — a URL, a
/// repository, what GitHub answered — is in the server's log, not in the probe's answer.
#[test]
fn healthz_says_when_the_follow_loop_is_failing() -> Result<()> {
    let tree = fixture_clone("follow-mint")?;
    // GitHub's API answers nothing, so no installation token can be minted.
    let github = support::fake_http::FakeHttp::start()?;
    let api = github.url("");
    git(
        &tree,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{api}/other-org/memory.git"),
        ],
    )?;
    let app = support::fake_github::FakeGithub::app_credential();
    let server = Server::start_with_env(
        &tree,
        &["--pull-interval", "1"],
        &[
            ("CTX_GITHUB_API", api.as_str()),
            ("CTX_CREDENTIAL_GITHUB_APP", app.as_str()),
        ],
    )?;
    let failing = within(60, || {
        let health = server.get(server.tailnet, "/healthz", &[])?;
        let ok = health
            .body
            .get("following")
            .and_then(|following| following.get("ok"))
            .cloned();
        Ok((ok == Some(json!(false))).then_some(health))
    });
    // A pass that failed is followed by the next one. The loop used to stop for good at the
    // first failure it printed: `main` held the process's stderr locked, and the loop's
    // message waited for it forever.
    let lookup = "/repos/other-org/memory/installation";
    let retried = within(60, || Ok((github.hits(lookup)? >= 3).then_some(())));
    drop(server);
    remove_fixture(&tree)?;

    let health = failing?;
    retried?;
    assert_eq!(health.status, 200, "unexpected body: {}", health.body);
    let error = health
        .body
        .get("following")
        .and_then(|following| following.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !error.is_empty(),
        "the probe does not say the loop fails: {}",
        health.body
    );
    assert!(
        !error.contains("installation") && !error.contains(&api),
        "the open probe repeats why the loop fails: {}",
        health.body
    );
    Ok(())
}

/// The real git on this machine's `PATH`.
fn real_git() -> Result<std::path::PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .context("no git on PATH")
}

/// A `PATH` whose `git` is killed — as the kernel's out-of-memory killer kills a process — when
/// it is asked for `origin`'s URL while `trigger` exists, and is the real git otherwise.
fn path_with_a_git_killed_reading_the_remote(root: &Path, trigger: &Path) -> Result<String> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    let script = bin.join("git");
    crate::support::write_executable(
        &script,
        &format!(
            "#!/bin/sh\nif [ -e '{}' ]; then\n  for argument in \"$@\"; do\n    case \"$argument\" in\n      get-url|remote.origin.url) kill -KILL $$ ;;\n    esac\n  done\nfi\nexec '{}' \"$@\"\n",
            trigger.display(),
            real_git()?.display()
        ),
    )?;
    let inherited = std::env::var_os("PATH").context("PATH is not set")?;
    let joined =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited)))?;
    joined
        .into_string()
        .map_err(|_| anyhow::anyhow!("PATH is not valid UTF-8"))
}

/// A git killed while it names `origin` said nothing about the remote. The writer took that
/// for "this clone has no remote": it skipped the pull and the push, committed, and answered
/// 200 with a commit nobody published, which the next write then threw away.
#[test]
fn a_write_whose_remote_cannot_be_read_is_never_reported_stored() -> Result<()> {
    let tree = fixture_clone("write-remote-unread")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let trigger = root.join("kill-remote-reads");
    let path = path_with_a_git_killed_reading_the_remote(root, &trigger)?;
    let server = Server::start_with_env(&tree, &["--pull-interval", "3600"], &[("PATH", &path)])?;
    std::fs::write(&trigger, "")?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/unread.md", &good_record(), "session-unread"),
    )?;
    std::fs::remove_file(&trigger)?;
    drop(server);
    let bare = remote_of(&tree)?;
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let remote_head = git(&bare, &["rev-parse", "main"])?;
    let local_head = git(&tree, &["rev-parse", "HEAD"])?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    assert!(
        !listing.lines().any(|name| name == "decisions/unread.md"),
        "the write reached the remote, so this test checks nothing: {listing}"
    );
    assert_ne!(
        reply.status, 200,
        "a write nobody published was reported stored: {}",
        reply.body
    );
    assert_eq!(
        local_head, remote_head,
        "the clone kept a commit the remote never took"
    );
    assert_eq!(status, "", "the failed write left the clone dirty");
    Ok(())
}

/// Only "this is not a git repository" means a copy has no `main` to follow. A clone git
/// cannot open — a configuration it cannot read, a directory it will not trust — used to be
/// taken for none: the follow loop skipped the pull and called the pass good, and `/healthz`
/// answered 200 for a copy that saw no merge. No write can start from such a clone, so the
/// probe answers 503, as it does for any clone that cannot be the start of a write.
#[test]
fn a_clone_git_cannot_open_is_reported_not_passed_over() -> Result<()> {
    let tree = fixture_clone("follow-unopenable")?;
    let server = Server::start(&tree, &["--pull-interval", "1"])?;
    let config = tree.join(".git/config");
    let kept = std::fs::read_to_string(&config)?;
    std::fs::write(&config, format!("{kept}[[[ a line git cannot read\n"))?;
    let reported = within(60, || {
        let health = server.get(server.tailnet, "/healthz", &[])?;
        let following = health
            .body
            .get("following")
            .and_then(|following| following.get("ok"))
            .cloned();
        Ok((health.status != 200 && following == Some(json!(false))).then_some(health))
    });
    std::fs::write(&config, kept)?;
    drop(server);
    remove_fixture(&tree)?;

    let health = reported.context(
        "/healthz never said the clone cannot be opened, nor that the follow loop fails",
    )?;
    assert_eq!(health.status, 503, "unexpected body: {}", health.body);
    assert_eq!(
        health.body.get("error").and_then(Value::as_str),
        Some("the health check failed; the server's log says why"),
        "unexpected body: {}",
        health.body
    );
    assert!(
        !health.body.to_string().contains("bad config"),
        "the open probe repeats git's error: {}",
        health.body
    );
    Ok(())
}

/// A write whose rollback failed left the refused record on disk, and the server read its
/// tree back from that disk: the record nothing would let into the graph was served to every
/// reader until a later pass cleaned the clone. A lock a killed git left behind is enough for
/// the rollback to fail.
#[test]
fn a_failed_rollback_never_serves_the_refused_record() -> Result<()> {
    let tree = fixture_clone("write-rollback-fails")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &["--pull-interval", "3600"])?;
    let lock = tree.join(".git/index.lock");
    std::fs::write(&lock, "")?;
    let refused = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/zeppelin.md",
            &broken_zeppelin_record(),
            "session-rollback",
        ),
    )?;
    let found = server.command(&["search", "zeppelin"])?;
    std::fs::remove_file(&lock)?;
    drop(server);
    let left = tree.join("decisions/zeppelin.md").exists();
    remove_fixture(&tree)?;

    assert!(
        left,
        "the rollback did not fail, so this test checks nothing"
    );
    assert_eq!(
        refused.status, 500,
        "a rollback that failed is the server's trouble: {}",
        refused.body
    );
    assert!(
        !found.text("stdout")?.contains("Zeppelin budget"),
        "the server serves the refused record: {}",
        found.body
    );
    Ok(())
}

/// After a failed rollback in a tree a person keeps, the refused record stays on disk: the
/// server throws nothing of theirs away. The next follow pass that moves the clone read the
/// tree back from that disk and served the record nothing would let into the graph (round-4
/// review of d5655fc, N4-3). It keeps serving the tree from before the write until the tree
/// holds nothing the remote lacks.
#[test]
fn a_follow_pass_after_a_failed_rollback_never_serves_the_refused_record() -> Result<()> {
    let tree = fixture_clone("follow-after-rollback")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let server = Server::start(&tree, &["--pull-interval", "1"])?;
    let lock = tree.join(".git/index.lock");
    std::fs::write(&lock, "")?;
    let refused = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/zeppelin.md",
            &broken_zeppelin_record(),
            "session-rollback",
        ),
    )?;
    std::fs::remove_file(&lock)?;
    // Someone merges a page on GitHub, so the next follow pass fast-forwards the clone.
    let other = root.join("other");
    let bare = remote_of(&tree)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let other_text = other.to_str().context("clone path is not valid UTF-8")?;
    git(root, &["clone", "--quiet", bare_text, other_text])?;
    std::fs::write(
        other.join("helium.md"),
        "---\ntype: decision\nstatus: open\ntitle: Helium merged on GitHub\n---\nMerged.\n",
    )?;
    git(&other, &["add", "helium.md"])?;
    git(
        &other,
        &[
            "-c",
            "user.name=Merger",
            "-c",
            "user.email=merger@example.com",
            "commit",
            "--quiet",
            "-m",
            "merge helium",
        ],
    )?;
    git(&other, &["push", "--quiet", "origin", "HEAD:main"])?;
    let pulled = within(30, || {
        let reply = git(&tree, &["log", "-1", "--format=%s"])?;
        Ok(reply.contains("merge helium").then_some(()))
    });
    // Give the pass that pulled time to finish what it does with the tree.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let found = server.command(&["search", "zeppelin"])?;
    drop(server);
    let left = tree.join("decisions/zeppelin.md").exists();
    remove_fixture(&tree)?;

    assert_eq!(refused.status, 500, "{}", refused.body);
    assert!(
        left,
        "the rollback did not fail, so this test checks nothing"
    );
    pulled.context("the follow loop never pulled the merge")?;
    assert!(
        !found.text("stdout")?.contains("Zeppelin budget"),
        "the follow pass serves the refused record: {}",
        found.body
    );
    Ok(())
}
