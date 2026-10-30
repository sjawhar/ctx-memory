//! The Gmail source: its history, its listings, message dates and thread resolution.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use ctx_memory::source::{self, FetchRequest, Request, SourceRef};
use serde_json::{json, Value};

use crate::fixtures::{
    fetch_from, fetch_request, remove_scratch, run_conformance, scratch, source_spec, timestamp,
};
use crate::support;
use crate::support::fake_google::{clear_google_environment, environment_lock, FakeGoogle};

#[test]
fn a_gmail_history_entry_whose_message_is_gone_is_skipped() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("gmail-gone")?;
    let server = FakeGoogle::start(vec![
        (
            "/gmail/v1/users/me/history",
            json!({
                "history": [{"id": "900", "messagesAdded": [
                    {"message": {"id": "aaaaaaaaaaaaaaaa"}},
                    {"message": {"id": "bbbbbbbbbbbbbbbb"}}
                ]}],
                "historyId": "1000"
            }),
        ),
        (
            // The second message was deleted after the history named it, so Gmail answers
            // its route with 404; this server answers an unregistered route the same way.
            "/gmail/v1/users/me/messages/aaaaaaaaaaaaaaaa",
            json!({
                "id": "aaaaaaaaaaaaaaaa",
                "threadId": "cccccccccccccccc",
                "internalDate": "1792944000000",
                "payload": {
                    "headers": [{"name": "Subject", "value": "An example"}],
                    "mimeType": "text/plain",
                    "body": {"data": "Zmlyc3Q="}
                }
            }),
        ),
    ])?;
    server.use_in_environment("ya29.gmail-test");

    let spec = source_spec("gmail", "")?;
    let request = Request::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: "gmail".to_owned(),
            kind: "gmail".to_owned(),
            scheme: "gmail".to_owned(),
        },
        config: json!({}),
        window_end: timestamp("2026-10-25T18:00:00Z")?,
        backfill_hours: 24,
        cursors: BTreeMap::from([("me".to_owned(), "850@2026-10-24T00:00:00Z".to_owned())]),
        discovered: Vec::new(),
        state_dir: root.join("state"),
        exclude: Vec::new(),
    });
    let source = source::open(&spec, "gmail")?;
    let outcome = run_conformance(source.as_ref(), &request);

    clear_google_environment();
    remove_scratch(&root)?;

    // One message gone does not stop the stream, and its history is still walked to the end.
    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    let pointers: Vec<String> = outcome
        .records
        .iter()
        .map(|record| record.pointer.clone())
        .collect();
    assert_eq!(pointers, vec!["gmail:aaaaaaaaaaaaaaaa".to_owned()]);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.message.contains("1 message(s)")),
        "{:?}",
        outcome.warnings
    );
    assert_eq!(
        outcome
            .streams
            .get("me")
            .and_then(|stream| stream.cursor.clone()),
        Some("1000@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

/// A Gmail message in `full` format, dated `internal_date` in epoch milliseconds.
fn gmail_message(id: &str, internal_date: &str) -> Value {
    json!({
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": internal_date,
        "payload": {
            "headers": [{"name": "Subject", "value": id}],
            "mimeType": "text/plain",
            "body": {"data": "Zmlyc3Q="}
        }
    })
}

#[test]
fn a_gmail_history_id_that_expires_mid_catch_up_lists_again_from_the_start() -> Result<()> {
    let _environment = environment_lock();
    // 2026-10-06T12:00:00Z, in the first of three slices from 2026-10-05T18:00:00Z.
    let early = ("00000000000000c1", "1791288000000");
    // 2026-10-08T12:00:00Z: imported during the catch-up with its old date.
    let imported = ("00000000000000c2", "1791460800000");
    let spec = source_spec("gmail", "")?;

    let root = scratch("gmail-expired-catch-up")?;
    let server = FakeGoogle::start(vec![
        (
            "/gmail/v1/users/me/profile",
            json!({"emailAddress": "ada@example.com", "historyId": "4242"}),
        ),
        (
            "/gmail/v1/users/me/messages/00000000000000c1",
            gmail_message(early.0, early.1),
        ),
        (
            "/gmail/v1/users/me/messages?",
            json!({"messages": [{"id": early.0}]}),
        ),
    ])?;
    server.use_in_environment("ya29.gmail-test");
    let first = fetch_from(
        &root,
        "gmail",
        json!({}),
        &[],
        "2026-10-25T18:00:00Z",
        20 * 24,
    )?;
    let outcome = run_conformance(source::open(&spec, "gmail")?.as_ref(), &first);
    clear_google_environment();
    remove_scratch(&root)?;
    assert!(!outcome.has_errors(), "{:#?}", outcome.streams);
    let cursor = outcome
        .streams
        .get("me")
        .and_then(|stream| stream.cursor.clone())
        .context("the first slice proposed a cursor")?;

    // Gmail then drops history ID 4242, so no history will name the imported mail.
    let root = scratch("gmail-expired-catch-up")?;
    let server = FakeGoogle::start_answering(vec![
        (
            "/gmail/v1/users/me/history?startHistoryId=4242",
            404,
            support::fake_google::not_found("Requested entity was not found."),
        ),
        (
            "/gmail/v1/users/me/profile",
            200,
            json!({"emailAddress": "ada@example.com", "historyId": "4400"}).to_string(),
        ),
        (
            "/gmail/v1/users/me/messages/00000000000000c1",
            200,
            gmail_message(early.0, early.1).to_string(),
        ),
        (
            "/gmail/v1/users/me/messages/00000000000000c2",
            200,
            gmail_message(imported.0, imported.1).to_string(),
        ),
        (
            "/gmail/v1/users/me/messages?",
            200,
            json!({"messages": [{"id": early.0}, {"id": imported.0}]}).to_string(),
        ),
    ])?;
    server.use_in_environment("ya29.gmail-test");
    let second = fetch_from(
        &root,
        "gmail",
        json!({}),
        &[("me", &cursor)],
        "2026-10-25T19:00:00Z",
        20 * 24,
    )?;
    let outcome = run_conformance(source::open(&spec, "gmail")?.as_ref(), &second);
    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.streams);
    let pointers: Vec<&str> = outcome
        .records
        .iter()
        .map(|record| record.pointer.as_str())
        .collect();
    assert!(
        pointers.contains(&"gmail:00000000000000c2"),
        "the imported mail was never read: {pointers:?}"
    );
    assert_eq!(
        outcome
            .streams
            .get("me")
            .and_then(|stream| stream.cursor.clone()),
        Some("2026-10-05T18:00:00Z/2026-10-12T18:00:00Z#4400".to_owned())
    );
    Ok(())
}

#[test]
fn a_gmail_404_that_is_not_gmails_not_found_ends_the_stream_and_holds_its_cursor() -> Result<()> {
    let _environment = environment_lock();
    // Only Gmail's `notFound` says a message is gone. An empty 404 or one with no reason came
    // from something else, and skipping the message would move the cursor past mail still there.
    for (case, body) in [
        ("an empty body", String::new()),
        (
            "no reason",
            json!({"error": {"code": 404, "message": "Not Found"}}).to_string(),
        ),
    ] {
        let root = scratch("gmail-foreign-404")?;
        let server = FakeGoogle::start_answering(vec![
            (
                "/gmail/v1/users/me/history",
                200,
                json!({
                    "history": [{"id": "900", "messagesAdded": [
                        {"message": {"id": "bbbbbbbbbbbbbbbb"}}
                    ]}],
                    "historyId": "1000"
                })
                .to_string(),
            ),
            ("/gmail/v1/users/me/messages/bbbbbbbbbbbbbbbb", 404, body),
        ])?;
        server.use_in_environment("ya29.gmail-test");

        let spec = source_spec("gmail", "")?;
        let Request::Fetch(mut fetch) = fetch_request(&root, "gmail", "gmail", json!({}))? else {
            anyhow::bail!("fetch_request builds a fetch");
        };
        fetch
            .cursors
            .insert("me".to_owned(), "850@2026-10-24T00:00:00Z".to_owned());
        let request = Request::Fetch(fetch);
        let source = source::open(&spec, "gmail")?;
        let outcome = run_conformance(source.as_ref(), &request);

        clear_google_environment();
        remove_scratch(&root)?;

        let stream = outcome
            .streams
            .get("me")
            .with_context(|| format!("{case}: the me stream ended"))?;
        assert_eq!(stream.cursor, None, "{case}");
        assert!(
            stream
                .error
                .as_deref()
                .is_some_and(|error| error.contains("bbbbbbbbbbbbbbbb")),
            "{case}: {stream:?}"
        );
    }
    Ok(())
}

#[test]
fn a_gmail_message_dated_in_the_future_is_stored_and_the_cursor_moves() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("gmail-future-dated")?;
    let server = FakeGoogle::start(vec![
        (
            "/gmail/v1/users/me/history",
            json!({
                "history": [
                    {"id": "900", "messagesAdded": [{"message": {"id": "eeeeeeeeeeeeeeee"}}]},
                    {"id": "950", "messagesAdded": [{"message": {"id": "aaaaaaaaaaaaaaaa"}}]}
                ],
                "historyId": "1000"
            }),
        ),
        (
            "/gmail/v1/users/me/messages/eeeeeeeeeeeeeeee",
            gmail_message("eeeeeeeeeeeeeeee", "4102444800000"),
        ),
        (
            "/gmail/v1/users/me/messages/aaaaaaaaaaaaaaaa",
            gmail_message("aaaaaaaaaaaaaaaa", "1792944000000"),
        ),
    ])?;
    server.use_in_environment("ya29.gmail-test");

    let spec = source_spec("gmail", "")?;
    let request = fetch_from(
        &root,
        "gmail",
        json!({}),
        &[("me", "850@2026-10-24T00:00:00Z")],
        "2026-10-25T18:00:00Z",
        24,
    )?;
    let outcome = run_conformance(source::open(&spec, "gmail")?.as_ref(), &request);
    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.streams);
    assert_eq!(
        outcome
            .records
            .iter()
            .map(|record| (record.pointer.clone(), record.ts, record.context))
            .collect::<Vec<_>>(),
        vec![
            (
                "gmail:eeeeeeeeeeeeeeee".to_owned(),
                timestamp("2026-10-25T18:00:00Z")?,
                false
            ),
            (
                "gmail:aaaaaaaaaaaaaaaa".to_owned(),
                timestamp("2026-10-25T16:00:00Z")?,
                false
            ),
        ]
    );
    assert_eq!(
        outcome
            .streams
            .get("me")
            .and_then(|stream| stream.cursor.clone()),
        Some("1000@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn gmail_thread_resolution_works_without_a_slack_token_and_orders_messages() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("gmail-resolve")?;
    let server = FakeGoogle::start(vec![(
        "/gmail/v1/users/me/threads/cccccccccccccccc",
        json!({"messages": [
            {
                "id": "bbbbbbbbbbbbbbbb",
                "threadId": "cccccccccccccccc",
                "internalDate": "1792947600000",
                "payload": {
                    "headers": [{"name": "Subject", "value": "Second message"}],
                    "mimeType": "text/plain",
                    "body": {"data": "c2Vjb25kIEdtYWlsIG1lc3NhZ2U"}
                }
            },
            {
                "id": "aaaaaaaaaaaaaaaa",
                "threadId": "cccccccccccccccc",
                "internalDate": "1792944000000",
                "payload": {
                    "headers": [{"name": "Subject", "value": "First message"}],
                    "mimeType": "text/plain",
                    "body": {"data": "Zmlyc3QgR21haWwgbWVzc2FnZQ"}
                }
            }
        ]}),
    )])?;
    let tree = root.join("tree");
    std::fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"gmail\"\nkind = \"gmail\"\n",
    )?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args([
            "--json",
            "--memory",
            tree.to_str().context("tree path is not valid UTF-8")?,
            "resolve",
            "gmail:thread/cccccccccccccccc",
        ])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_GOOGLE_API_BASE", server.base())
        .env("CTX_CREDENTIAL_GOOGLE", "ya29.gmail-test")
        .env_remove("SLACK_TOKEN")
        .output()
        .context("running Gmail thread resolution")?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    let text = response
        .get("data")
        .and_then(|data| data.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .context("Gmail thread resolution returned text")?;
    let first = text
        .find("first Gmail message")
        .context("first message present")?;
    let second = text
        .find("second Gmail message")
        .context("second message present")?;
    assert!(first < second);
    Ok(())
}
