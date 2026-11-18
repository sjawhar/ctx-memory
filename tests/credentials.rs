//! Credentials come from Secret Manager with the identity the metadata server hands out, and
//! the GitHub App buys a token that may write one repository and nothing else.

mod support;

use std::sync::{Mutex, MutexGuard};

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ctx_memory::api::github::{App, TokenRequest};
use ctx_memory::config::TreeConfig;
use ctx_memory::credentials::{self, Secret};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
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

/// A `[credentials]` file entry that starts with `~/` is read under `HOME`, as the README's own
/// `github_app = { file = "~/.config/ctx/github-app.json" }` is written.
#[test]
fn a_credentials_file_under_home_is_read() -> Result<()> {
    let _guard = guard();
    let home = support::scratch("credentials-home")?;
    let contents = r#"{"app_id":"1"}"#;
    std::fs::write(home.join("github-app.json"), contents)?;
    let mut config = credentials::Config::new();
    config.insert(
        "github_app".to_owned(),
        credentials::Source::File {
            file: "~/github-app.json".into(),
        },
    );

    let previous = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let secret = credentials::load_with(&config, "github_app");
    if let Some(previous) = previous {
        std::env::set_var("HOME", previous);
    } else {
        std::env::remove_var("HOME");
    }
    support::remove_scratch(&home)?;

    assert_eq!(secret?.expose(), contents);
    Ok(())
}

