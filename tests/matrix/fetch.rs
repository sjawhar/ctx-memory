//! A fetch: which rooms it reads, what it records, and where it leaves each room's cursor.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use ctx_memory::source::StreamStatus;
use serde_json::json;

use crate::fixtures::{
    at, cursor_at, cursors, ends, guard, homeserver, ms, paths, planned, pointer, records, remove,
    room_reads, scratch, texts, touched, untouched, warnings, Fetch, ALICE, BANK, BOB, BOOKS,
    CODES, FAMILY,
};
use crate::support::fake_matrix::{query, FakeMatrix};

/// The state the source keeps for the next run.
fn stored_state(dir: &std::path::Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(dir.join("state.json")).context("reading state.json")?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[test]
fn a_first_fetch_reads_the_backfill_window_and_names_the_newest_event() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.text(
        FAMILY,
        ALICE,
        ms(-30 * 60)?,
        "Older than the backfill window",
    )?;
    let first = server.text(FAMILY, BOB, ms(-20 * 60)?, "Dinner on Sunday?")?;
    server.send(
        FAMILY,
        "m.room.member",
        BOB,
        ms(-19 * 60)?,
        json!({"membership": "join"}),
    )?;
    let newest = server.text(FAMILY, ALICE, ms(-60)?, "Yes, at seven.")?;
    let dir = scratch("first-fetch")?;

    let messages = Fetch::new(&server, &dir).run()?;
    remove(&dir)?;

    assert_eq!(planned(&messages), [FAMILY]);
    assert_eq!(
        texts(&messages),
        [
            (pointer(FAMILY, &first), "Dinner on Sunday?".to_owned()),
            (pointer(FAMILY, &newest), "Yes, at seven.".to_owned()),
        ]
    );
    let first_record = records(&messages)
        .into_iter()
        .next()
        .context("the fetch sent a record")?;
    assert_eq!(
        first_record.author.as_deref(),
        Some("Bob (@bob:example.org)")
    );
    assert_eq!(first_record.author_id.as_deref(), Some(BOB));
    assert_eq!(first_record.label.as_deref(), Some("Family"));
    assert_eq!(first_record.stream, FAMILY);
    assert_eq!(first_record.thread, None);
    assert_eq!(
        cursors(&messages).get(FAMILY),
        Some(&cursor_at(&newest, -60)?)
    );
    Ok(())
}

#[test]
fn a_homeserver_without_timestamp_to_event_is_read_back_from_the_end() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.without_timestamp_to_event()?;
    server.text(
        FAMILY,
        ALICE,
        ms(-30 * 60)?,
        "Older than the backfill window",
    )?;
    let first = server.text(FAMILY, BOB, ms(-20 * 60)?, "Dinner on Sunday?")?;
    let newest = server.text(FAMILY, ALICE, ms(-60)?, "Yes, at seven.")?;
    let dir = scratch("no-timestamp-to-event")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [
            (pointer(FAMILY, &first), "Dinner on Sunday?".to_owned()),
            (pointer(FAMILY, &newest), "Yes, at seven.".to_owned()),
        ]
    );
    assert_eq!(
        cursors(&messages).get(FAMILY),
        Some(&cursor_at(&newest, -60)?)
    );
    Ok(())
}

#[test]
fn a_room_read_forward_from_its_cursor_finds_an_event_delivered_late_with_an_old_date() -> Result<()>
{
    let _environment = guard();
    let server = homeserver()?;
    let stored = server.text(FAMILY, ALICE, ms(-120)?, "Stored last run")?;
    let after = server.text(FAMILY, BOB, ms(-60)?, "Sent after it")?;
    // A bridge delivers a message sent before the stored one only now.
    let late = server.text(FAMILY, BOB, ms(-180)?, "Sent earlier, bridged late")?;
    let dir = scratch("late")?;
    let start = BTreeMap::from([(FAMILY.to_owned(), cursor_at(&stored, -120)?)]);

    let messages = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&start)
        .run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [
            (pointer(FAMILY, &after), "Sent after it".to_owned()),
            (
                pointer(FAMILY, &late),
                "Sent earlier, bridged late".to_owned()
            ),
        ]
    );
    assert_eq!(
        cursors(&messages).get(FAMILY),
        Some(&cursor_at(&late, -180)?)
    );
    Ok(())
}

