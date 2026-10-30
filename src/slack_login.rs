//! `ctx slack login`: the browser sign-in that seeds the Slack credential.
//!
//! It signs in with the ctx-memory Slack app, an internal app of the workspace that only ctx uses
//! (`slack/app-manifest.yaml`; `docs/sources.md`, "The ctx-memory Slack app"). The
//! flow is Slack's OAuth v2 flow for a user token: the browser authorizes with `user_scope`, and
//! the code is exchanged at `oauth.v2.access` with the app's client id and secret, which come from
//! the `slack_app` credential. The app has token rotation on, so the token expires in 12 hours
//! and comes with a refresh token, which the fetch job redeems. It is saved as the `slack`
//! credential.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::slack::{
    AppCredential, AppKind, SlackApp, SLACK_ACCESS_URL, SLACK_AUTHORIZE_URL, SLACK_REDIRECT_URI,
    SLACK_SCOPES,
};
use crate::credentials::{self, Secret};
use crate::output::{self, Outcome, Sink};

/// The loopback address the Slack app redirects to; the only one its manifest registers.
const CALLBACK_ADDRESS: &str = "127.0.0.1:3118";

/// Runs the browser sign-in and saves the Slack credential.
///
/// # Errors
///
/// Returns an error when the app's credential cannot be read, the callback port cannot be
/// bound, the sign-in is refused, or the credential cannot be saved. The credential is saved
/// before this returns success.
pub fn run(sink: &mut Sink<'_>, json: bool) -> Result<Outcome> {
    let app = SlackApp::load()?;
    let state = random_state()?;
    let listener = TcpListener::bind(CALLBACK_ADDRESS).with_context(|| {
        format!("binding {CALLBACK_ADDRESS} for the Slack callback; is another sign-in running?")
    })?;
    let authorize = authorize_url(&app.client_id, &state)?;
    writeln!(
        sink.err,
        "Open this URL in a browser to sign in to Slack with the ctx-memory app:\n\n{authorize}\n"
    )?;
    writeln!(
        sink.err,
        "If the browser cannot reach {CALLBACK_ADDRESS}, paste the redirected URL here and press Enter."
    )?;
    let _ = open_browser(&authorize);
    let code = await_code(listener, &state)?;
    finish(
        sink,
        json,
        &app,
        &code,
        SLACK_ACCESS_URL,
        Utc::now(),
        |credential| credentials::save("slack", credential),
    )
}

/// Exchanges the code at `access_url`, saves the credential with `save`, and then reports who
/// signed in. Nothing it prints, and nothing it saves, carries the client secret.
fn finish(
    sink: &mut Sink<'_>,
    json: bool,
    app: &SlackApp,
    code: &str,
    access_url: &str,
    now: DateTime<Utc>,
    save: impl FnOnce(&Secret) -> Result<()>,
) -> Result<Outcome> {
    let credential = exchange(app, code, access_url, now)?;
    // Saved before success is reported: the fetch job reads only what is stored.
    save(&credential.to_secret()?).context("saving the Slack credential")?;
    output::emit(
        sink,
        json,
        "slack login",
        &serde_json::json!({
            "saved": true,
            "team_id": credential.team_id,
            "user_id": credential.user_id,
        }),
        |_, out| {
            writeln!(
                out,
                "Signed in to Slack as {} in workspace {}. The credential is saved.",
                credential.user_id, credential.team_id
            )
        },
    )?;
    Ok(Outcome::Pass)
}

/// Builds the authorization URL: the app's client id, its user scopes, the registered redirect,
/// and the `state` the callback must return. It asks for no bot scope.
fn authorize_url(client_id: &str, state: &str) -> Result<String> {
    let user_scope = SLACK_SCOPES.join(",");
    let url = reqwest::Url::parse_with_params(
        SLACK_AUTHORIZE_URL,
        &[
            ("client_id", client_id),
            ("user_scope", user_scope.as_str()),
            ("redirect_uri", SLACK_REDIRECT_URI),
            ("state", state),
        ],
    )
    .context("building the Slack authorize URL")?;
    Ok(url.to_string())
}

/// Exchanges the authorization code for the user token at `access_url`, presenting the app's
/// client id and secret with HTTP Basic, as Slack recommends for `oauth.v2.access`.
///
/// Slack answers `200` even for a failure, with `{"ok":false,"error":...}`, so the body is read
/// before the status. The app has token rotation on, so its token comes with an expiry and a
/// refresh token; a token without them means rotation is off, and the fetch could not renew the
/// sign-in the way it renews every other, so it is refused rather than stored.
fn exchange(
    app: &SlackApp,
    code: &str,
    access_url: &str,
    now: DateTime<Utc>,
) -> Result<AppCredential> {
    let form = [
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", SLACK_REDIRECT_URI.to_owned()),
    ];
    let body = app.post(access_url, &form)?;
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bail!("Slack refused the sign-in: {error:?}");
    }
    let user = body
        .get("authed_user")
        .ok_or_else(|| anyhow!("the Slack token response has no authed_user"))?;
    let expires_in = user.get("expires_in").and_then(Value::as_i64);
    let refresh = user
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty());
    let (Some(expires_in), Some(refresh)) = (expires_in, refresh) else {
        bail!(
            "Slack issued a token that does not expire, so this Slack app has token rotation off; ctx needs the ctx-memory app with token rotation on (slack/app-manifest.yaml). Turn it on under OAuth & Permissions (it cannot be turned off again) and sign in again. Nothing was saved"
        );
    };
    Ok(AppCredential {
        kind: AppKind::App,
        access: text(user.get("access_token"), "authed_user.access_token")?,
        refresh: refresh.to_owned(),
        expires: now + chrono::Duration::seconds(expires_in),
        client_id: app.client_id.clone(),
        token_url: access_url.to_owned(),
        team_id: text(body.get("team").and_then(|team| team.get("id")), "team.id")?,
        user_id: text(user.get("id"), "authed_user.id")?,
    })
}

/// A non-empty string field of the token response, or an error naming the field.
fn text(value: Option<&Value>, field: &str) -> Result<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the Slack token response has no {field}"))
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
    let body =
        "ctx received the Slack sign-in. Return to the terminal to see whether it was saved.";
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
mod tests;
