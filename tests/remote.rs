//! Remote mode's client: where it sends a caller's records and identity token.
//!
//! A `ctx` with `CTX_SERVER` set sends every command and record to that server, and inside
//! Google Cloud it sends an identity token with each. These tests run the real client against
//! loopback stand-ins, and a stand-in HTTP proxy for a server off this machine, so what they
//! prove is the request that left the client, or that none did.

mod support;

use std::io::Write as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::json;

use support::fake_http::FakeHttp;
use support::Local;

/// Where the metadata server mints an identity token.
const IDENTITY: &str = "/computeMetadata/v1/instance/service-accounts/default/identity";

/// What a server answers a forwarded command with.
fn answered() -> String {
    json!({ "stdout": "answered\n", "stderr": "", "exit_code": 0 }).to_string()
}

/// A metadata server that mints `token` for any audience.
fn metadata(token: &str) -> Result<FakeHttp> {
    let metadata = FakeHttp::start()?;
    metadata.json(IDENTITY, 200, token)?;
    Ok(metadata)
}

/// Runs `ctx search quarterly` as a remote client in exactly `environment`: no tree, and no
/// identity, token or proxy setting the test does not give it.
fn search(environment: &[(&str, &str)]) -> Result<Local> {
    run_client(&["search", "quarterly"], None, environment)
}

