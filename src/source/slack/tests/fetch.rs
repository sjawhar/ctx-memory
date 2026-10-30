//! Slack discovery, thread-state, fetch and live-resolution tests.

use super::fixtures::*;
#[test]
fn reply_after_window_end_is_left_for_the_next_window() -> Result<()> {
    let root = scratch("reply-after-window")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::days(2));
    let mut index = ThreadIndex::new();
    index.insert(
        "C0EXAMPLE01".to_owned(),
        BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
    );
    write_index(&root, &index)?;
    let future = slack_ts(end + Duration::minutes(1));
    let mut api = FakeSlackApi::default();
    api.replies_by_thread.insert(
        ("C0EXAMPLE01".to_owned(), parent),
        vec![json!({"ts": future, "text": "later", "user": "U0EXAMPLE9"})],
    );

    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert!(records(&messages).is_empty());
    Ok(())
}

#[test]
fn a_reply_to_any_tracked_thread_within_the_poll_horizon_is_fetched() -> Result<()> {
    let root = scratch("all-tracked-threads")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let mut parents = Vec::new();
    let mut tracked = BTreeMap::new();
    for offset in 0_i64..150 {
        let parent = slack_ts(end - Duration::days(2) - Duration::seconds(offset));
        tracked.insert(parent.clone(), slack_ts(end - Duration::days(1)));
        parents.push(parent);
    }
    let oldest = parents
        .last()
        .cloned()
        .context("test created tracked parents")?;
    let mut index = ThreadIndex::new();
    index.insert("C0EXAMPLE01".to_owned(), tracked);
    write_index(&root, &index)?;
    let reply = slack_ts(end - Duration::hours(1));
    let mut api = FakeSlackApi::default();
    api.replies_by_thread.insert(
        ("C0EXAMPLE01".to_owned(), oldest.clone()),
        vec![
            json!({"ts": oldest, "text": "parent", "user": "U0EXAMPLE9"}),
            json!({"ts": reply, "text": "new reply", "user": "U0EXAMPLE9"}),
        ],
    );

    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(api.reply_calls.borrow().len(), 150);
    assert!(api
        .reply_calls
        .borrow()
        .iter()
        .any(|(_, parent)| parent == &oldest));
    assert_eq!(records(&messages).len(), 2);
    assert!(records(&messages)
        .iter()
        .any(|record| record.pointer.ends_with(&reply)));
    Ok(())
}

#[test]
fn threads_past_the_horizon_are_counted_in_a_warning() -> Result<()> {
    let root = scratch("thread-horizon")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::days(40));
    let mut index = ThreadIndex::new();
    index.insert(
        "C0EXAMPLE01".to_owned(),
        BTreeMap::from([(parent, slack_ts(end - Duration::days(40)))]),
    );
    write_index(&root, &index)?;

    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let messages = fetch_messages(&request, &FakeSlackApi::default())?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        warnings(&messages),
        vec!["1 tracked thread(s) idle over 30 days not polled"]
    );
    Ok(())
}
#[test]
fn discovers_public_channels_and_listed_private_only() -> Result<()> {
    let api = FakeSlackApi {
        conversations: vec![
            json!({"id": "C0PUBLICIN0", "is_private": false, "is_member": true}),
            json!({"id": "C0PUBLICOUT", "is_private": false, "is_member": false}),
            json!({"id": "G0PRIVLIST0", "is_private": true, "is_member": true}),
            json!({"id": "G0PRIVUNLST", "is_private": true, "is_member": true}),
            json!({"id": "C0GROUPDM01", "is_mpim": true}),
        ],
        ..Default::default()
    };
    let config = SlackConfig {
        workspace: "acme".to_owned(),
        channels: vec![
            "G0PRIVLIST0".to_owned(),
            "C0GROUPDM01".to_owned(),
            "D0DIRECT001".to_owned(),
        ],
        thread_poll_days: 30,
    };

    let (streams, warnings) = discover_streams(&api, &config, false)?;

    assert_eq!(
        streams,
        vec!["C0PUBLICIN0".to_owned(), "G0PRIVLIST0".to_owned()]
    );
    assert!(!streams.iter().any(|stream| stream == "C0PUBLICOUT"));
    assert!(!streams.iter().any(|stream| stream == "G0PRIVUNLST"));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("C0GROUPDM01") && warning.contains("group message")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("D0DIRECT001")));
    Ok(())
}
#[test]
fn fetched_parent_and_reply_update_the_thread_index() -> Result<()> {
    let root = scratch("thread-index")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::hours(2));
    let latest_parent = slack_ts(end - Duration::hours(1));
    let latest_reply = slack_ts(end - Duration::minutes(30));
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        "C0EXAMPLE01".to_owned(),
        vec![json!({
            "ts": parent,
            "reply_count": 1,
            "latest_reply": latest_parent,
            "text": "parent",
        })],
    );
    api.replies_by_thread.insert(
        ("C0EXAMPLE01".to_owned(), parent.clone()),
        vec![json!({
            "ts": latest_reply,
            "thread_ts": parent,
            "text": "reply",
        })],
    );

    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let _messages = fetch_messages(&request, &api)?;
    let index = read_thread_index(&root.join("state"))?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        index
            .get("C0EXAMPLE01")
            .and_then(|threads| threads.get(&parent))
            .map(String::as_str),
        Some(latest_reply.as_str())
    );
    Ok(())
}

