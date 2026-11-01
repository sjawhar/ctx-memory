//! The server's clone, the follow loop, recovery after a failed write, and `/healthz`.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, install_hook, put_body, within};
use crate::support;
use crate::support::{commit, fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};

#[test]
fn merged_pull_request_is_searchable_within_a_minute() -> Result<()> {
    let tree = fixture_clone("follow-merge")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &["--pull-interval", "1"])?;

    // A person merges a record straight into main, as a pull request does.
    let root = tree.parent().context("the fixture has a root")?;
    let bare = remote_of(&tree)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let other = root.join("merged");
    git(root, &["clone", "--quiet", bare_text, "merged"])?;
    std::fs::create_dir_all(other.join("decisions"))?;
    std::fs::write(
        other.join("decisions/merged.md"),
        "---\ntype: decision\nstatus: open\ntitle: Merged by a person\n---\nThe board approved the helium budget.\n",
    )?;
    git(&other, &["add", "."])?;
    commit(&other, "a record merged on GitHub")?;
    git(&other, &["push", "--quiet", "origin", "main"])?;

    let found = within(10, || {
        let reply = server.command(&["search", "helium"])?;
        Ok(reply
            .text("stdout")
            .map(str::to_owned)
            .ok()
            .filter(|stdout| stdout.contains("Merged by a person")))
    });
    drop(server);
    remove_fixture(&tree)?;
    found?;
    Ok(())
}

#[test]
fn a_new_window_becomes_resolvable() -> Result<()> {
    let tree = fixture_clone("follow-window")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &["--pull-interval", "1"])?;
    // The window the fetch wrote is in the index the server built before it bound its port.
    let first = server.command(&["resolve", "echo:C0EXAMPLE01/record"])?;
    assert_eq!(first.field("exit_code")?, &json!(0));

    // The fetch job writes a later window on the share, which the server has not indexed.
    let root = tree.parent().context("the fixture has a root")?;
    write_window(root, "20260102T000000Z-0001", "C0EXAMPLE02")?;

    let resolved = within(10, || {
        let reply = server.command(&["resolve", "echo:C0EXAMPLE02/record"])?;
        Ok(reply
            .text("stdout")
            .map(str::to_owned)
            .ok()
            .filter(|stdout| stdout.contains("echo C0EXAMPLE02")))
    });
    drop(server);
    remove_fixture(&tree)?;
    resolved?;
    Ok(())
}

/// Writes one completed window on the share, as the fetch job leaves it: a record of the echo
/// source in `stream`, under the pointer `echo:<stream>/record`.
fn write_window(root: &Path, id: &str, stream: &str) -> Result<()> {
    let window = root.join("data/windows").join(id);
    std::fs::create_dir_all(&window)?;
    std::fs::write(
        window.join("manifest.json"),
        format!(
            "{{\"version\":1,\"window\":\"{id}\",\"generated_at\":\"2026-01-02T00:00:00Z\",\"window_end\":\"2026-01-02T00:00:00Z\",\"sources\":{{}},\"warnings\":[],\"exit_status\":\"ok\"}}\n"
        ),
    )?;
    std::fs::write(
        window.join("records.jsonl"),
        format!(
            "{{\"source\":\"echo\",\"record\":{{\"pointer\":\"echo:{stream}/record\",\"stream\":\"{stream}\",\"ts\":\"2026-01-01T23:00:00Z\",\"text\":\"echo {stream}\"}}}}\n"
        ),
    )?;
    std::fs::write(window.join("rendered.md"), "# window\n")?;
    Ok(())
}

