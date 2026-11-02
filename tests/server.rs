//! `ctx serve` answers read commands over HTTP with the output the local command prints.

mod support;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use support::{ctx_local, fixture_tree, remove_fixture, Server};

#[test]
fn remote_search_matches_local() -> Result<()> {
    let tree = fixture_tree("server-search")?;
    let local = ctx_local(&tree, &["search", "quarterly"])?;
    let server = Server::start(&tree, &[])?;
    let reply = server.command(&["search", "quarterly"])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.text("stdout")?, local.stdout);
    assert_eq!(reply.text("stderr")?, local.stderr);
    assert_eq!(
        reply.field("exit_code")?,
        &json!(i64::from(local.exit_code))
    );
    assert!(
        local.stdout.contains("Renewal terms"),
        "local search found nothing: {}",
        local.stdout
    );
    Ok(())
}

/// A remote caller that asks for help reads what a local one reads: the text on stdout and
/// exit 0. Help is clap's answer, not an error, and the server used to report it as one.
#[test]
fn remote_help_answers_as_local_help_does() -> Result<()> {
    let tree = fixture_tree("server-help")?;
    let local = ctx_local(&tree, &["search", "--help"])?;
    let server = Server::start(&tree, &[])?;
    let reply = server.command(&["search", "--help"])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(local.exit_code, 0, "local help failed: {}", local.stderr);
    assert!(local.stdout.contains("Usage"), "{}", local.stdout);
    assert_eq!(reply.status, 200, "unexpected body: {}", reply.body);
    assert_eq!(
        reply.field("exit_code")?,
        &json!(0),
        "unexpected body: {}",
        reply.body
    );
    assert_eq!(reply.text("stdout")?, local.stdout);
    assert_eq!(reply.text("stderr")?, "");
    Ok(())
}

#[test]
fn command_outside_allowlist_is_400() -> Result<()> {
    let tree = fixture_tree("server-allowlist")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.command(&["fetch"])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 400);
    assert!(
        reply.text("error")?.contains("fetch"),
        "unexpected body: {}",
        reply.body
    );
    Ok(())
}

#[test]
fn healthz_reports_the_loaded_tree() -> Result<()> {
    let tree = fixture_tree("server-health")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.get(server.tailnet, "/healthz", &[])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.field("ok")?, &json!(true));
    assert_eq!(reply.field("windows")?, &json!(0));
    Ok(())
}

const AUDIENCE: &str = "https://ctx-memory.example.run.app";

/// Who the test server answers: the people of the company and its agent service accounts.
const PRINCIPALS: &str = "@example.com,@example-project.iam.gserviceaccount.com";

/// The lease environment a Cloud Run copy carries: a bucket, and a fake Cloud Storage and
/// metadata server for it, served on `keys` beside the Google keys.
fn lease_env(keys: &support::fake_http::FakeHttp) -> Result<Vec<(String, String)>> {
    keys.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.server", "expires_in": 3599 }).to_string(),
    )?;
    let storage = support::fake_gcs::FakeGcs::start()?;
    Ok(vec![
        ("GCE_METADATA_HOST".to_owned(), keys.address().to_string()),
        ("CTX_STORAGE_URL".to_owned(), storage.url()),
        ("CTX_LEASE_BUCKET".to_owned(), "ctx-leases".to_owned()),
    ])
}

/// Starts a server whose ingress listener trusts the fixture key set, with the write lease
/// every Cloud Run copy holds.
fn ingress_server(tree: &std::path::Path, keys: &support::fake_http::FakeHttp) -> Result<Server> {
    keys.json("/certs", 200, support::JWKS)?;
    let certs = keys.url("/certs");
    let lease = lease_env(keys)?;
    Server::start_with_env(
        tree,
        &[
            "--listen",
            "127.0.0.1:0",
            "--audience",
            AUDIENCE,
            "--allow-principal",
            PRINCIPALS,
            "--google-keys",
            &certs,
        ],
        &borrowed(&lease),
    )
}

/// A token Google signed for this server's audience says which server the caller meant, not
/// who the caller is: any Google account in any project can mint one. The server answers the
/// principals its deployment names and nobody else, and that check is its own — Cloud Run's
/// invoker policy is declared in another repository.
#[test]
fn ingress_refuses_a_principal_outside_the_allow_list() -> Result<()> {
    let tree = fixture_tree("server-ingress-principal")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let token = support::google_token(AUDIENCE, "someone@example.net")?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[("Authorization", &format!("Bearer {token}"))],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401, "unexpected body: {}", reply.body);
    assert!(
        reply.text("error")?.contains("someone@example.net"),
        "unexpected body: {}",
        reply.body
    );
    Ok(())
}

/// Each request at either door is one JSON line on the server's standard output, written when
/// its answer is ready, so who reads the graph, and how, can be counted: when, which door, who
/// called, which command, the record a `record show` names, how it ended, the status, how long
/// it took and how many bytes the answer held. A search is logged by the length of its query,
/// never by its words, and no line carries what a record says or the caller's token.
#[test]
fn each_request_at_either_door_is_one_line_of_the_request_log() -> Result<()> {
    let tree = fixture_tree("server-request-log")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let mut server = ingress_server(&tree, &keys)?;
    let ingress = server.ingress_address()?;
    let agent = "cycle@example-project.iam.gserviceaccount.com";
    let token = support::google_token(AUDIENCE, agent)?;
    let bearer = format!("Bearer {token}");
    let query = "quarterly zeppelin-7f3a";
    let start = chrono::Utc::now();
    let search = server.command(&["search", query])?;
    let show = server.command(&["record", "show", "decisions/renewal.md"])?;
    let open = server.post(
        ingress,
        "/v1/command",
        &json!({ "argv": ["--json", "open"] }),
        &[("Authorization", &bearer)],
    )?;
    let anonymous = server.post(
        ingress,
        "/v1/command",
        &json!({ "argv": ["search", query] }),
        &[],
    )?;
    let health = server.get(server.tailnet, "/healthz", &[])?;
    let printed = server.stop_and_read_stdout()?;
    let window = (start, chrono::Utc::now());
    drop(server);
    remove_fixture(&tree)?;

    let statuses = [
        search.status,
        show.status,
        open.status,
        anonymous.status,
        health.status,
    ];
    assert_eq!(statuses, [200, 200, 200, 401, 200], "unexpected answers");
    let text = printed.join("\n");
    for secret in [
        "zeppelin",
        "The vendor renewal keeps",
        token.as_str(),
        "Bearer",
    ] {
        assert!(
            !text.contains(secret),
            "the request log holds {secret:?}: {text}"
        );
    }
    let lines = request_lines(&printed)?;
    let [searched, shown, opened, refused, probed] = lines.as_slice() else {
        anyhow::bail!("not one line per request: {text}");
    };
    let expected = [
        (
            searched,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": "ada@example.com",
                "command": "search", "query_chars": query.chars().count(), "result": "ok",
                "status": 200, "response_bytes": search.bytes,
            }),
        ),
        (
            shown,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": "ada@example.com",
                "command": "record show", "path": "decisions/renewal.md", "result": "ok",
                "status": 200, "response_bytes": show.bytes,
            }),
        ),
        (
            opened,
            json!({
                "severity": "INFO", "door": "identity-token", "caller": agent,
                "command": "open", "result": "ok", "status": 200, "response_bytes": open.bytes,
            }),
        ),
        (
            refused,
            json!({
                "severity": "WARNING", "door": "identity-token", "caller": null,
                "command": null, "result": "unidentified", "status": 401,
                "response_bytes": anonymous.bytes,
            }),
        ),
        (
            probed,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": null, "command": "healthz",
                "result": "ok", "status": 200, "response_bytes": health.bytes,
            }),
        ),
    ];
    for (line, expected) in expected {
        assert_logged(line, &expected, window)?;
    }
    Ok(())
}