#[test]
fn a_room_further_behind_than_the_budget_moves_its_cursor_every_run() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let stored = server.text(FAMILY, ALICE, ms(-600)?, "Stored last run")?;
    let mut sent = Vec::new();
    for index in 0..250 {
        sent.push(server.text(
            FAMILY,
            BOB,
            ms(-500 + index)?,
            &format!("Backlog message {index}"),
        )?);
    }
    let dir = scratch("budget")?;
    let mut position = BTreeMap::from([(FAMILY.to_owned(), cursor_at(&stored, -600)?)]);

    // The name, the cursor's context and one page: the run reads 100 messages, then stops.
    let first = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .with("request_budget", json!(3))
        .cursors(&position)
        .run()?;
    assert_eq!(records(&first).len(), 100);
    let hundredth = sent.get(99).context("100 messages were sent")?;
    assert_eq!(
        cursors(&first).get(FAMILY),
        Some(&cursor_at(hundredth, -500 + 99)?)
    );
    assert!(
        warnings(&first)
            .iter()
            .any(|warning| warning.contains("request budget ran out")),
        "{:?}",
        warnings(&first)
    );
    position = cursors(&first);

    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .with("request_budget", json!(3))
        .cursors(&position)
        .run()?;
    assert_eq!(
        records(&second).first().map(|record| record.text.as_str()),
        Some("Backlog message 100")
    );
    position = cursors(&second);

    let third = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&position)
        .run()?;
    remove(&dir)?;
    assert_eq!(records(&third).len(), 50);
    let last = sent.last().context("messages were sent")?;
    assert_eq!(
        cursors(&third).get(FAMILY),
        Some(&cursor_at(last, -500 + 249)?)
    );
    Ok(())
}

#[test]
fn one_run_reads_at_most_fifty_pages_of_a_room_and_the_rest_of_the_rooms_too() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    let stored = server.text(FAMILY, ALICE, ms(-6000)?, "Stored last run")?;
    for index in 0..5050 {
        server.text(FAMILY, BOB, ms(-5900 + index)?, &format!("Busy {index}"))?;
    }
    let books = server.text(BOOKS, BOB, ms(-60)?, "Book news")?;
    let dir = scratch("room-pages")?;
    let start = BTreeMap::from([(FAMILY.to_owned(), cursor_at(&stored, -6000)?)]);

    let first = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&start)
        .run()?;
    let family = |messages: &[ctx_memory::source::Message]| {
        records(messages)
            .into_iter()
            .filter(|record| record.stream == FAMILY)
            .count()
    };
    assert_eq!(family(&first), 5000);
    assert!(records(&first)
        .iter()
        .any(|record| record.pointer == pointer(BOOKS, &books)));
    assert!(warnings(&first)
        .iter()
        .any(|warning| warning.contains("50 pages")));

    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;
    assert_eq!(family(&second), 50);
    Ok(())
}