/// A copy builds its index before it binds a port. The index is in the state directory, which
/// a container's image does not carry, so a copy that left the build to the first caller that
/// needed one — or to its first follow pass, a pull interval later — answered that caller
/// minutes later. The pull interval here is long enough that no pass runs during the test, so
/// only the build this copy makes before it binds can have indexed the window.
#[test]
fn the_index_is_built_before_the_listeners_are_bound() -> Result<()> {
    let tree = fixture_clone("index-before-listen")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    write_window(root, "20260102T000000Z-0001", "C0EXAMPLE02")?;

    let server = Server::start(&tree, &["--pull-interval", "600"])?;
    let resolved = server
        .command(&["resolve", "echo:C0EXAMPLE02/record"])
        .and_then(|reply| reply.text("stdout").map(str::to_owned));
    drop(server);
    remove_fixture(&tree)?;
    let resolved = resolved?;
    assert!(
        resolved.contains("echo C0EXAMPLE02"),
        "the window was not indexed before the listeners were bound: {resolved}"
    );
    Ok(())
}

/// A copy that starts reads only the windows its index has not indexed yet. Each one used to
/// re-read every window on the share at its first pass, because the loop kept that watermark
/// in memory: the deployment's copy of 2026-11-01 held the tree for 157 s doing it, and every
/// read waited behind it. The store keeps the watermark, so the window read here is the new
/// one; the old one cannot be read at all, and reading it would be the failure.
#[test]
fn a_restarted_copy_reads_only_the_windows_its_index_has_not_seen() -> Result<()> {
    let tree = fixture_clone("index-watermark")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let mut server = Server::start(&tree, &["--pull-interval", "1"])?;
    let indexed = within(10, || {
        let reply = server.command(&["resolve", "echo:C0EXAMPLE01/record"])?;
        Ok(reply
            .text("stdout")
            .map(str::to_owned)
            .ok()
            .filter(|stdout| stdout.contains("echo C0EXAMPLE01")))
    });
    server.terminate()?;
    drop(server);
    indexed?;

    // The copy that replaces it finds the window it already indexed unreadable, and a new one
    // beside it. A pass that read the share from the start would fail on the first and never
    // reach the second.
    let fetched = std::fs::read_dir(root.join("data/windows"))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("2026"))
        .context("the fetch wrote no window")?;
    std::fs::write(
        root.join("data/windows")
            .join(&fetched)
            .join("records.jsonl"),
        "this window cannot be read\n",
    )?;
    write_window(root, "20260103T000000Z-0000", "C0EXAMPLE03")?;

    let server = Server::start(&tree, &["--pull-interval", "1"])?;
    let resolved = within(10, || {
        let reply = server.command(&["resolve", "echo:C0EXAMPLE03/record"])?;
        Ok(reply
            .text("stdout")
            .map(str::to_owned)
            .ok()
            .filter(|stdout| stdout.contains("echo C0EXAMPLE03")))
    });
    drop(server);
    remove_fixture(&tree)?;
    resolved?;
    Ok(())
}

/// Leaves what a copy stopped mid-write leaves: a half-written tracked file and an untracked
/// one.
fn leave_a_half_written_write(tree: &Path) -> Result<()> {
    std::fs::write(
        tree.join("rules.md"),
        "---\ntype: rules\n---\n# Half-written\n",
    )?;
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(tree.join("decisions/stray.md"), "a file nobody committed\n")?;
    Ok(())
}

/// How one `ctx serve` start ended.
enum Started {
    /// It stopped by itself, with this exit code and standard error.
    Refused(Option<i32>, String),
    /// It bound its listener and was serving; the test stopped it.
    Serving,
}

/// Starts `ctx serve` on a tree and reports whether it refused or began serving.
fn start_serve(tree: &Path, environment: &[(&str, &str)]) -> Result<Started> {
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = support::binary();
    command
        .args([
            "--memory",
            tree_text,
            "serve",
            "--tailnet-listen",
            "127.0.0.1:0",
        ])
        .envs(support::tree_env(tree)?)
        .envs(environment.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().context("starting ctx serve")?;
    let stdout = child.stdout.take().context("ctx serve has no stdout")?;
    let (lines, listened) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)).map_while(Result::ok) {
            if line.starts_with("tailnet-listen ") {
                let _ = lines.send(());
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                std::io::Read::read_to_string(&mut pipe, &mut stderr)?;
            }
            return Ok(Started::Refused(status.code(), stderr));
        }
        if listened.try_recv().is_ok() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Started::Serving);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ctx serve neither refused nor started within two minutes");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A tree somebody keeps and works in is theirs. `ctx serve` used to reset and clean whatever
