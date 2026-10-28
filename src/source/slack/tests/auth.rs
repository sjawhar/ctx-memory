//! Slack credential and token-rotation tests.

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

fn near_expiry_credential(now: DateTime<Utc>) -> SlackCredential {
    SlackCredential {
        access: "old-access".to_owned(),
        refresh: "old-refresh".to_owned(),
        expires: now + Duration::minutes(1),
        client_id: "cid".to_owned(),
        token_url: "https://unused.example/token".to_owned(),
        resource: "https://mcp.slack.com".to_owned(),
    }
}

fn renewed(credential: &SlackCredential, now: DateTime<Utc>) -> SlackCredential {
    SlackCredential {
        access: "new-access".to_owned(),
        refresh: "new-refresh".to_owned(),
        expires: now + Duration::hours(12),
        client_id: credential.client_id.clone(),
        token_url: credential.token_url.clone(),
        resource: credential.resource.clone(),
    }
}

/// An authenticator that renews any credential to `renewed` and records each save into `log`
/// as `save:<access>:<refresh>`, interleaved with the fake Slack server's calls.
fn recording_authenticator(
    credential: SlackCredential,
    now: DateTime<Utc>,
    log: Arc<Mutex<Vec<String>>>,
) -> impl super::super::auth::SlackAuth {
    Authenticator {
        credential,
        now,
        refresh: |credential: &SlackCredential, now| Ok(renewed(credential, now)),
        save: move |saved: &SlackCredential| -> Result<()> {
            log.lock()
                .map_err(|error| anyhow!("locking the request log: {error}"))?
                .push(format!("save:{}:{}", saved.access, saved.refresh));
            Ok(())
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
            log.lock()
                .map_err(|error| anyhow!("locking the request log: {error}"))?
                .push(format!("api:{authorization}"));
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

#[test]
fn credential_that_expires_during_fetch_is_refreshed_before_the_first_call() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut credential = near_expiry_credential(now);
    credential.expires = now + Duration::minutes(30);
    let auth = recording_authenticator(credential, now, Arc::clone(&log));
    let (api_url, server) = token_expiry_server(Arc::clone(&log))?;
    let root = scratch("expires-during-fetch")?;
    let request = fetch_request(&root, now, Vec::new(), Vec::new());
    let config = parse_config(&request.config)?;
    let result = run_fetch(&request, &config, Some(&api_url), &auth, &mut |_| Ok(()));
    server
        .join()
        .map_err(|_| anyhow!("token expiry Slack server panicked"))??;
    std::fs::remove_dir_all(&root)?;
    result?;

    let events = log
        .lock()
        .map_err(|error| anyhow!("locking the request log: {error}"))?
        .clone();
    assert_eq!(
        events,
        [
            "save:new-access:new-refresh",
            "api:Bearer new-access",
            "api:Bearer new-access",
        ],
        "the renewed credential is saved before both discovery pages use it"
    );
    Ok(())
}

#[test]
fn renewed_token_is_saved_before_use() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let auth = recording_authenticator(near_expiry_credential(now), now, Arc::clone(&log));
    let (api_url, server) = recording_slack_server(
        vec![
            r#"{"ok":true,"channels":[{"id":"C0EXAMPLE01","is_private":false,"is_member":true}]}"#
                .to_owned(),
            r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
            r#"{"ok":true,"messages":[]}"#.to_owned(),
        ],
        Arc::clone(&log),
    )?;
    let root = scratch("renewed-token")?;
    let request = fetch_request(&root, now, Vec::new(), Vec::new());
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    run_fetch(&request, &config, Some(&api_url), &auth, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    server
        .join()
        .map_err(|_| anyhow!("recording Slack server panicked"))??;
    std::fs::remove_dir_all(&root)?;

    let events = log
        .lock()
        .map_err(|error| anyhow!("locking the request log: {error}"))?
        .clone();
    assert_eq!(
        events,
        [
            "save:new-access:new-refresh",
            "api:Bearer new-access",
            "api:Bearer new-access",
            "api:Bearer new-access",
        ],
        "every call must use the saved token"
    );
    Ok(())
}

#[test]
fn save_failure_means_no_use() -> Result<()> {
    let now = at("2026-10-25T18:00:00Z")?;
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let auth = Authenticator {
        credential: near_expiry_credential(now),
        now,
        refresh: |credential: &SlackCredential, now| Ok(renewed(credential, now)),
        save: |_credential: &SlackCredential| -> Result<()> {
            bail!("secret manager is unreachable")
        },
    };
    let (api_url, server) = recording_slack_server(Vec::new(), Arc::clone(&log))?;
    let root = scratch("save-failure")?;
    let request = fetch_request(&root, now, Vec::new(), Vec::new());
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    let result = run_fetch(&request, &config, Some(&api_url), &auth, &mut |message| {
        messages.push(message);
        Ok(())
    });
    server
        .join()
        .map_err(|_| anyhow!("recording Slack server panicked"))??;
    std::fs::remove_dir_all(&root)?;

    let error = result
        .err()
        .context("a save failure must fail the Slack fetch")?;
    assert!(format!("{error:#}").contains("secret manager is unreachable"));
    assert!(
        messages.is_empty(),
        "no hello or records are emitted before the failure: {messages:?}"
    );
    let events = log
        .lock()
        .map_err(|error| anyhow!("locking the request log: {error}"))?
        .clone();
    assert!(
        events.iter().all(|event| !event.starts_with("api:")),
        "no Slack API call may run: {events:?}"
    );
    Ok(())
}
