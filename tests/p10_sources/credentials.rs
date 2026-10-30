//! Google credentials and the checks that span Google sources: the calendar source, the cursors
//! a whole Google fetch saves, a refreshed credential and one read from Secret Manager.

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::DateTime;
use ctx_memory::source;
use serde_json::{json, Value};

use crate::fixtures::{fetch_request, remove_scratch, run_conformance, scratch, source_spec};
use crate::support::fake_google::{clear_google_environment, environment_lock, FakeGoogle};
use crate::support::fake_http::FakeHttp;

#[test]
fn calendar_fetch_calls_the_configured_calendar_id() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("calendar-id")?;
    let server = FakeGoogle::start(vec![(
        "/calendar/v3/calendars/team%40example.com/events",
        json!({
            "items": [{
                "id": "EVENT-EXAMPLE",
                "summary": "Weekly sync",
                "updated": "2026-10-25T17:00:00Z",
                "start": {"dateTime": "2026-10-25T09:00:00Z"},
                "end": {"dateTime": "2026-10-25T10:00:00Z"},
                "attendees": [{"email": "alice@example.com", "responseStatus": "accepted"}]
            }],
            "nextSyncToken": "sync-2"
        }),
    )])?;
    server.use_in_environment("ya29.calendar-test");

    let spec = source_spec("calendar", "calendars = [\"team@example.com\"]\n")?;
    let request = fetch_request(
        &root,
        "calendar",
        "gcal",
        json!({ "calendars": ["team@example.com"] }),
    )?;
    let source = source::open(&spec, "gcal")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let requests = server.requests()?;

    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert!(
        requests
            .iter()
            .any(|request| request.contains("team%40example.com")),
        "{requests:?}"
    );
    let record = outcome
        .records
        .first()
        .context("the fetch stored a record")?;
    assert_eq!(record.pointer, "gcal:team@example.com/EVENT-EXAMPLE");
    assert!(record.text.contains("alice@example.com"), "{}", record.text);
    let cursor = outcome
        .streams
        .get("team@example.com")
        .and_then(|stream| stream.cursor.clone())
        .context("the calendar stream proposed a cursor")?;
    // The cursor names the sync token Google gave, and the time the calendar is stored
    // through, which a run whose token Google refuses lists from.
    let (token, stored_through) = cursor
        .rsplit_once('@')
        .context("the cursor carries the time it is stored through")?;
    assert_eq!(token, "sync-2");
    DateTime::parse_from_rfc3339(stored_through)
        .with_context(|| format!("parsing {stored_through}"))?;
    Ok(())
}

#[test]
fn a_google_fetch_saves_the_cursor_each_api_gave_it() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("google-cursors")?;
    let tree = root.join("tree");
    let data = root.join("data");
    std::fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"gmail\"\nkind = \"gmail\"\n\n[[sources]]\nid = \"drive\"\nkind = \"drive\"\nmeet = false\n\n[[sources]]\nid = \"calendar\"\nkind = \"calendar\"\ncalendars = [\"primary\"]\n",
    )?;
    let server = FakeGoogle::start(vec![
        (
            "/gmail/v1/users/me/profile",
            json!({"emailAddress": "ada@example.com", "historyId": "185843"}),
        ),
        ("/gmail/v1/users/me/messages", json!({"messages": []})),
        (
            "/drive/v3/changes/startPageToken",
            json!({"startPageToken": "4897"}),
        ),
        ("/drive/v3/files", json!({"files": []})),
        (
            "/calendar/v3/calendars/primary/events",
            json!({"items": [], "nextSyncToken": "sync-2"}),
        ),
    ])?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;

    let fetched = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree_text, "fetch"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_DATA_DIR", &data)
        .env("CTX_GOOGLE_API_BASE", server.base())
        .env("CTX_CREDENTIAL_GOOGLE", "ya29.cursor-test")
        .output()
        .context("running the Google fetch")?;
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let cursors: Value = serde_json::from_slice(&std::fs::read(data.join("cursors.json"))?)?;
    remove_scratch(&root)?;

    // The fetch itself moves each stream's cursor; there is no separate advance command.
    // Each cursor names what its API gave, and the time the stream is stored through, which a
    // run whose history, sync token or changes token the API refuses lists from.
    for (path, token) in [
        ("/sources/gmail/me/cursor", "185843"),
        ("/sources/calendar/primary/cursor", "sync-2"),
        ("/sources/drive/drive/cursor", "4897"),
    ] {
        let cursor = cursors
            .pointer(path)
            .and_then(Value::as_str)
            .with_context(|| format!("{path} is saved: {cursors}"))?;
        let (saved, stored_through) = cursor
            .rsplit_once('@')
            .with_context(|| format!("{cursor} carries the time it is stored through"))?;
        assert_eq!(saved, token, "{cursors}");
        DateTime::parse_from_rfc3339(stored_through)
            .with_context(|| format!("parsing {stored_through}"))?;
    }
    Ok(())
}