/// Runs `ctx` with `args` as a remote client in exactly `environment`, with `input` on standard
/// input when there is one: no tree, and no identity, token or proxy setting the test does not
/// give it.
fn run_client(args: &[&str], input: Option<&str>, environment: &[(&str, &str)]) -> Result<Local> {
    let cwd = support::scratch("remote-client-cwd")?;
    let mut command = support::binary();
    command
        .args(args)
        .current_dir(&cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in [
        "CTX_MEMORY",
        "CTX_SERVER",
        "CTX_SERVER_AUDIENCE",
        "CTX_IMPERSONATE_SA",
        "GCE_METADATA_HOST",
        "CTX_IAM_CREDENTIALS_URL",
        "CTX_SESSION",
        "OMP_SESSION_ID",
        "PI_SESSION_ID",
        "CLAUDE_CODE_SESSION_ID",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(name);
    }
    command.envs(environment.iter().copied());
    let output = (|| -> Result<std::process::Output> {
        let mut child = command.spawn().context("running ctx as a remote client")?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes())?;
        }
        child
            .wait_with_output()
            .context("waiting for ctx as a remote client")
    })();
    support::remove_scratch(&cwd)?;
    let output = output?;
    Ok(Local {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// `server.url("")` with the encoding a query string gives it.
fn query_encoded(text: &str) -> String {
    text.replace(':', "%3A").replace('/', "%2F")
}

/// The token is minted for the origin it is sent to, and sent there.
#[test]
fn a_token_is_minted_for_the_server_it_is_sent_to() -> Result<()> {
    let server = FakeHttp::start()?;
    server.json("/v1/command", 200, &answered())?;
    let metadata = metadata("token-for-this-server")?;
    let origin = server.url("");

    let client = search(&[
        ("CTX_SERVER", &origin),
        ("CTX_SERVER_AUDIENCE", &origin),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
    ])?;

    let minted = metadata.requests()?;
    let received = server.requests()?;
    assert_eq!(client.exit_code, 0, "{}", client.stderr);
    assert_eq!(client.stdout, "answered\n");
    assert!(
        minted.first().is_some_and(|request| request
            .target
            .contains(&format!("audience={}", query_encoded(&origin)))),
        "the token was not minted for {origin}: {minted:?}"
    );
    assert_eq!(
        received
            .first()
            .and_then(|request| request.headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer token-for-this-server")
    );
    Ok(())
}

/// Runs `ctx search quarterly` against a stand-in server in `environment`, and returns what the
/// client printed and the session header of each request the server got.
fn sessions_sent(environment: &[(&str, &str)]) -> Result<(Local, Vec<Option<String>>)> {
    let server = FakeHttp::start()?;
    server.json("/v1/command", 200, &answered())?;
    let origin = server.url("");
    let client = search(&[&[("CTX_SERVER", origin.as_str())][..], environment].concat())?;
    let sent = server
        .requests()?
        .into_iter()
        .map(|request| request.headers.get("x-ctx-session").cloned())
        .collect();
    Ok((client, sent))
}

/// One variable a test sets for the client, and its value.
type Binding = (&'static str, &'static str);

/// Every request names the caller's session in `X-Ctx-Session`, so the server's log can tell
/// apart sessions that reach it under one identity: `CTX_SESSION` when it is set, else an
/// exported Oh My Pi id, else the id Oh My Pi's bash tool gives every command its agent runs,
/// else Claude Code's, else none. An empty variable counts as unset.
#[test]
fn every_request_names_the_caller_s_session() -> Result<()> {
    let omp = ("OMP_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000001");
    let pi = ("PI_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000002");
    let claude = (
        "CLAUDE_CODE_SESSION_ID",
        "5f0c7d1e-2222-4333-8444-555566667777",
    );
    let cases: [(&[Binding], Option<&str>); 6] = [
        (
            &[("CTX_SESSION", "ctx-agent-x7k2p"), omp, claude],
            Some("ctx-agent-x7k2p"),
        ),
        (&[omp, pi, claude], Some(omp.1)),
        (&[pi, claude], Some(pi.1)),
        (&[("CTX_SESSION", ""), claude], Some(claude.1)),
        (&[claude], Some(claude.1)),
        (&[], None),
    ];
    for (environment, expected) in cases {
        let (client, sent) = sessions_sent(environment)?;
        assert_eq!(client.exit_code, 0, "{}", client.stderr);
        assert_eq!(client.stderr, "", "{environment:?}");
        assert_eq!(sent, [expected.map(str::to_owned)], "{environment:?}");
    }
    Ok(())
}

/// A session id the header cannot carry, white space included, is not sent, and no later
/// variable is sent in its place: the caller chose that name, so the command still runs,
/// unnamed, and the caller is told in one line which variable did not go.
#[test]
fn a_session_the_header_cannot_carry_is_not_sent_and_the_caller_is_told() -> Result<()> {
    for unfit in ["chief of staff", " "] {
        let (client, sent) = sessions_sent(&[
            ("CTX_SESSION", unfit),
            ("OMP_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000001"),
        ])?;
        assert_eq!(client.exit_code, 0, "{unfit:?}: {}", client.stderr);
        assert_eq!(client.stdout, "answered\n", "{unfit:?}");
        assert_eq!(sent, [None::<String>], "{unfit:?}");
        assert!(
            client
                .stderr
                .starts_with("ctx: CTX_SESSION is not sent to the server as this session's id"),
            "{unfit:?}: {}",
            client.stderr
        );
        assert_eq!(
            client.stderr.lines().count(),
            1,
            "{unfit:?}: {}",
            client.stderr
        );
    }
    Ok(())
}

/// The protocol version this release's client speaks, pinned as the wire carries it, so a
/// release that changes it changes these tests too.
const PROTOCOL: &str = "1";

/// Every request names the protocol version the client speaks in `X-Ctx-Protocol`, so a server
/// can refuse a route whose meaning changed since that version rather than misread it: a read, a
/// write, and the client's own asks for the server's version, at `/v1/protocol` and, for this
/// server that does not answer it, at `/healthz`.
#[test]
fn every_request_names_the_client_s_protocol_version() -> Result<()> {
    let marker = json!({
        "marker": {
            "version": 1,
            "window": "20260102T000000Z-0000",
            "advanced_at": "2026-01-02T01:00:00Z",
            "by": "ada@example.com (s)"
        }
    });
    let server = FakeHttp::start()?;
    server.json("/v1/command", 200, &answered())?;
    server.json("/v1/marker/advance", 200, &marker.to_string())?;
    server.json_with_headers(
        "/healthz",
        200,
        &[("X-Ctx-Protocol", PROTOCOL)],
        &json!({ "ok": true }).to_string(),
    )?;
    let origin = server.url("");
    let environment = [("CTX_SERVER", origin.as_str())];
    let ran = [
        search(&environment)?,
        run_client(
            &["marker", "advance", "20260102T000000Z-0000"],
            None,
            &environment,
        )?,
        run_client(&["protocol"], None, &environment)?,
    ];

    for client in &ran {
        assert_eq!(client.exit_code, 0, "{}{}", client.stdout, client.stderr);
    }
    let sent: Vec<(String, Option<String>)> = server
        .requests()?
        .into_iter()
        .map(|request| (request.path, request.headers.get("x-ctx-protocol").cloned()))
        .collect();
    let expected: Vec<(String, Option<String>)> = [
        "/v1/command",
        "/v1/marker/advance",
        "/v1/protocol",
        "/healthz",
    ]
    .iter()
    .map(|path| ((*path).to_owned(), Some(PROTOCOL.to_owned())))
    .collect();
    assert_eq!(sent, expected);
    Ok(())
}

/// `ctx protocol` says which protocol version this client speaks and which one its server names
/// in `X-Ctx-Protocol` on `/healthz`, whatever the probe's status: a wedged copy still speaks
/// one. These servers answer `/v1/protocol` with a `404` that names no version, as a server
/// older than the version header does, so the client asks `/healthz` after it. A server older
/// than that header names none, which is protocol 0. An answer that names none and is not a
/// success is not taken for such a server, since a proxy or a load balancer answers like that in
/// the server's place: the command fails and names the status. With no server configured, only
/// the client's version is known.
#[test]
fn ctx_protocol_reports_the_client_s_and_the_server_s_versions() -> Result<()> {
    let reported = |headers: &[(&str, &str)], status: u16| -> Result<Local> {
        let server = FakeHttp::start()?;
        let body = json!({ "ok": status == 200 }).to_string();
        server.json_with_headers("/healthz", status, headers, &body)?;
        run_client(
            &["--json", "protocol"],
            None,
            &[("CTX_SERVER", &server.url(""))],
        )
    };
    let newer = reported(&[("X-Ctx-Protocol", "7")], 200)?;
    let wedged = reported(&[("X-Ctx-Protocol", "7")], 503)?;
    let older = reported(&[], 200)?;
    let proxy = reported(&[], 403)?;
    let alone = run_client(&["--json", "protocol"], None, &[])?;

    for (what, client, server) in [
        ("a newer server", &newer, json!(7)),
        ("a wedged server", &wedged, json!(7)),
        ("a server older than the header", &older, json!(0)),
        ("no server", &alone, serde_json::Value::Null),
    ] {
        assert_eq!(
            client.exit_code, 0,
            "{what}: {}{}",
            client.stdout, client.stderr
        );
        let envelope: serde_json::Value = serde_json::from_str(client.stdout.trim())
            .with_context(|| format!("{what}: decoding {}", client.stdout))?;
        assert_eq!(
            envelope.get("data"),
            Some(&json!({ "client": 1, "server": server })),
            "{what}: {}",
            client.stdout
        );
    }
    assert_ne!(proxy.exit_code, 0, "{}", proxy.stdout);
    assert!(
        format!("{}{}", proxy.stdout, proxy.stderr).contains("403"),
        "the failure does not name the status: {}{}",
        proxy.stdout,
        proxy.stderr
    );
    Ok(())
}

/// One path a stand-in server answers: the path, its status and its headers.
type Answer<'a> = (&'a str, u16, &'a [(&'a str, &'a str)]);

/// Runs `ctx --json protocol` against a server that answers each of `answers`, and every other
/// path `404` naming no version. Returns the client and the paths it asked, in order.
fn protocol_against(answers: &[Answer<'_>]) -> Result<(Local, Vec<String>)> {
    let server = FakeHttp::start()?;
    for (path, status, headers) in answers {
        let body = json!({ "ok": *status == 200 }).to_string();
        server.json_with_headers(path, *status, headers, &body)?;
    }
    let client = run_client(
        &["--json", "protocol"],
        None,
        &[("CTX_SERVER", &server.url(""))],
    )?;
    let asked = server
        .requests()?
        .into_iter()
        .map(|request| request.path)
        .collect();
    Ok((client, asked))
}

/// `ctx protocol` asks `/v1/protocol` first, because on a `run.app` URL Cloud Run's front end
/// answers `/healthz` itself with a `404` that names no version. A server with the route names
/// its version there. A v0.3.2 to v0.3.5 server has no such route, but its `404` names its
/// version as every answer of it does, which is enough: the client does not ask `/healthz`,
/// which Cloud Run would answer in its place. Only a `404` that names no version, a server's
/// older than the version header, sends the client on to `/healthz`.
#[test]
fn ctx_protocol_asks_the_protocol_route_first_and_healthz_only_after_a_bare_404() -> Result<()> {
    let newer = protocol_against(&[
        ("/v1/protocol", 200, &[("X-Ctx-Protocol", "7")]),
        ("/healthz", 200, &[("X-Ctx-Protocol", "6")]),
    ])?;
    // Cloud Run's own 404 at /healthz names no version.
    let without_the_route = protocol_against(&[
        ("/v1/protocol", 404, &[("X-Ctx-Protocol", "1")]),
        ("/healthz", 404, &[]),
    ])?;
    let older_than_the_header = protocol_against(&[("/healthz", 200, &[])])?;

    for (what, (client, asked), server, paths) in [
        ("a server with the route", &newer, 7, &["/v1/protocol"][..]),
        (
            "a v0.3.5 server behind Cloud Run",
            &without_the_route,
            1,
            &["/v1/protocol"][..],
        ),
        (
            "a server older than the header",
            &older_than_the_header,
            0,
            &["/v1/protocol", "/healthz"][..],
        ),
    ] {
        assert_eq!(
            client.exit_code, 0,
            "{what}: {}{}",
            client.stdout, client.stderr
        );
        let envelope: serde_json::Value = serde_json::from_str(client.stdout.trim())
            .with_context(|| format!("{what}: decoding {}", client.stdout))?;
        assert_eq!(
            envelope.get("data"),
            Some(&json!({ "client": 1, "server": server })),
            "{what}: {}",
            client.stdout
        );
        assert_eq!(asked, paths, "{what}");
    }
    Ok(())
}

/// An answer at `/v1/protocol` that names no version and is not a server's `404` — Cloud Run
/// refusing the caller's token, say — fails `ctx protocol` at once, naming the status, as
/// `/healthz`'s did before. A `404` that names no version at both paths is what a proxy answers,
/// or Cloud Run for a server older than the version header: it fails with exit 2 and names both
/// paths, so it is not taken for protocol 0.
#[test]
fn ctx_protocol_fails_when_no_answer_names_a_version() -> Result<()> {
    let refused = protocol_against(&[("/v1/protocol", 403, &[])])?;
    let nowhere = protocol_against(&[])?;

    for (what, (client, asked), words, paths) in [
        (
            "a refusal at the route",
            &refused,
            &[
                "403",
                "/v1/protocol",
                "naming no protocol version",
                "proxy's",
            ][..],
            &["/v1/protocol"][..],
        ),
        (
            "a 404 at both paths",
            &nowhere,
            &[
                "404",
                "/v1/protocol",
                "/healthz",
                "naming no protocol version",
                "proxy's",
            ][..],
            &["/v1/protocol", "/healthz"][..],
        ),
    ] {
        let printed = format!("{}{}", client.stdout, client.stderr);
        assert_eq!(client.exit_code, 2, "{what}: {printed}");
        for word in words {
            assert!(printed.contains(*word), "{what}: no {word:?} in {printed}");
        }
        assert_eq!(asked, paths, "{what}");
    }
    Ok(())
}

/// A token says which server it is for, and the server that receives one minted for another
/// could call that one as the caller. So a `CTX_SERVER_AUDIENCE` that names another server is
/// refused before a token is minted or a request is sent.
#[test]
fn a_token_minted_for_another_server_is_never_sent() -> Result<()> {
    let server = FakeHttp::start()?;
    server.json("/v1/command", 200, &answered())?;
    let metadata = metadata("token-for-another-server")?;
    let origin = server.url("");

    let client = search(&[
        ("CTX_SERVER", &origin),
        ("CTX_SERVER_AUDIENCE", "https://ctx.example.run.app"),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
    ])?;

    let received = server.requests()?;
    let minted = metadata.hits(IDENTITY)?;
    assert_ne!(client.exit_code, 0, "{}", client.stdout);
    assert!(
        client.stderr.contains("CTX_SERVER_AUDIENCE") && client.stderr.contains(&origin),
        "the refusal does not name the audience and the origin it must equal: {}",
        client.stderr
    );
    assert!(
        received.is_empty(),
        "the server received a request: {received:?}"
    );
    assert_eq!(minted, 0, "a token was minted for another server");
    Ok(())
}

/// Plain HTTP to a server off this machine and off the tailnet carries the caller's records
/// and token readable by anyone on the way. The client refuses it before it mints a token or
/// sends anything; the proxy stands in for the network a request to such a host would cross.
#[test]
fn plain_http_to_a_host_off_this_machine_is_refused() -> Result<()> {
    let proxy = FakeHttp::start()?;
    let metadata = metadata("token-for-plain-http")?;
    let proxy_url = proxy.url("");

    let client = search(&[
        ("CTX_SERVER", "http://ctx.example.com"),
        ("CTX_SERVER_AUDIENCE", "http://ctx.example.com"),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
        ("HTTP_PROXY", &proxy_url),
        ("http_proxy", &proxy_url),
        ("NO_PROXY", "127.0.0.1,localhost"),
        ("no_proxy", "127.0.0.1,localhost"),
    ])?;

    let sent = proxy.requests()?;
    let minted = metadata.hits(IDENTITY)?;
    assert_ne!(client.exit_code, 0, "{}", client.stdout);
    assert!(
        client.stderr.contains("http://ctx.example.com") && client.stderr.contains("https://"),
        "the refusal does not name the server and what it takes: {}",
        client.stderr
    );
    assert!(sent.is_empty(), "a request left the client: {sent:?}");
    assert_eq!(minted, 0, "a token was minted for a plain-HTTP server");
    Ok(())
}

/// Plain HTTP is taken only to loopback or a tailnet name, where something other than TLS
/// keeps the record and the token from being read on the way. A proxy named by `HTTP_PROXY`
/// or `ALL_PROXY` would carry both outside that, unencrypted, so plain HTTP goes to the server
/// direct (round-4 review of d5655fc, N4-6). The tailnet name here resolves nowhere, so that
/// request fails; what matters is that the proxy never saw it.
#[test]
fn plain_http_never_goes_through_a_proxy() -> Result<()> {
    let proxy = FakeHttp::start()?;
    proxy.json(
        "http://ctx-memory.example.ts.net/v1/command",
        200,
        &answered(),
    )?;
    let proxy_url = proxy.url("");
    let server = FakeHttp::start()?;
    server.json("/v1/command", 200, &answered())?;
    let proxied = [
        ("HTTP_PROXY", proxy_url.as_str()),
        ("http_proxy", proxy_url.as_str()),
        ("ALL_PROXY", proxy_url.as_str()),
        ("all_proxy", proxy_url.as_str()),
    ];

    let tailnet = search(
        &[
            &[("CTX_SERVER", "http://ctx-memory.example.ts.net")][..],
            &proxied[..],
        ]
        .concat(),
    )?;
    let loopback_url = server.url("");
    let loopback = search(&[&[("CTX_SERVER", loopback_url.as_str())][..], &proxied[..]].concat())?;

    let sent = proxy.requests()?;
    assert!(
        sent.is_empty(),
        "plain HTTP went through the proxy: {:?}",
        sent.iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !tailnet.stderr.contains("https://"),
        "the tailnet name was refused rather than tried: {}",
        tailnet.stderr
    );
    assert_eq!(loopback.exit_code, 0, "{}", loopback.stderr);
    assert_eq!(loopback.stdout, "answered\n");
    assert_eq!(server.requests()?.len(), 1);
    Ok(())
}

/// An answer that redirects to another origin is not followed: the request, its record and
/// its token are meant for the server named, and go nowhere else.
#[test]
fn a_redirect_to_another_origin_is_not_followed() -> Result<()> {
    let elsewhere = FakeHttp::start()?;
    elsewhere.json("/v1/command", 200, &answered())?;
    let server = FakeHttp::start()?;
    server.json_with_headers(
        "/v1/command",
        307,
        &[("Location", &elsewhere.url("/v1/command"))],
        "{}",
    )?;
    let metadata = metadata("token-for-the-redirecting-server")?;
    let origin = server.url("");

    let client = search(&[
        ("CTX_SERVER", &origin),
        ("CTX_SERVER_AUDIENCE", &origin),
        ("GCE_METADATA_HOST", &metadata.address().to_string()),
    ])?;

    let followed = elsewhere.requests()?;
    let asked = server.requests()?;
    assert_ne!(client.exit_code, 0, "{}", client.stdout);
    assert!(
        client.stderr.contains("another origin"),
        "the refusal does not say where the redirect went: {}",
        client.stderr
    );
    assert!(
        followed.is_empty(),
        "the client followed the redirect to another origin: {followed:?}"
    );
    assert_eq!(asked.len(), 1, "{asked:?}");
    Ok(())
}

/// Every control character in `text` but a tab or a newline.
fn controls(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect()
}

/// What a server prints is shown without control characters, even from a server that left
/// them in: an older server printed source text as it came.
#[test]
fn a_server_s_output_is_printed_without_control_characters() -> Result<()> {
    let server = FakeHttp::start()?;
    server.json(
        "/v1/command",
        200,
        &json!({
            "stdout": "the vendor \u{1b}]0;owned\u{7}agreed \u{9b}2J\n",
            "stderr": "warning \u{1b}[2Jhere\n",
            "exit_code": 0
        })
        .to_string(),
    )?;

    let client = search(&[("CTX_SERVER", &server.url(""))])?;

    assert_eq!(client.exit_code, 0, "{}", client.stderr);
    assert_eq!(client.stdout, "the vendor ]0;ownedagreed 2J\n");
    assert_eq!(
        controls(&client.stderr),
        Vec::<char>::new(),
        "{:?}",
        client.stderr
    );
    Ok(())
}

/// A server's error message is its own text, and is shown without control characters.
#[test]
fn a_server_s_error_is_printed_without_control_characters() -> Result<()> {
    let server = FakeHttp::start()?;
    server.json(
        "/v1/command",
        400,
        &json!({ "error": "no \u{1b}]0;owned\u{7}such \u{9b}2Jcommand" }).to_string(),
    )?;

    let client = search(&[("CTX_SERVER", &server.url(""))])?;

    assert_eq!(client.exit_code, 2, "{}", client.stdout);
    assert!(
        client.stderr.contains("no ]0;ownedsuch 2Jcommand"),
        "the error is missing: {:?}",
        client.stderr
    );
    assert_eq!(
        controls(&client.stderr),
        Vec::<char>::new(),
        "{:?}",
        client.stderr
    );
    Ok(())
}

/// The words of the one line a client writes on standard error each time it sends a deferred
/// write again.
const SENT_AGAIN: &str = "sending it again in";

/// What the server answers a write with while it hands its write lease over: `503`, with how
/// many seconds to wait in `Retry-After`.
fn deferral(server: &FakeHttp, route: &str, pauses: &[&str]) -> Result<()> {
    let body = json!({ "error": "the server lease is held by server@old-copy" }).to_string();
    for (n, pause) in pauses.iter().enumerate() {
        if n == 0 {
            server.json_with_headers(route, 503, &[("Retry-After", pause)], &body)?;
        } else {
            server.then_json_with_headers(route, 503, &[("Retry-After", pause)], &body)?;
        }
    }
    Ok(())
}

/// A rollout starts the new copy of the server before the old one has given the write lease
/// up, and the new copy answers a write `503` with `Retry-After` until it takes the lease.
/// The ingest cycle's write failed there and the cycle carried on with its records unwritten;
/// each write route now sends the write again after the pause the server asks for, saying so
/// once per try on standard error, and lands it.
#[test]
fn every_write_is_sent_again_after_the_pause_a_deferring_server_asks_for() -> Result<()> {
    let marker = json!({
        "marker": {
            "version": 1,
            "window": "20260102T000000Z-0000",
            "advanced_at": "2026-01-02T01:00:00Z",
            "by": "ada@example.com (s)"
        }
    })
    .to_string();
    let committed = json!({ "commit": "c0ffee" }).to_string();
    let batch = json!([{ "path": "decisions/cadence.md", "content": "a record" }]).to_string();
    let page = json!({
        "page": {
            "version": 1,
            "window": "20260102T000000Z-0000",
            "worked": 3,
            "last": { "ts": "2026-01-01T23:00:00Z", "pointer": "echo:C0EXAMPLE03/record" },
            "recorded_at": "2026-01-02T01:00:00Z",
            "by": "ada@example.com (s)"
        }
    })
    .to_string();
    let writes: [(&[&str], Option<&str>, &str, &str); 5] = [
        (
            &["record", "put", "decisions/cadence.md", "-"],
            Some("a record"),
            "/v1/records/put",
            committed.as_str(),
        ),
        (
            &["record", "put", "--batch", "-"],
            Some(batch.as_str()),
            "/v1/records/put-batch",
            committed.as_str(),
        ),
        (
            &["record", "rm", "decisions/cadence.md"],
            None,
            "/v1/records/delete",
            committed.as_str(),
        ),
        (
            &["marker", "advance", "20260102T000000Z-0000"],
            None,
            "/v1/marker/advance",
            marker.as_str(),
        ),
        (
            &[
                "marker",
                "page",
                "20260102T000000Z-0000",
                "echo:C0EXAMPLE03/record",
            ],
            None,
            "/v1/marker/page",
            page.as_str(),
        ),
    ];
    for (args, input, route, answer) in writes {
        let server = FakeHttp::start()?;
        deferral(&server, route, &["1", "1"])?;
        server.then_json(route, 200, answer)?;

        let started = Instant::now();
        let client = run_client(args, input, &[("CTX_SERVER", &server.url(""))])?;
        let took = started.elapsed();

        assert_eq!(client.exit_code, 0, "{route}: {}", client.stderr);
        assert_eq!(server.hits(route)?, 3, "{route}: {}", client.stderr);
        assert_eq!(
            client
                .stderr
                .lines()
                .filter(|line| line.contains(SENT_AGAIN))
                .count(),
            2,
            "{route}: one line per try sent again: {}",
            client.stderr
        );
        assert!(
            took >= Duration::from_secs(2),
            "{route}: the client did not wait the pauses the server asked for: {took:?}"
        );
    }
    Ok(())
}

/// A write waits at most 180 seconds in all. A pause that would take it past that is not
/// waited: the write fails at once, says why, and says nothing was written.
#[test]
fn a_deferred_write_is_given_up_once_the_next_pause_would_pass_the_bound() -> Result<()> {
    let server = FakeHttp::start()?;
    deferral(&server, "/v1/records/put", &["1", "179"])?;
    server.then_json(
        "/v1/records/put",
        200,
        &json!({ "commit": "c0ffee" }).to_string(),
    )?;

    let started = Instant::now();
    let client = run_client(
        &["record", "put", "decisions/cadence.md", "-"],
        Some("a record"),
        &[("CTX_SERVER", &server.url(""))],
    )?;
    let took = started.elapsed();

    assert_ne!(client.exit_code, 0, "{}", client.stdout);
    assert_eq!(server.hits("/v1/records/put")?, 2, "{}", client.stderr);
    assert_eq!(
        client
            .stderr
            .lines()
            .filter(|line| line.contains(SENT_AGAIN))
            .count(),
        1,
        "{}",
        client.stderr
    );
    for words in ["180 s", "nothing was written", "server@old-copy"] {
        assert!(
            client.stderr.contains(words),
            "the error does not say {words:?}: {}",
            client.stderr
        );
    }
    assert!(
        took < Duration::from_secs(60),
        "the client waited a pause past its bound: {took:?}"
    );
    Ok(())
}

/// Only a deferral is sent again. A `503` with no `Retry-After` is the server saying it cannot
/// tell whether the write landed, which a second send could write twice; and a read is never
/// sent again, whatever it is answered.
#[test]
fn only_a_deferred_write_is_sent_again() -> Result<()> {
    let server = FakeHttp::start()?;
    server.json(
        "/v1/records/put",
        503,
        &json!({ "error": "the write's outcome is unknown" }).to_string(),
    )?;
    server.json_with_headers(
        "/v1/command",
        503,
        &[("Retry-After", "1")],
        &json!({ "error": "busy" }).to_string(),
    )?;

    let unknown = run_client(
        &["record", "put", "decisions/cadence.md", "-"],
        Some("a record"),
        &[("CTX_SERVER", &server.url(""))],
    )?;
    let read = search(&[("CTX_SERVER", &server.url(""))])?;

    assert_ne!(unknown.exit_code, 0, "{}", unknown.stdout);
    assert_eq!(server.hits("/v1/records/put")?, 1, "{}", unknown.stderr);
    assert_ne!(read.exit_code, 0, "{}", read.stdout);
    assert_eq!(server.hits("/v1/command")?, 1, "{}", read.stderr);
    for client in [&unknown, &read] {
        assert!(!client.stderr.contains(SENT_AGAIN), "{}", client.stderr);
    }
    Ok(())
}

/// `marker advance newest` names no window: the server resolves `newest` when it moves the
/// marker. Sent again after a pause, it could move the marker past a window the fetch completed
/// during that pause, which no session read. So a deferred `newest` is never sent again; the
/// advance fails and the caller names the window it handled.
#[test]
fn a_deferred_advance_to_newest_is_not_sent_again() -> Result<()> {
    let server = FakeHttp::start()?;
    deferral(&server, "/v1/marker/advance", &["1"])?;

    let client = run_client(
        &["marker", "advance", "newest"],
        None,
        &[("CTX_SERVER", &server.url(""))],
    )?;

    assert_ne!(client.exit_code, 0, "{}", client.stdout);
    assert_eq!(server.hits("/v1/marker/advance")?, 1, "{}", client.stderr);
    assert!(!client.stderr.contains(SENT_AGAIN), "{}", client.stderr);
    Ok(())
}