#[test]
fn a_thread_index_write_keeps_what_another_fetch_added() -> Result<()> {
    // Every machine fetching a tree shares the index, and the fetch lock does not travel
    // with it: it locks the machine's own state directory (`crate::fetch::acquire_lock`).
    // Two fetches can therefore be in flight at once, and dropping the thread the other
    // one added would leave it unpolled and lose its replies.
    let root = scratch("thread-index-merge")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let mine = slack_ts(end - Duration::hours(3));
    let theirs = slack_ts(end - Duration::hours(2));
    let newer = slack_ts(end - Duration::minutes(10));
    let mut concurrent = ThreadIndex::new();
    update_thread_activity(&mut concurrent, "C0EXAMPLE01", &theirs, &theirs)?;
    update_thread_activity(&mut concurrent, "C0EXAMPLE01", &mine, &mine)?;
    write_thread_index(&root, &concurrent)?;

    let mut held = ThreadIndex::new();
    update_thread_activity(&mut held, "C0EXAMPLE01", &mine, &newer)?;
    write_thread_index(&root, &held)?;
    let written = read_thread_index(&root)?;
    std::fs::remove_dir_all(&root)?;

    let threads = written
        .get("C0EXAMPLE01")
        .context("the channel keeps its tracked threads")?;
    assert_eq!(
        threads.get(&theirs).map(String::as_str),
        Some(theirs.as_str()),
        "the thread the concurrent fetch added survives"
    );
    assert_eq!(
        threads.get(&mine).map(String::as_str),
        Some(newer.as_str()),
        "the newer activity on a shared thread wins"
    );
    Ok(())
}

#[test]
fn resolve_uses_thread_replies_and_message_history() -> Result<()> {
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::hours(2));
    let reply = slack_ts(end - Duration::hours(1));
    let mut api = FakeSlackApi {
        conversations: vec![json!({"id": "C0EXAMPLE01", "is_private": false, "is_member": true})],
        ..Default::default()
    };
    api.replies_by_thread.insert(
        ("C0EXAMPLE01".to_owned(), parent.clone()),
        vec![json!({
            "ts": reply,
            "thread_ts": parent,
            "text": "reply",
        })],
    );
    api.histories.insert(
        "C0EXAMPLE01".to_owned(),
        vec![json!({"ts": parent, "text": "parent"})],
    );
    let request = ResolveRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: json!({"workspace": "acme"}),
        pointers: vec![format!("slack:C0EXAMPLE01/{parent}")],
        state_dir: std::env::temp_dir(),
    };
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    emit_resolve_hello(&request, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    resolve_with_api(&request, &config, &api, &mut |message| {
        messages.push(message);
        Ok(())
    })?;

    assert_eq!(records(&messages).len(), 2);
    assert!(records(&messages)
        .iter()
        .any(|record| record.pointer.ends_with(&parent)));
    assert!(records(&messages)
        .iter()
        .any(|record| record.pointer.ends_with(&reply)));
    Ok(())
}

