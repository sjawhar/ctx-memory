//! Slack credential and token-rotation tests.

use super::super::auth::{fetch_authenticator, SlackAuth, SAVE_RETRY_PAUSES};
use super::fixtures::*;

#[test]
fn missing_credential_names_the_variable() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow!("locking test environment: {error}"))?;
    let end = at("2026-10-25T18:00:00Z")?;
    let root = scratch("missing-credential")?;
    let request = fetch_request(&root, end, Vec::new(), Vec::new());
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        "CTX_SECRET_SLACK",
        "CTX_CREDENTIAL_SLACK",
        "CTX_CREDENTIAL_SLACK_FILE",
    ]
    .into_iter()
    .map(|name| {
        let previous = std::env::var_os(name);
        std::env::remove_var(name);
        (name, previous)
    })
    .collect();
    let result = SlackSource::new().run(&Request::Fetch(request), &mut |_| Ok(()));
    for (name, previous) in saved {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        }
    }
    std::fs::remove_dir_all(&root)?;

    let error = result
        .err()
        .context("a missing Slack credential should fail the fetch")?;
    assert!(format!("{error:#}").contains("CTX_CREDENTIAL_SLACK"));
    Ok(())
}

/// The ctx-memory app's credential, with `old-access` expiring at `expires`.
fn app_credential(expires: DateTime<Utc>) -> SlackCredential {
    SlackCredential::App(AppCredential {
        kind: AppKind::App,
        access: "old-access".to_owned(),
        refresh: "old-refresh".to_owned(),
        expires,
        client_id: "123.456".to_owned(),
        token_url: "https://unused.example/token".to_owned(),
        team_id: "T0EXAMPLE1".to_owned(),
        user_id: "U0EXAMPLE9".to_owned(),
    })
}

/// The Claude Code app's credential, with `old-access` expiring at `expires`.
fn claude_code_credential(expires: DateTime<Utc>) -> SlackCredential {
    SlackCredential::ClaudeCode(ClaudeCodeCredential {
        access: "old-access".to_owned(),
        refresh: "old-refresh".to_owned(),
        expires,
        client_id: "cid".to_owned(),
        token_url: "https://unused.example/token".to_owned(),
        resource: "https://mcp.slack.com".to_owned(),
    })
}

/// `credential` renewed to `new-access` and `new-refresh`, in its own shape.
fn renewed(credential: &SlackCredential, now: DateTime<Utc>) -> SlackCredential {
    let expires = now + Duration::hours(12);
    match credential {
        SlackCredential::App(app) => SlackCredential::App(AppCredential {
            access: "new-access".to_owned(),
            refresh: "new-refresh".to_owned(),
            expires,
            ..app.clone()
        }),
        SlackCredential::ClaudeCode(claude) => SlackCredential::ClaudeCode(ClaudeCodeCredential {
            access: "new-access".to_owned(),
            refresh: "new-refresh".to_owned(),
            expires,
            ..claude.clone()
        }),
    }
}

fn push(log: &Mutex<Vec<String>>, event: String) -> Result<()> {
    log.lock()
        .map_err(|error| anyhow!("locking the request log: {error}"))?
        .push(event);
    Ok(())
}

fn events(log: &Mutex<Vec<String>>) -> Result<Vec<String>> {
    Ok(log
        .lock()
        .map_err(|error| anyhow!("locking the request log: {error}"))?
        .clone())
}

/// An authenticator that holds the fetch lease, renews any credential to `renewed`, and records
/// each refresh, save and pause into `log`, interleaved with the fake Slack server's calls.
/// `failing_saves` saves fail before one succeeds.
fn recording_authenticator(
    credential: SlackCredential,
    now: DateTime<Utc>,
    log: Arc<Mutex<Vec<String>>>,
    failing_saves: usize,
) -> impl SlackAuth {
    let refresh_log = Arc::clone(&log);
    let pause_log = Arc::clone(&log);
    let saves = std::cell::Cell::new(0_usize);
    Authenticator {
        credential,
        now,
        may_rotate: true,
        refresh: move |credential: &SlackCredential, now| {
            push(&refresh_log, "refresh".to_owned())?;
            Ok(renewed(credential, now))
        },
        save: move |saved: &SlackCredential| -> Result<()> {
            saves.set(saves.get() + 1);
            if saves.get() <= failing_saves {
                push(&log, "save-failed".to_owned())?;
                bail!("secret manager is unreachable");
            }
            let refresh = match saved {
                SlackCredential::App(app) => app.refresh.clone(),
                SlackCredential::ClaudeCode(claude) => claude.refresh.clone(),
            };
            push(&log, format!("save:{}:{refresh}", saved.access()))
        },
        pause: move |pause: std::time::Duration| {
            let _ = push(&pause_log, format!("pause:{}s", pause.as_secs()));
        },
    }
}

