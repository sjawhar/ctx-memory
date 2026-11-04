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
    let reply = server.get(server.tailnet_address()?, "/healthz", &[])?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.field("ok")?, &json!(true));
    assert_eq!(reply.field("windows")?, &json!(0));
    Ok(())
}

/// The protocol version this release's server speaks, pinned as the wire carries it, so a
/// release that changes it changes these tests too.
const PROTOCOL: &str = "1";

/// The server names its protocol version in `/healthz` and in `X-Ctx-Protocol` on every answer,
/// a refusal and an unknown path among them, so a deployment and a client can each read it
/// whatever they asked.
#[test]
fn healthz_and_every_answer_name_the_server_s_protocol_version() -> Result<()> {
    let tree = fixture_tree("server-protocol")?;
    let server = Server::start(&tree, &[])?;
    let address = server.tailnet_address()?;
    let health = server.get(address, "/healthz", &[])?;
    let command = server.command(&["rules"])?;
    let anonymous = server.post(address, "/v1/command", &json!({ "argv": ["rules"] }), &[])?;
    let unknown = server.protocol_of(address, "/v1/no-such-route")?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(health.status, 200, "{}", health.body);
    assert_eq!(health.field("protocol")?, &json!(1), "{}", health.body);
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    assert_eq!(unknown.0, 404);
    for (what, status, protocol) in [
        ("/healthz", health.status, health.protocol.as_deref()),
        ("a command", command.status, command.protocol.as_deref()),
        ("a refusal", anonymous.status, anonymous.protocol.as_deref()),
        ("an unknown path", unknown.0, unknown.1.as_deref()),
    ] {
        assert_eq!(protocol, Some(PROTOCOL), "{what} ({status})");
    }
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

/// Each request at any door is one JSON line on the server's standard output, written when
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
    let health = server.get(server.tailnet_address()?, "/healthz", &[])?;
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
    let tailnet = server.tailnet_address()?;
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
        server.tailnet_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[],
    )?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 401);
    Ok(())
}

/// The one person every request at `--local-listen` is, in the tests below.
const LOCAL_CALLER: &str = "Ada Lovelace <ada@example.org>";

/// The arguments of a server whose local door is [`LOCAL_CALLER`].
const LOCAL_DOOR: [&str; 4] = [
    "--local-listen",
    "127.0.0.1:0",
    "--local-caller",
    LOCAL_CALLER,
];

/// On a machine with one person on it, `--local-listen` is that person. The `ctx` client sends
/// it no identity at all, as it sends none to a loopback server: a read is answered, and a
/// record put is committed and pushed with the `--local-caller` as its author and committer.
#[test]
fn a_local_listener_answers_the_local_caller_with_no_identity() -> Result<()> {
    let tree = support::fixture_clone("server-local-door")?;
    support::fetch_window(&tree)?;
    let server = Server::start(&tree, &LOCAL_DOOR)?;
    let origin = format!("http://{}", server.local_address()?);
    let environment = [("CTX_SERVER", origin.as_str())];
    let elsewhere = support::scratch("server-local-door-cwd")?;
    let rules = support::ctx_remote(&elsewhere, &environment, &["rules"], None)?;
    let put = support::ctx_remote(
        &elsewhere,
        &environment,
        &["record", "put", "decisions/local.md", "-"],
        Some(&support::cited_record()),
    )?;
    // The tailnet door beside it still names nobody without its login header.
    let tailnet = server.post(
        server.tailnet_address()?,
        "/v1/command",
        &json!({ "argv": ["rules"] }),
        &[],
    )?;
    drop(server);
    let bare = support::remote_of(&tree)?;
    let listing = support::git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let identities = support::git(
        &bare,
        &["log", "-1", "--format=%an <%ae>%n%cn <%ce>", "main"],
    )?;
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(rules.exit_code, 0, "{}{}", rules.stdout, rules.stderr);
    assert!(rules.stdout.contains("# Rules"), "{}", rules.stdout);
    assert_eq!(put.exit_code, 0, "{}{}", put.stdout, put.stderr);
    assert_eq!(
        tailnet.status, 401,
        "the tailnet door took the local caller: {}",
        tailnet.body
    );
    assert!(
        listing.lines().any(|name| name == "decisions/local.md"),
        "the record is not on the remote: {listing}"
    );
    assert_eq!(identities, format!("{LOCAL_CALLER}\n{LOCAL_CALLER}"));
    Ok(())
}