/// tree it was started in, and a person who ran it in their own checkout lost the work in it.
/// It now refuses to start, naming that work, and leaves every file as it was. The refusal is
/// one line, as every line the server writes to standard error is: it listed the work one
/// item to a line, and on Cloud Run each line is its own log entry (round-5 review of
/// d9cb9f7, N5-1).
#[test]
fn serve_refuses_a_dirty_tree_it_did_not_clone() -> Result<()> {
    let tree = fixture_clone("serve-dirty-theirs")?;
    leave_a_half_written_write(&tree)?;
    let started = start_serve(&tree, &[])?;
    let rules = std::fs::read_to_string(tree.join("rules.md"))?;
    let stray = tree.join("decisions/stray.md").exists();
    remove_fixture(&tree)?;

    assert!(
        stray,
        "the server removed an untracked file in a tree it did not clone"
    );
    assert!(
        rules.contains("# Half-written"),
        "the server reset a tracked file in a tree it did not clone: {rules}"
    );
    let Started::Refused(code, stderr) = started else {
        anyhow::bail!("ctx serve started on a tree holding uncommitted work");
    };
    assert_eq!(code, Some(2), "{stderr}");
    for named in ["rules.md", "decisions/stray.md"] {
        assert!(
            stderr.contains(named),
            "the refusal does not name {named}: {stderr}"
        );
    }
    assert_eq!(
        stderr.trim_end().lines().count(),
        1,
        "the error that stopped the server is not one line: {stderr}"
    );
    Ok(())
}

/// A clone the server made itself from `CTX_MEMORY_REMOTE` is nobody's work: whatever a copy
/// stopped mid-write left in it is thrown away, so the server starts from `main`.
#[test]
fn serve_resets_a_dirty_clone_it_made() -> Result<()> {
    let seed = fixture_clone("serve-dirty-ours")?;
    let bare = remote_of(&seed)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let root = seed.parent().context("the fixture has a root")?;
    let clone = root.join("served");
    std::fs::create_dir_all(&clone)?;
    let remote = [("CTX_MEMORY_REMOTE", bare_text)];
    drop(Server::start_with_env(&clone, &[], &remote)?);
    leave_a_half_written_write(&clone)?;

    let server = Server::start_with_env(&clone, &[], &remote)?;
    let reply = server.get(server.tailnet, "/healthz", &[])?;
    drop(server);
    let status = git(&clone, &["status", "--porcelain"])?;
    let rules = std::fs::read_to_string(clone.join("rules.md"))?;
    let stray = clone.join("decisions/stray.md").exists();
    remove_fixture(&seed)?;

    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(status, "", "the server left its own clone dirty");
    assert!(
        rules.contains("# Rules"),
        "rules.md was not restored: {rules}"
    );
    assert!(!stray, "the untracked file survived the reset");
    Ok(())
}

/// A failed write puts the tree back where the write found it, which throws away whatever
/// else changed there since. In a tree somebody works in, that would be their work, so the
/// server writes into such a tree only while it holds no work but the write's own.
#[test]
fn a_write_is_refused_while_their_tree_holds_uncommitted_work() -> Result<()> {
    let tree = fixture_clone("write-dirty-theirs")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    std::fs::write(tree.join("notes.md"), "a draft somebody is writing\n")?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body(
            "decisions/while-they-work.md",
            &good_record(),
            "session-dirty",
        ),
    )?;
    drop(server);
    let draft = std::fs::read_to_string(tree.join("notes.md")).ok();
    let listing = git(
        &remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 409, "unexpected body: {}", reply.body);
    assert!(
        reply.text("error")?.contains("notes.md"),
        "unexpected body: {}",
        reply.body
    );
    assert_eq!(draft.as_deref(), Some("a draft somebody is writing\n"));
    assert!(
        !listing.contains("while-they-work"),
        "the write reached the remote: {listing}"
    );
    Ok(())
}

