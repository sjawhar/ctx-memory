//! A live resolve: a stored pointer still names the message as the homeserver serves it now.

use anyhow::Result;
use serde_json::json;

use crate::fixtures::{
    guard, homeserver, ms, paths, pointer, records, resolve, texts, ALICE, BOB, FAMILY,
};

#[test]
fn a_pointer_resolves_to_its_message_as_last_edited() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let meet = server.text(FAMILY, ALICE, ms(-300)?, "Meet at 6")?;
    server.send(
        FAMILY,
        "m.room.message",
        ALICE,
        ms(-200)?,
        json!({
            "msgtype": "m.text",
            "body": "* Meet at 7",
            "m.new_content": {"msgtype": "m.text", "body": "Meet at 7"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": meet},
        }),
    )?;

    let messages = resolve(&server, json!({}), &[pointer(FAMILY, &meet)])?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &meet), "Meet at 7".to_owned())]
    );
    let record = records(&messages);
    assert_eq!(
        record.first().and_then(|record| record.author.as_deref()),
        Some("Alice (@alice:example.org)")
    );
    assert_eq!(
        record.first().and_then(|record| record.label.as_deref()),
        Some("Family")
    );
    Ok(())
}

#[test]
fn a_thread_reply_resolves_with_its_thread() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let root = server.text(FAMILY, BOB, ms(-50)?, "Who is coming on Saturday?")?;
    let thread = |body: &str| json!({"msgtype": "m.text", "body": body, "m.relates_to": {"rel_type": "m.thread", "event_id": root}});
    let first = server.send(FAMILY, "m.room.message", ALICE, ms(-40)?, thread("Me"))?;
    let second = server.send(FAMILY, "m.room.message", BOB, ms(-30)?, thread("Me too"))?;

    let messages = resolve(&server, json!({}), &[pointer(FAMILY, &first)])?;

    assert_eq!(
        texts(&messages),
        [
            (
                pointer(FAMILY, &root),
                "Who is coming on Saturday?".to_owned()
            ),
            (pointer(FAMILY, &first), "Me".to_owned()),
            (pointer(FAMILY, &second), "Me too".to_owned()),
        ]
    );
    let threads: Vec<Option<String>> = records(&messages)
        .into_iter()
        .map(|record| record.thread.clone())
        .collect();
    assert_eq!(
        threads,
        [
            None,
            Some(pointer(FAMILY, &root)),
            Some(pointer(FAMILY, &root))
        ]
    );
    Ok(())
}

#[test]
fn a_removed_message_resolves_as_removed_and_an_unknown_one_not_at_all() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let oops = server.text(FAMILY, BOB, ms(-60)?, "wrong chat")?;
    server.send(
        FAMILY,
        "m.room.redaction",
        BOB,
        ms(-59)?,
        json!({"redacts": oops}),
    )?;

    let messages = resolve(
        &server,
        json!({}),
        &[pointer(FAMILY, &oops), pointer(FAMILY, "$neverexisted")],
    )?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &oops), "[message removed]".to_owned())]
    );
    Ok(())
}

#[test]
fn a_pointer_into_a_room_the_source_does_not_read_is_refused() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    server.alias(FAMILY, "#family:example.org")?;

    let by_id = resolve(
        &server,
        json!({"exclude_rooms": [FAMILY]}),
        &[pointer(FAMILY, &news)],
    );
    let by_name = resolve(
        &server,
        json!({"exclude_rooms": ["fam*"]}),
        &[pointer(FAMILY, &news)],
    );
    // The family room has a name, so only reading its alias too can refuse it.
    let by_alias = resolve(
        &server,
        json!({"exclude_rooms": ["#Family:example.org"]}),
        &[pointer(FAMILY, &news)],
    );

    for refused in [by_id, by_name, by_alias] {
        let error = refused
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(error.contains("not a room this source reads"), "{error}");
    }
    let read = paths(&server)?;
    assert!(
        read.iter().all(|path| !path.contains("/event/")),
        "a refused pointer's message is never read: {read:?}"
    );
    Ok(())
}