/// Every answer is one line of the request log, whoever gave it: a command clap refused, a body
/// that is not a request, and the answers axum gives before any handler runs, for a method a
/// route does not take, a body over its limit and a path no route has. A dossier is logged by
/// the length of the entity it looks up, as a search is by its query's, and an answer to HEAD
/// carries no body.
#[test]
fn every_answer_is_one_line_whoever_gave_it() -> Result<()> {
    use reqwest::Method;
    let tree = fixture_tree("server-request-log-answers")?;
    let mut server = Server::start(&tree, &[])?;
    let tailnet = server.tailnet;
    let ada = [("Tailscale-User-Login", "ada@example.com")];
    let entity = "Zebra Holdings";
    let start = chrono::Utc::now();
    let dossier = server.command(&["dossier", entity])?;
    let rejected = server.command(&["record", "show", "decisions/renewal.md", "--bogus"])?;
    let unreadable = server.post_as("ada@example.com", "/v1/command", &json!({ "argv": 5 }))?;
    let wrong_method = server.raw(Method::GET, tailnet, "/v1/records/put", None, &ada)?;
    let nowhere = server.raw(Method::GET, tailnet, "/nowhere", None, &[])?;
    let probe = server.raw(Method::HEAD, tailnet, "/healthz", None, &[])?;
    // Last, and on a connection of its own: axum may close it before the whole body is sent,
    // and a client that reused it would fail the next request. The line is what is tested.
    let too_large = reqwest::blocking::Client::new()
        .post(format!("http://{tailnet}/v1/records/put"))
        .header("Tailscale-User-Login", "ada@example.com")
        .body(vec![b'a'; 3 << 20])
        .send()
        .ok()
        .and_then(|reply| {
            let status = reply.status().as_u16();
            reply.bytes().ok().map(|body| (status, body.len()))
        });
    let printed = server.stop_and_read_stdout()?;
    let window = (start, chrono::Utc::now());
    drop(server);
    remove_fixture(&tree)?;

    let text = printed.join("\n");
    for secret in [entity, "Zebra", "renewal", "bogus"] {
        assert!(
            !text.contains(secret),
            "the request log holds {secret:?}: {text}"
        );
    }
    let lines = request_lines(&printed)?;
    let [dossiered, refused, unread, wrong, lost, probed, large] = lines.as_slice() else {
        anyhow::bail!("not one line per request: {text}");
    };
    let exit = |reply: &support::Reply| -> Result<String> {
        let code = reply.field("exit_code")?.as_u64().context("no exit code")?;
        Ok(if code == 0 {
            "ok".to_owned()
        } else {
            format!("exit-{code}")
        })
    };
    let ada = "ada@example.com";
    let expected = [
        (
            dossiered,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": ada, "command": "dossier",
                "query_chars": entity.chars().count(), "result": exit(&dossier)?, "status": 200,
                "response_bytes": dossier.bytes,
            }),
        ),
        (
            refused,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": ada, "command": "record show",
                "result": exit(&rejected)?, "status": 200, "response_bytes": rejected.bytes,
            }),
        ),
        (
            unread,
            json!({
                "severity": "WARNING", "door": "tailnet", "caller": ada, "command": null,
                "result": "bad-request", "status": 400, "response_bytes": unreadable.bytes,
            }),
        ),
        (
            wrong,
            json!({
                "severity": "WARNING", "door": "tailnet", "caller": null, "command": "record put",
                "result": "method-not-allowed", "status": 405, "response_bytes": wrong_method.1,
            }),
        ),
        (
            lost,
            json!({
                "severity": "WARNING", "door": "tailnet", "caller": null, "command": null,
                "result": "not-found", "status": 404, "response_bytes": nowhere.1,
            }),
        ),
        (
            probed,
            json!({
                "severity": "INFO", "door": "tailnet", "caller": null, "command": "healthz",
                "result": "ok", "status": 200, "response_bytes": 0,
            }),
        ),
    ];
    assert_eq!(exit(&rejected)?, "exit-2", "clap did not refuse the argv");
    assert_eq!([wrong_method.0, nowhere.0, probe.0], [405, 404, 200]);
    assert_eq!(probe.1, 0, "an answer to HEAD carried a body");
    for (line, expected) in expected {
        assert_logged(line, &expected, window)?;
    }
    assert_too_large(large, too_large);
    Ok(())
}

/// Asserts the line of a body over axum's limit, and when its caller read the answer before
/// the connection closed, that the line counts that answer's bytes.
fn assert_too_large(line: &Value, answer: Option<(u16, usize)>) {
    for (field, value) in [
        ("status", json!(413)),
        ("result", json!("too-large")),
        ("command", json!("record put")),
        ("caller", Value::Null),
    ] {
        assert_eq!(line.get(field), Some(&value), "{line}");
    }
    if let Some((status, bytes)) = answer {
        assert_eq!(status, 413, "{line}");
        assert_eq!(line.get("response_bytes"), Some(&json!(bytes)), "{line}");
    }
}

/// A log collector that stops reading costs lines of the request log, never answers: with the
/// server's standard output full, every request is still answered, and standard error, which
/// another collector reads, says once that lines are dropped.
#[test]
fn a_stalled_request_log_costs_lines_not_answers() -> Result<()> {
    let stalled = stalled_server("server-request-log-stalled", Pipes::Apart)?;
    stalled.assert_every_request_answered();
    assert_eq!(
        stalled
            .stderr
            .lines()
            .filter(|line| line.starts_with("the request log is behind standard output"))
            .count(),
        1,
        "{}",
        stalled.stderr
    );
    Ok(())
}

/// Standard error can be the same full pipe as standard output, as when a runtime collects
/// both streams together and stops reading: saying that lines are dropped must not stop a
/// request either.
#[test]
fn a_stalled_pipe_both_streams_share_costs_no_answers() -> Result<()> {
    stalled_server("server-request-log-stalled-shared", Pipes::Shared)?
        .assert_every_request_answered();
    Ok(())
}

/// More healthz lines than a pipe shrunk to one page and the request log's queue hold together.
const STALLED_REQUESTS: usize = 1500;

/// Where a stalled server's two streams go.
#[derive(Clone, Copy)]
enum Pipes {
    /// Standard output and standard error each to a pipe of its own, neither read.
    Apart,
    /// Both to one pipe, not read.
    Shared,
}

/// What a server whose standard output nobody read did with [`STALLED_REQUESTS`] requests.
struct Stalled {
    /// How many requests in a row it answered with success before the first that it did not.
    answered: usize,
    /// The first request it did not answer, and why.
    failure: Option<String>,
    /// Whether it stopped cleanly on SIGTERM within a minute.
    stopped: Option<std::process::ExitStatus>,
    /// Its standard error, read once it had stopped (empty when it shares standard output).
    stderr: String,
}

impl Stalled {
    fn assert_every_request_answered(&self) {
        assert_eq!(
            (self.answered, self.failure.as_deref()),
            (STALLED_REQUESTS, None),
            "{}",
            self.stderr
        );
        assert!(
            self.stopped.is_some_and(|status| status.success()),
            "ctx serve did not stop cleanly ({:?}): {}",
            self.stopped,
            self.stderr
        );
    }
}

