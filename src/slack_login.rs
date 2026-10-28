//! `ctx slack login`: the browser sign-in that seeds the Slack credential.
//!
//! This reproduces omp's own sign-in (`docs/sources.md`, "Slack source"): an OAuth
//! authorization-code flow with PKCE against the Claude Code Slack app, a public client that
//! needs no secret. The code is exchanged for a user token at Slack's token endpoint and
//! saved as the `slack` credential. Only the fetch job ever refreshes it afterwards.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use anyhow::{anyhow, bail, Context as _, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::api::client;
use crate::api::slack::{
    SlackCredential, SLACK_AUTHORIZE_URL, SLACK_CLIENT_ID, SLACK_REDIRECT_URI, SLACK_RESOURCE,
    SLACK_SCOPES, SLACK_TOKEN_URL,
};
use crate::credentials;
use crate::output::{self, Outcome, Sink};

/// Slack's token lifetime, used only when a token response omits `expires_in`.
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 43_200;
/// The loopback address the Slack app redirects to; the only one this client accepts.
const CALLBACK_ADDRESS: &str = "127.0.0.1:3118";

/// Runs the browser sign-in and saves the Slack credential.
///
/// # Errors
///
/// Returns an error when the callback port cannot be bound, the sign-in is refused, or the
/// credential cannot be saved. The credential is saved before this returns success, so the
/// fetch job never redeems a refresh token that was not stored.
pub fn run(sink: &mut Sink<'_>, json: bool) -> Result<Outcome> {
    let (verifier, challenge) = pkce()?;
    let state = random_state()?;
    let listener = TcpListener::bind(CALLBACK_ADDRESS).with_context(|| {
        format!("binding {CALLBACK_ADDRESS} for the Slack callback; is another sign-in running?")
    })?;
    let authorize = authorize_url(&state, &challenge)?;
    writeln!(
        sink.err,
        "Open this URL in a browser to sign in to Slack:\n\n{authorize}\n"
    )?;
    writeln!(
        sink.err,
        "If the browser cannot reach {CALLBACK_ADDRESS}, paste the redirected URL here and press Enter."
    )?;
    let _ = open_browser(&authorize);
    let code = await_code(listener, &state)?;
    let credential = exchange(&code, &verifier, Utc::now(), |url, form| {
        let (status, _headers, body) = client::post_form(url, form)?;
        if !status.is_success() {
            bail!("the Slack token endpoint returned {status}");
        }
        Ok(body)
    })?;
    // Saved before anything uses it: a token that is not stored is lost the moment it is used.
    credentials::save("slack", &credential.to_secret()?).context("saving the Slack credential")?;
    output::emit(
        sink,
        json,
        "slack login",
        &serde_json::json!({ "saved": true }),
        |_, out| writeln!(out, "Signed in to Slack. The credential is saved."),
    )?;
    Ok(Outcome::Pass)
}

/// Builds the authorization URL omp builds, with the same parameters in the same order.
fn authorize_url(state: &str, challenge: &str) -> Result<String> {
    let scope = SLACK_SCOPES.join(" ");
    let url = reqwest::Url::parse_with_params(
        SLACK_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", SLACK_CLIENT_ID),
            ("scope", scope.as_str()),
            ("resource", SLACK_RESOURCE),
            ("redirect_uri", SLACK_REDIRECT_URI),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .context("building the Slack authorize URL")?;
    Ok(url.to_string())
}

/// Exchanges the authorization code for a user token, with PKCE and no client secret.
fn exchange(
    code: &str,
    verifier: &str,
    now: DateTime<Utc>,
    post: impl Fn(&str, &[(&str, String)]) -> Result<Value>,
) -> Result<SlackCredential> {
    let form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", SLACK_REDIRECT_URI.to_owned()),
        ("client_id", SLACK_CLIENT_ID.to_owned()),
        ("code_verifier", verifier.to_owned()),
        ("resource", SLACK_RESOURCE.to_owned()),
    ];
    let body = post(SLACK_TOKEN_URL, &form)?;
    if body.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bail!("Slack rejected the sign-in: {error:?}");
    }
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("the Slack token response has no access_token"))?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("the Slack token response has no refresh_token"))?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECONDS);
    Ok(SlackCredential {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now + chrono::Duration::seconds(expires_in),
        client_id: SLACK_CLIENT_ID.to_owned(),
        token_url: SLACK_TOKEN_URL.to_owned(),
        resource: SLACK_RESOURCE.to_owned(),
    })
}