#[test]
fn configured_scheme_is_used_for_fetched_records() -> Result<()> {
    let root = scratch("configured-scheme")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let message = slack_ts(end - Duration::hours(1));
    let expected = format!("acme-slack:C0EXAMPLE01/{message}");
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        "C0EXAMPLE01".to_owned(),
        vec![json!({"ts": message, "text": "message"})],
    );
    let mut request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    request.source.scheme = "acme-slack".to_owned();

    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(records(&messages).len(), 1);
    assert_eq!(
        records(&messages)
            .first()
            .map(|record| record.pointer.as_str()),
        Some(expected.as_str())
    );
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one credential setup drives both the fetch and the live resolve of one parent"
)]
fn fractional_parent_is_retained_for_fetch_and_live_resolution() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow!("locking test environment: {error}"))?;
    let root = scratch("fractional-parent")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = format!("{}.123456", (end - Duration::days(2)).timestamp());
    let reply = format!("{}.654321", (end - Duration::hours(1)).timestamp());
    let thread = format!(
        r#"{{"ok":true,"messages":[{{"ts":"{parent}","text":"parent"}},{{"ts":"{reply}","thread_ts":"{parent}","text":"reply"}}]}}"#
    );
    let mut index = ThreadIndex::new();
    index.insert(
        "C0EXAMPLE01".to_owned(),
        BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
    );
    write_index(&root, &index)?;
    let credential = r#"{"access":"test-token","refresh":"r","expires":"2099-01-01T00:00:00Z","client_id":"cid","token_url":"https://unused.example/token","resource":"https://mcp.slack.com"}"#;
    let previous_credential = std::env::var_os("CTX_CREDENTIAL_SLACK");
    let previous_secret = std::env::var_os("CTX_SECRET_SLACK");
    std::env::remove_var("CTX_SECRET_SLACK");
    std::env::set_var("CTX_CREDENTIAL_SLACK", credential);
    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let (fetch_api, fetch_requests, fetch_server) = fake_http_api(vec![
        r#"{"ok":true,"channels":[{"id":"C0EXAMPLE01","is_private":false,"is_member":true}]}"#
            .to_owned(),
        r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
        r#"{"ok":true,"messages":[]}"#.to_owned(),
        thread.clone(),
    ])?;
    let source = SlackSource::with_api(fetch_api);
    let mut fetched = Vec::new();
    source.run(&Request::Fetch(request), &mut |message| {
        fetched.push(message);
        Ok(())
    })?;
    fetch_server
        .join()
        .map_err(|_| anyhow!("fetch fake API server panicked"))??;
    let fetch_request = fetch_requests
        .lock()
        .map_err(|error| anyhow!("locking fetch requests: {error}"))?
        .iter()
        .find(|request| request.contains("conversations.replies"))
        .cloned()
        .context("fetch called conversations.replies")?;
    assert!(fetch_request.contains(&format!("ts={parent}")));
    assert_eq!(records(&fetched).len(), 2);
    assert!(records(&fetched)
        .iter()
        .any(|record| record.pointer.ends_with(&parent) && record.context));

    let resolve_request = Request::Resolve(ResolveRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: json!({"workspace": "acme"}),
        pointers: vec![format!("slack:C0EXAMPLE01/{parent}")],
        state_dir: root.join("state"),
    });
    let (resolve_api, resolve_requests, resolve_server) = fake_http_api(vec![
        r#"{"ok":true,"channels":[{"id":"C0EXAMPLE01","is_private":false,"is_member":true}]}"#
            .to_owned(),
        thread,
        r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
    ])?;
    let source = SlackSource::with_api(resolve_api);
    let mut resolved = Vec::new();
    let resolve_result = source.run(&resolve_request, &mut |message| {
        resolved.push(message);
        Ok(())
    });
    match previous_credential {
        Some(value) => std::env::set_var("CTX_CREDENTIAL_SLACK", value),
        None => std::env::remove_var("CTX_CREDENTIAL_SLACK"),
    }
    if let Some(value) = previous_secret {
        std::env::set_var("CTX_SECRET_SLACK", value);
    }
    resolve_result?;
    resolve_server
        .join()
        .map_err(|_| anyhow!("resolve fake API server panicked"))??;
    let resolve_request = resolve_requests
        .lock()
        .map_err(|error| anyhow!("locking resolve requests: {error}"))?
        .iter()
        .find(|request| request.contains("conversations.replies"))
        .cloned()
        .context("resolve called conversations.replies")?;
    std::fs::remove_dir_all(&root)?;

    assert!(resolve_request.contains(&format!("ts={parent}")));
    assert_eq!(records(&resolved).len(), 2);
    assert!(records(&resolved)
        .iter()
        .any(|record| record.pointer.ends_with(&parent)));
    assert!(records(&resolved)
        .iter()
        .any(|record| record.pointer.ends_with(&reply)));
    Ok(())
}

#[test]
fn a_thread_index_that_cannot_be_saved_ends_no_stream_ok() -> Result<()> {
    // A stream that ends Ok moves its cursor past this window, and the threads found in it
    // are polled again only through the index. Ending Ok before the index is saved would
    // lose every later reply to them when the save fails.
    let root = scratch("index-save-fails")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::hours(2));
    let reply = slack_ts(end - Duration::hours(1));
    let channels = vec!["C0EXAMPLE01".to_owned(), "C0EXAMPLE02".to_owned()];
    let mut api = FakeSlackApi::default();
    for channel in &channels {
        api.histories.insert(
            channel.clone(),
            vec![json!({"ts": parent, "reply_count": 1, "latest_reply": reply, "text": "parent"})],
        );
        api.replies_by_thread.insert(
            (channel.clone(), parent.clone()),
            vec![
                json!({"ts": parent, "text": "parent"}),
                json!({"ts": reply, "thread_ts": parent, "text": "reply"}),
            ],
        );
    }
    api.corrupt_index = Some(root.join("state").join(THREAD_INDEX_FILE));
    let request = fetch_request(&root, end, channels.clone(), Vec::new());
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    let result = fetch_with_api(&request, &config, &channels, &api, &mut |message| {
        messages.push(message);
        Ok(())
    });
    std::fs::remove_dir_all(&root)?;

    let error = result
        .err()
        .context("a thread index that cannot be saved must fail the run")?;
    assert!(
        format!("{error:#}").contains(THREAD_INDEX_FILE),
        "{error:#}"
    );
    assert!(
        !messages.iter().any(|message| matches!(
            message,
            Message::StreamEnd {
                status: StreamStatus::Ok,
                ..
            }
        )),
        "no stream may end Ok before its threads are saved: {messages:?}"
    );
    Ok(())
}