/// Starts a server whose output pipes nobody reads past its listener line, each shrunk to one
/// page, asks it [`STALLED_REQUESTS`] health checks, stopping at the first it does not answer,
/// and stops it.
fn stalled_server(label: &str, pipes: Pipes) -> Result<Stalled> {
    use std::io::{BufRead as _, Read as _};
    let tree = fixture_tree(label)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let (stdout, stdout_writer) = std::io::pipe()?;
    let (stderr, stderr_writer) = match pipes {
        Pipes::Apart => {
            let (reader, writer) = std::io::pipe()?;
            (Some(reader), writer)
        }
        Pipes::Shared => (None, stdout_writer.try_clone()?),
    };
    #[cfg(target_os = "linux")]
    for reader in std::iter::once(&stdout).chain(stderr.as_ref()) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is this process's own open pipe; F_SETPIPE_SZ only resizes it.
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
    }
    let mut command = support::binary();
    command
        .args(["--memory", tree_text, "serve", "--tailnet-listen"])
        .args(["127.0.0.1:0", "--pull-interval", "3600"])
        .envs(support::tree_env(&tree)?)
        .stdout(stdout_writer)
        .stderr(stderr_writer);
    let mut child = command.spawn().context("starting ctx serve")?;
    // The writing ends this process held went to the child; `command` holds them until dropped.
    drop(command);
    let mut stdout = std::io::BufReader::new(stdout);
    // A shared pipe may carry a line of standard error before the listener line.
    let mut address = None;
    let mut printed = String::new();
    for _ in 0..10 {
        let mut line = String::new();
        if stdout.read_line(&mut line)? == 0 {
            break;
        }
        if let Some(bound) = line.trim().strip_prefix("tailnet-listen ") {
            address = Some(bound.to_owned());
            break;
        }
        printed.push_str(&line);
    }
    // Nobody reads the pipes from here on, as a collector that stopped would not.
    let (answered, failure) = match address {
        Some(address) => ask_health(&address, STALLED_REQUESTS),
        None => (
            0,
            Some(format!("ctx serve printed no listener: {printed:?}")),
        ),
    };
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: the pid is this process's own child, which has not been waited for.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    // The server waits a moment for its last lines, then stops whatever the pipes hold.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let stopped = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut stderr_text = String::new();
    if let Some(mut stderr) = stderr {
        stderr.read_to_string(&mut stderr_text)?;
    }
    drop(stdout);
    remove_fixture(&tree)?;
    Ok(Stalled {
        answered,
        failure,
        stopped,
        stderr: stderr_text,
    })
}

/// Asks `address` for `/healthz` up to `count` times, and stops at the first answer that is not
/// a success: how many were, and what the first that was not did.
fn ask_health(address: &str, count: usize) -> (usize, Option<String>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => return (0, Some(error.to_string())),
    };
    for asked in 0..count {
        match client.get(format!("http://{address}/healthz")).send() {
            Ok(reply) if reply.status().is_success() => {}
            Ok(reply) => return (asked, Some(format!("request {asked}: {}", reply.status()))),
            Err(error) => return (asked, Some(format!("request {asked}: {error}"))),
        }
    }
    (count, None)
}

/// The request log, one parsed line per line printed.
fn request_lines(printed: &[String]) -> Result<Vec<Value>> {
    printed
        .iter()
        .map(|line| serde_json::from_str(line).with_context(|| format!("{line:?}")))
        .collect()
}

/// Asserts one request log line holds exactly `expected`, beside a time inside `window`, when
/// its request was made, and a duration.
fn assert_logged(
    line: &Value,
    expected: &Value,
    (start, end): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
) -> Result<()> {
    let mut fields = line.as_object().context("a line is not an object")?.clone();
    let time = fields.remove("time").context("a line has no time")?;
    let time = time.as_str().context("a line's time is not text")?;
    let time = chrono::DateTime::parse_from_rfc3339(time).with_context(|| format!("{time:?}"))?;
    // The line gives whole milliseconds.
    let earliest = start - chrono::TimeDelta::milliseconds(1);
    assert!(
        earliest <= time && time <= end,
        "{line} is outside {start}..{end}"
    );
    let duration = fields
        .remove("duration_ms")
        .context("a line has no duration")?;
    assert!(duration.is_u64(), "{line}");
    assert_eq!(&Value::Object(fields), expected, "{line}");
    Ok(())
}

/// A server with no allow list would answer every Google account there is, so it does not
/// start at all.
#[test]
fn ingress_without_an_allow_list_does_not_start() -> Result<()> {
    let tree = fixture_tree("server-ingress-no-list")?;
    let keys = support::fake_http::FakeHttp::start()?;
    keys.json("/certs", 200, support::JWKS)?;
    let certs = keys.url("/certs");
    // The lease is in place, so the allow list is the one thing missing.
    let lease = lease_env(&keys)?;
    let started = Server::start_with_env(
        &tree,
        &[
            "--listen",
            "127.0.0.1:0",
            "--audience",
            AUDIENCE,
            "--google-keys",
            &certs,
        ],
        &borrowed(&lease),
    );
    remove_fixture(&tree)?;

    assert!(
        started.is_err(),
        "ctx serve bound an ingress port with no allowed principal"
    );
    Ok(())
}

/// A Cloud Run rollout runs the old copy and the new one at once, and the write lease is what
/// keeps only one of them writing. A copy on Cloud Run with no lease bucket would take no lease
/// and write beside the other, so it does not start — whether Cloud Run reaches it through the
/// ingress port or the platform says where it runs.
#[test]
fn a_cloud_run_copy_without_a_lease_bucket_does_not_start() -> Result<()> {
    let tree = fixture_tree("server-no-lease")?;
    let keys = support::fake_http::FakeHttp::start()?;
    keys.json("/certs", 200, support::JWKS)?;
    let certs = keys.url("/certs");
    let ingress = Server::start_with_env(
        &tree,
        &[
            "--listen",
            "127.0.0.1:0",
            "--audience",
            AUDIENCE,
            "--allow-principal",
            PRINCIPALS,
            "--google-keys",
            &certs,
        ],
        &[("CTX_LEASE_BUCKET", "")],
    );
    let on_cloud_run = Server::start_with_env(
        &tree,
        &[],
        &[("CTX_LEASE_BUCKET", ""), ("K_SERVICE", "ctx-memory")],
    );
    remove_fixture(&tree)?;

    assert!(
        ingress.is_err(),
        "ctx serve bound an ingress port with no write lease"
    );
    assert!(
        on_cloud_run.is_err(),
        "ctx serve started on Cloud Run with no write lease"
    );
    Ok(())
}

/// `/v1/command` is the read door. `delta --write` replaces `delta.json`, `delta.md` and
/// `open-snapshot.json` in the newest completed window — files the fetch job owns — and the
/// caller holds no write lease, so the argv is refused before the tree is touched.
#[test]
fn delta_write_is_not_a_remote_command() -> Result<()> {
    let tree = support::fixture_clone("server-delta-write")?;
    let window = support::fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let directory = root.join("data/windows").join(&window);
    let before = std::fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect::<std::collections::BTreeSet<_>>();

    let server = Server::start(&tree, &[])?;
    let written = server.command(&["delta", "--write"])?;
    let read = server.command(&["delta"])?;
    drop(server);
    let after = std::fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect::<std::collections::BTreeSet<_>>();
    remove_fixture(&tree)?;

    assert_eq!(written.status, 400, "unexpected body: {}", written.body);
    assert!(
        written.text("error")?.contains("delta"),
        "unexpected body: {}",
        written.body
    );
    assert_eq!(before, after, "the refused command wrote window files");
    assert_eq!(
        read.status, 200,
        "reading the delta should still work: {}",
        read.body
    );
    Ok(())
}

#[test]
fn ingress_without_token_is_401() -> Result<()> {
    let tree = fixture_tree("server-ingress-anon")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401);
    Ok(())
}

