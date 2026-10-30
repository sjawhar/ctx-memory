//! What GitHub receives from the source, and what each of its answers does to one run.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::source::{
    FetchRequest, GithubSource, Message, Request, Source, SourceRef, StreamStatus,
};
use serde_json::{json, Value};

use crate::fixtures::{guard, scratch, stream_ends};
use crate::support::fake_http::FakeHttp;

fn fetch_request(config: Value, state_dir: std::path::PathBuf) -> Result<Request> {
    Ok(Request::Fetch(FetchRequest {
        protocol: 1,
        source: SourceRef {
            id: "github".to_owned(),
            kind: "github".to_owned(),
            scheme: "github".to_owned(),
        },
        config,
        window_end: DateTime::parse_from_rfc3339("2026-10-27T00:00:00Z")?.with_timezone(&Utc),
        backfill_hours: 24,
        cursors: BTreeMap::new(),
        discovered: Vec::new(),
        state_dir,
        exclude: Vec::new(),
    }))
}

#[test]
fn mints_read_only_tokens_per_installation() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/app/installations/1000001/access_tokens",
        201,
        &json!({ "token": "ghs_example", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    // The excluded organization's installation is registered, so a stray call would succeed; the
    // assertion below proves it is never made.
    api.json(
        "/app/installations/1000003/access_tokens",
        201,
        &json!({ "token": "ghs_excluded", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({ "total_count": 0, "repositories": [] }).to_string(),
    )?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(
            json!({ "installations": [1_000_002, 1_000_001] }),
            scratch("mint")?,
        )?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let messages = outcome?;
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Hello { verbatim: true, .. })),
        "the source declares itself verbatim"
    );

    // Each configured installation is minted once; the excluded one is never touched.
    assert_eq!(api.hits("/app/installations/1000002/access_tokens")?, 1);
    assert_eq!(api.hits("/app/installations/1000001/access_tokens")?, 1);
    assert_eq!(api.hits("/app/installations/1000003/access_tokens")?, 0);

    let requests = api.requests()?;
    let minting = requests
        .iter()
        .filter(|request| request.path.ends_with("/access_tokens"))
        .collect::<Vec<_>>();
    assert_eq!(minting.len(), 2, "two installations were minted");
    for request in minting {
        assert_eq!(request.method, "POST");
        let body: Value = serde_json::from_str(&request.body)
            .with_context(|| format!("parsing the minting body: {}", request.body))?;
        let permissions = body
            .get("permissions")
            .and_then(Value::as_object)
            .context("the minting request names permissions")?;
        let mut levels: BTreeMap<String, String> = BTreeMap::new();
        for (name, level) in permissions {
            levels.insert(name.clone(), level.as_str().unwrap_or_default().to_owned());
        }
        assert_eq!(
            levels,
            BTreeMap::from([
                ("contents".to_owned(), "read".to_owned()),
                ("issues".to_owned(), "read".to_owned()),
                ("metadata".to_owned(), "read".to_owned()),
                ("pull_requests".to_owned(), "read".to_owned()),
            ]),
            "the token is read-only"
        );
        assert!(
            body.get("repositories").is_none(),
            "a read token is scoped to every repository the installation covers"
        );
    }
    Ok(())
}

#[test]
fn empty_repository_commits_409_is_not_a_stream_error() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [
                { "full_name": "other-org/empty-repo", "default_branch": "main" },
            ],
        })
        .to_string(),
    )?;
    api.json("/repos/other-org/empty-repo/issues", 200, "[]")?;
    // An empty repository (no commits yet) answers the commits listing with 409, exactly as
    // one did in a real fetch on 2026-10-27. That is "no commits", not a fetch
    // failure: the stream must end OK, so the fetch does not report a permanent error and the
    // repository's cursor can advance.
    api.json(
        "/repos/other-org/empty-repo/commits",
        409,
        &json!({ "message": "Git Repository is empty." }).to_string(),
    )?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(json!({ "installations": [1_000_002] }), scratch("empty")?)?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let messages = outcome?;
    let errored: Vec<(String, Option<String>)> = messages
        .iter()
        .filter_map(|message| match message {
            Message::StreamEnd {
                stream,
                status: StreamStatus::Error,
                error,
                ..
            } => Some((stream.clone(), error.clone())),
            _ => None,
        })
        .collect();
    assert!(
        errored.is_empty(),
        "an empty repository's 409 on the commits listing must not error the stream: {errored:?}"
    );
    Ok(())
}