/// Reads the authorization code from the first of two paths to complete: the local HTTP
/// callback, or a redirect URL pasted on standard input for a box without a forwarded port.
fn await_code(listener: TcpListener, state: &str) -> Result<String> {
    let (sender, receiver) = mpsc::channel::<Result<String>>();
    let http_sender = sender.clone();
    std::thread::spawn(move || {
        let _ = http_sender.send(accept_callback(&listener));
    });
    std::thread::spawn(move || {
        let _ = sender.send(read_pasted_url());
    });
    let target = receiver
        .recv()
        .context("waiting for the Slack sign-in callback")??;
    parse_callback(&target, state)
}

/// Extracts the authorization code from a callback target, checking the state.
fn parse_callback(target: &str, expected_state: &str) -> Result<String> {
    let url = reqwest::Url::parse(target)
        .or_else(|_| reqwest::Url::parse(&format!("http://localhost{target}")))
        .with_context(|| format!("parsing the callback target {target:?}"))?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        bail!("Slack returned an authorization error: {error}");
    }
    let state = state.ok_or_else(|| anyhow!("the callback carried no state"))?;
    if state != expected_state {
        bail!("the callback state did not match the request; discard it and sign in again");
    }
    code.ok_or_else(|| anyhow!("the callback carried no authorization code"))
}

/// Accepts one callback request, answers the browser, and returns its target.
fn accept_callback(listener: &TcpListener) -> Result<String> {
    let (mut stream, _) = listener.accept().context("accepting the Slack callback")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("reading the callback request")?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("the callback request carried no target"))?
        .to_owned();
    let body = "You are signed in to Slack for ctx. You may close this tab.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(target)
}

/// Reads one non-empty line from standard input: a pasted redirect URL. End of input never
/// completes the sign-in on its own, so this never races the HTTP callback with an empty read.
fn read_pasted_url() -> Result<String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if handle
            .read_line(&mut line)
            .context("reading a pasted callback URL")?
            == 0
        {
            std::thread::park();
            continue;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
}

/// Opens `url` in the box's default browser, best effort.
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("opening the browser")?;
    Ok(())
}

/// A PKCE verifier and its S256 challenge.
fn pkce() -> Result<(String, String)> {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32)?);
    let challenge = challenge_of(&verifier);
    Ok((verifier, challenge))
}

/// The S256 challenge for a verifier: base64url(SHA-256(verifier)), without padding.
fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A random `state`: 16 bytes as hex.
fn random_state() -> Result<String> {
    use std::fmt::Write as _;
    let mut state = String::with_capacity(32);
    for byte in random_bytes(16)? {
        write!(state, "{byte:02x}").context("formatting the sign-in state")?;
    }
    Ok(state)
}

