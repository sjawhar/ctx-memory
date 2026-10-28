//! The server's lease: one holder across copies, checked before each push and the marker, and
//! given back when the server stops.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{Duration, Utc};
use ctx_memory::lease::Lease;
use serde_json::{json, Value};

use crate::fixtures::{frozen, reads, Cloud, BUCKET};
use crate::support;
use crate::support::{
    binary, cited_record, fetch_window, fixture_clone, remove_fixture, wait_for_path, Server,
};

#[test]
fn a_write_is_refused_while_another_copy_holds_the_server_lease() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-server-refused")?;
    fetch_window(&tree)?;
    let holder = Lease::acquire(BUCKET, "server", "server@old-copy")?;
    let carried = cloud.environment();
    let environment: Vec<(&str, &str)> = carried
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();

    let server = Server::start_with_env(&tree, &[], &environment)?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }),
    )?;
    drop(server);

    let listing = support::git(
        &support::remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    holder.check()?;
    remove_fixture(&tree)?;
    assert_eq!(reply.status, 503, "unexpected body: {}", reply.body);
    assert!(
        reply.body.to_string().contains("server@old-copy"),
        "the refusal does not name the holder: {}",
        reply.body
    );
    assert!(
        !listing.lines().any(|name| name == "decisions/cadence.md"),
        "the refused write reached the remote: {listing}"
    );
    Ok(())
}

#[test]
fn the_server_takes_the_lease_once_and_keeps_it() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-server-holds")?;
    fetch_window(&tree)?;
    let carried = cloud.environment();
    let environment: Vec<(&str, &str)> = carried
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();

    let server = Server::start_with_env(&tree, &[], &environment)?;
    let first = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }),
    )?;
    let second = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({ "path": "decisions/cadence-two.md", "content": cited_record(), "session": "s" }),
    )?;
    drop(server);

    let (_, held) = cloud.held_by("server")?;
    let writes = cloud.storage.writes()?;
    remove_fixture(&tree)?;
    assert_eq!(first.status, 200, "unexpected body: {}", first.body);
    assert_eq!(second.status, 200, "unexpected body: {}", second.body);
    // One acquisition, kept: the first write creates the object, and every later one renews
    // it, replacing exactly the generation the write before it made, under the same holder. A
    // renewal falls due every 30 seconds, so a run that outlasts one shows one; a second
    // acquisition would create the object again or carry another holder.
    assert!(!writes.is_empty(), "the server never took the lease");
    let preconditions: Vec<&str> = writes
        .iter()
        .map(|write| write.parameter("ifGenerationMatch").unwrap_or_default())
        .collect();
    let chained: Vec<String> = (0..writes.len()).map(|n| n.to_string()).collect();
    assert_eq!(
        preconditions, chained,
        "the server took the lease more than once: {writes:?}"
    );
    let mut holders = Vec::new();
    for write in &writes {
        let body: Value = serde_json::from_str(&write.body)?;
        holders.push(
            body.get("holder")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
    }
    holders.dedup();
    assert_eq!(holders.len(), 1, "the lease changed holders: {writes:?}");
    assert!(
        held.get("holder")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .starts_with("server@"),
        "the lease is not held by the server: {held}"
    );
    Ok(())
}

/// Makes the server's next pull announce itself and wait: it runs this instead of
/// `git-upload-pack`, touches the first path once the pull has begun, and goes on only once
/// the second exists. Returns both paths.
fn hold_the_pull(tree: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = tree.parent().context("the fixture has a root")?;
    let pulling = root.join("pulling");
    let resume = root.join("resume");
    let wrapper = root.join("upload-pack");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\nexec git-upload-pack \"$@\"\n",
            pulling.display(),
            resume.display()
        ),
    )?;
    let mut permissions = std::fs::metadata(&wrapper)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&wrapper, permissions)?;
    let wrapper_text = wrapper.to_str().context("wrapper path is not UTF-8")?;
    support::git(tree, &["config", "remote.origin.uploadpack", wrapper_text])?;
    Ok((pulling, resume))
}