/// The README's own `[credentials]` entry, as a person writes it in `ctx.toml`:
/// `github_app = { file = "~/…" }`. The tree's configuration parses, and the credential is read
/// from the file under `HOME`.
#[test]
fn the_readme_credentials_entry_in_ctx_toml_reads_a_file_under_home() -> Result<()> {
    let _guard = guard();
    let root = support::scratch("credentials-readme-entry")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\n\n[credentials]\ngithub_app = { file = \"~/github-app.json\" }\n",
    )?;
    let contents = r#"{"app_id":"1"}"#;
    std::fs::write(root.join("github-app.json"), contents)?;

    let previous = std::env::var_os("HOME");
    std::env::set_var("HOME", &root);
    let secret = TreeConfig::load(&tree)
        .and_then(|config| credentials::load_with(&config.credentials, "github_app"));
    if let Some(previous) = previous {
        std::env::set_var("HOME", previous);
    } else {
        std::env::remove_var("HOME");
    }
    support::remove_scratch(&root)?;

    assert_eq!(secret?.expose(), contents);
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
        "private_key": support::SIGNING_KEY,
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

/// Every `app_id` the loader refuses, as the JSON text a credential carries.
const REFUSED_APP_IDS: [&str; 13] = [
    "-5706923",
    "5706923.0",
    "1e6",
    // One more than the largest u64, which serde_json reads as a float.
    "18446744073709551616",
    r#""""#,
    r#""+5706923""#,
    r#"" 5706923""#,
    r#""5706923\n""#,
    r#""٥٧٠٦٩٢٣""#,
    r#""Iv23liFixture""#,
    "true",
    "null",
    "[5706923]",
];

/// Builds the app from `credential`, as the `github_app` credential is read.
fn app_from(credential: &Value) -> Result<App> {
    App::from_secret(&Secret::new(credential.to_string()))
}

/// The `iss` claim of the JWT `app` signs, read by checking the JWT with the fixture key set.
fn signed_issuer(app: &App) -> Result<Value> {
    let keys: JwkSet = serde_json::from_str(support::JWKS)?;
    let key = keys.keys.first().context("no fixture key")?;
    let key = DecodingKey::from_jwk(key)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_aud = false;
    let token = jsonwebtoken::decode::<Value>(&app.jwt()?, &key, &validation)?;
    token.claims.get("iss").cloned().context("no iss")
}

/// A credential as GitHub gives it, as the main-push key holds one: a numeric `app_id`, the
/// `client_id` beside it, and a PKCS#1 key.
#[test]
fn a_main_push_shaped_credential_signs_with_its_numeric_app_id() -> Result<()> {
    let key = support::signing_key_pkcs1()?;
    assert!(
        key.starts_with("-----BEGIN RSA "),
        "the key is not the PKCS#1 PEM GitHub hands out"
    );
    let credential = json!({
        "app_id": 5_706_923,
        "client_id": "Iv23liFixture",
        "private_key": key,
    });
    let app = app_from(&credential)?;
    assert_eq!(signed_issuer(&app)?, json!("5706923"));
    Ok(())
}

#[test]
fn a_numeric_app_id_signs_as_its_decimal_text() -> Result<()> {
    let credential = json!({ "app_id": 5_706_923, "private_key": support::SIGNING_KEY });
    let app = app_from(&credential)?;
    assert_eq!(signed_issuer(&app)?, json!("5706923"));
    Ok(())
}

#[test]
fn a_digit_string_app_id_signs_as_written() -> Result<()> {
    for written in ["5706923", "0123"] {
        let credential = json!({ "app_id": written, "private_key": support::SIGNING_KEY });
        let app = app_from(&credential)?;
        assert_eq!(
            signed_issuer(&app)?,
            json!(written),
            "the app_id {written:?} signs as another issuer"
        );
    }
    Ok(())
}

#[test]
fn an_app_id_that_is_not_a_whole_number_or_digits_is_refused_without_its_value() -> Result<()> {
    let key = serde_json::to_string(support::SIGNING_KEY)?;
    let mut rows = Vec::new();
    for raw in REFUSED_APP_IDS {
        let written: Value = serde_json::from_str(raw)?;
        // A refused string must not appear in the error. Every text holds the empty one.
        let content = written
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        let credential = format!(r#"{{"app_id": {raw}, "private_key": {key}}}"#);
        rows.push((raw.to_owned(), credential, content));
    }
    let missing = format!(r#"{{"private_key": {key}}}"#);
    rows.push(("(missing)".to_owned(), missing, None));
    for (row, credential, content) in rows {
        let error = App::from_secret(&Secret::new(credential))
            .err()
            .with_context(|| format!("the app_id {row} was accepted"))?;
        let text = format!("{error:#}");
        assert!(
            text.starts_with("the credential is not the expected JSON ("),
            "the app_id {row} was refused with: {text}"
        );
        if let Some(content) = content {
            assert!(
                !text.contains(&content),
                "the refusal of the app_id {row} repeats it: {text}"
            );
        }
    }
    Ok(())
}

#[test]
fn swapped_app_credential_fields_never_print_the_key() -> Result<()> {
    let key = support::signing_key_pkcs1()?;
    let swapped = json!({ "app_id": key, "private_key": "5706923" });
    let error = app_from(&swapped)
        .err()
        .context("a credential with its fields swapped was accepted")?;
    let text = format!("{error:#}");
    // The id check refuses it, not the key reader: the PEM in `app_id` is what must stay out.
    assert!(
        text.starts_with("the credential is not the expected JSON ("),
        "the swapped credential was refused with: {text}"
    );
    assert!(
        !text.contains("PRIVATE KEY"),
        "the refusal names the key: {text}"
    );
    for line in key.lines().filter(|line| !line.is_empty()) {
        assert!(
            !text.contains(line),
            "the refusal carries a line of the key: {line}"
        );
    }
    Ok(())
}

/// A credential that holds only its key, as a JSON string, is refused without the key. A named
/// credential that does not load stops `ctx` with this error, so it reaches the standard error
/// of every clone, pull and push, and the server's logs.
#[test]
fn a_credential_that_is_only_its_key_is_refused_without_the_key() -> Result<()> {
    let key = support::signing_key_pkcs1()?;
    let error = App::from_secret(&Secret::new(serde_json::to_string(&key)?))
        .err()
        .context("a credential that is only its key was accepted")?;
    let text = format!("{error:#}");
    assert!(
        text.starts_with("the credential is not the expected JSON ("),
        "the credential that is only its key was refused with: {text}"
    );
    assert!(
        !text.contains("PRIVATE KEY"),
        "the refusal names the key: {text}"
    );
    for line in key.lines().filter(|line| !line.is_empty()) {
        assert!(
            !text.contains(line),
            "the refusal carries a line of the key: {line}"
        );
    }
    Ok(())
}

/// A credential that is only its key, as a JSON string, with more text after it, is refused
/// without the key: a line copied with its trailing comma, a stray brace on the next line, or
/// the key written twice. serde reports the string before it reaches the text that keeps the
/// whole secret from being JSON.
#[test]
fn a_key_only_credential_with_text_after_it_is_refused_without_the_key() -> Result<()> {
    let key = support::signing_key_pkcs1()?;
    let quoted = serde_json::to_string(&key)?;
    let twice = format!(" {quoted}");
    for after in [",", "\n}", twice.as_str()] {
        let error = App::from_secret(&Secret::new(format!("{quoted}{after}")))
            .err()
            .with_context(|| format!("the key followed by {after:?} was accepted"))?;
        let text = format!("{error:#}");
        assert!(
            text.starts_with("the credential is not the expected JSON ("),
            "the key followed by {after:?} was refused with: {text}"
        );
        assert!(
            !text.contains("PRIVATE KEY"),
            "the refusal of the key followed by {after:?} names the key: {text}"
        );
        for line in key.lines().filter(|line| !line.is_empty()) {
            assert!(
                !text.contains(line),
                "the refusal of the key followed by {after:?} carries a line of the key: {line}"
            );
        }
    }
    Ok(())
}
