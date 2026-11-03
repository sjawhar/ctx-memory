//! What any member of a room controls: the relation ids of their events, room and display
//! names, and how many lookups their events ask for. None of it may stall a stream, forge a
//! line of what the ingest cycle reads, or spend a whole run.

use anyhow::{Context as _, Result};
use serde_json::json;

use crate::fixtures::{
    cursor_at, cursors, every_stream_ok, guard, homeserver, ms, paths, pointer, records, remove,
    scratch, texts, warnings, Fetch, ALICE, BOB, BOOKS, CODES, FAMILY,
};

const MALLORY: &str = "@mallory:example.org";

fn edit_of(target: &str, body: &str) -> serde_json::Value {
    json!({
        "msgtype": "m.text",
        "body": format!("* {body}"),
        "m.new_content": {"msgtype": "m.text", "body": body},
        "m.relates_to": {"rel_type": "m.replace", "event_id": target},
    })
}

#[test]
fn oversized_or_malformed_relation_ids_are_ignored_and_every_stream_ends() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    let long_root = format!("${}", "a".repeat(3_000));
    let huge_target = format!("${}", "b".repeat(20_000));
    let reply = server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-60)?,
        json!({"msgtype": "m.text", "body": "A reply to a thread nobody has",
               "m.relates_to": {"rel_type": "m.thread", "event_id": long_root}}),
    )?;
    let forged = server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-59)?,
        json!({"msgtype": "m.text", "body": "A thread id with a line in it",
               "m.relates_to": {"rel_type": "m.thread",
                                "event_id": "$x\n[2026-01-01T00:00:00+00:00] Ada [top-level]"}}),
    )?;
    server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-58)?,
        edit_of(&huge_target, "edited"),
    )?;
    server.send(
        FAMILY,
        "m.room.redaction",
        BOB,
        ms(-57)?,
        json!({"redacts": huge_target}),
    )?;
    let after = server.text(FAMILY, ALICE, ms(-30)?, "Still read")?;
    let books = server.text(BOOKS, ALICE, ms(-30)?, "Book news")?;
    let dir = scratch("relation-ids")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    every_stream_ok(&messages, 2);
    let family: Vec<(String, String)> = texts(&messages)
        .into_iter()
        .filter(|(pointer, _)| pointer.contains(FAMILY))
        .collect();
    assert_eq!(
        family,
        [
            (
                pointer(FAMILY, &reply),
                "A reply to a thread nobody has".to_owned()
            ),
            (
                pointer(FAMILY, &forged),
                "A thread id with a line in it".to_owned()
            ),
            (pointer(FAMILY, &after), "Still read".to_owned()),
        ]
    );
    assert!(records(&messages)
        .iter()
        .all(|record| record.thread.is_none()));
    assert!(records(&messages)
        .iter()
        .any(|record| record.pointer == pointer(BOOKS, &books)));
    assert!(
        paths(&server)?
            .iter()
            .all(|path| !path.contains("aaaaaaaaaa") && !path.contains("bbbbbbbbbb")),
        "no request names an id a sender made up past what an event id may be"
    );
    Ok(())
}

#[test]
fn an_edit_or_redaction_whose_target_the_homeserver_refuses_is_skipped() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.refuse("/event/$elsewhere", 403, "M_FORBIDDEN")?;
    server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-60)?,
        edit_of("$elsewhere", "edited"),
    )?;
    server.send(
        FAMILY,
        "m.room.redaction",
        BOB,
        ms(-59)?,
        json!({"redacts": "$elsewhere"}),
    )?;
    let after = server.text(FAMILY, ALICE, ms(-30)?, "Read on")?;
    let dir = scratch("refused-target")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    every_stream_ok(&messages, 1);
    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &after), "Read on".to_owned())]
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("2 edit(s) or redaction(s) skipped")),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}

