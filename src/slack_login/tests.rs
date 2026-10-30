//! `ctx slack login` tests: the authorize URL, the callback, the manifest the app is created
//! from, and the code exchange against a stub of Slack's token endpoint.

use std::cell::RefCell;

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};

use super::*;
use crate::api::slack::stub;

const CLIENT_ID: &str = "1234567890.0987654321";
/// A client secret no output, error or stored credential may carry.
const CLIENT_SECRET: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn app() -> Result<SlackApp> {
    SlackApp::from_secret(&Secret::new(format!(
        r#"{{"client_id":"{CLIENT_ID}","client_secret":"{CLIENT_SECRET}"}}"#
    )))
}

fn now() -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(1_700_000_000, 0).ok_or_else(|| anyhow!("constructing test time"))
}

/// What one `finish` run left behind: its result, what it printed on each stream, every
/// credential it saved, the request the stub received, and the stub's URL.
struct Login {
    result: Result<Outcome>,
    out: String,
    err: String,
    saved: Vec<String>,
    request: String,
    url: String,
}

fn login(json: bool, status: &str, body: &str) -> Result<Login> {
    let (url, address, server) = stub::token_endpoint(status, body)?;
    let saved = RefCell::new(Vec::new());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = {
        let mut sink = Sink::new(&mut out, &mut err);
        finish(
            &mut sink,
            json,
            &app()?,
            "the-code",
            &url,
            now()?,
            |credential| {
                saved.borrow_mut().push(credential.expose().to_owned());
                Ok(())
            },
        )
    };
    let request = stub::finish(address, server)?;
    Ok(Login {
        result,
        out: String::from_utf8(out)?,
        err: String::from_utf8(err)?,
        saved: saved.into_inner(),
        request,
        url,
    })
}

/// What `oauth.v2.access` answers for an app that asked for user scopes alone and has token
/// rotation on: the user token under `authed_user`, with its lifetime and a refresh token
/// (<https://docs.slack.dev/reference/methods/oauth.v2.access>, "Response";
/// <https://docs.slack.dev/authentication/using-token-rotation>, "Refresh a token").
fn granted() -> String {
    json!({
        "ok": true,
        "app_id": "A0EXAMPLE1",
        "authed_user": {
            "id": "U0EXAMPLE9",
            "scope": "channels:history,channels:read,groups:history,groups:read,users:read",
            "access_token": "xoxe.xoxp-1-user-token",
            "expires_in": 43_200,
            "refresh_token": "xoxe-1-refresh-token",
            "token_type": "user",
        },
        "team": { "id": "T0EXAMPLE1" },
        "enterprise": null,
        "is_enterprise_install": false,
    })
    .to_string()
}

fn url_pairs(body: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://localhost/?{body}"))
        .map(|url| url.query_pairs().into_owned().collect())
        .unwrap_or_default()
}