#[test]
fn an_incremental_sync_skips_every_room_without_a_new_message() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    server.text(FAMILY, ALICE, ms(-120)?, "Family news")?;
    server.text(BOOKS, BOB, ms(-90)?, "Chapter three next week")?;
    let dir = scratch("incremental")?;

    let first = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .run()?;
    let since = stored_state(&dir)?
        .get("since")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("an incremental fetch stores its sync token")?;

    let fresh = server.text(BOOKS, ALICE, ms(-30)?, "Chapter four, actually")?;
    server.clear_requests()?;
    let second = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .cursors(&cursors(&first))
        .run()?;

    untouched(&server, FAMILY)?;
    touched(&server, BOOKS)?;
    let syncs: Vec<_> = server
        .requests()?
        .into_iter()
        .filter(|request| request.path.ends_with("/sync"))
        .collect();
    assert_eq!(syncs.len(), 1);
    let sync = syncs.first().context("one sync")?;
    assert_eq!(query(&sync.target).get("since"), Some(&since));
    assert_eq!(
        texts(&second),
        [(pointer(BOOKS, &fresh), "Chapter four, actually".to_owned())]
    );
    assert_eq!(cursors(&second).get(FAMILY), cursors(&first).get(FAMILY));

    // A room whose cursor is not the one the source settled, as when the window that moved it
    // was never stored, is read again whatever the sync says: the books room here.
    server.clear_requests()?;
    let _third = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;
    untouched(&server, FAMILY)?;
    touched(&server, BOOKS)?;
    Ok(())
}

#[test]
fn a_full_sync_skips_rooms_with_nothing_new_and_acknowledges_nothing() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    server.room(BANK, Some("Old thread"))?;
    server.text(FAMILY, ALICE, ms(-120)?, "Family news")?;
    server.text(BOOKS, BOB, ms(-90)?, "Chapter three next week")?;
    // Quiet since long before the backfill window: the first run asks only whether it has any
    // event since, and later runs skip it.
    server.text(BANK, BOB, ms(-60 * 24 * 10)?, "Ten days ago")?;
    let dir = scratch("full")?;

    let first = Fetch::new(&server, &dir).run()?;
    let bank_reads = room_reads(&server, BANK)?;
    assert_eq!(bank_reads.len(), 1, "{bank_reads:?}");
    assert!(bank_reads
        .iter()
        .all(|path| path.ends_with("/timestamp_to_event")));
    assert_eq!(
        cursors(&first).get(BANK).map(String::as_str),
        Some("2026-10-24T18:00:00Z"),
        "a room with nothing since the backfill start keeps a cursor that names no event"
    );

    let fresh = server.text(BOOKS, ALICE, ms(-30)?, "Chapter four, actually")?;
    // A message followed by a reaction: the room's newest event is not a message.
    let thanks = server.text(FAMILY, BOB, ms(-25)?, "Thanks for dinner")?;
    server.send(
        FAMILY,
        "m.reaction",
        ALICE,
        ms(-20)?,
        json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": thanks, "key": "+1"}}),
    )?;
    server.clear_requests()?;
    let second = Fetch::new(&server, &dir).cursors(&cursors(&first)).run()?;
    untouched(&server, BANK)?;
    assert_eq!(
        texts(&second),
        [
            (pointer(BOOKS, &fresh), "Chapter four, actually".to_owned()),
            (pointer(FAMILY, &thanks), "Thanks for dinner".to_owned()),
        ]
    );

    server.clear_requests()?;
    let third = Fetch::new(&server, &dir).cursors(&cursors(&second)).run()?;
    remove(&dir)?;
    for room in [FAMILY, BOOKS, BANK] {
        untouched(&server, room)?;
    }
    assert!(records(&third).is_empty());
    assert_eq!(cursors(&third), cursors(&second));

    for request in server.requests()? {
        assert_eq!(request.method, "GET");
        assert!(
            !request.path.contains("receipt") && !request.path.contains("read_markers"),
            "{}",
            request.path
        );
        if request.path.ends_with("/sync") {
            let parameters = query(&request.target);
            assert_eq!(
                parameters.get("set_presence").map(String::as_str),
                Some("offline")
            );
            assert_eq!(
                parameters.get("since"),
                None,
                "a full sync passes no since token"
            );
        }
    }
    Ok(())
}

