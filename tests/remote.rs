//! Remote mode's client: where it sends a caller's records and identity token.
//!
//! A `ctx` with `CTX_SERVER` set sends every command and record to that server, and inside
//! Google Cloud it sends an identity token with each. These tests run the real client against
//! loopback stand-ins, and a stand-in HTTP proxy for a server off this machine, so what they
//! prove is the request that left the client, or that none did.

mod support;

use std::process::Stdio;

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
    let cwd = support::scratch("remote-client-cwd")?;
    let mut command = support::binary();
    command
        .args(["search", "quarterly"])
        .current_dir(&cwd)
        .stdin(Stdio::null());
    for name in [
        "CTX_MEMORY",
        "CTX_SERVER",
        "CTX_SERVER_AUDIENCE",
        "CTX_IMPERSONATE_SA",
        "GCE_METADATA_HOST",
        "CTX_IAM_CREDENTIALS_URL",
        "CTX_SESSION",
        "OMP_SESSION_ID",
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
    let output = command.output().context("running ctx as a remote client");
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
/// apart sessions that reach it under one identity: `CTX_SESSION` when it is set, else the id
/// Oh My Pi gives every command its agent runs, else Claude Code's, else none. An empty
/// variable counts as unset.
#[test]
fn every_request_names_the_caller_s_session() -> Result<()> {
    let omp = ("OMP_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000001");
    let claude = (
        "CLAUDE_CODE_SESSION_ID",
        "5f0c7d1e-2222-4333-8444-555566667777",
    );
    let cases: [(&[Binding], Option<&str>); 5] = [
        (
            &[("CTX_SESSION", "ctx-agent-x7k2p"), omp, claude],
            Some("ctx-agent-x7k2p"),
        ),
        (&[omp, claude], Some(omp.1)),
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