#[test]
fn a_first_reply_to_a_message_read_without_replies_is_fetched_next_run() -> Result<()> {
    // A reply never appears in the channel's history, and a message nobody had answered
    // when it was read carries no `latest_reply` to track its thread by.
    let root = scratch("first-reply")?;
    let first_end = at("2026-10-25T11:00:00Z")?;
    let second_end = at("2026-10-25T12:00:00Z")?;
    let parent = slack_ts(at("2026-10-25T10:50:00Z")?);
    let reply = slack_ts(at("2026-10-25T11:20:00Z")?);
    let channel = "C0EXAMPLE01".to_owned();

    let mut unanswered = FakeSlackApi::default();
    unanswered.histories.insert(
        channel.clone(),
        vec![json!({"ts": parent, "text": "question", "user": "U0EXAMPLE9"})],
    );
    let first = fetch_messages(
        &fetch_request(&root, first_end, vec![channel.clone()], Vec::new()),
        &unanswered,
    )?;

    let mut answered = FakeSlackApi::default();
    answered.histories.insert(
        channel.clone(),
        vec![json!({
            "ts": parent,
            "thread_ts": parent,
            "reply_count": 1,
            "latest_reply": reply,
            "text": "question",
            "user": "U0EXAMPLE9",
        })],
    );
    answered.replies_by_thread.insert(
        (channel.clone(), parent.clone()),
        vec![
            json!({"ts": parent, "thread_ts": parent, "text": "question", "user": "U0EXAMPLE9"}),
            json!({"ts": reply, "thread_ts": parent, "text": "answer", "user": "U0EXAMPLE9"}),
        ],
    );
    let mut request = fetch_request(&root, second_end, vec![channel.clone()], Vec::new());
    request.cursors.insert(
        channel,
        first_end.to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let second = fetch_messages(&request, &answered)?;
    std::fs::remove_dir_all(&root)?;

    assert!(records(&first)
        .iter()
        .any(|record| record.pointer.ends_with(&parent)));
    let answer = records(&second)
        .into_iter()
        .find(|record| record.pointer.ends_with(&reply))
        .context("the first reply is fetched in the window it was posted in")?;
    assert!(!answer.context);
    Ok(())
}

#[test]
fn a_first_reply_is_looked_for_only_within_the_thread_poll_horizon() -> Result<()> {
    // The horizon bounds the read-back: a message older than `thread_poll_days` is neither
    // read again nor polled, so the fetch never polls every message forever.
    let root = scratch("first-reply-horizon")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let recent = slack_ts(end - Duration::days(1));
    let old = slack_ts(end - Duration::days(3));
    let recent_reply = slack_ts(end - Duration::minutes(30));
    let old_reply = slack_ts(end - Duration::minutes(20));
    let channel = "C0EXAMPLE01".to_owned();
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        channel.clone(),
        vec![
            json!({"ts": old, "reply_count": 1, "latest_reply": old_reply, "text": "old"}),
            json!({"ts": recent, "reply_count": 1, "latest_reply": recent_reply, "text": "recent"}),
        ],
    );
    for (parent, reply) in [(&old, &old_reply), (&recent, &recent_reply)] {
        api.replies_by_thread.insert(
            (channel.clone(), parent.clone()),
            vec![
                json!({"ts": parent, "text": "parent"}),
                json!({"ts": reply, "thread_ts": parent, "text": "reply"}),
            ],
        );
    }
    let mut request = fetch_request(&root, end, vec![channel.clone()], Vec::new());
    request.config = json!({"workspace": "acme", "channels": [channel], "thread_poll_days": 2});
    request.cursors.insert(
        channel.clone(),
        (end - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert!(records(&messages)
        .iter()
        .any(|record| record.pointer.ends_with(&recent_reply)));
    assert!(!records(&messages)
        .iter()
        .any(|record| record.pointer.ends_with(&old_reply)));
    assert_eq!(*api.reply_calls.borrow(), vec![(channel, recent)]);
    Ok(())
}

fn stream_cursor(messages: &[Message], channel: &str) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::StreamEnd { stream, cursor, .. } if stream == channel => cursor.clone(),
        _ => None,
    })
}

#[test]
fn history_is_read_back_48_hours_for_first_replies_and_threads_are_followed_longer() -> Result<()> {
    // A message nobody had answered is read again for 48 hours after it is posted; a thread
    // with replies is followed for `thread_poll_days` after its last reply.
    let root = scratch("first-reply-48h")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let channel = "C0EXAMPLE01".to_owned();
    let recent = slack_ts(end - Duration::days(1));
    let older = slack_ts(end - Duration::days(3));
    let tracked = slack_ts(end - Duration::days(20));
    let replies = [
        (&recent, slack_ts(end - Duration::minutes(20))),
        (&older, slack_ts(end - Duration::minutes(30))),
        (&tracked, slack_ts(end - Duration::minutes(10))),
    ];
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        channel.clone(),
        vec![
            json!({"ts": older, "reply_count": 1, "latest_reply": replies[1].1, "text": "older"}),
            json!({"ts": recent, "reply_count": 1, "latest_reply": replies[0].1, "text": "recent"}),
        ],
    );
    for (parent, reply) in &replies {
        api.replies_by_thread.insert(
            (channel.clone(), (*parent).clone()),
            vec![
                json!({"ts": parent, "text": "parent"}),
                json!({"ts": reply, "thread_ts": parent, "text": "reply"}),
            ],
        );
    }
    let mut index = ThreadIndex::new();
    index.insert(
        channel.clone(),
        BTreeMap::from([(tracked.clone(), slack_ts(end - Duration::days(1)))]),
    );
    write_index(&root, &index)?;
    let mut request = fetch_request(&root, end, vec![channel.clone()], Vec::new());
    request.cursors.insert(
        channel.clone(),
        (end - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        *api.history_calls.borrow(),
        vec![(channel.clone(), end - Duration::hours(48), end)]
    );
    let fetched: Vec<&str> = records(&messages)
        .iter()
        .map(|record| record.pointer.as_str())
        .collect();
    assert!(fetched
        .iter()
        .any(|pointer| pointer.ends_with(&replies[0].1)));
    assert!(fetched
        .iter()
        .any(|pointer| pointer.ends_with(&replies[2].1)));
    // The cost: a first reply more than 48 hours after its message is not read.
    assert!(!fetched
        .iter()
        .any(|pointer| pointer.ends_with(&replies[1].1)));
    assert!(!api
        .reply_calls
        .borrow()
        .contains(&(channel.clone(), older.clone())));
    Ok(())
}

