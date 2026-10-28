//! The leases the `ctx` command takes on the share: the fetch, `delta --write` and a local
//! `marker advance`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{Duration, Utc};
use ctx_memory::lease::Lease;
use serde_json::{json, Value};

use crate::fixtures::{frozen, Cloud, BUCKET};
use crate::support;
use crate::support::fake_embed;
use crate::support::{binary, remove_fixture, scratch};

/// A tree whose only source is the echo fixture, with its own state and data directories.
fn source_tree(label: &str) -> Result<PathBuf> {
    let root = scratch(label)?;
    let tree = root.join("tree");
    support::write_source_tree(&tree)?;
    Ok(tree)
}

fn run_fetch(tree: &Path, environment: &[(String, String)]) -> Result<Output> {
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = format!(
        "{}:{}",
        fixtures.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    binary()
        .args([
            "--memory",
            tree_text,
            "fetch",
            "--window-end",
            "2026-01-01T00:00:00Z",
        ])
        .envs(support::tree_env(tree)?)
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .env("PATH", path)
        .output()
        .context("running ctx fetch")
}

/// Every window directory the fetch left behind, staged or complete.
fn windows(tree: &Path) -> Result<Vec<String>> {
    let root = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?
        .join("data")
        .join("windows");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

#[test]
fn a_fetch_whose_lease_is_held_elsewhere_writes_nothing() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-fetch-refused")?;
    let holder = Lease::acquire(BUCKET, "fetch", "fetch@another-machine")?;

    let output = run_fetch(&tree, &cloud.environment())?;

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let left = windows(&tree)?;
    holder.check()?;
    remove_fixture(&tree)?;
    assert!(
        !output.status.success(),
        "the fetch ran while another copy held the lease: {stderr}"
    );
    assert!(
        stderr.contains("fetch@another-machine"),
        "the fetch does not say who holds the lease: {stderr}"
    );
    assert!(left.is_empty(), "the refused fetch wrote windows: {left:?}");
    Ok(())
}

#[test]
fn a_fetch_holds_the_lease_and_releases_it() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-fetch-holds")?;

    let output = run_fetch(&tree, &cloud.environment())?;

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let left = windows(&tree)?;
    let writes = cloud.storage.writes()?;
    let deletes = cloud.storage.deletes()?;
    let object = cloud.storage.object("fetch")?;
    remove_fixture(&tree)?;
    assert!(output.status.success(), "the fetch failed: {stderr}");
    assert_eq!(left.len(), 1, "the fetch wrote no window: {left:?}");
    assert_eq!(
        writes
            .first()
            .and_then(|write| write.parameter("ifGenerationMatch")),
        Some("0"),
        "the fetch did not create the lease object"
    );
    assert_eq!(deletes.len(), 1, "the fetch did not release its lease");
    assert!(
        object.is_none(),
        "the lease object outlived the fetch: {object:?}"
    );
    Ok(())
}

/// Cloud Scheduler can start the fetch job while its last execution still runs, and the two
/// executions share the data directory but no `flock`: the lease is what keeps one of them the
/// only writer. A fetch that runs as a Cloud Run job with no lease bucket would take no lease
/// and write beside the other, so it does not start. A fetch on a dev box with no bucket stays
/// single by its `flock`, and runs.
#[test]
fn a_cloud_run_fetch_job_without_a_lease_bucket_does_not_start() -> Result<()> {
    let tree = source_tree("lease-fetch-cloud-run")?;
    let no_bucket = ("CTX_LEASE_BUCKET".to_owned(), String::new());
    let job = ("CLOUD_RUN_JOB".to_owned(), "ctx-fetch".to_owned());

    let on_cloud_run = run_fetch(&tree, &[no_bucket.clone(), job])?;
    let left_on_cloud_run = windows(&tree)?;
    let on_a_dev_box = run_fetch(&tree, &[no_bucket])?;
    let left_on_a_dev_box = windows(&tree)?;
    remove_fixture(&tree)?;

    let refused = String::from_utf8_lossy(&on_cloud_run.stderr).into_owned();
    assert!(
        !on_cloud_run.status.success(),
        "a Cloud Run fetch job ran with no write lease: {refused}"
    );
    assert!(
        refused.contains("CTX_LEASE_BUCKET"),
        "the refusal does not name what is missing: {refused}"
    );
    assert!(
        left_on_cloud_run.is_empty(),
        "the refused fetch wrote windows: {left_on_cloud_run:?}"
    );
    assert!(
        on_a_dev_box.status.success(),
        "a dev-box fetch with no lease bucket failed: {}",
        String::from_utf8_lossy(&on_a_dev_box.stderr)
    );
    assert_eq!(
        left_on_a_dev_box.len(),
        1,
        "the dev-box fetch wrote no window: {left_on_a_dev_box:?}"
    );
    Ok(())
}