/// A fake Slack HTTP endpoint for `requests` calls. It records each request's Authorization
/// header into a shared, ordered log, interleaved with whatever else writes to that log, and
/// answers each call with `respond(call index, authorization)`.
fn slack_server(
    requests: usize,
    log: Arc<Mutex<Vec<String>>>,
    mut respond: impl FnMut(usize, &str) -> Result<String> + Send + 'static,
) -> Result<(String, JoinHandle<Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = std::thread::spawn(move || -> Result<()> {
        for call in 0..requests {
            let (mut stream, _) = listener.accept()?;
            let mut request = BufReader::new(stream.try_clone()?);
            let mut first_line = String::new();
            request.read_line(&mut first_line)?;
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                let bytes_read = request.read_line(&mut line)?;
                if bytes_read == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("authorization") {
                        authorization = value.trim().to_owned();
                    }
                }
            }
            push(&log, format!("api:{authorization}"))?;
            let body = respond(call, &authorization)?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes())?;
        }
        Ok(())
    });
    Ok((format!("http://{address}"), server))
}

/// A fake Slack HTTP endpoint that answers with `responses` in order.
fn recording_slack_server(
    responses: Vec<String>,
    log: Arc<Mutex<Vec<String>>>,
) -> Result<(String, JoinHandle<Result<()>>)> {
    let requests = responses.len();
    let mut responses = responses.into_iter();
    slack_server(requests, log, move |_, _| {
        responses
            .next()
            .context("the fake Slack server has no response left")
    })
}

/// The three answers one channel's fetch takes: discovery, the channel's name, its history.
fn one_channel() -> Vec<String> {
    vec![
        r#"{"ok":true,"channels":[{"id":"C0EXAMPLE01","is_private":false,"is_member":true}]}"#
            .to_owned(),
        r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
        r#"{"ok":true,"messages":[]}"#.to_owned(),
    ]
}

/// A fake Slack API whose old token expires after the first discovery page. A refreshed token
/// stays valid through the second page, so the fetch must refresh before it starts.
fn token_expiry_server(log: Arc<Mutex<Vec<String>>>) -> Result<(String, JoinHandle<Result<()>>)> {
    slack_server(2, log, |page, authorization| {
        let body = match (page, authorization) {
            (0, "Bearer old-access" | "Bearer new-access") => {
                r#"{"ok":true,"channels":[],"response_metadata":{"next_cursor":"next"}}"#
            }
            (1, "Bearer old-access") => r#"{"ok":false,"error":"invalid_auth"}"#,
            (1, "Bearer new-access") => {
                r#"{"ok":true,"channels":[],"response_metadata":{"next_cursor":""}}"#
            }
            _ => bail!("unexpected token {authorization:?} on discovery page {page}"),
        };
        Ok(body.to_owned())
    })
}

/// Runs the Slack fetch with `auth` against a server answering `responses`. The fetch's result
/// is reported before the server is joined, so a fetch that failed before its calls cannot leave
/// the server waiting.
fn fetch_with(
    auth: &dyn SlackAuth,
    now: DateTime<Utc>,
    responses: Vec<String>,
    log: &Arc<Mutex<Vec<String>>>,
    label: &str,
) -> Result<Result<()>> {
    let expected = responses.len();
    let (api_url, server) = recording_slack_server(responses, Arc::clone(log))?;
    let root = scratch(label)?;
    let request = fetch_request(&root, now, Vec::new(), Vec::new());
    let config = parse_config(&request.config)?;
    let result = run_fetch(&request, &config, Some(&api_url), auth, &mut |_| Ok(()));
    std::fs::remove_dir_all(&root)?;
    let calls = events(log)?
        .iter()
        .filter(|event| event.starts_with("api:"))
        .count();
    if calls == expected {
        server
            .join()
            .map_err(|_| anyhow!("recording Slack server panicked"))??;
    }
    Ok(result)
}

#[test]
fn credential_that_expires_during_fetch_is_refreshed_before_the_first_call() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    for credential in [
        app_credential(now + Duration::minutes(30)),
        claude_code_credential(now + Duration::minutes(30)),
    ] {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let auth = recording_authenticator(credential, now, Arc::clone(&log), 0);
        let (api_url, server) = token_expiry_server(Arc::clone(&log))?;
        let root = scratch("expires-during-fetch")?;
        let request = fetch_request(&root, now, Vec::new(), Vec::new());
        let config = parse_config(&request.config)?;
        let result = run_fetch(&request, &config, Some(&api_url), &auth, &mut |_| Ok(()));
        std::fs::remove_dir_all(&root)?;
        result?;
        server
            .join()
            .map_err(|_| anyhow!("token expiry Slack server panicked"))??;
        assert_eq!(
            events(&log)?,
            [
                "refresh",
                "save:new-access:new-refresh",
                "api:Bearer new-access",
                "api:Bearer new-access",
            ],
            "the renewed credential is saved before both discovery pages use it"
        );
    }
    Ok(())
}