/// Cryptographically random bytes from the operating system.
fn random_bytes(count: usize) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buffer = vec![0_u8; count];
    file.read_exact(&mut buffer)
        .context("reading /dev/urandom")?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn challenge_matches_the_rfc7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_carries_omps_parameters_in_order() -> Result<()> {
        let url = authorize_url("STATE", "CHALLENGE")?;
        let parsed = reqwest::Url::parse(&url).context("parsing the authorize URL")?;
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("slack.com"));
        assert_eq!(parsed.path(), "/oauth/v2_user/authorize");
        let query = parsed.query().context("authorize URL has a query")?;
        assert!(
            query.starts_with("response_type=code&client_id="),
            "unexpected order: {query}"
        );
        let pairs: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("client_id").map(String::as_str),
            Some(SLACK_CLIENT_ID)
        );
        assert_eq!(
            pairs.get("resource").map(String::as_str),
            Some(SLACK_RESOURCE)
        );
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(SLACK_REDIRECT_URI)
        );
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("CHALLENGE")
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(pairs.get("state").map(String::as_str), Some("STATE"));
        assert_eq!(
            pairs.get("scope").map(String::as_str),
            Some("channels:history channels:read groups:history groups:read users:read")
        );
        Ok(())
    }

    #[test]
    fn parse_callback_extracts_the_code() -> Result<()> {
        let code = parse_callback("/callback?code=abc123&state=xyz", "xyz")?;
        assert_eq!(code, "abc123");
        Ok(())
    }

    #[test]
    fn parse_callback_accepts_a_full_url() -> Result<()> {
        let code = parse_callback("http://localhost:3118/callback?code=abc&state=s", "s")?;
        assert_eq!(code, "abc");
        Ok(())
    }

    #[test]
    fn parse_callback_rejects_a_mismatched_state() {
        let error = parse_callback("/callback?code=abc&state=other", "expected")
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            error.contains("state did not match"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_callback_reports_a_slack_error() {
        let error = parse_callback("/callback?error=access_denied&state=s", "s")
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(error.contains("access_denied"), "unexpected error: {error}");
    }

    #[test]
    fn exchange_sends_pkce_without_a_secret_and_parses_the_token() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow!("constructing test time"))?;
        let seen = std::cell::RefCell::new(Vec::new());
        let credential = exchange("the-code", "the-verifier", now, |url, form| {
            let owned: Vec<(String, String)> = form
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect();
            seen.borrow_mut().push((url.to_owned(), owned));
            Ok(json!({
                "ok": true,
                "access_token": "xoxe.xoxp-token",
                "refresh_token": "xoxe-refresh",
                "expires_in": 43_200,
            }))
        })?;
        let calls = seen.into_inner();
        assert_eq!(calls.len(), 1);
        let (url, form) = calls.first().ok_or_else(|| anyhow!("no call"))?;
        assert_eq!(url, SLACK_TOKEN_URL);
        assert!(form.contains(&("grant_type".to_owned(), "authorization_code".to_owned())));
        assert!(form.contains(&("code".to_owned(), "the-code".to_owned())));
        assert!(form.contains(&("code_verifier".to_owned(), "the-verifier".to_owned())));
        assert!(form.contains(&("redirect_uri".to_owned(), SLACK_REDIRECT_URI.to_owned())));
        assert!(form.contains(&("client_id".to_owned(), SLACK_CLIENT_ID.to_owned())));
        assert!(form.contains(&("resource".to_owned(), SLACK_RESOURCE.to_owned())));
        assert!(!form.iter().any(|(key, _)| key == "client_secret"));
        assert_eq!(credential.access, "xoxe.xoxp-token");
        assert_eq!(credential.refresh, "xoxe-refresh");
        assert_eq!(credential.expires, now + chrono::Duration::seconds(43_200));
        assert_eq!(credential.client_id, SLACK_CLIENT_ID);
        assert_eq!(credential.token_url, SLACK_TOKEN_URL);
        Ok(())
    }

    #[test]
    fn exchange_rejects_a_slack_error() -> Result<()> {
        let now = DateTime::from_timestamp(1_700_000_000, 0)
            .ok_or_else(|| anyhow!("constructing test time"))?;
        let error = exchange("bad", "verifier", now, |_url, _form| {
            Ok(json!({ "ok": false, "error": "invalid_code" }))
        })
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
        assert!(error.contains("invalid_code"), "unexpected error: {error}");
        Ok(())
    }
}