#[test]
fn ingress_with_wrong_audience_is_401() -> Result<()> {
    let tree = fixture_tree("server-ingress-audience")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let token = support::google_token("https://another.example.run.app", "ada@example.com")?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[("Authorization", &format!("Bearer {token}"))],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401);
    Ok(())
}

#[test]
fn ingress_with_a_google_token_runs_the_command() -> Result<()> {
    let tree = fixture_tree("server-ingress-token")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let token = support::google_token(
        AUDIENCE,
        "ctx-agent@example-project.iam.gserviceaccount.com",
    )?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["search", "quarterly"] }),
        &[("Authorization", &format!("Bearer {token}"))],
    )?;
    let fetched = keys.hits("/certs")?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200);
    assert!(
        reply.text("stdout")?.contains("Renewal terms"),
        "unexpected output: {}",
        reply.body
    );
    assert_eq!(fetched, 1, "the key set should be fetched once and kept");
    Ok(())
}

#[test]
fn ingress_ignores_the_tailscale_header() -> Result<()> {
    let tree = fixture_tree("server-ingress-tailscale")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[("Tailscale-User-Login", "ada@example.com")],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401);
    Ok(())
}

#[test]
fn tailnet_listener_uses_tailscale_login() -> Result<()> {
    let tree = fixture_tree("server-tailnet-login")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        "/v1/command",
        &json!({ "argv": ["rules"] }),
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200);
    assert!(
        reply.text("stdout")?.contains("# Rules"),
        "unexpected output: {}",
        reply.body
    );
    Ok(())
}

#[test]
fn tailnet_listener_without_login_header_is_401() -> Result<()> {
    let tree = fixture_tree("server-tailnet-anon")?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post(
        server.tailnet,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401);
    Ok(())
}

/// A fake metadata server that hands out an identity token for this server's audience, as
/// Cloud Run's own does for a service running inside Google Cloud.
fn metadata_identity(email: &str) -> Result<support::fake_http::FakeHttp> {
    let metadata = support::fake_http::FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/identity",
        200,
        &support::google_token(AUDIENCE, email)?,
    )?;
    Ok(metadata)
}

/// The environment a caller inside Google Cloud carries.
///
/// The client asks for a token for the origin it sends to, so `CTX_SERVER_AUDIENCE` is the
/// test server's loopback origin. The fake metadata server answers every audience with a token
/// for the server's own `AUDIENCE`, which the server is started with before its port is known.
fn cloud_env(
    server: &Server,
    metadata: &support::fake_http::FakeHttp,
) -> Result<Vec<(String, String)>> {
    let origin = format!("http://{}", server.ingress_address()?);
    Ok(vec![
        ("CTX_SERVER".to_owned(), origin.clone()),
        ("CTX_SERVER_AUDIENCE".to_owned(), origin),
        (
            "GCE_METADATA_HOST".to_owned(),
            metadata.address().to_string(),
        ),
    ])
}

fn borrowed(environment: &[(String, String)]) -> Vec<(&str, &str)> {
    environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

#[test]
fn client_prints_remote_output_and_exit_code() -> Result<()> {
    let tree = fixture_tree("remote-output")?;
    let local = ctx_local(&tree, &["sweep"])?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ctx-agent@example-project.iam.gserviceaccount.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-output-cwd")?;
    let remote = support::ctx_remote(&elsewhere, &borrowed(&environment), &["sweep"], None)?;
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(
        local.exit_code, 1,
        "the fixture tree should have a structural finding: {}",
        local.stdout
    );
    assert_eq!(remote.exit_code, local.exit_code);
    assert_eq!(remote.stdout, local.stdout);
    Ok(())
}

#[test]
fn remote_mode_never_reads_a_local_tree() -> Result<()> {
    let tree = fixture_tree("remote-no-tree")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ctx-agent@example-project.iam.gserviceaccount.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-no-tree-cwd")?;
    let remote = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["search", "quarterly"],
        None,
    )?;
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(remote.exit_code, 0, "{}", remote.stderr);
    assert!(
        remote.stdout.contains("Renewal terms"),
        "unexpected output: {}",
        remote.stdout
    );
    Ok(())
}

/// The server runs `ctx open` for a remote caller, so `--waiting-on` and `--overdue` filter the
/// server's tree, each item carries `waiting_on` and `tracked_by`, and the caller reads what a
/// local run prints.
#[test]
fn remote_open_filters_by_waiting_on_and_overdue() -> Result<()> {
    let tree = fixture_tree("remote-open-waiting")?;
    std::fs::create_dir_all(tree.join("questions"))?;
    std::fs::write(
        tree.join("questions/budget.md"),
        "---\ntype: question\nstatus: open\nrevisit_by: 2000-01-01\n\
         waiting_on: [Ada Lovelace]\ntracked_by: [\"https://tracker.example.com/ask/1\"]\n\
         ---\nWho approves the budget?\n",
    )?;
    std::fs::write(
        tree.join("questions/venue.md"),
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-01\n\
         waiting_on: [Grace Hopper]\n---\nWhich venue?\n",
    )?;
    let both = [
        "--json",
        "open",
        "--waiting-on",
        "ada lovelace",
        "--overdue",
    ];
    let local = ctx_local(&tree, &both)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ctx-agent@example-project.iam.gserviceaccount.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-open-waiting-cwd")?;
    let run = |arguments: &[&str]| {
        support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, None)
    };
    let outcome = (|| {
        Ok::<_, anyhow::Error>((
            run(&["--json", "open", "--waiting-on", "ada lovelace"])?,
            run(&["--json", "open", "--waiting-on", "Grace Hopper"])?,
            run(&["--json", "open", "--overdue"])?,
            run(&both)?,
        ))
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let (ada, grace, overdue, remote_both) = outcome?;

    let items = |run: &support::Local| -> Result<Vec<Value>> {
        assert_eq!(run.exit_code, 0, "{}{}", run.stdout, run.stderr);
        let envelope: Value = serde_json::from_str(run.stdout.trim())
            .with_context(|| format!("decoding {}", run.stdout))?;
        envelope
            .pointer("/data/items")
            .and_then(Value::as_array)
            .cloned()
            .with_context(|| format!("no items: {}", run.stdout))
    };
    let paths = |run: &support::Local| -> Result<Vec<String>> {
        Ok(items(run)?
            .iter()
            .filter_map(|item| item.get("path").and_then(Value::as_str))
            .map(str::to_owned)
            .collect())
    };
    assert_eq!(paths(&ada)?, ["questions/budget.md"]);
    assert_eq!(paths(&grace)?, ["questions/venue.md"]);
    assert_eq!(paths(&overdue)?, ["questions/budget.md"]);
    let budget = items(&ada)?;
    let budget = budget.first().context("no budget item")?;
    assert_eq!(budget.get("waiting_on"), Some(&json!(["Ada Lovelace"])));
    assert_eq!(
        budget.get("tracked_by"),
        Some(&json!(["https://tracker.example.com/ask/1"]))
    );
    assert_eq!(local.exit_code, 0, "{}", local.stderr);
    assert_eq!(remote_both.exit_code, local.exit_code);
    assert_eq!(remote_both.stdout, local.stdout);
    Ok(())
}

#[test]
fn record_put_reads_stdin() -> Result<()> {
    let tree = support::fixture_clone("remote-record-put")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let mut environment = cloud_env(&server, &metadata)?;
    environment.push(("CTX_SESSION".to_owned(), "session-remote".to_owned()));
    let elsewhere = support::scratch("remote-record-put-cwd")?;
    let remote = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["record", "put", "decisions/stdin.md", "-"],
        Some(&support::cited_record()),
    )?;
    drop(server);
    let bare = support::remote_of(&tree)?;
    let listing = support::git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let message = support::git(&bare, &["log", "-1", "--format=%B", "main"])?;
    let author = support::git(&bare, &["log", "-1", "--format=%ae", "main"])?;
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(remote.exit_code, 0, "{}{}", remote.stdout, remote.stderr);
    assert!(
        listing.lines().any(|name| name == "decisions/stdin.md"),
        "the record is not on the remote: {listing}"
    );
    assert!(
        message.contains("Session: session-remote"),
        "the commit lost the session: {message}"
    );
    assert_eq!(author, "ada@example.com");
    Ok(())
}