#[test]
fn renewed_token_is_saved_before_use() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let auth = recording_authenticator(
        app_credential(now + Duration::minutes(1)),
        now,
        Arc::clone(&log),
        0,
    );
    fetch_with(&auth, now, one_channel(), &log, "renewed-token")??;
    assert_eq!(
        events(&log)?,
        [
            "refresh",
            "save:new-access:new-refresh",
            "api:Bearer new-access",
            "api:Bearer new-access",
            "api:Bearer new-access",
        ],
        "every call must use the saved token"
    );
    Ok(())
}

/// A save that fails for a moment is tried again after a pause, and the renewed token is used
/// only once a save has succeeded.
#[test]
fn a_save_that_fails_for_a_moment_is_retried_before_use() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let auth = recording_authenticator(
        app_credential(now + Duration::minutes(1)),
        now,
        Arc::clone(&log),
        2,
    );
    fetch_with(&auth, now, one_channel(), &log, "save-retried")??;
    assert_eq!(
        events(&log)?,
        [
            "refresh",
            "save-failed",
            "pause:2s",
            "save-failed",
            "pause:5s",
            "save:new-access:new-refresh",
            "api:Bearer new-access",
            "api:Bearer new-access",
            "api:Bearer new-access",
        ]
    );
    Ok(())
}

/// When no save succeeds the renewed token is never used: the fetch fails before any Slack
/// call, with the save's error, what it means for the stored sign-in, and the fix.
#[test]
fn save_failure_means_no_use() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let attempts = SAVE_RETRY_PAUSES.len() + 1;
    let auth = recording_authenticator(
        app_credential(now + Duration::minutes(1)),
        now,
        Arc::clone(&log),
        attempts,
    );
    let mut messages = Vec::new();
    let (api_url, _server) = recording_slack_server(Vec::new(), Arc::clone(&log))?;
    let root = scratch("save-failure")?;
    let request = fetch_request(&root, now, Vec::new(), Vec::new());
    let config = parse_config(&request.config)?;
    let result = run_fetch(&request, &config, Some(&api_url), &auth, &mut |message| {
        messages.push(message);
        Ok(())
    });
    std::fs::remove_dir_all(&root)?;

    let error = result
        .err()
        .map(|error| format!("{error:#}"))
        .context("a save failure must fail the Slack fetch")?;
    assert!(
        error.contains("could not be saved after 4 tries"),
        "{error}"
    );
    assert!(error.contains("secret manager is unreachable"), "{error}");
    // The job's log line keeps the first few hundred characters; however long the save's own
    // error, the fix is in them.
    let padded = format!("{error} {}", "x".repeat(400));
    assert!(
        crate::redact::one_line(&padded).contains("run `ctx slack login`"),
        "the log line loses the fix: {error}"
    );
    assert!(
        !error.contains("new-access") && !error.contains("new-refresh"),
        "{error}"
    );
    assert!(
        messages.is_empty(),
        "no hello or records are emitted before the failure: {messages:?}"
    );
    assert_eq!(
        events(&log)?,
        [
            "refresh",
            "save-failed",
            "pause:2s",
            "save-failed",
            "pause:5s",
            "save-failed",
            "pause:15s",
            "save-failed",
        ],
        "no Slack API call may run"
    );
    Ok(())
}

/// A process that does not hold the fetch lease (a dev box's fetch, a second fetch) never
/// redeems the refresh token: a token near expiry fails the fetch with the reason, and one that
/// outlives the fetch is used as it is.
#[test]
fn only_the_fetch_lease_holder_refreshes() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    for credential in [
        app_credential(now + Duration::minutes(30)),
        claude_code_credential(now + Duration::minutes(30)),
    ] {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let refresh_log = Arc::clone(&log);
        let auth = Authenticator {
            credential,
            now,
            may_rotate: false,
            refresh: move |credential: &SlackCredential, now| {
                push(&refresh_log, "refresh".to_owned())?;
                Ok(renewed(credential, now))
            },
            save: |_: &SlackCredential| -> Result<()> { bail!("must not save") },
            pause: |_: std::time::Duration| {},
        };
        let error = auth
            .access_token()
            .err()
            .map(|error| format!("{error:#}"))
            .context("a near-expiry token without the lease must fail")?;
        assert!(
            error.contains("holds the fetch lease (CTX_LEASE_BUCKET)"),
            "{error}"
        );
        assert!(error.contains("2026-10-25T18:30:00Z"), "{error}");
        assert!(events(&log)?.is_empty(), "no refresh may run");
    }

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let auth = Authenticator {
        credential: app_credential(now + Duration::hours(6)),
        now,
        may_rotate: false,
        refresh: |_: &SlackCredential, _| -> Result<SlackCredential> { bail!("must not refresh") },
        save: |_: &SlackCredential| -> Result<()> { bail!("must not save") },
        pause: |_: std::time::Duration| {},
    };
    fetch_with(&auth, now, one_channel(), &log, "no-lease-fresh-token")??;
    assert_eq!(
        events(&log)?,
        [
            "api:Bearer old-access",
            "api:Bearer old-access",
            "api:Bearer old-access",
        ]
    );
    Ok(())
}