#[test]
fn with_sync_off_every_room_is_read_and_no_sync_is_sent() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    let family = server.text(FAMILY, ALICE, ms(-120)?, "Family news")?;
    let books = server.text(BOOKS, BOB, ms(-90)?, "Chapter three next week")?;
    let dir = scratch("off")?;

    let first = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;

    assert!(!paths(&server)?.iter().any(|path| path.ends_with("/sync")));
    assert_eq!(texts(&first).len(), 2);
    assert!(records(&second).is_empty());
    assert_eq!(
        cursors(&second).get(FAMILY),
        Some(&cursor_at(&family, -120)?)
    );
    assert_eq!(cursors(&second).get(BOOKS), Some(&cursor_at(&books, -90)?));
    Ok(())
}

#[test]
fn an_edit_changes_the_message_it_names_and_a_redaction_removes_it() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let meet = server.text(FAMILY, ALICE, ms(-300)?, "Meet at 6")?;
    let oops = server.text(FAMILY, BOB, ms(-290)?, "the door code is wrong")?;
    let dir = scratch("edits")?;
    let first = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;

    let edit = |target: &str, body: &str| {
        json!({
            "msgtype": "m.text",
            "body": format!("* {body}"),
            "m.new_content": {"msgtype": "m.text", "body": body},
            "m.relates_to": {"rel_type": "m.replace", "event_id": target},
        })
    };
    server.send(
        FAMILY,
        "m.room.message",
        ALICE,
        ms(-200)?,
        edit(&meet, "Meet at 7"),
    )?;
    // An edit by anyone but the sender is not a valid replacement.
    server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-190)?,
        edit(&meet, "Meet at midnight"),
    )?;
    server.send(
        FAMILY,
        "m.room.redaction",
        BOB,
        ms(-180)?,
        json!({"redacts": oops}),
    )?;
    let fresh = server.text(FAMILY, BOB, ms(-100)?, "On my wya")?;
    server.send(
        FAMILY,
        "m.room.message",
        BOB,
        ms(-99)?,
        edit(&fresh, "On my way"),
    )?;

    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&second),
        [
            (pointer(FAMILY, &meet), "Meet at 7".to_owned()),
            (pointer(FAMILY, &oops), "[message removed]".to_owned()),
            (pointer(FAMILY, &fresh), "On my way".to_owned()),
        ]
    );
    let edited = records(&second)
        .into_iter()
        .next()
        .context("the edit is a record")?;
    assert_eq!(
        edited.ts,
        at("2026-10-25T13:00:00Z")?,
        "an edit keeps its message's date"
    );
    assert_eq!(edited.author.as_deref(), Some("Alice (@alice:example.org)"));
    Ok(())
}

#[test]
fn media_reads_as_a_placeholder_and_a_thread_reply_names_its_root() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let photo = server.send(
        FAMILY,
        "m.room.message",
        ALICE,
        ms(-60)?,
        json!({"msgtype": "m.image", "body": "The view from the top", "filename": "summit.jpg", "url": "mxc://example.org/abc"}),
    )?;
    let root = server.text(FAMILY, BOB, ms(-50)?, "Who is coming on Saturday?")?;
    let reply = server.send(
        FAMILY,
        "m.room.message",
        ALICE,
        ms(-40)?,
        json!({"msgtype": "m.text", "body": "Me", "m.relates_to": {"rel_type": "m.thread", "event_id": root}}),
    )?;
    server.send(
        FAMILY,
        "m.room.encrypted",
        BOB,
        ms(-30)?,
        json!({"algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "AAAA"}),
    )?;
    let dir = scratch("media")?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [
            (
                pointer(FAMILY, &photo),
                "[image: summit.jpg]\nThe view from the top".to_owned()
            ),
            (
                pointer(FAMILY, &root),
                "Who is coming on Saturday?".to_owned()
            ),
            (pointer(FAMILY, &reply), "Me".to_owned()),
        ]
    );
    let threads: Vec<Option<String>> = records(&messages)
        .into_iter()
        .map(|record| record.thread.clone())
        .collect();
    assert_eq!(threads, [None, None, Some(pointer(FAMILY, &root))]);
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("1 encrypted event(s) skipped")),
        "{:?}",
        warnings(&messages)
    );
    assert!(
        !paths(&server)?
            .iter()
            .any(|path| path.contains("/media/") || path.contains("/download/")),
        "a placeholder downloads nothing"
    );
    Ok(())
}