/// With no `CTX_SESSION` set, the id Oh My Pi's bash tool gives every command its agent runs
/// (`PI_SESSION_ID`) names the session: the server's request log names it on each request, a
/// read's and a write's alike, and the write's commit carries it in its `Session:` line.
#[test]
fn the_session_an_agent_runs_in_names_its_requests_and_its_commits() -> Result<()> {
    let tree = support::fixture_clone("remote-session-omp")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let mut server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let omp = "01a1b2c3-0000-7000-8000-000000000001";
    let mut environment = cloud_env(&server, &metadata)?;
    environment.push(("PI_SESSION_ID".to_owned(), omp.to_owned()));
    let elsewhere = support::scratch("remote-session-omp-cwd")?;
    let put = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["record", "put", "decisions/session.md", "-"],
        Some(&support::cited_record()),
    );
    let open = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["--json", "open"],
        None,
    );
    let printed = server.stop_and_read_stdout();
    drop(server);
    let message = support::remote_of(&tree)
        .and_then(|bare| support::git(&bare, &["log", "-1", "--format=%B", "main"]));
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    let (put, open, printed, message) = (put?, open?, printed?, message?);
    assert_eq!(put.exit_code, 0, "{}{}", put.stdout, put.stderr);
    assert_eq!(open.exit_code, 0, "{}{}", open.stdout, open.stderr);
    let trailer = format!("Session: {omp}");
    assert!(
        message.lines().any(|line| line == trailer),
        "the commit does not name the session: {message}"
    );
    let named: Vec<(Option<Value>, Option<Value>)> = request_lines(&printed)?
        .iter()
        .map(|line| (line.get("command").cloned(), line.get("session").cloned()))
        .collect();
    assert_eq!(
        named,
        [
            (Some(json!("record put")), Some(json!(omp))),
            (Some(json!("open")), Some(json!(omp))),
        ],
        "{printed:?}"
    );
    Ok(())
}

/// `ctx record put --batch -` reads a JSON list of records and writes them in one commit, so
/// two new records that link each other land together; a batch the server refuses prints each
/// finding with the record it is about, exits 1, and writes nothing.
#[test]
fn record_put_batch_writes_records_that_link_each_other_in_one_commit() -> Result<()> {
    let tree = support::fixture_clone("remote-record-put-batch")?;
    support::fetch_window(&tree)?;
    let bare = support::remote_of(&tree)?;
    let start = support::git(&bare, &["rev-parse", "main"])?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let mut environment = cloud_env(&server, &metadata)?;
    environment.push(("CTX_SESSION".to_owned(), "session-batch".to_owned()));
    let elsewhere = support::scratch("remote-record-put-batch-cwd")?;
    let linking = |title: &str, target: &str| {
        support::cited_record().replace("Keep the echo cadence", title)
            + &format!("\nSee [[{target}]].\n")
    };
    let pair = json!([
        {"path": "decisions/left.md", "content": linking("The left of a pair", "decisions/right")},
        {"path": "decisions/right.md", "content": linking("The right of a pair", "decisions/left")},
    ])
    .to_string();
    let dangling = json!([
        {"path": "decisions/fine.md", "content": support::cited_record()},
        {"path": "decisions/dangling.md", "content": linking("A dangling link", "decisions/gone")},
    ])
    .to_string();
    let run = |stdin: &str| {
        support::ctx_remote(
            &elsewhere,
            &borrowed(&environment),
            &["record", "put", "--batch", "-"],
            Some(stdin),
        )
    };
    let written = run(&pair);
    let refused = run(&dangling);
    drop(server);
    let (written, refused) = (written?, refused?);
    let head = support::git(&bare, &["rev-parse", "main"])?;
    let parent = support::git(&bare, &["rev-parse", "main~1"])?;
    let changed = support::git(&bare, &["show", "--name-only", "--format=", "main"])?;
    let message = support::git(&bare, &["log", "-1", "--format=%B", "main"])?;
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(written.exit_code, 0, "{}{}", written.stdout, written.stderr);
    assert_eq!(
        written.stdout,
        format!("{head} decisions/left.md\n{head} decisions/right.md\n")
    );
    assert_eq!(parent, start, "the batch is not one commit on top of main");
    assert_eq!(
        changed.lines().collect::<Vec<_>>(),
        vec!["decisions/left.md", "decisions/right.md"]
    );
    assert!(
        message.contains("Session: session-batch"),
        "the commit lost the session: {message}"
    );
    assert_eq!(refused.exit_code, 1, "{}{}", refused.stdout, refused.stderr);
    assert!(
        refused
            .stdout
            .starts_with("refused 2 records; none was written\n")
            && refused
                .stdout
                .contains("  decisions/dangling.md: gap — the link decisions/gone has no file"),
        "{}",
        refused.stdout
    );
    Ok(())
}