#[test]
fn an_empty_repository_is_a_quiet_stream_whose_cursor_moves() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [
                { "full_name": "example-org/slack-tui", "default_branch": "main" },
            ],
        })
        .to_string(),
    )?;
    api.json("/repos/example-org/slack-tui/issues", 200, "[]")?;
    api.json(
        "/repos/example-org/slack-tui/commits",
        409,
        &json!({ "message": "Git Repository is empty." }).to_string(),
    )?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(json!({ "installations": [1_000_002] }), scratch("quiet")?)?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let messages = outcome?;
    // No commits means no records, and a cursor that reaches the window's end like any other
    // quiet stream, so the next fetch starts from there instead of re-reading the window.
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::Record(_))),
        "an empty repository carries no records"
    );
    assert_eq!(
        stream_ends(&messages),
        vec![(
            "github:example-org/slack-tui".to_owned(),
            StreamStatus::Ok,
            Some("2026-10-27T00:00:00Z".to_owned()),
            None,
        )],
        "one stream ends OK with a cursor at the window's end",
    );
    Ok(())
}

#[test]
fn a_rate_limited_listing_holds_its_cursor_instead_of_erroring() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [
                { "full_name": "other-org/busy-repo", "default_branch": "main" },
            ],
        })
        .to_string(),
    )?;
    // GitHub answers a rate limit it is protecting with 429 (or 403 with the limit spent). The
    // run has to come back later, which is what holding the cursor does; erroring the stream
    // would make every fetch exit non-zero while the limit lasts.
    api.json(
        "/repos/other-org/busy-repo/issues",
        429,
        &json!({ "message": "You have exceeded a secondary rate limit." }).to_string(),
    )?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(json!({ "installations": [1_000_002] }), scratch("busy")?)?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let messages = outcome?;
    // The window started 24 hours before its end; the held cursor reads from there next time.
    assert_eq!(
        stream_ends(&messages),
        vec![(
            "github:other-org/busy-repo".to_owned(),
            StreamStatus::Ok,
            Some("2026-10-26T00:00:00Z".to_owned()),
            None,
        )],
        "one stream ends OK with a held cursor",
    );
    let warnings: Vec<&String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Warning { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("rate limit") && warning.contains("cursor is held")),
        "the stream says why it stopped: {warnings:?}"
    );
    Ok(())
}

#[test]
fn a_throttled_repository_listing_skips_one_installation_only() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/app/installations/1000001/access_tokens",
        201,
        &json!({ "token": "ghs_example", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    // Each installation carries its own rate limit, so the first one being refused says nothing
    // about the second. The first listing is throttled; the second answers.
    api.then_json(
        "/installation/repositories",
        429,
        &json!({ "message": "You have exceeded a secondary rate limit." }).to_string(),
    )?;
    api.then_json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [
                { "full_name": "example-org/example-repo", "default_branch": "main" },
            ],
        })
        .to_string(),
    )?;
    api.json("/repos/example-org/example-repo/issues", 200, "[]")?;
    api.json("/repos/example-org/example-repo/commits", 200, "[]")?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(
            json!({ "installations": [1_000_002, 1_000_001] }),
            scratch("throttled-listing")?,
        )?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    // The refused installation costs a warning, not the run: the installation that answered is
    // still read, and its cursor still advances.
    let messages = outcome?;
    let warnings: Vec<&String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Warning { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("1000002") && warning.contains("rate limit")),
        "the refused installation is named: {warnings:?}"
    );
    assert_eq!(
        stream_ends(&messages),
        vec![(
            "github:example-org/example-repo".to_owned(),
            StreamStatus::Ok,
            Some("2026-10-27T00:00:00Z".to_owned()),
            None,
        )],
        "only the installation that answered has a stream, and its cursor advances",
    );
    Ok(())
}

#[test]
fn a_plus_in_the_default_branch_reaches_github_percent_encoded() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [
                { "full_name": "other-org/coq-elpi", "default_branch": "v3.3.1+qvar" },
            ],
        })
        .to_string(),
    )?;
    api.json("/repos/other-org/coq-elpi/issues", 200, "[]")?;
    api.json("/repos/other-org/coq-elpi/commits", 200, "[]")?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<Vec<Message>> {
        let request = fetch_request(json!({ "installations": [1_000_002] }), scratch("plus")?)?;
        let mut messages = Vec::new();
        GithubSource::new().run(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let messages = outcome?;
    assert_eq!(
        stream_ends(&messages),
        vec![(
            "github:other-org/coq-elpi".to_owned(),
            StreamStatus::Ok,
            Some("2026-10-27T00:00:00Z".to_owned()),
            None,
        )],
        "the stream the plus branch belongs to ends OK",
    );
    // The real client sent the branch as `%2B`, so GitHub reads it as `+` rather than a space.
    let commits = api
        .requests()?
        .into_iter()
        .find(|request| request.path == "/repos/other-org/coq-elpi/commits")
        .context("the commits listing was requested")?;
    assert!(
        commits.target.contains("sha=v3.3.1%2Bqvar"),
        "the branch reaches GitHub percent-encoded: {}",
        commits.target
    );
    assert!(
        !commits.target.contains("sha=v3.3.1+qvar"),
        "a raw `+` must not be sent: {}",
        commits.target
    );
    Ok(())
}