#[test]
fn an_authorized_user_credential_is_refreshed_before_the_api_call() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("google-refresh")?;
    let server = FakeGoogle::start(vec![
        (
            "/token",
            json!({"access_token": "ya29.refreshed", "expires_in": 3599}),
        ),
        (
            "/calendar/v3/calendars/team%40example.com/events",
            json!({"items": [], "nextSyncToken": "sync-2"}),
        ),
    ])?;
    std::env::set_var("CTX_GOOGLE_API_BASE", server.base());
    std::env::set_var(
        "CTX_CREDENTIAL_GOOGLE",
        r#"{"type":"authorized_user","client_id":"id","client_secret":"secret","refresh_token":"refresh-p10"}"#,
    );

    let spec = source_spec("calendar", "calendars = [\"team@example.com\"]\n")?;
    let request = fetch_request(
        &root,
        "calendar",
        "gcal",
        json!({ "calendars": ["team@example.com"] }),
    )?;
    let source = source::open(&spec, "gcal")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let requests = server.requests()?;
    let bearers = server.bearers()?;

    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert_eq!(
        requests.first().map(String::as_str),
        Some("POST /token"),
        "{requests:?}"
    );
    assert_eq!(bearers, vec!["ya29.refreshed".to_owned()], "{bearers:?}");
    Ok(())
}

/// In Google Cloud the credential is a Secret Manager version, read with the identity the
/// metadata server hands the running job: no credential value is in the environment at all.
#[test]
fn the_google_credential_comes_from_secret_manager() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("google-secret-manager")?;
    let metadata = FakeHttp::start()?;
    let manager = FakeHttp::start()?;
    metadata.json(
        "/computeMetadata/v1/instance/service-accounts/default/token",
        200,
        &json!({ "access_token": "ya29.metadata", "expires_in": 3599 }).to_string(),
    )?;
    manager.json(
        "/v1/projects/123456789012/secrets/example-google-oauth/versions/latest:access",
        200,
        &json!({ "payload": { "data": STANDARD.encode("ya29.from-secret-manager") } }).to_string(),
    )?;
    let server = FakeGoogle::start(vec![(
        "/calendar/v3/calendars/primary/events",
        json!({"items": [], "nextSyncToken": "sync-2"}),
    )])?;
    std::env::set_var("CTX_GOOGLE_API_BASE", server.base());
    std::env::set_var("GCE_METADATA_HOST", metadata.address().to_string());
    std::env::set_var("CTX_SECRET_MANAGER_URL", manager.url(""));
    std::env::set_var(
        "CTX_SECRET_GOOGLE",
        "projects/123456789012/secrets/example-google-oauth",
    );

    let spec = source_spec("calendar", "calendars = [\"primary\"]\n")?;
    let request = fetch_request(
        &root,
        "calendar",
        "gcal",
        json!({ "calendars": ["primary"] }),
    )?;
    let source = source::open(&spec, "gcal")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let bearers = server.bearers()?;
    let read = manager.requests()?;

    std::env::remove_var("CTX_SECRET_GOOGLE");
    std::env::remove_var("CTX_SECRET_MANAGER_URL");
    std::env::remove_var("GCE_METADATA_HOST");
    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert_eq!(
        bearers,
        vec!["ya29.from-secret-manager".to_owned()],
        "{bearers:?}"
    );
    let access = read.first().context("the secret was read")?;
    assert_eq!(
        access.path,
        "/v1/projects/123456789012/secrets/example-google-oauth/versions/latest:access"
    );
    Ok(())
}