/// The one-machine setup in docs/deployment.md serves `--local-listen` alone, with no tailnet
/// door beside it. The `ctx` client reads and writes there, and the server stops cleanly.
#[test]
fn a_local_listener_serves_on_its_own() -> Result<()> {
    let tree = support::fixture_clone("server-local-alone")?;
    support::fetch_window(&tree)?;
    let mut server = Server::start_without_tailnet(&tree, &LOCAL_DOOR, &[])?;
    let tailnet = server.tailnet;
    let origin = format!("http://{}", server.local_address()?);
    let environment = [("CTX_SERVER", origin.as_str())];
    let elsewhere = support::scratch("server-local-alone-cwd")?;
    let rules = support::ctx_remote(&elsewhere, &environment, &["rules"], None)?;
    let put = support::ctx_remote(
        &elsewhere,
        &environment,
        &["record", "put", "decisions/alone.md", "-"],
        Some(&support::cited_record()),
    )?;
    let stopped = server.terminate();
    drop(server);
    let listing = support::remote_of(&tree)
        .and_then(|bare| support::git(&bare, &["ls-tree", "-r", "--name-only", "main"]));
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(tailnet, None, "the server opened a tailnet door");
    assert_eq!(rules.exit_code, 0, "{}{}", rules.stdout, rules.stderr);
    assert!(rules.stdout.contains("# Rules"), "{}", rules.stdout);
    assert_eq!(put.exit_code, 0, "{}{}", put.stdout, put.stderr);
    let listing = listing?;
    assert!(
        listing.lines().any(|name| name == "decisions/alone.md"),
        "the record is not on the remote: {listing}"
    );
    stopped
}

/// The local door asks no proof, so it answers no web page in the machine's browser, at any
/// path: not one that posts to it from another origin, which the browser names in `Origin`;
/// not one that loads it as an image or a script, a GET with no `Origin`, which the browser
/// still marks with `Sec-Fetch-Site`; and not one whose own host name was pointed at this
/// machine (DNS rebinding), which still names that host in `Host`. The same requests without
/// any of those are answered.
#[test]
fn a_local_listener_answers_no_web_page() -> Result<()> {
    let tree = support::fixture_clone("server-local-browser")?;
    let window = support::fetch_window(&tree)?;
    let server = Server::start(&tree, &LOCAL_DOOR)?;
    let local = server.local_address()?;
    let rebound = format!("example.org:{}", local.port());
    let page = ("Origin", "https://example.org");
    let context = format!("/v1/windows/{window}/context");
    let rules = json!({ "argv": ["rules"] });
    let post = |path: &str, body: &Value, headers: &[(&str, &str)]| {
        server
            .post(local, path, body, headers)
            .map(|reply| reply.status)
    };
    let get = |path: &str, headers: &[(&str, &str)]| {
        server
            .raw(reqwest::Method::GET, local, path, None, headers)
            .map(|(status, _)| status)
    };
    let asked = [
        post("/v1/command", &rules, &[]),
        post("/v1/command", &rules, &[page]),
        post("/v1/command", &rules, &[("Host", rebound.as_str())]),
        post(
            "/v1/records/put",
            &json!({ "path": "decisions/page.md", "content": support::cited_record() }),
            &[page],
        ),
        get(&context, &[]),
        get(&context, &[("Sec-Fetch-Site", "cross-site")]),
        get("/healthz", &[]),
        get("/healthz", &[("Host", rebound.as_str())]),
        get("/nowhere", &[]),
        get("/nowhere", &[page]),
    ];
    drop(server);
    remove_fixture(&tree)?;

    let statuses = asked.into_iter().collect::<Result<Vec<_>>>()?;
    assert_eq!(statuses, [200, 401, 401, 401, 200, 401, 200, 401, 404, 401]);
    Ok(())
}