/// Runs one local `ctx` command against the tree, in the environment `environment` adds.
fn run_local(tree: &Path, arguments: &[&str], environment: &[(String, String)]) -> Result<Output> {
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    binary()
        .args(["--memory", tree_text])
        .args(arguments)
        .envs(support::tree_env(tree)?)
        .env_remove("CTX_SERVER")
        .env_remove("CTX_EMBED_MODEL")
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .output()
        .with_context(|| format!("running ctx {}", arguments.join(" ")))
}

/// The one completed window a fetch left behind.
fn completed_window(tree: &Path) -> Result<PathBuf> {
    let name = windows(tree)?
        .into_iter()
        .find(|name| !name.starts_with('.'))
        .ok_or_else(|| anyhow!("the fetch left no completed window"))?;
    let parent = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?;
    Ok(parent.join("data").join("windows").join(name))
}

/// The environment of a copy that takes no lease, which is how the window these tests start
/// from is fetched.
fn no_bucket() -> (String, String) {
    ("CTX_LEASE_BUCKET".to_owned(), String::new())
}

/// The server moves the marker on the share under its lease. A `ctx marker advance` run on a
/// machine with no server writes the same file, so it takes the same lease: while another copy
/// holds it the marker does not move, and once it is free the command takes it, moves the
/// marker and gives it back.
#[test]
fn a_local_marker_advance_takes_the_server_lease() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-marker-local")?;
    let fetched = run_fetch(&tree, &[no_bucket()])?;
    let marker = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?
        .join("data")
        .join("marker.json");
    let holder = Lease::acquire(BUCKET, "server", "server@cloud-run")?;

    let refused = run_local(
        &tree,
        &["marker", "advance", "newest"],
        &cloud.environment(),
    )?;
    let moved_while_held = marker.exists();
    holder.check()?;
    holder.release()?;
    let writes_before = cloud.storage.writes()?.len();
    let advanced = run_local(
        &tree,
        &["marker", "advance", "newest"],
        &cloud.environment(),
    )?;
    let moved = marker.exists();
    let writes = cloud.storage.writes()?;
    let object = cloud.storage.object("server")?;
    remove_fixture(&tree)?;

    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let why = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert_eq!(refused.status.code(), Some(2), "{why}");
    assert!(
        why.contains("server@cloud-run") && why.contains("server lease"),
        "the refusal does not say which lease is held, or by whom: {why}"
    );
    assert!(
        !moved_while_held,
        "the marker moved while another copy held the server lease"
    );
    assert!(
        advanced.status.success(),
        "{}",
        String::from_utf8_lossy(&advanced.stderr)
    );
    assert!(moved, "the marker did not move once the lease was free");
    assert_eq!(
        writes
            .get(writes_before)
            .and_then(|write| write.parameter("ifGenerationMatch")),
        Some("0"),
        "the advance did not take the server lease"
    );
    assert!(
        object.is_none(),
        "the advance kept the server lease: {object:?}"
    );
    Ok(())
}

/// What Cloud Storage answers while it cannot take a request, which it sends as JSON broken
/// over lines.
const STORAGE_DOWN: &str = "We encountered an internal error. Please try again.";

