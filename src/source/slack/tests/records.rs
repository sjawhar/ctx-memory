//! Slack record conversion tests.

use super::fixtures::*;
#[test]
fn permalink_uses_the_configured_workspace() -> Result<()> {
    let record = record_from_message(
        &json!({"ts": "1792951800.000100", "text": "hello"}),
        "C0EXAMPLE01",
        "#general",
        &BTreeMap::new(),
        "acme",
    )?;

    let url = record.url.context("Slack record has a permalink")?;
    assert!(url.starts_with("https://acme.slack.com/archives/"));
    Ok(())
}

#[test]
fn pre_window_parent_is_sent_as_a_context_record() -> Result<()> {
    let root = scratch("pre-window-parent")?;
    let end = at("2026-10-25T18:00:00Z")?;
    let parent = slack_ts(end - Duration::days(2));
    let mut index = ThreadIndex::new();
    index.insert(
        "C0EXAMPLE01".to_owned(),
        BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
    );
    write_index(&root, &index)?;
    let reply = slack_ts(end - Duration::hours(1));
    let mut api = FakeSlackApi::default();
    api.replies_by_thread.insert(
        ("C0EXAMPLE01".to_owned(), parent.clone()),
        vec![
            json!({"ts": parent, "text": "parent", "user": "U0EXAMPLE9"}),
            json!({
                "ts": reply,
                "thread_ts": parent,
                "text": "reply",
                "user": "U0EXAMPLE9"
            }),
        ],
    );

    let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
    let messages = fetch_messages(&request, &api)?;
    std::fs::remove_dir_all(&root)?;

    let parent_record = records(&messages)
        .into_iter()
        .find(|record| record.pointer.ends_with(&parent))
        .context("fetched reply has its parent context record")?;
    assert!(parent_record.context);
    Ok(())
}