/// A record path that holds a credential is refused by the server, which shows it redacted; the
/// client's own refusal output, text or JSON, shows the paths it sent redacted too, single put
/// and batch alike. A batch that is not a list of records is refused before anything is sent,
/// and the error says where it went wrong without quoting the value it could not read.
#[test]
fn record_put_never_repeats_a_credential_the_caller_gave_it() -> Result<()> {
    let tree = support::fixture_clone("remote-record-put-credential")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-record-put-credential-cwd")?;
    let token = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let leaky = format!("decisions/{token}.md");
    let batch = json!([
        {"path": "decisions/plain.md", "content": support::cited_record()},
        {"path": leaky, "content": support::cited_record()},
    ])
    .to_string();
    let not_records = json!([token]).to_string();
    let run = |arguments: &[&str], stdin: &str| {
        support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, Some(stdin))
    };
    let outcome = (|| -> Result<_> {
        Ok([
            run(&["record", "put", "--batch", "-"], &batch)?,
            run(&["--json", "record", "put", "--batch", "-"], &batch)?,
            run(&["record", "put", &leaky, "-"], &support::cited_record())?,
            run(
                &["--json", "record", "put", &leaky, "-"],
                &support::cited_record(),
            )?,
            run(&["record", "put", "--batch", "-"], &not_records)?,
        ])
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let [batch_text, batch_json, single_text, single_json, malformed] = outcome?;

    for (label, run) in [
        ("batch", &batch_text),
        ("batch --json", &batch_json),
        ("single", &single_text),
        ("single --json", &single_json),
    ] {
        assert_eq!(run.exit_code, 1, "{label}: {}{}", run.stdout, run.stderr);
        assert!(
            run.stdout.contains("credential") && run.stdout.contains("[redacted]"),
            "{label}: {}",
            run.stdout
        );
    }
    assert_eq!(
        malformed.exit_code, 2,
        "{}{}",
        malformed.stdout, malformed.stderr
    );
    assert!(
        malformed.stderr.contains("line 1, column"),
        "{}",
        malformed.stderr
    );
    for run in [
        &batch_text,
        &batch_json,
        &single_text,
        &single_json,
        &malformed,
    ] {
        assert!(
            !run.stdout.contains(&token) && !run.stderr.contains(&token),
            "the client repeated the credential: {}{}",
            run.stdout,
            run.stderr
        );
    }
    Ok(())
}

/// `ctx record show` through the server prints what a local run prints, from the server's own
/// committed tree, and changes nothing: no commit, no marker, no cursor. A path that names no
/// record, or one outside the tree, fails as it does locally, and the file outside is never sent.
#[test]
fn remote_record_show_answers_as_local_does_and_moves_nothing() -> Result<()> {
    let tree = support::fixture_clone("remote-record-show")?;
    support::fetch_window(&tree)?;
    let root = tree
        .parent()
        .context("the fixture has a root")?
        .to_path_buf();
    std::fs::write(
        root.join("outside.md"),
        "---\ntype: decision\n---\nOUTSIDE THE TREE\n",
    )?;
    let bare = support::remote_of(&tree)?;
    let head = support::git(&bare, &["rev-parse", "main"])?;
    let cursors = std::fs::read(root.join("data/cursors.json"))?;
    let local = ctx_local(&tree, &["record", "show", "index.md"])?;
    let local_missing = ctx_local(&tree, &["record", "show", "decisions/missing.md"])?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ctx-agent@example-project.iam.gserviceaccount.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-record-show-cwd")?;
    let outcome = (|| -> Result<_> {
        let run = |arguments: &[&str]| {
            support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, None)
        };
        Ok((
            run(&["record", "show", "index.md"])?,
            run(&["--json", "record", "show", "index.md"])?,
            run(&["record", "show", "decisions/missing.md"])?,
            run(&["record", "show", "../outside.md"])?,
        ))
    })();
    drop(server);
    let head_after = support::git(&bare, &["rev-parse", "main"])?;
    let cursors_after = std::fs::read(root.join("data/cursors.json"))?;
    let marker = root.join("data/marker.json").exists();
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let (text, json, missing, outside) = outcome?;

    let record = "---\ntype: index\n---\n# Index\n\nRecords live under decisions/.\n";
    assert_eq!(local.exit_code, 0, "{}", local.stderr);
    assert_eq!(local.stdout, record);
    assert_eq!(text.exit_code, 0, "{}{}", text.stdout, text.stderr);
    assert_eq!(text.stdout, local.stdout);
    let envelope: Value = serde_json::from_str(json.stdout.trim())
        .with_context(|| format!("decoding {}{}", json.stdout, json.stderr))?;
    assert_eq!(envelope.pointer("/data/path"), Some(&json!("index.md")));
    assert_eq!(envelope.pointer("/data/text"), Some(&json!(record)));
    assert_eq!(missing.exit_code, 2, "{}{}", missing.stdout, missing.stderr);
    assert_eq!(missing.stdout, "");
    assert_eq!(missing.stderr, local_missing.stderr);
    assert_eq!(outside.exit_code, 2, "{}{}", outside.stdout, outside.stderr);
    assert!(
        !outside.stdout.contains("OUTSIDE") && !outside.stderr.contains("OUTSIDE"),
        "the server sent a file outside its tree: {}{}",
        outside.stdout,
        outside.stderr
    );
    assert!(
        outside.stderr.contains("../outside.md"),
        "the refusal does not name the path: {}",
        outside.stderr
    );
    assert_eq!(head_after, head, "a read made a commit");
    assert_eq!(cursors_after, cursors, "a read moved a cursor");
    assert!(!marker, "a read moved the marker");
    Ok(())
}