/// The lines of `output`'s standard error, each checked to be one whole line of the forms
/// given: a line that starts with none of them is the rest of a line that was broken.
fn stray_lines(output: &Output, forms: &[&str]) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !forms.iter().any(|form| line.starts_with(form)))
        .map(str::to_owned)
        .collect()
}

/// A lease that cannot be given back says so in one line. Cloud Storage answers an error with
/// JSON broken over lines, and the line saying so quoted it as it came, so on Cloud Run one
/// refused release became a run of log entries, only the first in a form anyone matches
/// (round-5 review of d9cb9f7, N5-1). The fetch gives its lease back at its end, and a local
/// command after its one write.
#[test]
fn a_lease_that_cannot_be_given_back_says_so_in_one_line() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-release-one-line")?;
    cloud.storage.refuse("DELETE", 503, STORAGE_DOWN)?;

    let fetched = run_fetch(&tree, &cloud.environment())?;
    let advanced = run_local(
        &tree,
        &["marker", "advance", "newest"],
        &cloud.environment(),
    )?;
    remove_fixture(&tree)?;

    for (output, form) in [
        (&fetched, "releasing the fetch lease: "),
        (&advanced, "releasing the lease: "),
    ] {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr
                .lines()
                .any(|line| line.starts_with(form) && line.contains(STORAGE_DOWN)),
            "no `{form}` line carries Cloud Storage's reason: {stderr}"
        );
        assert_eq!(
            stray_lines(output, &[form]),
            Vec::<String>::new(),
            "a line was broken: {stderr}"
        );
    }
    Ok(())
}

/// A lease dropped without being given back, as when the fetch stops before it starts, frees
/// itself as it goes, and says so in one line when Cloud Storage refuses. So does the error
/// that stopped the fetch: the fetch job runs on Cloud Run too.
#[test]
fn a_dropped_lease_that_cannot_be_given_back_says_so_in_one_line() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-drop-one-line")?;
    cloud.storage.refuse("DELETE", 503, STORAGE_DOWN)?;

    let stopped = run_local(
        &tree,
        &["fetch", "--source", "no-such-source"],
        &cloud.environment(),
    )?;
    let deletes = cloud.storage.deletes()?;
    remove_fixture(&tree)?;

    let stderr = String::from_utf8_lossy(&stopped.stderr).into_owned();
    assert_eq!(stopped.status.code(), Some(2), "{stderr}");
    assert_eq!(deletes.len(), 1, "the dropped lease was never given back");
    assert!(
        stderr.lines().any(|line| {
            line.starts_with("releasing the fetch lease: ") && line.contains(STORAGE_DOWN)
        }),
        "the refused release is not said, or not with Cloud Storage's reason: {stderr}"
    );
    assert_eq!(
        stray_lines(
            &stopped,
            &[
                "releasing the fetch lease: ",
                "source no-such-source is not configured"
            ]
        ),
        Vec::<String>::new(),
        "a line was broken: {stderr}"
    );
    Ok(())
}

/// `ctx delta --write` replaces a window's delta files, which the fetch writes under its lease
/// on the share. While another copy holds the fetch lease it writes nothing; once the lease is
/// free it takes it, writes, and gives it back.
#[test]
fn a_local_delta_write_takes_the_fetch_lease() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-delta-local")?;
    let fetched = run_fetch(&tree, &[no_bucket()])?;
    let window = completed_window(&tree)?;
    let delta = window.join("delta.json");
    std::fs::remove_file(&delta).with_context(|| format!("removing {}", delta.display()))?;
    let holder = Lease::acquire(BUCKET, "fetch", "fetch@cloud-run")?;

    let refused = run_local(&tree, &["delta", "--write"], &cloud.environment())?;
    let written_while_held = delta.exists();
    holder.check()?;
    holder.release()?;
    let writes_before = cloud.storage.writes()?.len();
    let written = run_local(&tree, &["delta", "--write"], &cloud.environment())?;
    let written_after = delta.exists();
    let writes = cloud.storage.writes()?;
    let object = cloud.storage.object("fetch")?;
    remove_fixture(&tree)?;

    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let why = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert_eq!(refused.status.code(), Some(2), "{why}");
    assert!(
        why.contains("fetch@cloud-run") && why.contains("fetch lease"),
        "the refusal does not say which lease is held, or by whom: {why}"
    );
    assert!(
        !written_while_held,
        "delta --write wrote while another copy held the fetch lease"
    );
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(
        written_after,
        "delta --write wrote nothing once the lease was free"
    );
    assert_eq!(
        writes
            .get(writes_before)
            .and_then(|write| write.parameter("ifGenerationMatch")),
        Some("0"),
        "delta --write did not take the fetch lease"
    );
    assert!(
        object.is_none(),
        "delta --write kept the fetch lease: {object:?}"
    );
    Ok(())
}