/// The lease lets another copy take over 30 seconds after the last check this copy passed,
/// and a write can spend longer than that between its first check and its push. So the
/// writer checks again right before it pushes: a copy whose lease was taken over while it
/// pulled publishes nothing and leaves its clone as it found it.
#[test]
fn a_write_that_loses_its_lease_before_the_push_publishes_nothing() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-server-lost")?;
    fetch_window(&tree)?;
    let (pulling, resume) = hold_the_pull(&tree)?;
    let carried = cloud.environment();
    let environment: Vec<(&str, &str)> = carried
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();

    // No follow pass may pull while the write does: it would wait on the wrapper too.
    let server = Server::start_with_env(&tree, &["--pull-interval", "3600"], &environment)?;
    let taking_over = std::thread::spawn(move || -> Result<Lease> {
        wait_for_path(&pulling, "the write's pull")?;
        let takeover = Lease::acquire_with(
            BUCKET,
            "server",
            "server@takeover",
            frozen(Utc::now() + Duration::seconds(121)),
        );
        std::fs::write(&resume, "")?;
        takeover
    });
    let reply = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }),
    )?;
    // A copy whose check failed gives its handle up but never deletes the object: when the
    // check failed by age, the object is still this copy's, and deleting it would let the
    // next copy start at once, while a push of this copy's may still be in flight.
    let deleted = cloud.storage.deletes()?;
    drop(server);
    let takeover = taking_over
        .join()
        .map_err(|_| anyhow!("the takeover thread panicked"))?;
    let listing = support::git(
        &support::remote_of(&tree)?,
        &["ls-tree", "-r", "--name-only", "main"],
    )?;
    let status = support::git(&tree, &["status", "--porcelain"])?;
    remove_fixture(&tree)?;

    takeover?.check()?;
    assert!(
        deleted.is_empty(),
        "the copy that failed its check deleted the lease object: {deleted:?}"
    );
    assert_eq!(reply.status, 503, "unexpected body: {}", reply.body);
    assert!(
        !listing.lines().any(|name| name == "decisions/cadence.md"),
        "a write that lost its lease reached the remote: {listing}"
    );
    assert_eq!(status, "", "the write left the clone dirty");
    Ok(())
}

/// The marker is on the share every copy reads, and only the lease holder may move it. The
/// marker endpoint checked the lease and only then waited for the tree, which a write holds
/// for as long as git takes: by the time it moved the marker, another copy could have taken
/// the lease over and started writing.
#[test]
fn a_marker_advance_checks_the_lease_after_it_waits_for_the_tree() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-marker-wait")?;
    let window = fetch_window(&tree)?;
    let (pulling, resume) = hold_the_pull(&tree)?;
    let carried = cloud.environment();
    let environment: Vec<(&str, &str)> = carried
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();

    let server = Server::start_with_env(&tree, &["--pull-interval", "3600"], &environment)?;
    let (advanced, takeover) = std::thread::scope(|scope| -> Result<_> {
        let writing = scope.spawn(|| {
            server.post_as(
                "ada@example.com",
                "/v1/records/put",
                &json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }),
            )
        });
        wait_for_path(&pulling, "the write's pull")?;
        let before = reads(&cloud.storage)?;
        let advancing = scope.spawn(|| {
            server.post_as(
                "ada@example.com",
                "/v1/marker/advance",
                &json!({ "window": window, "session": "s" }),
            )
        });
        // Give the advance time to reach the tree. An advance that checks the lease before it
        // waits reads the lease object now, while this copy still holds it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while reads(&cloud.storage)? == before && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let takeover = Lease::acquire_with(
            BUCKET,
            "server",
            "server@takeover",
            frozen(Utc::now() + Duration::seconds(121)),
        );
        std::fs::write(&resume, "")?;
        writing
            .join()
            .map_err(|_| anyhow!("the writing thread panicked"))??;
        let advanced = advancing
            .join()
            .map_err(|_| anyhow!("the advancing thread panicked"))??;
        Ok((advanced, takeover))
    })?;
    drop(server);
    let marker = tree
        .parent()
        .context("the fixture has a root")?
        .join("data/marker.json");
    let moved = marker.exists();
    remove_fixture(&tree)?;

    takeover?.check()?;
    assert_eq!(advanced.status, 503, "unexpected body: {}", advanced.body);
    assert!(!moved, "a copy that had lost the lease moved the marker");
    Ok(())
}