/// A cycle writes back the whole record `record show` printed, so the server sends a record
/// as its tree holds it. The server names its own directories by placeholders in what a
/// command prints, and doing that to a record's text would change the words a write puts back
/// whenever a record quotes one of those paths.
#[test]
fn remote_record_show_sends_the_record_unchanged() -> Result<()> {
    let tree = fixture_tree("remote-record-show-paths")?;
    let root = tree
        .parent()
        .context("the fixture has a root")?
        .canonicalize()?;
    let record = format!(
        "---\ntype: note\n---\nThe tree is at {}, its data at {} and its state at {}.\n",
        tree.canonicalize()?.display(),
        root.join("data").display(),
        root.join("state").display()
    );
    std::fs::write(tree.join("decisions/paths.md"), &record)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ctx-agent@example-project.iam.gserviceaccount.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-record-show-paths-cwd")?;
    let outcome = (|| -> Result<_> {
        let run = |arguments: &[&str]| {
            support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, None)
        };
        Ok((
            run(&["--json", "record", "show", "decisions/paths.md"])?,
            run(&["record", "show", "decisions/paths.md"])?,
        ))
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let (json, text) = outcome?;

    assert_eq!(json.exit_code, 0, "{}{}", json.stdout, json.stderr);
    let envelope: Value = serde_json::from_str(json.stdout.trim())
        .with_context(|| format!("decoding {}{}", json.stdout, json.stderr))?;
    assert_eq!(envelope.pointer("/data/text"), Some(&json!(record)));
    assert_eq!(text.exit_code, 0, "{}{}", text.stdout, text.stderr);
    assert_eq!(text.stdout, record);
    Ok(())
}

/// `record` is a first word `/v1/command` answers for `record show` alone: a record write
/// has its own endpoint, which holds the write lease, and is refused at the read door.
#[test]
fn record_writes_are_not_read_commands() -> Result<()> {
    let tree = fixture_tree("server-record-write-door")?;
    let server = Server::start(&tree, &[])?;
    let put = server.command(&["record", "put", "decisions/renewal.md", "-"])?;
    let batch = server.command(&["record", "put", "--batch", "-"])?;
    let rm = server.command(&["record", "rm", "decisions/renewal.md"])?;
    let show = server.command(&["record", "show", "decisions/renewal.md"])?;
    drop(server);
    let still_there = tree.join("decisions/renewal.md").exists();
    remove_fixture(&tree)?;

    for reply in [&put, &batch, &rm] {
        assert_eq!(reply.status, 400, "unexpected body: {}", reply.body);
        assert!(
            reply.text("error")?.contains("own endpoint"),
            "unexpected body: {}",
            reply.body
        );
    }
    assert!(still_there, "a refused remove removed the record");
    assert_eq!(show.status, 200, "unexpected body: {}", show.body);
    assert!(
        show.text("stdout")?.contains("quarterly cadence"),
        "unexpected body: {}",
        show.body
    );
    Ok(())
}

/// `record show` is answered to the principals every read is answered to, and to no one else.
#[test]
fn record_show_refuses_a_principal_outside_the_allow_list() -> Result<()> {
    let tree = fixture_tree("server-record-show-principal")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let token = support::google_token(AUDIENCE, "someone@example.net")?;
    let reply = server.post(
        server.ingress_address()?,
        "/v1/command",
        &json!({ "argv": ["record", "show", "decisions/renewal.md"] }),
        &[("Authorization", &format!("Bearer {token}"))],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401, "unexpected body: {}", reply.body);
    assert!(
        reply.body.get("stdout").is_none(),
        "the refusal carries output: {}",
        reply.body
    );
    Ok(())
}

/// In remote mode `ctx cite verify <file>` names a draft on the caller's machine, which the
/// server's tree does not hold. Forwarded as a path, it matched nothing on the server and
/// answered `checked 0` with exit 0. The client sends
/// the draft's text, and the server checks it against its sources; a path that names neither
/// a local file nor a record of the server's tree is refused.
#[test]
fn remote_cite_verify_checks_a_local_draft_as_content() -> Result<()> {
    let tree = support::fixture_clone("remote-cite-draft")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-cite-draft-cwd")?;
    let outcome = (|| -> Result<_> {
        std::fs::create_dir_all(elsewhere.join("recs")).context("creating the drafts")?;
        std::fs::write(elsewhere.join("recs/cited.md"), support::cited_record())
            .context("writing the cited draft")?;
        std::fs::write(
            elsewhere.join("recs/broken.md"),
            support::cited_record().replace("echo C0EXAMPLE01", "words the stream never said"),
        )
        .context("writing the broken draft")?;
        let run = |arguments: &[&str]| {
            support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, None)
        };
        Ok((
            run(&["--json", "cite", "verify", "recs/cited.md"])?,
            run(&["--json", "cite", "verify", "recs/broken.md"])?,
            run(&["--json", "cite", "verify", "recs/missing.md"])?,
        ))
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let (cited, broken, missing) = outcome?;

    let checked = |run: &support::Local| -> Result<Value> {
        let envelope: Value = serde_json::from_str(&run.stdout)
            .with_context(|| format!("not JSON: {}{}", run.stdout, run.stderr))?;
        Ok(envelope
            .pointer("/data/checked")
            .cloned()
            .unwrap_or(Value::Null))
    };
    assert_eq!(cited.exit_code, 0, "{}{}", cited.stdout, cited.stderr);
    assert_eq!(checked(&cited)?, json!(1), "{}", cited.stdout);
    assert_eq!(broken.exit_code, 1, "{}{}", broken.stdout, broken.stderr);
    assert_eq!(checked(&broken)?, json!(1), "{}", broken.stdout);
    assert_ne!(missing.exit_code, 0, "{}{}", missing.stdout, missing.stderr);
    Ok(())
}

/// A remote draft's own path can hold the bytes of the server's tree path, without naming that
/// tree: `/client/<server tree>-notes.md`. The server hid every substring of its directories,
/// so the broken-quote finding came back under an invented `/client<tree>-notes.md` path
/// (round-6 review of 2f052eb). Only a complete server path prefix is hidden.
#[test]
fn remote_cite_verify_keeps_a_drafts_path_that_contains_the_servers_tree_path() -> Result<()> {
    let tree = support::fixture_clone("remote-cite-path-substring")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-cite-path-substring-cwd")?;
    let root = tree
        .canonicalize()
        .context("canonicalizing the server tree")?;
    let root_without_slash = root
        .strip_prefix(std::path::Path::new("/"))
        .context("the server tree is absolute")?;
    let name = root
        .file_name()
        .context("the server tree has no file name")?
        .to_string_lossy();
    let draft = elsewhere
        .join("client")
        .join(root_without_slash)
        .with_file_name(format!("{name}-notes.md"));
    let bad = support::cited_record().replace("echo C0EXAMPLE01", "words the stream never said");
    let outcome = (|| -> Result<_> {
        let parent = draft.parent().context("the draft has a parent")?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(&draft, bad).context("writing the draft")?;
        let draft_text = draft
            .to_str()
            .context("the draft path is not valid UTF-8")?
            .to_owned();
        let run = support::ctx_remote(
            &elsewhere,
            &borrowed(&environment),
            &["--json", "cite", "verify", &draft_text],
            None,
        )?;
        Ok((run, draft_text))
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let (run, draft_text) = outcome?;

    let envelope: Value = serde_json::from_str(&run.stdout)
        .with_context(|| format!("not JSON: {}{}", run.stdout, run.stderr))?;
    assert_eq!(run.exit_code, 1, "{}{}", run.stdout, run.stderr);
    let files: Vec<&str> = envelope
        .pointer("/data/broken")
        .and_then(Value::as_array)
        .context("the answer lists no broken citation")?
        .iter()
        .filter_map(|entry| entry.get("file").and_then(Value::as_str))
        .collect();
    assert_eq!(files, [draft_text], "{}", run.stdout);
    Ok(())
}

/// A draft whose frontmatter does not parse, sent to the server by `cite verify`, is reported
/// and fails, as a tree file is: the server checked its pointer, read no quote and answered
/// exit 0 (round-5 review of d9cb9f7, N5-2), while a write of the same text is refused.
#[test]
fn remote_cite_verify_reports_a_draft_whose_frontmatter_does_not_parse() -> Result<()> {
    let tree = support::fixture_clone("remote-cite-unparsed")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-cite-unparsed-cwd")?;
    // The quote is left open, so its closing mark is the source's opening one.
    let draft =
        support::cited_record().replace("quote: \"echo C0EXAMPLE01\"", "quote: \"echo C0EXAMPLE01");
    let outcome = (|| -> Result<_> {
        std::fs::write(elsewhere.join("unparsed.md"), &draft).context("writing the draft")?;
        support::ctx_remote(
            &elsewhere,
            &borrowed(&environment),
            &["--json", "cite", "verify", "unparsed.md"],
            None,
        )
    })();
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;
    let run = outcome?;

    assert_ne!(draft, support::cited_record(), "the draft was not broken");
    let envelope: Value = serde_json::from_str(&run.stdout)
        .with_context(|| format!("not JSON: {}{}", run.stdout, run.stderr))?;
    assert_eq!(run.exit_code, 1, "{}{}", run.stdout, run.stderr);
    let unchecked: Vec<&str> = envelope
        .pointer("/data/unchecked")
        .and_then(Value::as_array)
        .context("the answer lists no unchecked entries")?
        .iter()
        .filter_map(|entry| entry.get("pointer").and_then(Value::as_str))
        .collect();
    assert_eq!(unchecked, ["frontmatter"], "{}", run.stdout);
    Ok(())
}

#[test]
fn impersonation_requests_id_token_for_audience() -> Result<()> {
    let tree = fixture_tree("remote-impersonation")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = support::fake_http::FakeHttp::start()?;
    let iam = support::fake_http::FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.pod" }).to_string(),
    )?;
    let account = "ctx-client@example-project.iam.gserviceaccount.com";
    let token = support::google_token(AUDIENCE, account)?;
    iam.json(
        &format!("/v1/projects/-/serviceAccounts/{account}:generateIdToken"),
        200,
        &json!({ "token": token }).to_string(),
    )?;
    let elsewhere = support::scratch("remote-impersonation-cwd")?;
    let origin = format!("http://{}", server.ingress_address()?);
    let remote = support::ctx_remote(
        &elsewhere,
        &[
            ("CTX_SERVER", &origin),
            ("CTX_SERVER_AUDIENCE", &origin),
            ("CTX_IMPERSONATE_SA", account),
            ("GCE_METADATA_HOST", &metadata.address().to_string()),
            ("CTX_IAM_CREDENTIALS_URL", &iam.url("")),
        ],
        &["search", "quarterly"],
        None,
    )?;
    let minted = iam.requests()?;
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(remote.exit_code, 0, "{}{}", remote.stdout, remote.stderr);
    assert!(
        remote.stdout.contains("Renewal terms"),
        "unexpected output: {}",
        remote.stdout
    );
    let request = minted
        .first()
        .ok_or_else(|| anyhow::anyhow!("the client asked for no identity token"))?;
    assert_eq!(
        request.path,
        format!("/v1/projects/-/serviceAccounts/{account}:generateIdToken")
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer ya29.pod")
    );
    let body: serde_json::Value = serde_json::from_str(&request.body)?;
    assert_eq!(
        body.get("audience").and_then(serde_json::Value::as_str),
        Some(origin.as_str())
    );
    Ok(())
}

/// `ctx --json pending` is how a caller writes it, and clap takes a global flag in either
/// place. The client used to forward the flag as the first argument and the server used to
/// read the first argument as the command name, so the pair answered
/// `400 --json is not a remote command` for a command it serves.
#[test]
fn remote_command_accepts_a_leading_global_flag() -> Result<()> {
    let tree = support::fixture_clone("remote-leading-global")?;
    support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-leading-global-cwd")?;
    let leading = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["--json", "pending"],
        None,
    )?;
    let trailing = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["pending", "--json"],
        None,
    )?;
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(
        leading.exit_code, 0,
        "a leading global flag was read as the command: {}{}",
        leading.stdout, leading.stderr
    );
    let envelope: serde_json::Value = serde_json::from_str(leading.stdout.trim())
        .with_context(|| format!("decoding {}", leading.stdout))?;
    let other: serde_json::Value = serde_json::from_str(trailing.stdout.trim())
        .with_context(|| format!("decoding {}", trailing.stdout))?;
    let windows = |envelope: &serde_json::Value| {
        envelope
            .get("data")
            .and_then(|data| data.get("windows"))
            .cloned()
    };
    // The delta carries the moment it was generated, which differs between two runs; the
    // windows are what the command answers.
    assert_eq!(
        windows(&envelope),
        windows(&other),
        "the two spellings of one command answered differently"
    );
    assert!(
        windows(&envelope).is_some_and(|windows| !windows.as_array().is_none_or(Vec::is_empty)),
        "unexpected envelope: {}",
        leading.stdout
    );
    Ok(())
}

/// The server decides which tree it serves. `--memory=<path>` says the same thing as
/// `--memory <path>`, and was refused in only one of its two spellings.
#[test]
fn the_tree_cannot_be_named_remotely_in_either_spelling() -> Result<()> {
    let tree = fixture_tree("server-remote-memory")?;
    let server = Server::start(&tree, &[])?;
    let separate = server.command(&["--memory", "/tmp/elsewhere", "rules"])?;
    let inline = server.command(&["--memory=/tmp/elsewhere", "rules"])?;
    drop(server);
    remove_fixture(&tree)?;

    for reply in [&separate, &inline] {
        assert_eq!(reply.status, 400, "unexpected body: {}", reply.body);
        assert!(
            reply.text("error")?.contains("cannot be set remotely"),
            "unexpected body: {}",
            reply.body
        );
    }
    Ok(())
}

/// `ctx marker advance` answers in one shape wherever it runs: the envelope's `data` holds
/// `marker`, as `marker show` does, and the text names the window, who moved it and when.
/// Through the server it used to answer the bare marker, and its text was the marker's JSON.
#[test]
fn remote_marker_advance_answers_as_local_does() -> Result<()> {
    let tree = support::fixture_clone("remote-marker-advance")?;
    let window = support::fetch_window(&tree)?;
    let keys = support::fake_http::FakeHttp::start()?;
    let server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-marker-advance-cwd")?;
    let json = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["--json", "marker", "advance", &window],
        None,
    )?;
    let text = support::ctx_remote(
        &elsewhere,
        &borrowed(&environment),
        &["marker", "advance", &window],
        None,
    )?;
    drop(server);
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(json.exit_code, 0, "{}{}", json.stdout, json.stderr);
    let envelope: serde_json::Value = serde_json::from_str(json.stdout.trim())
        .with_context(|| format!("decoding {}", json.stdout))?;
    assert_eq!(
        envelope
            .pointer("/data/marker/window")
            .and_then(serde_json::Value::as_str),
        Some(window.as_str()),
        "unexpected envelope: {}",
        json.stdout
    );
    assert_eq!(text.exit_code, 0, "{}{}", text.stdout, text.stderr);
    assert_eq!(
        text.stdout.split_whitespace().next(),
        Some(window.as_str()),
        "unexpected text: {}",
        text.stdout
    );
    Ok(())
}