/// A Vertex AI stand-in that answers the first embedding request only after taking the fetch
/// lease over, the way a second copy does once the first has gone two minutes without a
/// renewal. A fetch that asks it for an embedding while it computes its delta therefore loses
/// its lease in the middle of the compute, which is where a long Vertex call leaves it.
///
/// The thread returns the lease it took. Every later request is refused, which the delta takes
/// as meaning search being unavailable.
fn vertex_that_takes_the_lease() -> Result<(SocketAddr, JoinHandle<Result<Lease>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").context("binding the Vertex stand-in")?;
    let address = listener.local_addr()?;
    let answering = std::thread::spawn(move || -> Result<Lease> {
        let (mut stream, _) = listener
            .accept()
            .context("accepting the embedding request")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        if !request_line.starts_with("POST ") {
            bail!("the fetch asked for no embedding");
        }
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                break;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value
                        .trim()
                        .parse()
                        .context("reading the request's length")?;
                }
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body)?;
        let request: Value =
            serde_json::from_slice(&body).context("decoding the embedding request")?;
        let takeover = Lease::acquire_with(
            BUCKET,
            "fetch",
            "fetch@takeover",
            frozen(Utc::now() + Duration::seconds(121)),
        )?;
        let predictions = request
            .get("instances")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|instance| {
                let text = instance
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({ "embeddings": { "values": fake_embed::vector(text) } })
            })
            .collect::<Vec<_>>();
        let answer = json!({ "predictions": predictions }).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        )?;
        stream.flush()?;
        Ok(takeover)
    });
    Ok((address, answering))
}

#[test]
fn a_fetch_that_loses_its_lease_during_the_delta_publishes_no_window() -> Result<()> {
    let cloud = Cloud::start()?;
    let tree = source_tree("lease-fetch-lost")?;
    let (vertex, answering) = vertex_that_takes_the_lease()?;
    let mut environment = cloud.environment();
    environment.extend(
        [
            ("CTX_EMBED_MODEL", "text-embedding-005"),
            ("GOOGLE_CLOUD_PROJECT", "example-project"),
            ("GOOGLE_CLOUD_LOCATION", "us-west1"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
    environment.push(("CTX_VERTEX_API_BASE".to_owned(), format!("http://{vertex}")));

    let output = run_fetch(&tree, &environment)?;

    if !answering.is_finished() {
        // The fetch never asked for an embedding: wake the stand-in so that it says so.
        drop(TcpStream::connect(vertex));
    }
    let takeover = answering
        .join()
        .map_err(|_| anyhow!("the Vertex stand-in panicked"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let published = windows(&tree)?
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .collect::<Vec<_>>();
    remove_fixture(&tree)?;
    takeover?.check()?;
    assert!(
        !output.status.success(),
        "the fetch finished after losing its lease: {stderr}"
    );
    assert!(
        stderr.contains("taken over"),
        "the fetch does not say it lost the lease: {stderr}"
    );
    assert!(
        published.is_empty(),
        "a fetch that lost its lease published a window: {published:?}"
    );
    Ok(())
}