/// Each request at the local door is one line of the request log, at door `local`, naming the
/// `--local-caller`, here given as `CTX_SERVER_LOCAL_CALLER`. A write the checks refuse is
/// refused there as at every other door, and commits nothing. A web page's request is refused
/// before any route sees it, and still logged, naming nobody.
#[test]
fn the_local_door_logs_each_request_as_the_local_caller() -> Result<()> {
    let tree = support::fixture_clone("server-local-log")?;
    support::fetch_window(&tree)?;
    let mut server = Server::start_with_env(
        &tree,
        &["--local-listen", "127.0.0.1:0"],
        &[("CTX_SERVER_LOCAL_CALLER", LOCAL_CALLER)],
    )?;
    let local = server.local_address()?;
    let put = |path: &str, content: &str| {
        server.post(
            local,
            "/v1/records/put",
            &json!({ "path": path, "content": content, "session": "s" }),
            &[],
        )
    };
    let rules = server.post(local, "/v1/command", &json!({ "argv": ["rules"] }), &[])?;
    let written = put("decisions/local.md", &support::cited_record())?;
    let gap = format!(
        "{}\nIt replaces [[a-record-that-does-not-exist]].\n",
        support::cited_record()
    );
    let refused = put("decisions/gap.md", &gap)?;
    let page = server.post(
        local,
        "/v1/records/put",
        &json!({ "path": "decisions/page.md", "content": support::cited_record(), "session": "s" }),
        &[("Sec-Fetch-Site", "cross-site")],
    )?;
    let printed = server.stop_and_read_stdout()?;
    drop(server);
    let listing = support::remote_of(&tree)
        .and_then(|bare| support::git(&bare, &["ls-tree", "-r", "--name-only", "main"]));
    remove_fixture(&tree)?;

    assert_eq!(
        [rules.status, written.status, refused.status, page.status],
        [200, 200, 422, 401],
        "unexpected answers: {} {} {} {}",
        rules.body,
        written.body,
        refused.body,
        page.body
    );
    let listing = listing?;
    assert!(
        !listing
            .lines()
            .any(|name| name == "decisions/gap.md" || name == "decisions/page.md"),
        "the refused record reached the remote: {listing}"
    );
    let fields = ["door", "caller", "command", "result"];
    let logged: Vec<[Option<Value>; 4]> = request_lines(&printed)?
        .iter()
        .map(|line| fields.map(|field| line.get(field).cloned()))
        .collect();
    let expected = |command: &str, result: &str| {
        ["local", "ada@example.org", command, result].map(|text| Some(json!(text)))
    };
    assert_eq!(
        logged,
        [
            expected("rules", "ok"),
            expected("record put", "ok"),
            expected("record put", "refused"),
            [
                Some(json!("local")),
                Some(Value::Null),
                Some(json!("record put")),
                Some(json!("unidentified")),
            ],
        ],
        "{printed:?}"
    );
    Ok(())
}

/// Starts `ctx serve` with `arguments` on `tree`, outside any shared deployment unless
/// `environment` says otherwise, and returns what it said on standard error as it refused to
/// start. A server that binds a port instead fails the test.
fn serve_refusal(
    tree: &std::path::Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<String> {
    use std::process::Stdio;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = support::binary();
    command
        .args(["--memory", tree_text, "serve"])
        .args(arguments)
        .envs(support::tree_env(tree)?)
        .env_remove("CTX_LEASE_BUCKET")
        .env_remove("K_SERVICE")
        .env_remove("CLOUD_RUN_JOB")
        .env_remove("CTX_SERVER_LOCAL_CALLER")
        .envs(environment.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("starting ctx serve")?;
    let stdout = support::read_lines(child.stdout.take().context("ctx serve has no stdout")?);
    let stderr = support::read_lines(child.stderr.take().context("ctx serve has no stderr")?);
    // A server prints one line for each port it binds; one that refuses prints none and exits,
    // which closes the pipe.
    match stdout.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(line) => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ctx serve started instead of refusing: {line}");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ctx serve neither bound a port nor stopped within 120s");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
    }
    let status = child.wait().context("waiting for ctx serve")?;
    let said = stderr.iter().collect::<Vec<_>>().join("\n");
    anyhow::ensure!(
        !status.success(),
        "ctx serve exited 0 and bound nothing: {said}"
    );
    Ok(said)
}

/// The local door names every request there as one person, so only a process on this machine
/// may reach it: an address off the loopback interface is refused, and nothing is bound.
#[test]
fn a_local_listener_takes_a_loopback_address_only() -> Result<()> {
    let tree = fixture_tree("server-local-off-loopback")?;
    let said: Result<Vec<String>> = ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"]
        .iter()
        .map(|address| {
            serve_refusal(
                &tree,
                &["--local-listen", address, "--local-caller", LOCAL_CALLER],
                &[],
            )
        })
        .collect();
    remove_fixture(&tree)?;

    for said in said? {
        assert!(
            said.contains("--local-listen takes a loopback address"),
            "{said}"
        );
    }
    Ok(())
}

