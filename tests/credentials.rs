//! Credentials come from Secret Manager with the identity the metadata server hands out, and
//! the GitHub App buys a token that may write one repository and nothing else.

mod support;

use std::sync::{Mutex, MutexGuard};

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ctx_memory::api::github::{App, TokenRequest};
use ctx_memory::credentials::{self, Secret};
use serde_json::{json, Value};

use support::fake_http::FakeHttp;

/// Environment variables are process-wide, so these tests take turns.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

fn guard() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn secret_manager_answers_with_the_metadata_identity() -> Result<()> {
    let _guard = guard();
    let metadata = FakeHttp::start()?;
    let manager = FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.test", "expires_in": 3599 }).to_string(),
    )?;
    manager.json(
        "/v1/projects/123456789012/secrets/ctx-slack/versions/latest:access",
        200,
        &json!({ "payload": { "data": STANDARD.encode("{\"access\":\"xoxp-test\"}") } })
            .to_string(),
    )?;
    std::env::set_var("GCE_METADATA_HOST", metadata.address().to_string());
    std::env::set_var("CTX_SECRET_MANAGER_URL", manager.url(""));
    std::env::set_var(
        "CTX_SECRET_SLACK",
        "projects/123456789012/secrets/ctx-slack",
    );

    let secret = credentials::load("slack");

    std::env::remove_var("CTX_SECRET_SLACK");
    std::env::remove_var("CTX_SECRET_MANAGER_URL");
    std::env::remove_var("GCE_METADATA_HOST");

    let secret = secret?;
    assert_eq!(secret.expose(), "{\"access\":\"xoxp-test\"}");
    let requests = manager.requests()?;
    let access = requests.first().context("the secret was read")?;
    assert_eq!(access.method, "GET");
    assert_eq!(
        access.headers.get("authorization").map(String::as_str),
        Some("Bearer ya29.test")
    );
    let asked = metadata.requests()?;
    assert_eq!(
        asked
            .first()
            .and_then(|request| request.headers.get("metadata-flavor"))
            .map(String::as_str),
        Some("Google")
    );
    Ok(())
}

#[test]
fn a_rotated_credential_is_saved_as_a_new_version() -> Result<()> {
    let _guard = guard();
    let metadata = FakeHttp::start()?;
    let manager = FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.test" }).to_string(),
    )?;
    manager.json(
        "/v1/projects/123456789012/secrets/ctx-slack:addVersion",
        200,
        &json!({ "name": "projects/123456789012/secrets/ctx-slack/versions/2" }).to_string(),
    )?;
    std::env::set_var("GCE_METADATA_HOST", metadata.address().to_string());
    std::env::set_var("CTX_SECRET_MANAGER_URL", manager.url(""));
    std::env::set_var(
        "CTX_SECRET_SLACK",
        "projects/123456789012/secrets/ctx-slack",
    );

    let saved = credentials::save(
        "slack",
        &Secret::new("{\"access\":\"xoxp-new\"}".to_owned()),
    );

    std::env::remove_var("CTX_SECRET_SLACK");
    std::env::remove_var("CTX_SECRET_MANAGER_URL");
    std::env::remove_var("GCE_METADATA_HOST");

    saved?;
    let requests = manager.requests()?;
    let added = requests.first().context("a version was added")?;
    assert_eq!(added.method, "POST");
    let body: Value = serde_json::from_str(&added.body)?;
    let data = body
        .get("payload")
        .and_then(|payload| payload.get("data"))
        .and_then(Value::as_str)
        .context("the version carries a payload")?;
    assert_eq!(
        String::from_utf8(STANDARD.decode(data)?)?,
        "{\"access\":\"xoxp-new\"}"
    );
    Ok(())
}

#[test]
fn the_app_mints_a_token_for_one_repository_only() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/repos/other-org/memory/installation",
        200,
        &json!({ "id": 1_000_002 }).to_string(),
    )?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_test", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let minted = (|| -> Result<_> {
        let app = App::load(&credentials::Config::new())?;
        let installation = app.installation_for("other-org", "memory")?;
        let token =
            app.installation_token(installation, &TokenRequest::contents_write("memory"))?;
        Ok((installation, token))
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let (installation, token) = minted?;
    assert_eq!(installation, 1_000_002);
    assert_eq!(token.token, "ghs_test");

    let requests = api.requests()?;
    let minting = requests
        .iter()
        .find(|request| request.path == "/app/installations/1000002/access_tokens")
        .context("the token was minted")?;
    let body: Value = serde_json::from_str(&minting.body)?;
    assert_eq!(
        body,
        json!({
            "repositories": ["memory"],
            "permissions": { "contents": "write", "metadata": "read" }
        })
    );
    let authorization = minting
        .headers
        .get("authorization")
        .context("the request is signed")?;
    let jwt = authorization
        .strip_prefix("Bearer ")
        .context("the app presents a bearer token")?;
    let header = jsonwebtoken::decode_header(jwt)?;
    assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);
    Ok(())
}