#[test]
fn a_rate_limited_request_waits_as_asked_and_a_limit_that_holds_stops_the_run() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    let dir = scratch("rate-limit")?;
    server.throttle("/messages", 2, 20)?;

    let messages = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &news), "Family news".to_owned())]
    );
    let limited = server
        .requests()?
        .iter()
        .filter(|request| request.path.ends_with("/messages"))
        .count();
    assert_eq!(
        limited, 4,
        "two refused tries, then the page back from the first event, then the page forward to the room's end"
    );

    server.text(FAMILY, BOB, ms(-30)?, "More news")?;
    server.throttle("/messages", 100, 20)?;
    let held = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&messages))
        .run()?;
    server.throttle("/messages", 0, 20)?;
    remove(&dir)?;
    assert!(records(&held).is_empty());
    assert_eq!(cursors(&held).get(FAMILY), cursors(&messages).get(FAMILY));
    assert!(
        warnings(&held)
            .iter()
            .any(|warning| warning.contains("rate limit")),
        "{:?}",
        warnings(&held)
    );
    Ok(())
}

#[test]
fn a_room_that_fails_keeps_its_cursor_while_the_others_move() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    let stored = server.text(FAMILY, ALICE, ms(-300)?, "Stored")?;
    let shelved = server.text(BOOKS, BOB, ms(-300)?, "Stored too")?;
    server.text(FAMILY, BOB, ms(-60)?, "Unread while the room fails")?;
    let fresh = server.text(BOOKS, ALICE, ms(-60)?, "New in the book club")?;
    server.fail(FAMILY)?;
    let dir = scratch("failing")?;
    let start = BTreeMap::from([
        (FAMILY.to_owned(), cursor_at(&stored, -300)?),
        (BOOKS.to_owned(), cursor_at(&shelved, -300)?),
    ]);

    let messages = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&start)
        .run()?;
    remove(&dir)?;

    let ends = ends(&messages);
    let (status, cursor, error) = ends.get(FAMILY).context("the family room ended")?;
    assert_eq!(*status, StreamStatus::Error);
    assert_eq!(*cursor, None);
    assert!(
        error.as_deref().is_some_and(|error| error.contains("500")),
        "{error:?}"
    );
    assert_eq!(
        ends.get(BOOKS)
            .map(|(status, cursor, _)| (*status, cursor.clone())),
        Some((StreamStatus::Ok, Some(cursor_at(&fresh, -60)?)))
    );
    Ok(())
}

#[test]
fn excluded_rooms_are_never_read_by_id_alias_name_or_pointer_pattern() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    server.room(BANK, None)?;
    server.alias(BANK, "#bank-alerts:example.org")?;
    server.room(CODES, Some("Login Verification Codes"))?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    server.text(BOOKS, BOB, ms(-60)?, "Book news")?;
    server.text(BANK, BOB, ms(-60)?, "Your balance is low")?;
    server.text(CODES, BOB, ms(-60)?, "Your code is 000000")?;
    let dir = scratch("exclusions")?;

    let mut fetch = Fetch::new(&server, &dir).with(
        "exclude_rooms",
        json!(["*verification*", "#BANK-alerts:example.org"]),
    );
    fetch.exclude = vec![format!("matrix:{BOOKS}/*")];
    let messages = fetch.run()?;

    assert_eq!(planned(&messages), [FAMILY]);
    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &news), "Family news".to_owned())]
    );
    for room in [BOOKS, BANK, CODES] {
        untouched(&server, room)?;
    }

    // Without a sync, and with no name kept from an earlier run, a room's name is not known
    // before the run plans it: the run reads the name first and no message of a room the name
    // excludes.
    remove(&dir)?;
    let fresh_dir = scratch("exclusions-unsynced")?;
    server.clear_requests()?;
    let unsynced = Fetch::new(&server, &fresh_dir)
        .with("sync", json!("off"))
        .with("exclude_rooms", json!(["*verification*"]))
        .run()?;
    let code_reads = room_reads(&server, CODES)?;
    remove(&fresh_dir)?;
    assert!(!code_reads.is_empty(), "the run reads the room's name");
    assert!(
        code_reads.iter().all(|path| path.contains("/state/")),
        "{code_reads:?}"
    );
    assert!(records(&unsynced)
        .iter()
        .all(|record| record.stream != CODES));
    Ok(())
}