/// The local door is for a machine with one person on it, and never opens in a shared
/// deployment: not beside a lease bucket, which only a data directory that several copies share
/// needs, not on Cloud Run, which names its services in `K_SERVICE` and its jobs in
/// `CLOUD_RUN_JOB`, and not beside `--listen`, Cloud Run's ingress. The tailnet door is asked
/// for as well, so a copy that bound any port before it refused would print it and fail here.
#[test]
fn a_local_listener_never_opens_in_a_shared_deployment() -> Result<()> {
    let tree = fixture_tree("server-local-shared")?;
    let mut arguments = LOCAL_DOOR.to_vec();
    arguments.extend(["--tailnet-listen", "127.0.0.1:0"]);
    let mut beside_ingress = arguments.clone();
    beside_ingress.extend(["--listen", "127.0.0.1:0"]);
    let cases = [
        (
            &arguments,
            Some(("CTX_LEASE_BUCKET", "ctx-leases")),
            "CTX_LEASE_BUCKET",
        ),
        (&arguments, Some(("K_SERVICE", "ctx-memory")), "K_SERVICE"),
        (
            &arguments,
            Some(("CLOUD_RUN_JOB", "ctx-fetch")),
            "CLOUD_RUN_JOB",
        ),
        (&beside_ingress, None, "--listen is Cloud Run's ingress"),
    ];
    let said: Result<Vec<String>> = cases
        .iter()
        .map(|(arguments, setting, _)| serve_refusal(&tree, arguments, setting.as_slice()))
        .collect();
    remove_fixture(&tree)?;

    for (said, (_, _, named)) in said?.iter().zip(cases) {
        assert!(
            said.contains("--local-listen") && said.contains(named),
            "{named}: {said}"
        );
    }
    Ok(())
}

/// Every request at the local door is the `--local-caller`, who authors each commit made
/// there, so a server without one, or with one that is not `Name <email>`, does not start.
#[test]
fn a_local_listener_needs_a_local_caller_as_name_and_email() -> Result<()> {
    let tree = fixture_tree("server-local-caller")?;
    let missing = serve_refusal(&tree, &["--local-listen", "127.0.0.1:0"], &[]);
    let malformed: Result<Vec<String>> = [
        "ada@example.org",
        "Ada Lovelace",
        "<ada@example.org>",
        "Ada Lovelace <>",
        "Ada Lovelace <ada>",
        "Ada Lovelace <ada@example.org> again",
    ]
    .iter()
    .map(|caller| {
        serve_refusal(
            &tree,
            &["--local-listen", "127.0.0.1:0", "--local-caller", caller],
            &[],
        )
    })
    .collect();
    remove_fixture(&tree)?;

    let missing = missing?;
    assert!(
        missing.contains("--local-listen needs --local-caller"),
        "{missing}"
    );
    for said in malformed? {
        assert!(
            said.contains("--local-caller") && said.contains("Name <email>"),
            "{said}"
        );
    }
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

/// A tree with one source and one open question last reviewed on 2026-10-24, and `count`
/// completed windows on its share, each flagging a reply to that question. Returns the
/// `delta.json` of every window, oldest first.
fn flagged_share(tree: &std::path::Path, count: usize) -> Result<Vec<std::path::PathBuf>> {
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"server-test\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\n\
         scheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    std::fs::create_dir_all(tree.join("questions"))?;
    std::fs::write(
        tree.join("questions/example.md"),
        "---\ntype: question\nstatus: open\nlast_reviewed: 2026-10-24\n---\nWho reviews it?\n",
    )?;
    let windows = tree
        .parent()
        .context("the fixture has a root")?
        .join("data/windows");
    (1..=count).map(|n| flagged_window(&windows, n)).collect()
}

/// Writes the `n`th completed window of `windows`, whose delta flags one reply to the question
/// `flagged_share` writes, and returns its `delta.json`.
fn flagged_window(windows: &std::path::Path, n: usize) -> Result<std::path::PathBuf> {
    let name = format!("202610{:02}T120000Z-{n:04}", 24 + n);
    let end = format!("2026-10-{:02}T12:00:00Z", 24 + n);
    let dir = windows.join(&name);
    std::fs::create_dir_all(&dir)?;
    let manifest = json!({
        "version": 1, "window": name, "generated_at": end, "window_end": end,
        "sources": {}, "warnings": [], "exit_status": "ok",
    });
    std::fs::write(dir.join("manifest.json"), manifest.to_string())?;
    std::fs::write(dir.join("records.jsonl"), "")?;
    std::fs::write(dir.join("rendered.md"), "")?;
    let delta = json!({
        "version": 1, "window": name, "window_end": end, "generated_at": end,
        "previous_window": null, "new_messages": [],
        "unreviewed": [{
            "record": "questions/example.md", "kind": "question", "status": "open",
            "title": "questions/example.md", "owner": null, "last_reviewed": "2026-10-24",
            "replies": [{
                "pointer": format!("echo:C0EXAMPLE01/r{n}"),
                "ts": format!("2026-10-{:02}T10:00:00Z", 24 + n),
                "author": "Bob", "how": "pointer", "excerpt": "A review reply.",
            }],
        }],
        "closed_since_previous": [], "opened_since_previous": [],
    });
    let path = dir.join("delta.json");
    std::fs::write(&path, delta.to_string())?;
    Ok(path)
}

/// A server reads every window's delta for `ctx open` before it binds its listeners, under the
/// same `--index-wait` bound as its record index, so the first `ctx open` a fresh copy answers
/// reads no `delta.json` at all. The first `open` after a restart took 12.8 s in production
/// while the next took 29 ms, because the first caller read every window on the share.
///
/// Every window's delta is made unreadable once the server has bound, with its length and the
/// time it was last written untouched: an `open` that reads one then fails, and one that answers
/// from what the server read before it bound gives what a local run gave before.
#[test]
fn a_fresh_servers_first_open_reads_no_windows_delta() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let tree = fixture_tree("server-open-warm")?;
    let deltas = flagged_share(&tree, 3)?;
    let local = ctx_local(&tree, &["--json", "open"])?;
    let server = Server::start(&tree, &[])?;
    for path in &deltas {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))?;
    }
    let unreadable = deltas.iter().all(|path| std::fs::read(path).is_err());
    let reply = server.command(&["--json", "open"]);
    drop(server);
    for path in &deltas {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    remove_fixture(&tree)?;
    let reply = reply?;

    assert!(
        unreadable,
        "this test needs a user the file mode denies the read; run it as one"
    );
    assert_eq!(local.exit_code, 0, "{}", local.stderr);
    for n in 1..=3 {
        assert!(
            local.stdout.contains(&format!("echo:C0EXAMPLE01/r{n}")),
            "the local run lists every window's reply: {}",
            local.stdout
        );
    }
    assert_eq!(
        reply.field("exit_code")?,
        &json!(0),
        "the first open read a window's delta: {}",
        reply.body
    );
    assert_eq!(reply.text("stdout")?, local.stdout);
    Ok(())
}