/// Cloud Run, a load balancer or a proxy can answer in place of the server, in text or HTML.
/// The client read the body as JSON before it looked at the status, so the caller learned only
/// that the body did not decode, and never that the answer was a 503.
#[test]
fn the_client_reports_the_status_of_an_answer_that_is_not_json() -> Result<()> {
    let front = support::fake_http::FakeHttp::start()?;
    front.json("/v1/command", 503, "upstream connect error")?;
    let elsewhere = support::scratch("remote-not-json-cwd")?;
    let remote = support::ctx_remote(
        &elsewhere,
        &[("CTX_SERVER", &front.url(""))],
        &["search", "renewal"],
        None,
    )?;
    support::remove_scratch(&elsewhere)?;

    assert_eq!(remote.exit_code, 2, "{}{}", remote.stdout, remote.stderr);
    assert!(
        remote.stderr.contains("503") && remote.stderr.contains("upstream connect error"),
        "the caller was not told the server's answer: {}",
        remote.stderr
    );
    Ok(())
}

/// A refusal carries the findings that refused it. A 422 without them is not a refusal the
/// caller can act on, and used to be reported as "refused" with no reason.
#[test]
fn a_refusal_without_findings_is_an_error() -> Result<()> {
    let front = support::fake_http::FakeHttp::start()?;
    front.json("/v1/records/put", 422, "{}")?;
    let elsewhere = support::scratch("remote-bare-refusal-cwd")?;
    let remote = support::ctx_remote(
        &elsewhere,
        &[("CTX_SERVER", &front.url(""))],
        &["record", "put", "decisions/renewal.md", "-"],
        Some(&support::cited_record()),
    )?;
    support::remove_scratch(&elsewhere)?;

    assert_eq!(remote.exit_code, 2, "{}{}", remote.stdout, remote.stderr);
    assert!(
        remote.stderr.contains("422"),
        "the caller was not told the server's answer: {}",
        remote.stderr
    );
    Ok(())
}

/// With a server configured, a command runs on the server's tree. A tree the caller names
/// itself was dropped in silence, so `ctx --memory ~/example-memory sweep` swept the server's
/// tree and said nothing; the two cannot both be meant, and the command is refused before
/// anything is sent.
#[test]
fn a_named_tree_and_a_server_are_refused_together() -> Result<()> {
    let front = support::fake_http::FakeHttp::start()?;
    let elsewhere = support::scratch("remote-named-tree-cwd")?;
    let tree = elsewhere
        .to_str()
        .context("scratch path is not valid UTF-8")?;
    let remote = support::ctx_remote(
        &elsewhere,
        &[("CTX_SERVER", &front.url(""))],
        &["--memory", tree, "sweep"],
        None,
    )?;
    let asked = front.requests()?;
    support::remove_scratch(&elsewhere)?;

    assert_eq!(remote.exit_code, 2, "{}{}", remote.stdout, remote.stderr);
    assert!(
        remote.stderr.contains("--memory"),
        "the refusal does not name the flag: {}",
        remote.stderr
    );
    assert!(
        asked.is_empty(),
        "the command reached the server: {asked:?}"
    );
    Ok(())
}

/// A path is the one argument that may be any bytes. The client used to read its own
/// arguments as text and panic on one that is not, which broke the exit-code contract.
#[test]
fn an_argument_that_is_not_text_is_refused_not_a_panic() -> Result<()> {
    let front = support::fake_http::FakeHttp::start()?;
    let elsewhere = support::scratch("remote-not-text-cwd")?;
    let output = support::binary()
        .args(["cite", "verify"])
        .arg(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"decisions/\xff.md"))
        .current_dir(&elsewhere)
        .env("CTX_SERVER", front.url(""))
        .env_remove("CTX_MEMORY")
        .env_remove("CTX_SERVER_AUDIENCE")
        .output()
        .context("running ctx")?;
    let asked = front.requests()?;
    support::remove_scratch(&elsewhere)?;

    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        asked.is_empty(),
        "the command reached the server: {asked:?}"
    );
    Ok(())
}