#[test]
fn an_allowlist_reads_only_the_rooms_it_names() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    let books = server.text(BOOKS, BOB, ms(-60)?, "Book news")?;
    let dir = scratch("allowlist")?;

    let messages = Fetch::new(&server, &dir)
        .with("rooms", json!(["Book Club", "!notjoined:example.org"]))
        .run()?;
    remove(&dir)?;

    assert_eq!(planned(&messages), [BOOKS]);
    assert_eq!(
        texts(&messages),
        [(pointer(BOOKS, &books), "Book news".to_owned())]
    );
    untouched(&server, FAMILY)?;
    assert!(warnings(&messages)
        .iter()
        .any(|warning| warning.contains("!notjoined:example.org")));
    Ok(())
}

#[test]
fn an_event_after_the_window_end_waits_and_holds_what_follows_it() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    let early = server.text(FAMILY, ALICE, ms(-60)?, "Before the window end")?;
    let after = server.text(FAMILY, BOB, ms(2)?, "Sent while the fetch ran")?;
    let late = server.text(
        FAMILY,
        ALICE,
        ms(-30)?,
        "Bridged after it, dated before the end",
    )?;
    // Dated three days after the real clock: no clock has reached that date.
    let wrong_clock = (chrono::Utc::now() + chrono::Duration::days(3)).timestamp_millis();
    let skewed = server.text(FAMILY, BOB, wrong_clock, "A phone with a wrong clock")?;
    let dir = scratch("window-end")?;

    let first = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    assert_eq!(
        texts(&first),
        [(pointer(FAMILY, &early), "Before the window end".to_owned())]
    );
    assert_eq!(cursors(&first).get(FAMILY), Some(&cursor_at(&early, -60)?));

    let second = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&cursors(&first))
        .until("2026-10-25T18:10:00Z")
        .run()?;
    remove(&dir)?;
    assert_eq!(
        texts(&second),
        [
            (
                pointer(FAMILY, &after),
                "Sent while the fetch ran".to_owned()
            ),
            (
                pointer(FAMILY, &late),
                "Bridged after it, dated before the end".to_owned()
            ),
            (
                pointer(FAMILY, &skewed),
                "A phone with a wrong clock".to_owned()
            ),
        ]
    );
    let stored_at = records(&second)
        .last()
        .map(|record| record.ts)
        .context("the skewed message is a record")?;
    assert_eq!(stored_at, at("2026-10-25T18:10:00Z")?);
    assert!(warnings(&second)
        .iter()
        .any(|warning| warning.contains(&skewed) && warning.contains("wrong clock")));
    Ok(())
}

#[test]
fn a_cursor_whose_event_is_gone_reads_on_from_its_time() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.text(FAMILY, ALICE, ms(-300)?, "Before the cursor's time")?;
    let fresh = server.text(FAMILY, BOB, ms(-60)?, "After it")?;
    let dir = scratch("gone")?;
    let start = BTreeMap::from([(FAMILY.to_owned(), cursor_at("$purged", -120)?)]);

    let messages = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .cursors(&start)
        .run()?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &fresh), "After it".to_owned())]
    );
    assert!(warnings(&messages)
        .iter()
        .any(|warning| warning.contains("$purged")));
    Ok(())
}