/// What a fresh server's `ctx open` answers is what a local run answers, for the windows it read
/// before it bound and for a window written after: the cache it starts with follows the share.
#[test]
fn a_fresh_servers_open_answers_as_a_local_run_does() -> Result<()> {
    let tree = fixture_tree("server-open-warm-answer")?;
    flagged_share(&tree, 2)?;
    let server = Server::start(&tree, &[])?;
    let windows = tree
        .parent()
        .context("the fixture has a root")?
        .join("data/windows");
    let outcome = (|| {
        let first = server.command(&["--json", "open"])?;
        flagged_window(&windows, 3)?;
        let later = server.command(&["--json", "open"])?;
        let local = ctx_local(&tree, &["--json", "open"])?;
        Ok::<_, anyhow::Error>((first, later, local))
    })();
    drop(server);
    remove_fixture(&tree)?;
    let (first, later, local) = outcome?;

    assert_eq!(local.exit_code, 0, "{}", local.stderr);
    assert_eq!(first.field("exit_code")?, &json!(0), "{}", first.body);
    let first = first.text("stdout")?;
    assert!(first.contains("echo:C0EXAMPLE01/r2"), "{first}");
    assert!(!first.contains("echo:C0EXAMPLE01/r3"), "{first}");
    assert_eq!(later.text("stdout")?, local.stdout);
    assert!(
        local.stdout.contains("echo:C0EXAMPLE01/r3"),
        "{}",
        local.stdout
    );
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

/// A fixture tree whose echo source delivers five messages into one fetched window, two to a
/// page. Returns the tree and the window.
fn tree_of_pages(label: &str) -> Result<(std::path::PathBuf, String)> {
    let tree = support::fixture_clone(label)?;
    let config = std::fs::read_to_string(tree.join("ctx.toml"))?.replace(
        "streams = [\"C0EXAMPLE01\"]",
        "streams = [\"C0EXAMPLE01\", \"C0EXAMPLE02\", \"C0EXAMPLE03\", \"C0EXAMPLE04\", \"C0EXAMPLE05\"]",
    );
    std::fs::write(
        tree.join("ctx.toml"),
        format!("{config}\n[ingest]\npage_messages = 2\n"),
    )?;
    support::git(&tree, &["add", "."])?;
    support::commit(&tree, "pages of two messages")?;
    support::git(&tree, &["push", "--quiet", "origin", "main"])?;
    let window = support::fetch_window(&tree)?;
    Ok((tree, window))
}

/// Each `page` of `pages`, as `(worked, through, last)`.
fn spans(pages: &[Value]) -> Vec<(u64, u64, bool)> {
    pages
        .iter()
        .map(|page| {
            (
                page.get("worked").and_then(Value::as_u64).unwrap_or(99),
                page.get("through").and_then(Value::as_u64).unwrap_or(99),
                page.get("last")
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// A window larger than a page is worked a page a cycle through the server, as the scheduled
/// cycle reaches it: each cycle's `ctx pending` lists the next page, `ctx marker page` records it
/// through `/v1/marker/page` in the signed-in caller's name, and `ctx marker show` names the page
/// reached. The marker refuses to pass the window before its last page, with an error that says
/// what to run, and passes it once the answer lists that page. The request log names each page
/// record `marker page`.
#[test]
fn a_large_window_is_worked_a_page_a_cycle_through_the_server() -> Result<()> {
    let (tree, window) = tree_of_pages("remote-pages")?;
    let keys = support::fake_http::FakeHttp::start()?;
    let mut server = ingress_server(&tree, &keys)?;
    let metadata = metadata_identity("ada@example.com")?;
    let environment = cloud_env(&server, &metadata)?;
    let elsewhere = support::scratch("remote-pages-cwd")?;
    let ctx = |arguments: &[&str]| {
        support::ctx_remote(&elsewhere, &borrowed(&environment), arguments, None)
    };
    let envelope = |output: &support::Local| -> Result<Value> {
        serde_json::from_str(output.stdout.trim())
            .with_context(|| format!("decoding {}{}", output.stdout, output.stderr))
    };
    let mut pages = Vec::new();
    let mut refused = Vec::new();
    let mut shown = Vec::new();
    let advanced = loop {
        let pending = envelope(&ctx(&["--json", "pending"])?)?;
        let page = pending
            .pointer("/data/windows/0/page")
            .cloned()
            .with_context(|| format!("the window is not listed as a page: {pending}"))?;
        let ends_at = page
            .get("ends_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        pages.push(page.clone());
        if page.get("last").and_then(Value::as_bool) == Some(true) {
            break ctx(&["--json", "marker", "advance", &window])?;
        }
        let early = ctx(&["--json", "marker", "advance", &window])?;
        refused.push((early.exit_code, early.stdout, early.stderr));
        let recorded = ctx(&["--json", "marker", "page", &window, &ends_at])?;
        assert_eq!(
            recorded.exit_code, 0,
            "{}{}",
            recorded.stdout, recorded.stderr
        );
        let show = envelope(&ctx(&["--json", "marker", "show"])?)?;
        shown.push(show.pointer("/data/page").cloned().unwrap_or_default());
        assert!(pages.len() < 10, "the pages end");
    };
    let after = envelope(&ctx(&["--json", "marker", "show"])?)?;
    let log = server.stop_and_read_stdout()?;
    support::remove_scratch(&elsewhere)?;
    remove_fixture(&tree)?;

    assert_eq!(
        spans(&pages),
        vec![(0, 2, false), (2, 4, false), (4, 5, true)]
    );
    assert_eq!(refused.len(), 2);
    for (code, stdout, stderr) in &refused {
        assert_ne!(
            *code, 0,
            "the marker passed the window before its last page"
        );
        assert!(
            format!("{stdout}{stderr}").contains("ctx marker page"),
            "the refusal does not say what to run: {stdout}{stderr}"
        );
    }
    for (page, worked) in shown.iter().zip([2, 4]) {
        assert_eq!(
            page.get("window").and_then(Value::as_str),
            Some(window.as_str())
        );
        assert_eq!(page.get("worked").and_then(Value::as_u64), Some(worked));
        assert!(
            page.get("by")
                .and_then(Value::as_str)
                .is_some_and(|by| by.starts_with("ada@example.com")),
            "the page does not name the signed-in caller: {page}"
        );
    }
    assert_eq!(
        advanced.exit_code, 0,
        "{}{}",
        advanced.stdout, advanced.stderr
    );
    assert_eq!(
        after.pointer("/data/marker/window").and_then(Value::as_str),
        Some(window.as_str())
    );
    assert!(after.pointer("/data/page").is_none(), "{after}");
    let page_lines = log
        .iter()
        .filter(|line| line.contains("\"command\":\"marker page\""))
        .count();
    assert_eq!(
        page_lines, 2,
        "the request log does not name each page record: {log:?}"
    );
    Ok(())
}

/// The `page` that `ctx pending` lists for the window after the marker, read through
/// `/v1/command` with `headers`, which must be answered.
fn listed_page(server: &Server, headers: &[(&str, &str)]) -> Result<Value> {
    let reply = server.post(
        server.tailnet_address()?,
        "/v1/command",
        &json!({ "argv": ["--json", "pending"] }),
        headers,
    )?;
    anyhow::ensure!(
        reply.status == 200,
        "pending was not answered: {}",
        reply.body
    );
    let pending: Value = serde_json::from_str(reply.text("stdout")?)?;
    pending
        .pointer("/data/windows/0/page")
        .cloned()
        .with_context(|| format!("the window is not listed as a page: {pending}"))
}

/// The `ends_at` of a listed `page`.
fn ends_at(page: &Value) -> Result<String> {
    page.get("ends_at")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("the page names no end: {page}"))
}

/// A client older than paging names no protocol version, and a page record or a move of the
/// marker past a window with pages left is refused it with a `409` that names both versions,
/// rather than a `400` that reads as the client's own mistake. What did not change for it
/// answers it as before: its reads, and a move of the marker that passes no page left. A client
/// that names this server's version is answered as before, and one whose version is not a number
/// is refused.
#[test]
fn a_client_too_old_for_paging_gets_409_and_keeps_its_unchanged_routes() -> Result<()> {
    let (tree, window) = tree_of_pages("server-protocol-paging")?;
    let server = Server::start(&tree, &[])?;
    let address = server.tailnet_address()?;
    let user = ("Tailscale-User-Login", "ada@example.com");
    let old: &[(&str, &str)] = &[user];
    let current: &[(&str, &str)] = &[user, ("X-Ctx-Protocol", PROTOCOL)];
    let garbled: &[(&str, &str)] = &[user, ("X-Ctx-Protocol", "two")];
    let page = |headers: &[(&str, &str)], ends_at: &str| {
        let body = json!({ "window": window.as_str(), "ends_at": ends_at, "session": "s" });
        server.post(address, "/v1/marker/page", &body, headers)
    };
    let advance = |headers: &[(&str, &str)]| {
        let body = json!({ "window": window.as_str(), "session": "s" });
        server.post(address, "/v1/marker/advance", &body, headers)
    };
    let first = ends_at(&listed_page(&server, old)?)?;
    let old_page = page(old, &first)?;
    let old_advance = advance(old)?;
    let current_advance = advance(current)?;
    let garbled_page = page(garbled, &first)?;
    let mut recorded = Vec::new();
    let mut listed = listed_page(&server, old)?;
    while listed.get("last") != Some(&Value::Bool(true)) && recorded.len() < 5 {
        recorded.push(page(current, &ends_at(&listed)?)?.status);
        listed = listed_page(&server, old)?;
    }
    let last_advance = advance(old)?;
    drop(server);
    remove_fixture(&tree)?;

    for (what, reply) in [
        ("a page record", &old_page),
        ("a move past pages left", &old_advance),
    ] {
        assert_eq!(reply.status, 409, "{what}: {}", reply.body);
        let error = reply.text("error")?;
        assert!(
            error.contains("protocol 0") && error.contains(&format!("protocol {PROTOCOL}")),
            "{what}: the refusal does not name both versions: {error}"
        );
    }
    assert_eq!(current_advance.status, 400, "{}", current_advance.body);
    assert!(
        current_advance.text("error")?.contains("ctx marker page"),
        "{}",
        current_advance.body
    );
    assert_eq!(garbled_page.status, 400, "{}", garbled_page.body);
    assert_eq!(recorded, [200, 200]);
    assert_eq!(last_advance.status, 200, "{}", last_advance.body);
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

/// A tagged node gets no login header from Tailscale Serve. With its tag allowed, the server
/// asks tailscaled who the address in `X-Forwarded-For` is and answers the node by its tag,
/// which the request log names; a node whose tags are not allowed is refused.
#[test]
fn an_allowed_tagged_node_is_answered_by_its_tag() -> Result<()> {
    let tree = fixture_tree("server-tagged")?;
    let root = tree.parent().context("the tree has no parent")?.to_owned();
    let api = support::fake_tailscaled::FakeTailscaled::start(
        &root.join("tailscaled"),
        &[
            ("100.64.0.7", &["tag:ctx-ingest"]),
            ("100.64.0.8", &["tag:other"]),
        ],
    )?;
    let socket = api.socket.to_str().context("socket path is not UTF-8")?;
    let mut server = Server::start(
        &tree,
        &[
            "--tailnet-allow-tag",
            "tag:ctx-ingest,tag:ctx-legion",
            "--tailscale-socket",
            socket,
        ],
    )?;
    let body = json!({ "argv": ["search", "quarterly"] });
    let start = chrono::Utc::now();
    let allowed = server.post(
        server.tailnet_address()?,
        "/v1/command",
        &body,
        &[("X-Forwarded-For", "100.64.0.7")],
    )?;
    let other = server.post(
        server.tailnet_address()?,
        "/v1/command",
        &body,
        &[("X-Forwarded-For", "100.64.0.8")],
    )?;
    let printed = server.stop_and_read_stdout()?;
    let window = (start, chrono::Utc::now());
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(allowed.status, 200, "{}", allowed.body);
    assert_eq!(other.status, 401, "{}", other.body);
    assert!(
        other.text("error")?.contains("allows none of them"),
        "{}",
        other.body
    );
    let lines = request_lines(&printed)?;
    let [admitted, refused] = lines.as_slice() else {
        anyhow::bail!("not one line per request: {printed:?}");
    };
    assert_logged(
        admitted,
        &json!({
            "severity": "INFO", "door": "tailnet", "caller": "tag:ctx-ingest",
            "command": "search", "query_chars": 9, "result": "ok", "status": 200,
            "response_bytes": allowed.bytes,
        }),
        window,
    )?;
    assert_logged(
        refused,
        &json!({
            "severity": "WARNING", "door": "tailnet", "caller": null, "command": null,
            "result": "unidentified", "status": 401, "response_bytes": other.bytes,
        }),
        window,
    )?;
    Ok(())
}

/// Runs `ctx serve` with `arguments`, which must refuse to start, and returns its stderr.
fn refused_serve(label: &str, arguments: &[&str]) -> Result<String> {
    let tree = fixture_tree(label)?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let output = support::binary()
        .args(["--memory", tree_text, "serve"])
        .args(arguments)
        .args(["--pull-interval", "3600"])
        .envs(support::tree_env(&tree)?)
        .env_remove("CTX_SERVER_TAILNET_TAGS")
        .env_remove("CTX_TAILSCALE_SOCKET")
        .output()
        .context("running ctx serve")?;
    remove_fixture(&tree)?;
    assert!(!output.status.success(), "ctx serve started: {arguments:?}");
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// A deployment that allows a tag must name a well-formed tag, the tailnet listener the node
/// reaches and the local API that names it, or the server does not start.
#[test]
fn allowed_tags_need_a_tailnet_listener_and_a_socket() -> Result<()> {
    let socket = "/nonexistent/tailscaled.sock";
    for (label, arguments, expected) in [
        (
            "server-tag-malformed",
            vec![
                "--tailnet-listen",
                "127.0.0.1:0",
                "--tailnet-allow-tag",
                "ctx-ingest",
                "--tailscale-socket",
                socket,
            ],
            "is not a tailnet tag",
        ),
        (
            "server-tag-no-socket",
            vec![
                "--tailnet-listen",
                "127.0.0.1:0",
                "--tailnet-allow-tag",
                "tag:ctx-ingest",
            ],
            "--tailnet-allow-tag needs --tailscale-socket",
        ),
        (
            "server-tag-empty-socket",
            vec![
                "--tailnet-listen",
                "127.0.0.1:0",
                "--tailnet-allow-tag",
                "tag:ctx-ingest",
                "--tailscale-socket",
                "",
            ],
            // clap itself refuses an empty path before the server's own check.
            "--tailscale-socket",
        ),
        (
            "server-tag-no-listener",
            vec![
                "--listen",
                "127.0.0.1:0",
                "--tailnet-allow-tag",
                "tag:ctx-ingest",
                "--tailscale-socket",
                socket,
            ],
            "--tailnet-allow-tag needs --tailnet-listen",
        ),
    ] {
        let stderr = refused_serve(label, &arguments)?;
        assert!(stderr.contains(expected), "{label}: {stderr}");
    }
    Ok(())
}