#[test]
fn a_long_gap_in_a_channel_is_read_one_slice_at_a_time() -> Result<()> {
    let root = scratch("slack-slice")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let start = end - Duration::days(20);
    let channel = "C0EXAMPLE01".to_owned();
    let early = slack_ts(start + Duration::days(1));
    let late = slack_ts(start + Duration::days(10));
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        channel.clone(),
        vec![
            json!({"ts": early, "text": "early", "user": "U0EXAMPLE9"}),
            json!({"ts": late, "text": "late", "user": "U0EXAMPLE9"}),
        ],
    );
    let mut request = fetch_request(&root, end, vec![channel.clone()], Vec::new());
    request.cursors.insert(
        channel.clone(),
        start.to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    let slice_end = start + Duration::days(7);
    let fetched: Vec<&str> = records(&messages)
        .iter()
        .map(|record| record.pointer.as_str())
        .collect();
    assert!(fetched.iter().any(|pointer| pointer.ends_with(&early)));
    assert!(!fetched.iter().any(|pointer| pointer.ends_with(&late)));
    assert_eq!(
        stream_cursor(&messages, &channel),
        Some(slice_end.to_rfc3339_opts(SecondsFormat::Secs, true))
    );
    assert_eq!(
        *api.history_calls.borrow(),
        vec![(channel.clone(), start, slice_end)]
    );
    Ok(())
}

#[test]
fn a_channel_pointer_names_only_a_conversation_a_fetch_can_read() {
    assert!(is_channel("C0EXAMPLE01"));
    assert!(is_channel("G0PRIVATE01"));
    assert!(!is_channel("D0DIRECT001"));
}

/// A fake API that answers a thread in every conversation below, so only the scope check
/// can keep one from being read. The listing is what `conversations.list` returns for public
/// and private channels: a direct message is never in it.
fn scoped_api(timestamp: &str) -> FakeSlackApi {
    let mut api = FakeSlackApi {
        conversations: vec![
            json!({"id": "C0PUBLICIN0", "is_private": false, "is_member": true}),
            json!({"id": "C0PUBLICOUT", "is_private": false, "is_member": false}),
            json!({"id": "G0PRIVLIST0", "is_private": true, "is_member": true}),
            json!({"id": "G0PRIVUNLST", "is_private": true, "is_member": true}),
            json!({"id": "G0GROUPDM01", "is_private": true, "is_mpim": true, "is_member": true}),
        ],
        ..Default::default()
    };
    for channel in [
        "C0PUBLICIN0",
        "C0PUBLICOUT",
        "G0PRIVLIST0",
        "G0PRIVUNLST",
        "G0GROUPDM01",
        "D0DIRECT001",
    ] {
        api.replies_by_thread.insert(
            (channel.to_owned(), timestamp.to_owned()),
            vec![json!({"ts": timestamp, "text": "private words"})],
        );
    }
    api
}

fn resolve_one(
    api: &FakeSlackApi,
    listed: &[&str],
    pointer: &str,
) -> Result<(Result<()>, Vec<Message>)> {
    let request = ResolveRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: json!({"workspace": "acme", "channels": listed}),
        pointers: vec![pointer.to_owned()],
        state_dir: std::env::temp_dir(),
    };
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    let result = resolve_with_api(&request, &config, api, &mut |message| {
        messages.push(message);
        Ok(())
    });
    Ok((result, messages))
}

#[test]
fn resolve_reads_an_archived_channel_a_fetch_once_read() -> Result<()> {
    // An archived channel takes no new messages, so a fetch does not list it, but what the
    // fetch read there before is still in scope.
    let timestamp = "1792951800.000100";
    let mut api = scoped_api(timestamp);
    api.conversations.push(json!({
        "id": "C0ARCHIVED1",
        "is_private": false,
        "is_member": true,
        "is_archived": true,
    }));
    api.replies_by_thread.insert(
        ("C0ARCHIVED1".to_owned(), timestamp.to_owned()),
        vec![json!({"ts": timestamp, "text": "old words"})],
    );

    let (result, messages) = resolve_one(&api, &[], &format!("slack:C0ARCHIVED1/{timestamp}"))?;

    result?;
    assert_eq!(records(&messages).len(), 1);
    Ok(())
}