#[test]
fn a_stopped_server_frees_its_lease_at_once() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-server-stop")?;
    fetch_window(&tree)?;
    let carried = cloud.environment();
    let environment: Vec<(&str, &str)> = carried
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();

    let mut server = Server::start_with_env(&tree, &[], &environment)?;
    let put = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }),
    )?;
    // A Cloud Run rollout stops the old copy with SIGTERM. The copy that takes its place
    // must be able to write at once, not after the takeover window.
    server.terminate()?;

    let object = cloud.storage.object("server")?;
    let next = Lease::acquire(BUCKET, "server", "server@new-copy");
    remove_fixture(&tree)?;
    assert_eq!(put.status, 200, "unexpected body: {}", put.body);
    assert!(
        object.is_none(),
        "the stopped server kept its lease: {object:?}"
    );
    drop(next.context("the copy that replaces it waited for the takeover window")?);
    Ok(())
}

/// A renewal Cloud Storage refuses says so in one line. Once another copy takes the server's
/// lease over, the next renewal is answered `412` with Cloud Storage's JSON broken over lines,
/// and the line the renewal wrote quoted it as it came (round-5 review of d9cb9f7, N5-1): on
/// Cloud Run the takeover, the line an operator most needs, became a run of log entries, none
/// after the first in a form anyone matches.
#[test]
fn a_refused_renewal_says_so_in_one_line() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = fixture_clone("lease-renewal-one-line")?;
    fetch_window(&tree)?;
    let log = tree
        .parent()
        .context("the fixture has a root")?
        .join("serve.log");
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    // No follow pass runs during the test, so the renewal is the only thing that can log.
    let mut child = binary()
        .args([
            "--memory",
            tree_text,
            "serve",
            "--tailnet-listen",
            "127.0.0.1:0",
            "--pull-interval",
            "3600",
        ])
        .envs(support::tree_env(&tree)?)
        .envs(cloud.environment())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&log)?)
        .spawn()
        .context("starting ctx serve")?;
    let outcome = (|| -> Result<(u16, String)> {
        let stdout = child.stdout.take().context("ctx serve has no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let tailnet = loop {
            let line = lines
                .next()
                .context("ctx serve stopped before it listened")??;
            if let Some(address) = line.strip_prefix("tailnet-listen ") {
                break address.to_owned();
            }
        };
        // The first write takes the lease.
        let put = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?
            .post(format!("http://{tailnet}/v1/records/put"))
            .header("Tailscale-User-Login", "ada@example.com")
            .json(&json!({ "path": "decisions/cadence.md", "content": cited_record(), "session": "s" }))
            .send()?
            .status()
            .as_u16();
        // Another copy takes it over, as one may once two minutes pass with no renewal; the
        // server's next renewal, due within 30 seconds, is refused.
        let takeover = Lease::acquire_with(
            BUCKET,
            "server",
            "server@takeover",
            frozen(Utc::now() + Duration::seconds(121)),
        )?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !std::fs::read_to_string(&log)?.contains("renewing the server lease") {
            if std::time::Instant::now() > deadline {
                bail!("the server never said its renewal was refused");
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // The line is written in pieces; give the rest of it time to land.
        std::thread::sleep(std::time::Duration::from_secs(2));
        takeover.check()?;
        Ok((put, std::fs::read_to_string(&log)?))
    })();
    let _ = child.kill();
    let _ = child.wait();
    remove_fixture(&tree)?;

    let (put, logged) = outcome?;
    assert_eq!(put, 200, "the write that takes the lease failed: {logged}");
    assert!(
        logged.lines().any(|line| {
            line.starts_with("renewing the server lease: ") && line.contains("taken over")
        }),
        "the refused renewal does not say the lease was taken over: {logged}"
    );
    let stray: Vec<&str> = logged
        .lines()
        .filter(|line| !line.starts_with("renewing the server lease: "))
        .collect();
    assert!(
        stray.is_empty(),
        "a refused renewal broke its line: {stray:?}\n{logged}"
    );
    Ok(())
}