/// Commits a record in somebody's own tree and does not push it.
fn commit_without_pushing(tree: &Path) -> Result<()> {
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(
        tree.join("decisions/unpushed.md"),
        "---\ntype: decision\nstatus: open\n---\nA draft somebody committed and has not pushed.\n",
    )?;
    git(tree, &["add", "decisions/unpushed.md"])?;
    commit(tree, "local work not pushed yet")?;
    Ok(())
}

/// A commit somebody made in their own tree and has not pushed is theirs, like their
/// uncommitted work: the server must not publish it with the next write it makes there.
///
/// `ctx serve` refused to start in such a tree while it held uncommitted changes, but started
/// in one whose only difference from the remote was a local commit, and the first remote write
/// then pushed that commit to `main` — under its own author, without the check every write
/// through the server gets. Observed on the real surface: a put through `ctx serve` in a
/// person's tree published that person's unpushed commit, which held a record with no quote and
/// no tier.
#[test]
fn a_write_never_publishes_their_unpushed_commit() -> Result<()> {
    let tree = fixture_clone("write-unpushed-theirs")?;
    fetch_window(&tree)?;
    commit_without_pushing(&tree)?;

    let started = start_serve(&tree, &[])?;
    let reply = match &started {
        Started::Refused(_, _) => None,
        Started::Serving => {
            let server = Server::start(&tree, &[])?;
            let reply = server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body(
                    "decisions/next-write.md",
                    &good_record(),
                    "session-unpushed",
                ),
            )?;
            drop(server);
            Some(reply)
        }
    };
    let listing = git(
        &remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    let local = git(&tree, &["ls-tree", "-r", "--name-only", "HEAD"])?;
    remove_fixture(&tree)?;

    assert!(
        !listing.contains("decisions/unpushed.md"),
        "the server published a commit somebody had not pushed (write answered {:?}): {listing}",
        reply.map(|reply| (reply.status, reply.body))
    );
    assert!(
        local.contains("decisions/unpushed.md"),
        "the server removed a commit from a tree it did not clone: {local}"
    );
    let Started::Refused(code, stderr) = started else {
        anyhow::bail!("ctx serve started on a tree holding a commit the remote does not have");
    };
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("local work not pushed yet"),
        "the refusal does not name the commit: {stderr}"
    );
    Ok(())
}

/// The same commit made while the server runs is refused at the next write, which names it
/// and publishes nothing.
#[test]
fn a_write_is_refused_while_their_tree_holds_an_unpushed_commit() -> Result<()> {
    let tree = fixture_clone("write-unpushed-later")?;
    fetch_window(&tree)?;
    let server = Server::start(&tree, &[])?;
    commit_without_pushing(&tree)?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/next-write.md", &good_record(), "session-later"),
    )?;
    drop(server);
    let listing = git(
        &remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    let local = git(&tree, &["log", "-1", "--format=%s"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 409, "unexpected body: {}", reply.body);
    assert!(
        reply.text("error")?.contains("local work not pushed yet"),
        "the refusal does not name the commit: {}",
        reply.body
    );
    assert!(
        !listing.contains("decisions/unpushed.md") && !listing.contains("next-write"),
        "the write published something: {listing}"
    );
    assert_eq!(local, "local work not pushed yet", "their commit was moved");
    Ok(())
}

#[test]
fn serve_clones_the_remote_when_the_path_is_empty() -> Result<()> {
    let seed = fixture_clone("follow-clone")?;
    let bare = remote_of(&seed)?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    let root = seed.parent().context("the fixture has a root")?;
    let empty = root.join("fresh");
    std::fs::create_dir_all(&empty)?;
    let head = git(&bare, &["rev-parse", "main"])?;

    let server = Server::start_with_env(&empty, &[], &[("CTX_MEMORY_REMOTE", bare_text)])?;
    let reply = server.get(server.tailnet, "/healthz", &[])?;
    let rules = server.command(&["rules"])?;
    drop(server);
    let cloned = empty.join("ctx.toml").is_file();
    remove_fixture(&seed)?;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.text("head")?, head);
    assert!(cloned, "ctx serve did not clone the remote");
    assert!(
        rules.text("stdout")?.contains("# Rules"),
        "the cloned tree did not answer: {}",
        rules.body
    );
    Ok(())
}