#[test]
fn a_direct_message_pointer_is_refused_before_any_slack_call() -> Result<()> {
    let api = scoped_api("1792951800.000100");

    let (result, _messages) = resolve_one(&api, &[], "slack:D0DIRECT001/1792951800.000100")?;

    let error = result.err().context("a direct message must be refused")?;
    assert!(format!("{error:#}").contains("direct or group message"));
    assert_eq!(api.list_calls.get(), 0, "the refusal called Slack");
    Ok(())
}

#[test]
fn resolve_never_reads_a_direct_or_group_message() -> Result<()> {
    let timestamp = "1792951800.000100";
    let api = scoped_api(timestamp);
    for channel in ["D0DIRECT001", "G0GROUPDM01"] {
        // Listing a direct or group message does not bring it into scope either.
        let (result, messages) =
            resolve_one(&api, &[channel], &format!("slack:{channel}/{timestamp}"))?;
        let error = result
            .err()
            .with_context(|| format!("resolving {channel} must be refused"))?;
        assert!(
            format!("{error:#}").contains("direct or group message"),
            "{error:#}"
        );
        assert!(records(&messages).is_empty());
    }
    assert!(
        api.reply_calls.borrow().is_empty(),
        "a refused conversation was read: {:?}",
        api.reply_calls.borrow()
    );
    Ok(())
}

#[test]
fn resolve_reads_only_the_channels_a_fetch_reads() -> Result<()> {
    let timestamp = "1792951800.000100";
    let api = scoped_api(timestamp);
    for channel in ["C0PUBLICOUT", "G0PRIVUNLST"] {
        let (result, messages) = resolve_one(
            &api,
            &["G0PRIVLIST0"],
            &format!("slack:{channel}/{timestamp}"),
        )?;
        let error = result
            .err()
            .with_context(|| format!("resolving {channel} must be refused"))?;
        assert!(format!("{error:#}").contains(channel), "{error:#}");
        assert!(records(&messages).is_empty());
    }
    assert!(
        api.reply_calls.borrow().is_empty(),
        "a refused conversation was read: {:?}",
        api.reply_calls.borrow()
    );
    for channel in ["C0PUBLICIN0", "G0PRIVLIST0"] {
        let (result, messages) = resolve_one(
            &api,
            &["G0PRIVLIST0"],
            &format!("slack:{channel}/{timestamp}"),
        )?;
        result?;
        assert_eq!(records(&messages).len(), 1, "{channel}");
    }
    Ok(())
}

#[test]
fn a_fetch_asks_slack_about_each_person_and_channel_once() -> Result<()> {
    let root = scratch("mention-names")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let text = "<@U0EXAMPLEA> and <@U0EXAMPLED|ada>, <@U0UNKNOWN>: see <#C0EXAMPLE02|> \
                and <#C0LAUNCH01|>, not <#G0SECRET01|>";
    let mut api = FakeSlackApi::default();
    for (channel, hours) in [("C0EXAMPLE01", 2), ("C0EXAMPLE02", 1)] {
        api.histories.insert(
            channel.to_owned(),
            vec![json!({
                "ts": slack_ts(end - Duration::hours(hours)),
                "user": "U0AUTHOR01",
                "text": text,
            })],
        );
    }
    api.channels = BTreeMap::from([
        (
            "C0EXAMPLE01".to_owned(),
            json!({"name": "general", "is_private": false}),
        ),
        (
            "C0EXAMPLE02".to_owned(),
            json!({"name": "planning", "is_private": true}),
        ),
        (
            "C0LAUNCH01".to_owned(),
            json!({"name": "launch", "is_private": false}),
        ),
        (
            "G0SECRET01".to_owned(),
            json!({"name": "secret", "is_private": true}),
        ),
    ]);
    api.users = BTreeMap::from([
        (
            "U0AUTHOR01".to_owned(),
            Some(json!({"profile": {"display_name": "Kim"}})),
        ),
        (
            "U0EXAMPLEA".to_owned(),
            Some(json!({"profile": {"real_name": "Grace Hopper"}})),
        ),
        (
            "U0EXAMPLED".to_owned(),
            Some(json!({"real_name": "Ada Lovelace"})),
        ),
        ("U0UNKNOWN".to_owned(), None),
    ]);
    let request = fetch_request(
        &root,
        end,
        vec!["C0EXAMPLE01".to_owned(), "C0EXAMPLE02".to_owned()],
        Vec::new(),
    );

    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    // A private channel this run reads is named; one it does not read keeps its id.
    let expected = "@Grace Hopper and @Ada Lovelace, @U0UNKNOWN: see #planning \
                    and #launch, not #G0SECRET01";
    let fetched = records(&messages);
    assert_eq!(
        fetched
            .iter()
            .map(|record| record.text.as_str())
            .collect::<Vec<_>>(),
        [expected, expected]
    );
    assert!(fetched
        .iter()
        .all(|record| record.author.as_deref() == Some("Kim")));
    let mut users = api.user_calls.borrow().clone();
    users.sort();
    assert_eq!(
        users,
        ["U0AUTHOR01", "U0EXAMPLEA", "U0EXAMPLED", "U0UNKNOWN"],
        "one users.info call per person per run"
    );
    let mut channels = api.channel_calls.borrow().clone();
    channels.sort();
    assert_eq!(
        channels,
        ["C0EXAMPLE01", "C0EXAMPLE02", "C0LAUNCH01", "G0SECRET01"],
        "one conversations.info call per channel per run"
    );
    Ok(())
}

