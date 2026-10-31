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

fn people(names: &[(&str, &str)]) -> BTreeMap<String, String> {
    names
        .iter()
        .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
        .collect()
}

/// A message stored before mentions were named shows a person as the bare id; served, it
/// reads with the name known now, as a fetch writes it now.
#[test]
fn a_message_stored_with_a_bare_id_is_served_with_the_name() {
    let served = served_text(
        "cc <@U0EXAMPLEA> looks like you reviewed his PRs. Any thoughts?",
        "cc @U0EXAMPLEA looks like you reviewed his PRs. Any thoughts?",
        &people(&[("U0EXAMPLEA", "Kim Lee")]),
    );

    assert_eq!(
        served.as_deref(),
        Some("cc @Kim Lee looks like you reviewed his PRs. Any thoughts?")
    );
}

/// A person no name is known for reads as the label Slack wrote into the mention, else the
/// id, as a fetch writes it now: no longer as the `@U…|label` an earlier renderer wrote.
#[test]
fn a_mention_no_name_is_known_for_is_served_as_its_label_or_its_id() {
    let served = served_text(
        "<@U0EXAMPLEB|grace> and <@U0EXAMPLEC> please",
        "@U0EXAMPLEB|grace and @U0EXAMPLEC please",
        &BTreeMap::new(),
    );

    assert_eq!(served.as_deref(), Some("@grace and @U0EXAMPLEC please"));
}

/// A stored text that already names a mention keeps that name when no other is known, while
/// the person a name is known for reads with it; a name may hold spaces and stand right
/// beside another mention.
#[test]
fn a_name_the_stored_text_gives_is_kept_when_none_is_known() {
    let known = people(&[("U0EXAMPLEA", "Kim Lee")]);
    let upgraded = served_text(
        "<@U0EXAMPLEA> <@U0EXAMPLED> and <#C0EXAMPLE02> to decide",
        "@Kim @Ada Lovelace and #launch to decide",
        &known,
    );
    let unchanged = served_text(
        "<@U0EXAMPLEA> <@U0EXAMPLED> to decide",
        "@Kim Lee @Ada Lovelace to decide",
        &known,
    );

    assert_eq!(
        upgraded.as_deref(),
        Some("@Kim Lee @Ada Lovelace and #launch to decide")
    );
    assert_eq!(unchanged, None, "the stored text is already the served one");
}

/// An earlier renderer left `<!everyone>` and a channel as Slack wrote them and named a
/// person only from the authors of the same fetch. When nothing names that person now, the
/// new rendering would show the id where the stored text shows a name, so the stored text is
/// served; once the store names them, the message reads as a fetch writes it now.
#[test]
fn a_text_an_earlier_renderer_wrote_is_never_served_worse() {
    let text = "<@U0EXAMPLEA> see <#C0EXAMPLE02> <!everyone>";
    let stored = "@Kim see <#C0EXAMPLE02> <!everyone>";

    assert_eq!(served_text(text, stored, &BTreeMap::new()), None);
    assert_eq!(
        served_text(text, stored, &people(&[("U0EXAMPLEA", "Kim Lee")])).as_deref(),
        Some("@Kim Lee see #C0EXAMPLE02 @everyone")
    );
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