#[test]
fn an_unreadable_state_file_costs_a_full_look_and_no_record() -> Result<()> {
    let _environment = guard();
    let server: FakeMatrix = homeserver()?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    let dir = scratch("bad-state")?;
    std::fs::write(dir.join("state.json"), "not json")?;

    let messages = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .run()?;
    let state = stored_state(&dir)?;
    remove(&dir)?;

    assert_eq!(
        texts(&messages),
        [(pointer(FAMILY, &news), "Family news".to_owned())]
    );
    assert!(warnings(&messages)
        .iter()
        .any(|warning| warning.contains("state.json")));
    assert!(state.get("since").is_some());
    Ok(())
}

#[test]
fn a_named_room_excluded_by_its_alias_is_never_read() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BANK, Some("Alerts"))?;
    server.alias(BANK, "#bank-alerts:example.org")?;
    server.text(BANK, BOB, ms(-60)?, "Your balance is low")?;
    let news = server.text(FAMILY, ALICE, ms(-60)?, "Family news")?;
    let dir = scratch("alias-named")?;

    // A room read once, before the exclusion existed, keeps its name and no alias.
    let before = Fetch::new(&server, &dir).with("sync", json!("off")).run()?;
    assert_eq!(records(&before).len(), 2);
    server.text(BANK, BOB, ms(-30)?, "Your code is 000000")?;
    server.clear_requests()?;
    let unsynced = Fetch::new(&server, &dir)
        .with("sync", json!("off"))
        .with("exclude_rooms", json!(["#bank-alerts:example.org"]))
        .cursors(&cursors(&before))
        .run()?;
    let bank_reads = room_reads(&server, BANK)?;
    assert_ne!(bank_reads, Vec::<String>::new());
    assert!(
        bank_reads.iter().all(|path| path.contains("/state/")),
        "{bank_reads:?}"
    );
    assert!(records(&unsynced)
        .iter()
        .all(|record| record.stream != BANK));

    // The full sync gives its alias now, so the room is not planned. The sync leaves out only
    // rooms excluded by id, so a rename or a new alias is seen.
    server.clear_requests()?;
    let synced = Fetch::new(&server, &dir)
        .with("exclude_rooms", json!(["#bank-alerts:example.org"]))
        .run()?;
    remove(&dir)?;
    untouched(&server, BANK)?;
    assert_eq!(planned(&synced), [FAMILY]);
    let sync = server
        .requests()?
        .into_iter()
        .find(|request| request.path.ends_with("/sync"))
        .context("the run sent a sync")?;
    let filter: serde_json::Value = serde_json::from_str(
        query(&sync.target)
            .get("filter")
            .context("the sync has a filter")?,
    )?;
    assert_eq!(filter.pointer("/room/not_rooms"), Some(&json!([])));
    assert!(texts(&synced)
        .iter()
        .all(|(record, _)| *record == pointer(FAMILY, &news)));
    Ok(())
}

#[test]
fn a_room_excluded_for_a_while_is_read_again_when_it_comes_back() -> Result<()> {
    let _environment = guard();
    let server = homeserver()?;
    server.room(BOOKS, Some("Book club"))?;
    server.text(BOOKS, BOB, ms(-300)?, "Chapter one")?;
    let dir = scratch("re-included")?;
    let first = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .run()?;

    let missed = server.text(BOOKS, ALICE, ms(-200)?, "Chapter two")?;
    let excluded = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .with("exclude_rooms", json!([BOOKS]))
        .cursors(&cursors(&first))
        .run()?;
    assert_eq!(planned(&excluded), [FAMILY]);

    let back = Fetch::new(&server, &dir)
        .with("sync", json!("incremental"))
        .cursors(&cursors(&first))
        .run()?;
    remove(&dir)?;
    assert_eq!(
        texts(&back),
        [(pointer(BOOKS, &missed), "Chapter two".to_owned())]
    );
    Ok(())
}
