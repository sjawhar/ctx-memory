//! What a homeserver does that a source has to read through: pages it empties of ignored
//! users, `/timestamp_to_event` that goes by date and not by timeline order, renames, and a
//! `/sync` that never answers.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::json;

use crate::fixtures::{
    cursor_at, cursors, every_stream_ok, guard, homeserver, ms, planned, pointer, records, remove,
    scratch, texts, untouched, warnings, Fetch, ALICE, BOB, BOOKS, CODES, FAMILY,
};
use crate::support::fake_matrix::FakeMatrix;

const MALLORY: &str = "@mallory:example.org";

#[test]
fn a_page_the_homeserver_empties_of_ignored_users_does_not_end_the_room() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.ignore(MALLORY)?;
    let stored = server.text(FAMILY, ALICE, ms(-600)?, "Stored last run")?;
    for index in 0..150 {
        server.text(FAMILY, MALLORY, ms(-500 + index)?, "Ignored")?;
    }
    let after = server.text(FAMILY, ALICE, ms(-10)?, "After the ignored run")?;
    let dir = scratch("empty-page")?;
    let start = BTreeMap::from([(FAMILY.to_owned(), cursor_at(&stored, -600)?)]);

    let messages = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&start)
        .run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &after), "After the ignored run".to_owned())]
    );
    Ok(())
}

#[test]
fn a_read_back_from_the_end_pages_past_a_page_of_ignored_users() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.without_timestamp_to_event()?;
    server.ignore(MALLORY)?;
    let early = server.text(FAMILY, ALICE, ms(-600)?, "Before the ignored run")?;
    for index in 0..150 {
        server.text(FAMILY, MALLORY, ms(-500 + index)?, "Ignored")?;
    }
    let dir = scratch("empty-page-backward")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &early), "Before the ignored run".to_owned())]
    );
    Ok(())
}

#[test]
fn a_first_read_finds_messages_placed_before_a_late_delivery_with_an_older_date() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.text(FAMILY, ALICE, ms(-60 * 30)?, "Before the window")?;
    let recent = server.text(FAMILY, BOB, ms(-60)?, "Sent an hour ago")?;
    // A bridge delivers a message sent yesterday only now: it sits after the recent one, and
    // the homeserver finds it first by date.
    let late = server.text(FAMILY, ALICE, ms(-60 * 23)?, "Sent yesterday, bridged now")?;
    let dir = scratch("late-first")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [
            (pointer(FAMILY, &recent), "Sent an hour ago".to_owned()),
            (
                pointer(FAMILY, &late),
                "Sent yesterday, bridged now".to_owned()
            ),
        ]
    );
    Ok(())
}

#[test]
fn a_read_back_from_the_end_pages_on_past_one_late_event_with_an_old_date() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.without_timestamp_to_event()?;
    server.text(FAMILY, ALICE, ms(-60 * 20)?, "First in the window")?;
    for index in 0..100 {
        server.text(
            FAMILY,
            BOB,
            ms(-60 * 19 + index)?,
            &format!("Message {index}"),
        )?;
    }
    server.text(
        FAMILY,
        ALICE,
        ms(-60 * 30)?,
        "Bridged late, dated before the window",
    )?;
    let dir = scratch("late-backward")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    let read = texts(&messages);
    assert_eq!(read.len(), 101);
    assert_eq!(
        read.first().map(|(_, text)| text.as_str()),
        Some("First in the window")
    );
    Ok(())
}

#[test]
fn a_room_whose_newest_event_is_dated_before_the_window_is_still_read() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let message = server.text(FAMILY, BOB, ms(-120)?, "Read me")?;
    // A reaction a bridge delivers late, dated two days back, is the room's newest event.
    server.send(
        FAMILY,
        "m.reaction",
        ALICE,
        ms(-60 * 48)?,
        json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": message, "key": "+1"}}),
    )?;
    let dir = scratch("old-newest")?;

    let messages = Fetch::new(&server, &dir).run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &message), "Read me".to_owned())]
    );
    Ok(())
}

/// A homeserver with a room `exclude_rooms` leaves out by its name, and a message in it.
fn codes_room() -> Result<FakeMatrix> {
    let server = homeserver()?;
    server.room(CODES, Some("Login verification"))?;
    server.text(CODES, BOB, ms(-300)?, "Your code is 000000")?;
    Ok(server)
}

#[test]
fn a_room_renamed_out_of_an_exclusion_is_read_again_in_every_sync_mode() -> Result<()> {
    let _environment = guard();
    for mode in ["full", "incremental", "off"] {
        let server = codes_room()?;
        let dir = scratch(&format!("renamed-{mode}"))?;
        let excluding = |cursors: &BTreeMap<String, String>| {
            Fetch::new(&server, &dir)
                .with("sync", json!(mode))
                .with("exclude_rooms", json!(["*verification*"]))
                .cursors(cursors)
                .run()
        };
        let first = excluding(&BTreeMap::new())?;
        assert!(
            records(&first).iter().all(|record| record.stream != CODES),
            "{mode}: an excluded room has no message read"
        );

        server.rename(CODES, BOB, ms(-200)?, "Family dinner")?;
        let fresh = server.text(CODES, BOB, ms(-100)?, "Dinner at seven")?;
        let second = excluding(&cursors(&first))?;
        remove(&dir)?;
        assert!(
            texts(&second)
                .iter()
                .any(|(read, text)| *read == pointer(CODES, &fresh) && text == "Dinner at seven"),
            "{mode}: a room renamed out of its exclusion is read: {:?}",
            texts(&second)
        );
    }
    Ok(())
}

#[test]
fn a_room_renamed_into_an_allowlist_is_read_and_one_renamed_out_is_not() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Old name"))?;
    server.text(BOOKS, BOB, ms(-300)?, "Before the rename")?;
    let dir = scratch("renamed-allow")?;
    let allowing = |cursors: &BTreeMap<String, String>| {
        Fetch::new(&server, &dir)
            .with("rooms", json!(["Family*"]))
            .cursors(cursors)
            .run()
    };
    let first = allowing(&BTreeMap::new())?;
    assert_eq!(planned(&first), [FAMILY]);

    server.rename(BOOKS, BOB, ms(-200)?, "Family book club")?;
    server.rename(FAMILY, BOB, ms(-200)?, "Neighbours")?;
    let fresh = server.text(BOOKS, BOB, ms(-100)?, "Chapter one")?;
    server.text(FAMILY, BOB, ms(-100)?, "Not read any more")?;
    server.clear_requests()?;
    let second = allowing(&cursors(&first))?;
    remove(&dir)?;
    assert_eq!(planned(&second), [BOOKS]);
    untouched(&server, FAMILY)?;
    assert!(texts(&second)
        .iter()
        .any(|(read, _)| *read == pointer(BOOKS, &fresh)));
    Ok(())
}

#[test]
fn a_sync_that_gets_no_answer_leaves_every_room_to_be_read() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    server.hang_up("/sync")?;
    let dir = scratch("sync-no-answer")?;

    let messages = Fetch::new(&server, &dir).run()?;
    remove(&dir)?;

    every_stream_ok(&messages, 1);
    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &news), "Family news".to_owned())]
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("every room is read this run")),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}