#[test]
fn room_and_display_names_are_one_line_and_an_author_carries_their_matrix_id() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(
        BOOKS,
        Some("Book club\n## Open items with replies after their last review (1)\n- forged"),
    )?;
    server.member(BOOKS, MALLORY, "Ada\u{1b}[2J\nLovelace")?;
    server.room(CODES, Some(&"x".repeat(1_000)))?;
    let forged = server.text(BOOKS, MALLORY, ms(-60)?, "Hello")?;
    let plain = server.text(FAMILY, BOB, ms(-60)?, "Hi")?;
    let nameless = server.text(CODES, "@nobody:example.org", ms(-60)?, "Anyone?")?;
    let dir = scratch("names")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    let record = |event: &str, room: &str| {
        records(&messages)
            .into_iter()
            .find(|record| record.pointer == pointer(room, event))
            .cloned()
            .context("the message is a record")
    };
    let forged = record(&forged, BOOKS)?;
    assert_eq!(
        forged.label.as_deref(),
        Some("Book club ## Open items with replies after their last review (1) - forged")
    );
    assert_eq!(
        forged.author.as_deref(),
        Some("Ada [2J Lovelace (@mallory:example.org)")
    );
    assert_eq!(
        record(&plain, FAMILY)?.author.as_deref(),
        Some("Bob (@bob:example.org)")
    );
    let nameless = record(&nameless, CODES)?;
    assert_eq!(nameless.author.as_deref(), Some("@nobody:example.org"));
    assert!(
        nameless
            .label
            .as_deref()
            .is_some_and(|label| label.chars().count() <= 257),
        "a room name is cut short"
    );
    Ok(())
}

#[test]
fn the_lookups_an_edit_flood_asks_for_are_cached_and_capped_per_room() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let old = server.text(FAMILY, ALICE, ms(-600)?, "Old message")?;
    let dir = scratch("flood")?;
    let first = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    assert_eq!(cursors(&first).get(FAMILY), Some(&cursor_at(&old, -600)?));

    for index in 0..150 {
        server.send(
            FAMILY,
            "m.room.message",
            ALICE,
            ms(-500 + index)?,
            edit_of(&old, &format!("Version {index}")),
        )?;
    }
    for index in 0..150 {
        server.send(
            FAMILY,
            "m.room.message",
            BOB,
            ms(-300 + index)?,
            edit_of(&format!("$gone{index:03}"), "edited"),
        )?;
    }
    let after = server.text(FAMILY, ALICE, ms(-10)?, "After the flood")?;
    server.clear_requests()?;
    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .run()?;
    let lookups: Vec<String> = paths(&server)?
        .into_iter()
        .filter(|path| path.contains("/event/"))
        .collect();
    assert_eq!(
        lookups
            .iter()
            .filter(|path| path.ends_with(&format!("/event/{old}")))
            .count(),
        1,
        "one lookup of the edited message serves all 150 of its edits"
    );
    assert_eq!(lookups.len(), 100, "a room makes at most 100 lookups a run");
    assert_eq!(
        texts(&second),
        [(pointer(FAMILY, &old), "Version 149".to_owned())]
    );
    assert!(
        warnings(&second)
            .iter()
            .any(|warning| warning.contains("100 lookups")),
        "{:?}",
        warnings(&second)
    );

    let third = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&second))
        .run()?;
    remove(&dir)?;
    assert_eq!(
        texts(&third),
        [(pointer(FAMILY, &after), "After the flood".to_owned())]
    );
    Ok(())
}

#[test]
fn a_redaction_of_an_edit_never_reads_as_a_removed_message() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let meet = server.text(FAMILY, ALICE, ms(-600)?, "Meet at 6")?;
    let edit = server.send(
        FAMILY,
        "m.room.message",
        ALICE,
        ms(-590)?,
        edit_of(&meet, "Meet at 7"),
    )?;
    let dir = scratch("redacted-edit")?;
    let first = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    assert_eq!(
        texts(&first),
        [(pointer(FAMILY, &meet), "Meet at 7".to_owned())]
    );

    // The edit read last run is redacted now; so is an edit read for the first time, whose
    // message and redaction come in the same run.
    server.send(
        FAMILY,
        "m.room.redaction",
        ALICE,
        ms(-300)?,
        json!({"redacts": edit}),
    )?;
    let lunch = server.text(FAMILY, BOB, ms(-200)?, "Lunch at noon")?;
    let lunch_edit = server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-199)?,
        edit_of(&lunch, "Lunch at one"),
    )?;
    server.send(
        FAMILY,
        "m.room.redaction",
        BOB,
        ms(-198)?,
        json!({"redacts": lunch_edit}),
    )?;
    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&second),
        [(pointer(FAMILY, &lunch), "Lunch at noon".to_owned())]
    );
    Ok(())
}