#[test]
fn a_mention_whose_lookup_fails_keeps_its_id_and_is_warned() -> Result<()> {
    let root = scratch("mention-lookup-fails")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let mut api = FakeSlackApi::default();
    api.histories.insert(
        "C0EXAMPLE01".to_owned(),
        vec![json!({
            "ts": slack_ts(end - Duration::hours(1)),
            "user": "U0AUTHOR01",
            "text": "<@U0FLAKY01> please review",
        })],
    );
    api.users
        .insert("U0AUTHOR01".to_owned(), Some(json!({"real_name": "Kim"})));
    api.user_errors
        .insert("U0FLAKY01".to_owned(), "ratelimited".to_owned());
    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());

    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        records(&messages)
            .iter()
            .map(|record| record.text.as_str())
            .collect::<Vec<_>>(),
        ["@U0FLAKY01 please review"]
    );
    assert!(
        stream_cursor(&messages, "C0EXAMPLE01").is_some(),
        "the channel's stream still ends Ok"
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("U0FLAKY01") && warning.contains("ratelimited")),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}

#[test]
fn a_resolve_names_the_people_a_message_mentions() -> Result<()> {
    let timestamp = "1792951800.000100";
    let mut api = scoped_api(timestamp);
    api.replies_by_thread.insert(
        ("C0PUBLICIN0".to_owned(), timestamp.to_owned()),
        vec![json!({
            "ts": timestamp,
            "user": "U0AUTHOR01",
            "text": "<@U0EXAMPLEA> to decide",
        })],
    );
    api.users = BTreeMap::from([
        ("U0AUTHOR01".to_owned(), Some(json!({"real_name": "Kim"}))),
        (
            "U0EXAMPLEA".to_owned(),
            Some(json!({"real_name": "Grace Hopper"})),
        ),
    ]);

    let (result, messages) = resolve_one(&api, &[], &format!("slack:C0PUBLICIN0/{timestamp}"))?;
    result?;

    assert_eq!(
        records(&messages)
            .iter()
            .map(|record| record.text.as_str())
            .collect::<Vec<_>>(),
        ["@Grace Hopper to decide"]
    );
    Ok(())
}

fn stream_error(messages: &[Message], channel: &str) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::StreamEnd {
            stream,
            status: StreamStatus::Error,
            error,
            ..
        } if stream == channel => error.clone(),
        _ => None,
    })
}

/// Slack is asked about each id once per run, whatever needs it. A lookup that failed for a
/// mention still fails a channel whose author or label needs that id, as a failed author or
/// channel lookup always has, and is not asked again.
#[test]
fn a_failed_lookup_is_kept_for_the_whole_run() -> Result<()> {
    let root = scratch("mention-failure-kept")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let mut api = FakeSlackApi::default();
    for (channel, author, text) in [
        (
            "C0EXAMPLE01",
            "U0AUTHOR01",
            "<@U0FLAKY01> and <#C0EXAMPLE02|> please",
        ),
        ("C0EXAMPLE02", "U0AUTHOR01", "not read"),
        ("C0EXAMPLE03", "U0FLAKY01", "my own words"),
    ] {
        api.histories.insert(
            channel.to_owned(),
            vec![json!({
                "ts": slack_ts(end - Duration::hours(1)),
                "user": author,
                "text": text,
            })],
        );
    }
    api.users
        .insert("U0AUTHOR01".to_owned(), Some(json!({"real_name": "Kim"})));
    api.user_errors
        .insert("U0FLAKY01".to_owned(), "ratelimited".to_owned());
    api.channel_errors
        .insert("C0EXAMPLE02".to_owned(), "channel_not_found".to_owned());
    let request = fetch_request(
        &root,
        end,
        vec![
            "C0EXAMPLE01".to_owned(),
            "C0EXAMPLE02".to_owned(),
            "C0EXAMPLE03".to_owned(),
        ],
        Vec::new(),
    );

    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    assert_eq!(
        records(&messages)
            .iter()
            .map(|record| record.text.as_str())
            .collect::<Vec<_>>(),
        ["@U0FLAKY01 and #C0EXAMPLE02 please"]
    );
    assert!(stream_cursor(&messages, "C0EXAMPLE01").is_some());
    for (channel, cause) in [
        ("C0EXAMPLE02", "channel_not_found"),
        ("C0EXAMPLE03", "ratelimited"),
    ] {
        let error = stream_error(&messages, channel)
            .with_context(|| format!("{channel} must end in error"))?;
        assert!(error.contains(cause), "{channel}: {error}");
    }
    let count = |calls: &RefCell<Vec<String>>, id: &str| {
        calls.borrow().iter().filter(|call| *call == id).count()
    };
    assert_eq!(count(&api.user_calls, "U0FLAKY01"), 1);
    assert_eq!(count(&api.channel_calls, "C0EXAMPLE02"), 1);
    // The warning names the mention that kept its id. A channel Slack does not know is not a
    // failure to warn of: Slack answers so for a private channel the user is not in.
    let warned = warnings(&messages);
    assert!(
        warned.iter().any(|warning| warning.contains("U0FLAKY01")),
        "{warned:?}"
    );
    assert!(
        !warned.iter().any(|warning| warning.contains("C0EXAMPLE02")),
        "{warned:?}"
    );
    Ok(())
}

