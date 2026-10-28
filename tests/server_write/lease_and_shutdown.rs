//! The write lease while a server stops.

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, install_hook, put_body, within};
use crate::support;
use crate::support::{
    fetch_window, fixture_clone, git, remote_of, remove_fixture, wait_for_path, Server,
};

/// Sends a put as `user` and keeps the connection, so the caller can go away mid-write.
fn put_without_waiting(address: SocketAddr, user: &str, body: &Value) -> Result<TcpStream> {
    let body = body.to_string();
    let mut stream = TcpStream::connect(address)?;
    write!(
        stream,
        "POST /v1/records/put HTTP/1.1\r\nHost: {address}\r\nTailscale-User-Login: {user}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(stream)
}

/// A Cloud Run rollout stops the old copy with SIGTERM, and the copy frees its write lease so
/// the new one writes at once. A write whose caller hung up runs on after the request is gone,
/// and the copy freed the lease while that write was still pushing: for as long as the push
/// took, two copies could publish. The lease is freed only once the write has finished.
#[test]
fn a_stopping_server_frees_its_lease_only_after_a_write_its_caller_left() -> Result<()> {
    let tree = fixture_clone("stop-detached-write")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let metadata = support::fake_http::FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.server", "expires_in": 3599 }).to_string(),
    )?;
    let storage = support::fake_gcs::FakeGcs::start()?;
    let bare = remote_of(&tree)?;
    let pushing = root.join("pushing");
    let resume = root.join("resume");
    install_hook(
        &bare,
        "pre-receive",
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\n",
            pushing.display(),
            resume.display()
        ),
    )?;
    let metadata_host = metadata.address().to_string();
    let storage_url = storage.url();
    let mut server = Server::start_with_env(
        &tree,
        &["--pull-interval", "3600"],
        &[
            ("GCE_METADATA_HOST", &metadata_host),
            ("CTX_STORAGE_URL", &storage_url),
            ("CTX_LEASE_BUCKET", "ctx-leases"),
        ],
    )?;
    let address = server.tailnet;
    let caller = put_without_waiting(
        address,
        "ada@example.com",
        &put_body("decisions/left.md", &good_record(), "session-left"),
    )?;
    wait_for_path(&pushing, "the write's push")?;
    // The caller hangs up while the write is pushing, and the copy is told to stop.
    drop(caller);
    let stopping = std::thread::spawn(move || server.terminate());
    // Once the port refuses connections, the copy has taken the signal and closed every
    // connection: what is left is the write and the lease.
    let closed = within(60, || {
        Ok(TcpStream::connect(address).is_err().then_some(()))
    });
    std::thread::sleep(std::time::Duration::from_secs(1));
    let freed_while_pushing = storage.deletes()?.len();
    std::fs::write(&resume, "")?;
    let stopped = stopping
        .join()
        .map_err(|_| anyhow::anyhow!("the stopping thread panicked"))?;
    let freed = storage.deletes()?.len();
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    remove_fixture(&tree)?;

    closed.context("the stopping copy never closed its port")?;
    stopped?;
    assert_eq!(
        freed_while_pushing, 0,
        "the copy freed its lease while a write was still pushing"
    );
    assert!(
        listing.lines().any(|name| name == "decisions/left.md"),
        "the write its caller left did not finish: {listing}"
    );
    assert_eq!(freed, 1, "the stopped copy did not free its lease");
    Ok(())
}