/// The fetch's own authenticator takes its right to rotate from the fetch lease, which this
/// test process does not hold, so it refuses a near-expiry token before any refresh.
#[test]
fn the_fetch_authenticator_needs_the_fetch_lease_to_refresh() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let auth = fetch_authenticator(app_credential(now + Duration::minutes(5)), now);
    let error = auth
        .access_token()
        .err()
        .map(|error| format!("{error:#}"))
        .context("without the fetch lease the fetch may not refresh")?;
    assert!(error.contains("holds the fetch lease"), "{error}");
    Ok(())
}

/// Runs the Slack source's fetch the way `ctx fetch` does, with `credential` as the stored
/// Slack credential and `api` answering for Slack.
fn fetch_with_stored_credential(
    credential: &str,
    api: &str,
    root: &Path,
    now: DateTime<Utc>,
) -> Result<Result<()>> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow!("locking test environment: {error}"))?;
    let request = fetch_request(root, now, Vec::new(), Vec::new());
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        "CTX_SECRET_SLACK",
        "CTX_CREDENTIAL_SLACK",
        "CTX_CREDENTIAL_SLACK_FILE",
    ]
    .into_iter()
    .map(|name| {
        let previous = std::env::var_os(name);
        std::env::remove_var(name);
        (name, previous)
    })
    .collect();
    std::env::set_var("CTX_CREDENTIAL_SLACK", credential);
    let result = SlackSource::with_api(api).run(&Request::Fetch(request), &mut |_| Ok(()));
    std::env::remove_var("CTX_CREDENTIAL_SLACK");
    for (name, previous) in saved {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        }
    }
    Ok(result)
}

/// A token Slack has revoked fails the Slack fetch with a message that says what to run and
/// why, for either shape of stored credential, and never carries the token itself.
#[test]
fn a_revoked_token_fails_the_fetch_with_the_cause_and_the_fix() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    // The source judges expiry against the clock, so both tokens are far from expiry and the
    // fetch makes no refresh call.
    let app = r#"{"kind":"app","access":"xoxe.xoxp-1-app-token","refresh":"xoxe-1-r","expires":"2099-01-01T00:00:00Z","client_id":"123.456","token_url":"https://unused.example/token","team_id":"T0EXAMPLE1","user_id":"U0EXAMPLE9"}"#;
    let claude = r#"{"access":"xoxe.xoxp-old-token","refresh":"xoxe-1-old","expires":"2099-01-01T00:00:00Z","client_id":"cid","token_url":"https://unused.example/token","resource":"https://mcp.slack.com"}"#;
    for (label, credential, token) in [
        ("app", app, "xoxe.xoxp-1-app-token"),
        ("claude-code", claude, "xoxe.xoxp-old-token"),
    ] {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (api_url, server) = recording_slack_server(
            vec![r#"{"ok":false,"error":"token_revoked"}"#.to_owned()],
            Arc::clone(&log),
        )?;
        let root = scratch(&format!("revoked-{label}"))?;
        let result = fetch_with_stored_credential(credential, &api_url, &root, now)?;
        std::fs::remove_dir_all(&root)?;

        // Checked before the server is joined: a fetch that failed for another reason made no
        // call, and the server would wait for it.
        let error = result
            .err()
            .map(|error| format!("{error:#}"))
            .with_context(|| format!("{label}: a revoked token must fail the Slack fetch"))?;
        assert!(
            error.contains(
                "\"token_revoked\": run `ctx slack login` to sign in again. Slack revoked the stored token"
            ),
            "{label}: {error}"
        );
        // What `ctx fetch` prints for the source in the job's log: the error on one line, cut
        // to its first few hundred characters. The fix must survive the cut.
        let logged = crate::redact::one_line(&error);
        assert!(
            logged.contains("run `ctx slack login` to sign in again"),
            "{label}: the log line loses the fix: {logged}"
        );
        assert!(
            !error.contains(token),
            "{label}: the error carries the token: {error}"
        );
        server
            .join()
            .map_err(|_| anyhow!("recording Slack server panicked"))??;
        assert_eq!(
            events(&log)?,
            [format!("api:Bearer {token}")],
            "{label}: the fetch stops at the first refusal"
        );
    }
    Ok(())
}