/// A user Slack answers for with no name is shown by the mention's label, as an unknown one
/// is; the author line still falls back to the id.
#[test]
fn a_user_slack_gives_no_name_is_shown_by_the_mention_label() -> Result<()> {
    let timestamp = "1792951800.000100";
    let mut api = scoped_api(timestamp);
    api.replies_by_thread.insert(
        ("C0PUBLICIN0".to_owned(), timestamp.to_owned()),
        vec![json!({
            "ts": timestamp,
            "user": "U0NAMELESS",
            "text": "<@U0NAMELESS|lin> decides",
        })],
    );
    api.users
        .insert("U0NAMELESS".to_owned(), Some(json!({"id": "U0NAMELESS"})));

    let (result, messages) = resolve_one(&api, &[], &format!("slack:C0PUBLICIN0/{timestamp}"))?;
    result?;

    let record = records(&messages)
        .first()
        .copied()
        .context("the message resolves")?;
    assert_eq!(record.text, "@lin decides");
    assert_eq!(record.author.as_deref(), Some("U0NAMELESS"));
    Ok(())
}

/// Through Slack's own client: a user Slack answers `user_not_found` for and a channel it
/// answers `channel_not_found` for keep their ids without a warning, a known user is named, and
/// Slack is called exactly once for each.
#[test]
fn a_live_resolve_names_mentions_through_slacks_client() -> Result<()> {
    let _environment = ENVIRONMENT_LOCK
        .lock()
        .map_err(|error| anyhow!("locking test environment: {error}"))?;
    let timestamp = "1792951800.000100";
    let message = json!({
        "ok": true,
        "messages": [{
            "ts": timestamp,
            "user": "U0AUTHOR01",
            "text": "<@U0GONE0001> ask <@U0EXAMPLEA> in <#C0PRIVATE1|>",
        }],
    });
    let (api, requests, server) = fake_http_api(vec![
        r#"{"ok":true,"channels":[{"id":"C0EXAMPLE01","is_private":false,"is_member":true}]}"#
            .to_owned(),
        message.to_string(),
        r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
        r#"{"ok":true,"user":{"profile":{"display_name":"Kim"}}}"#.to_owned(),
        r#"{"ok":false,"error":"user_not_found"}"#.to_owned(),
        r#"{"ok":true,"user":{"real_name":"Grace Hopper"}}"#.to_owned(),
        r#"{"ok":false,"error":"channel_not_found"}"#.to_owned(),
    ])?;
    let credential = r#"{"access":"test-token","refresh":"r","expires":"2099-01-01T00:00:00Z","client_id":"cid","token_url":"https://unused.example/token","resource":"https://mcp.slack.com"}"#;
    let previous_credential = std::env::var_os("CTX_CREDENTIAL_SLACK");
    let previous_secret = std::env::var_os("CTX_SECRET_SLACK");
    std::env::remove_var("CTX_SECRET_SLACK");
    std::env::set_var("CTX_CREDENTIAL_SLACK", credential);
    let request = Request::Resolve(ResolveRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: json!({"workspace": "acme"}),
        pointers: vec![format!("slack:C0EXAMPLE01/{timestamp}")],
        state_dir: std::env::temp_dir(),
    });
    let mut messages = Vec::new();
    let result = SlackSource::with_api(api).run(&request, &mut |message| {
        messages.push(message);
        Ok(())
    });
    match previous_credential {
        Some(value) => std::env::set_var("CTX_CREDENTIAL_SLACK", value),
        None => std::env::remove_var("CTX_CREDENTIAL_SLACK"),
    }
    if let Some(value) = previous_secret {
        std::env::set_var("CTX_SECRET_SLACK", value);
    }
    result?;
    server
        .join()
        .map_err(|_| anyhow!("fake Slack API server panicked"))??;
    let methods = requests
        .lock()
        .map_err(|error| anyhow!("locking requests: {error}"))?
        .iter()
        .map(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|path| path.split('?').next())
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        methods,
        [
            "conversations.list",
            "conversations.replies",
            "conversations.info",
            "users.info",
            "users.info",
            "users.info",
            "conversations.info",
        ]
    );
    let resolved = records(&messages);
    assert_eq!(
        resolved
            .iter()
            .map(|record| (record.author.as_deref(), record.text.as_str()))
            .collect::<Vec<_>>(),
        [(Some("Kim"), "@U0GONE0001 ask @Grace Hopper in #C0PRIVATE1")]
    );
    assert!(warnings(&messages).is_empty(), "{:?}", warnings(&messages));
    Ok(())
}