/// A probe answers while a write holds the tree. `/healthz` used to wait for the tree lock,
/// so a push the remote held open held every health check with it, and a probe that gave up
/// would restart a copy that was only busy.
#[test]
fn healthz_answers_while_a_write_holds_the_tree() -> Result<()> {
    let tree = fixture_clone("health-busy")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let pushing = tree
        .parent()
        .context("the fixture has a root")?
        .join("pushing");
    install_hook(
        &bare,
        "pre-receive",
        &format!("#!/bin/sh\ntouch '{}'\nsleep 10\n", pushing.display()),
    )?;
    let server = Server::start(&tree, &[])?;
    let (written, probed) = std::thread::scope(|scope| -> Result<_> {
        let writing = scope.spawn(|| {
            server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &put_body("decisions/slow.md", &good_record(), "session-slow"),
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !pushing.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let started = std::time::Instant::now();
        let probe = server.get(server.tailnet, "/healthz", &[]);
        let elapsed = started.elapsed();
        let written = writing
            .join()
            .map_err(|_| anyhow::anyhow!("the writing thread panicked"))?;
        Ok((written, probe.map(|probe| (probe, elapsed))))
    })?;
    drop(server);
    remove_fixture(&tree)?;

    let (probe, elapsed) = probed?;
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the health check waited {elapsed:?} behind the write"
    );
    assert_eq!(probe.status, 200, "unexpected body: {}", probe.body);
    assert_eq!(probe.field("busy")?, &json!(true), "{}", probe.body);
    let written = written?;
    assert_eq!(written.status, 200, "unexpected body: {}", written.body);
    Ok(())
}

/// A probe that finds the tree busy answers from the last check the server made. The first
/// follow pass can take the tree before any probe arrives, which on a loaded machine it often
/// does, and the server had made no check then: the probe said `ok` with no head, about a
/// clone nothing had looked at. The server checks its clone before it takes any probe.
#[test]
fn a_probe_during_the_first_follow_pass_answers_from_a_check() -> Result<()> {
    let tree = fixture_clone("health-first-pass")?;
    let root = tree.parent().context("the fixture has a root")?;
    let pulling = root.join("pulling");
    let resume = root.join("resume");
    let wrapper = root.join("upload-pack");
    crate::support::write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\nexec git-upload-pack \"$@\"\n",
            pulling.display(),
            resume.display()
        ),
    )?;
    let wrapper_text = wrapper
        .to_str()
        .context("wrapper path is not valid UTF-8")?;
    git(&tree, &["config", "remote.origin.uploadpack", wrapper_text])?;
    let head = git(&tree, &["rev-parse", "HEAD"])?;

    let server = Server::start(&tree, &["--pull-interval", "1"])?;
    let probe = within(120, || Ok(pulling.exists().then_some(())))
        .and_then(|()| server.get(server.tailnet, "/healthz", &[]));
    std::fs::write(&resume, "")?;
    drop(server);
    remove_fixture(&tree)?;

    let probe = probe?;
    assert_eq!(probe.status, 200, "unexpected body: {}", probe.body);
    assert_eq!(
        probe.field("busy")?,
        &json!(true),
        "the probe did not land during the follow pass, so this test checks nothing: {}",
        probe.body
    );
    assert_eq!(
        probe.body.get("head").and_then(Value::as_str),
        Some(head.as_str()),
        "the probe answered without a check of the clone: {}",
        probe.body
    );
    Ok(())
}
