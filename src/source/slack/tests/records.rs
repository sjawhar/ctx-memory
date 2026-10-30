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

fn rendered(text: &str, names: &[(&str, &str)]) -> Result<Record> {
    let names = names
        .iter()
        .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
        .collect();
    record_from_message(
        &json!({"ts": "1792951800.000100", "user": "U0EXAMPLEA", "text": text}),
        "C0EXAMPLE01",
        "#general",
        &names,
        "acme",
    )
}

#[test]
fn a_mention_reads_as_the_name_the_author_line_uses() -> Result<()> {
    let text = "<@U0EXAMPLEA> can you ask <@U0EXAMPLED|ada> to decide?";
    let record = rendered(
        text,
        &[
            ("U0EXAMPLEA", "Grace Hopper"),
            ("U0EXAMPLED", "Ada Lovelace"),
        ],
    )?;

    assert_eq!(record.author.as_deref(), Some("Grace Hopper"));
    assert_eq!(
        record.text,
        "@Grace Hopper can you ask @Ada Lovelace to decide?"
    );
    assert_eq!(
        record.raw.get("text").and_then(Value::as_str),
        Some(text),
        "the record keeps Slack's own text, with the ids"
    );
    Ok(())
}

#[test]
fn a_mention_slack_cannot_name_keeps_its_label_or_its_id() -> Result<()> {
    let record = rendered("<@U0UNKNOWN> and <@U0EXAMPLED|ada>, please", &[])?;

    assert_eq!(record.text, "@U0UNKNOWN and @ada, please");
    Ok(())
}

#[test]
fn channel_group_and_broadcast_mentions_read_as_names() -> Result<()> {
    let record = rendered(
        "<#C0EXAMPLE02|design>, <#C0EXAMPLE03|>, <#C0EXAMPLE04>, \
         <!subteam^S0EXAMPLE1|@platform>, <!subteam^S0EXAMPLE2>, \
         <!here>, <!channel|channel>, <!everyone>",
        &[("C0EXAMPLE03", "launch")],
    )?;

    assert_eq!(
        record.text,
        "#design, #launch, #C0EXAMPLE04, @platform, @S0EXAMPLE2, @here, @channel, @everyone"
    );
    Ok(())
}

#[test]
fn typed_angle_brackets_are_not_mentions_and_links_stay() -> Result<()> {
    // Slack escapes a typed `<`, `>` and `&`, so only its own sequences are mentions.
    let record = rendered(
        "&lt;@U0EXAMPLEA&gt; &amp; <https://example.com/?a=1&amp;b=2|the doc> <@U0EXAMPLEA",
        &[("U0EXAMPLEA", "Grace Hopper")],
    )?;

    assert_eq!(
        record.text,
        "<@U0EXAMPLEA> & <https://example.com/?a=1&b=2|the doc> <@U0EXAMPLEA"
    );
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