#[test]
fn authorize_url_asks_for_the_apps_user_scopes_and_no_bot_scope() -> Result<()> {
    let url = authorize_url(CLIENT_ID, "STATE")?;
    let parsed = reqwest::Url::parse(&url).context("parsing the authorize URL")?;
    assert_eq!(parsed.scheme(), "https");
    assert_eq!(parsed.host_str(), Some("slack.com"));
    assert_eq!(parsed.path(), "/oauth/v2/authorize");
    let pairs: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(
        pairs,
        std::collections::BTreeMap::from([
            ("client_id".to_owned(), CLIENT_ID.to_owned()),
            (
                "user_scope".to_owned(),
                "channels:history,channels:read,groups:history,groups:read,users:read".to_owned()
            ),
            ("redirect_uri".to_owned(), SLACK_REDIRECT_URI.to_owned()),
            ("state".to_owned(), "STATE".to_owned()),
        ]),
        "no `scope` (a bot scope), and nothing of the Claude Code app's flow"
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

/// The manifest the app is created from registers exactly what the login asks for, and has
/// token rotation on. Slack's manifest reference limits the name to 35 characters and the
/// description to 140 (<https://docs.slack.dev/reference/app-manifest>, "Display").
#[test]
fn the_manifest_registers_what_the_login_asks_for() -> Result<()> {
    let manifest: Value = serde_saphyr::from_str(include_str!("../../slack/app-manifest.yaml"))
        .context("parsing slack/app-manifest.yaml")?;
    let description = manifest
        .pointer("/display_information/description")
        .and_then(Value::as_str)
        .context("the manifest has a description")?;
    assert!(
        !description.is_empty() && description.chars().count() <= 140,
        "the description is {} characters",
        description.chars().count()
    );
    assert_eq!(
        manifest,
        json!({
            "display_information": {
                "name": "ctx-memory",
                "description": description,
            },
            "oauth_config": {
                "redirect_urls": [SLACK_REDIRECT_URI],
                "scopes": { "user": SLACK_SCOPES },
            },
            "settings": {
                "org_deploy_enabled": false,
                "socket_mode_enabled": false,
                "token_rotation_enabled": true,
            },
        }),
        "no bot user, no bot scope, no events, no interactivity, and rotation on"
    );
    Ok(())
}

/// The whole exchange over HTTP: the code goes to `oauth.v2.access` with the client id and
/// secret in HTTP Basic, the rotating token is stored with its refresh token and expiry, and the
/// client secret reaches neither standard output, standard error nor the stored credential.
#[test]
fn login_stores_the_rotating_token_and_never_the_client_secret() -> Result<()> {
    for json in [false, true] {
        let login = login(json, "200 OK", &granted())?;
        assert!(
            matches!(login.result, Ok(Outcome::Pass)),
            "{:?}",
            login.result.err()
        );

        assert!(login
            .request
            .starts_with("POST /api/oauth.v2.access HTTP/1.1\r\n"));
        let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
        assert_eq!(
            stub::header(&login.request, "authorization"),
            Some(format!("Basic {basic}").as_str())
        );
        assert_eq!(
            stub::header(&login.request, "content-type"),
            Some("application/x-www-form-urlencoded")
        );
        let body = login.request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let mut fields = url_pairs(body);
        fields.sort();
        assert_eq!(
            fields,
            [
                ("code".to_owned(), "the-code".to_owned()),
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("redirect_uri".to_owned(), SLACK_REDIRECT_URI.to_owned()),
            ],
            "the secret travels in the Authorization header alone"
        );

        let [saved] = login.saved.as_slice() else {
            anyhow::bail!("saved {} credentials, want 1", login.saved.len());
        };
        let saved: Value = serde_json::from_str(saved)?;
        assert_eq!(
            saved,
            json!({
                "kind": "app",
                "access": "xoxe.xoxp-1-user-token",
                "refresh": "xoxe-1-refresh-token",
                "expires": (now()? + chrono::Duration::seconds(43_200))
                    .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                "client_id": CLIENT_ID,
                "token_url": login.url,
                "team_id": "T0EXAMPLE1",
                "user_id": "U0EXAMPLE9",
            })
        );

        if json {
            let envelope: Value = serde_json::from_str(login.out.trim())?;
            assert_eq!(
                envelope.get("data"),
                Some(&json!({ "saved": true, "team_id": "T0EXAMPLE1", "user_id": "U0EXAMPLE9" }))
            );
        } else {
            assert_eq!(
                login.out,
                "Signed in to Slack as U0EXAMPLE9 in workspace T0EXAMPLE1. The credential is saved.\n"
            );
        }
        for (stream, text) in [
            ("standard output", &login.out),
            ("standard error", &login.err),
        ] {
            assert!(
                !text.contains(CLIENT_SECRET),
                "{stream} carries the client secret"
            );
            assert!(!text.contains("xoxe"), "{stream} carries a token");
        }
        assert!(
            login
                .saved
                .iter()
                .all(|saved| !saved.contains(CLIENT_SECRET)),
            "the stored credential carries the client secret"
        );
    }
    Ok(())
}

/// A refused exchange saves nothing, prints nothing, and its error names Slack's reason but
/// never the client secret, whether Slack answers with an error or with a page that is not JSON.
#[test]
fn a_refused_sign_in_saves_nothing_and_never_shows_the_secret() -> Result<()> {
    for (status, body, reason) in [
        (
            "200 OK",
            r#"{"ok":false,"error":"bad_client_secret"}"#.to_owned(),
            "Slack refused the sign-in: \"bad_client_secret\"",
        ),
        (
            "200 OK",
            r#"{"ok":false,"error":"invalid_code"}"#.to_owned(),
            "Slack refused the sign-in: \"invalid_code\"",
        ),
        (
            "502 Bad Gateway",
            "<html>upstream unavailable</html>".to_owned(),
            "502 Bad Gateway with a body that is not JSON",
        ),
        (
            "200 OK",
            json!({ "ok": true, "team": { "id": "T0EXAMPLE1" } }).to_string(),
            "the Slack token response has no authed_user",
        ),
    ] {
        let login = login(false, status, &body)?;
        let error = login
            .result
            .err()
            .map(|error| format!("{error:#}"))
            .with_context(|| format!("{body}: the sign-in must fail"))?;
        assert!(error.contains(reason), "{body}: {error}");
        assert!(!error.contains(CLIENT_SECRET), "{body}: {error}");
        assert!(
            login.saved.is_empty(),
            "{body}: a refused sign-in saved a credential"
        );
        assert!(
            login.out.is_empty() && login.err.is_empty(),
            "{body}: it printed"
        );
    }
    Ok(())
}

/// An app with token rotation off gets a token with no expiry and no refresh token. The fetch
/// renews the sign-in only by its refresh token, so storing it would leave a credential the
/// fetch cannot keep: the login refuses it, says why, and saves nothing.
#[test]
fn a_token_that_does_not_expire_is_refused() -> Result<()> {
    let body = json!({
        "ok": true,
        "authed_user": {
            "id": "U0EXAMPLE9",
            "access_token": "xoxp-never-expires",
            "token_type": "user",
        },
        "team": { "id": "T0EXAMPLE1" },
    })
    .to_string();
    let login = login(false, "200 OK", &body)?;
    let error = login
        .result
        .err()
        .map(|error| format!("{error:#}"))
        .context("a token that does not expire must be refused")?;
    assert!(error.contains("token rotation off"), "{error}");
    assert!(error.contains("Nothing was saved"), "{error}");
    assert!(!error.contains("xoxp-never-expires"), "{error}");
    assert!(login.saved.is_empty(), "a refused token was saved");
    Ok(())
}
