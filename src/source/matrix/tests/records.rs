//! A Matrix event as a record.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::super::records::{
    author_line, is_event_id, is_room_id, kind, one_line, parse_pointer, pointer, text,
    thread_root, Kind, Room, REMOVED,
};

const ROOM: &str = "!dmroom:example.org";

fn at(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn room() -> Room<'static> {
    Room {
        scheme: "matrix",
        id: ROOM,
        label: "Alice",
    }
}

fn content(event: &Value) -> &Value {
    event.get("content").unwrap_or(&Value::Null)
}

fn message(id: &str, content: &Value) -> Value {
    json!({
        "type": "m.room.message",
        "event_id": id,
        "sender": "@alice:example.org",
        "origin_server_ts": 1_792_951_800_000_i64,
        "room_id": ROOM,
        "content": content,
    })
}

#[test]
fn a_pointer_names_the_room_and_the_event_and_reads_back() {
    let written = pointer("matrix", "!-room_v12id", "$abc+def/ghi");
    assert_eq!(written, "matrix:!-room_v12id/$abc+def/ghi");
    assert_eq!(
        parse_pointer(&written, "matrix"),
        Some(("!-room_v12id", "$abc+def/ghi"))
    );
    assert_eq!(
        parse_pointer(
            "matrix:!room:example.org:8448/$legacy:example.org",
            "matrix"
        ),
        Some(("!room:example.org:8448", "$legacy:example.org"))
    );
    for refused in [
        "matrix:!room:example.org",
        "matrix:room/$event",
        "matrix:!room/event",
        "slack:!room/$event",
        "matrix:!/$event",
    ] {
        assert_eq!(parse_pointer(refused, "matrix"), None, "{refused}");
    }
}

#[test]
fn a_text_message_reads_as_its_body() {
    let content = json!({"msgtype": "m.text", "body": "See you at the station at six."});
    assert_eq!(
        text(&content, &message("$e1", &content)).as_deref(),
        Some("See you at the station at six.")
    );
}

#[test]
fn a_reply_loses_the_quote_of_the_message_it_answers() {
    let content = json!({
        "msgtype": "m.text",
        "body": "> <@bob:example.org> Are we still on for six?\n> Bring the map.\n\nYes, six works.",
        "m.relates_to": {"m.in_reply_to": {"event_id": "$e0"}},
    });
    assert_eq!(
        text(&content, &message("$e1", &content)).as_deref(),
        Some("Yes, six works.")
    );
    // A reply without the fallback (Matrix 1.13 on) keeps a body that starts with a quote.
    let quoting = json!({
        "msgtype": "m.text",
        "body": "> a line I quote myself",
        "m.relates_to": {"m.in_reply_to": {"event_id": "$e0"}},
    });
    assert_eq!(
        text(&quoting, &message("$e2", &quoting)).as_deref(),
        Some("> a line I quote myself")
    );
    // A message that is not a reply keeps a body that starts with `> `.
    let plain = json!({"msgtype": "m.text", "body": "> not a reply"});
    assert_eq!(
        text(&plain, &message("$e3", &plain)).as_deref(),
        Some("> not a reply")
    );
}

#[test]
fn media_reads_as_a_placeholder_naming_the_file_with_its_caption() {
    for (content, expected) in [
        (
            json!({"msgtype": "m.image", "body": "beach.jpg", "url": "mxc://example.org/a"}),
            "[image: beach.jpg]",
        ),
        (
            json!({"msgtype": "m.image", "body": "Look at this view", "filename": "beach.jpg"}),
            "[image: beach.jpg]\nLook at this view",
        ),
        (
            json!({"msgtype": "m.video", "body": "clip.mp4", "filename": "clip.mp4"}),
            "[video: clip.mp4]",
        ),
        (
            json!({"msgtype": "m.audio", "body": "Voice message.ogg"}),
            "[audio: Voice message.ogg]",
        ),
        (
            json!({"msgtype": "m.file", "body": "lease.pdf"}),
            "[file: lease.pdf]",
        ),
        (json!({"msgtype": "m.file", "body": ""}), "[file]"),
    ] {
        assert_eq!(
            text(&content, &message("$m", &content)).as_deref(),
            Some(expected),
            "{content}"
        );
    }
    let sticker = json!({
        "type": "m.sticker",
        "event_id": "$s",
        "content": {"body": "thumbs up", "url": "mxc://example.org/s"},
    });
    assert_eq!(
        text(content(&sticker), &sticker).as_deref(),
        Some("[sticker: thumbs up]")
    );
}

#[test]
fn each_event_is_a_message_an_edit_a_redaction_a_removal_or_nothing() {
    let plain = message("$e1", &json!({"msgtype": "m.text", "body": "hello"}));
    assert_eq!(kind(&plain), Kind::Message);

    let edit = message(
        "$e2",
        &json!({
            "msgtype": "m.text",
            "body": "* hello there",
            "m.new_content": {"msgtype": "m.text", "body": "hello there"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$e1"},
        }),
    );
    assert_eq!(
        kind(&edit),
        Kind::Edit {
            target: "$e1",
            content: &json!({"msgtype": "m.text", "body": "hello there"}),
        }
    );

    let older_redaction =
        json!({"type": "m.room.redaction", "redacts": "$e1", "event_id": "$r1", "content": {}});
    assert_eq!(kind(&older_redaction), Kind::Redaction { target: "$e1" });
    let v11_redaction =
        json!({"type": "m.room.redaction", "event_id": "$r2", "content": {"redacts": "$e1"}});
    assert_eq!(kind(&v11_redaction), Kind::Redaction { target: "$e1" });

    let removed = json!({
        "type": "m.room.message",
        "event_id": "$e3",
        "content": {},
        "unsigned": {"redacted_because": {"type": "m.room.redaction"}},
    });
    assert_eq!(kind(&removed), Kind::Removed);

    let encrypted = json!({"type": "m.room.encrypted", "event_id": "$x", "content": {"algorithm": "m.megolm.v1.aes-sha2"}});
    assert_eq!(kind(&encrypted), Kind::Encrypted);

    let no_body = message("$e4", &json!({"msgtype": "m.text"}));
    assert_eq!(kind(&no_body), Kind::Other);
    let state = json!({"type": "m.room.name", "event_id": "$n", "content": {"name": "Renamed"}});
    assert_eq!(kind(&state), Kind::Other);
}

#[test]
fn a_record_carries_the_room_the_sender_and_the_thread() -> Result<()> {
    let reply = message(
        "$reply",
        &json!({
            "msgtype": "m.text",
            "body": "In the thread.",
            "m.relates_to": {"rel_type": "m.thread", "event_id": "$root"},
        }),
    );
    let ts = at("2026-10-24T08:10:00Z")?;
    let record = room()
        .record(&reply, content(&reply), &reply, "Alice", ts)
        .context("a thread reply is a record")?;
    assert_eq!(record.pointer, "matrix:!dmroom:example.org/$reply");
    assert_eq!(record.stream, ROOM);
    assert_eq!(record.ts, ts);
    assert_eq!(record.label.as_deref(), Some("Alice"));
    assert_eq!(record.author.as_deref(), Some("Alice"));
    assert_eq!(record.author_id.as_deref(), Some("@alice:example.org"));
    assert_eq!(
        record.thread.as_deref(),
        Some("matrix:!dmroom:example.org/$root")
    );
    assert_eq!(
        record.url.as_deref(),
        Some("https://matrix.to/#/%21dmroom%3Aexample.org/%24reply")
    );
    assert_eq!(record.text, "In the thread.");

    let top_level = message("$top", &json!({"msgtype": "m.text", "body": "Top level."}));
    let record = room()
        .record(&top_level, content(&top_level), &top_level, "Alice", ts)
        .context("a message is a record")?;
    assert_eq!(record.thread, None);
    Ok(())
}

#[test]
fn an_edit_is_the_original_message_with_the_new_text() -> Result<()> {
    let original = message(
        "$e1",
        &json!({
            "msgtype": "m.text",
            "body": "Meet at 6",
            "m.relates_to": {"rel_type": "m.thread", "event_id": "$root"},
        }),
    );
    let edit = message(
        "$e2",
        &json!({
            "msgtype": "m.text",
            "body": "* Meet at 7",
            "m.new_content": {"msgtype": "m.text", "body": "Meet at 7"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$e1"},
        }),
    );
    let ts = at("2026-10-24T08:00:00Z")?;
    let record = room()
        .record(
            &original,
            edit.pointer("/content/m.new_content")
                .unwrap_or(&Value::Null),
            &edit,
            "Alice",
            ts,
        )
        .context("an edit is a record")?;
    assert_eq!(record.pointer, "matrix:!dmroom:example.org/$e1");
    assert_eq!(record.text, "Meet at 7");
    assert_eq!(
        record.thread.as_deref(),
        Some("matrix:!dmroom:example.org/$root")
    );
    assert_eq!(record.raw, edit);
    Ok(())
}

#[test]
fn a_removed_message_says_so_and_keeps_its_pointer() -> Result<()> {
    let original = message("$e1", &json!({"msgtype": "m.text", "body": "oops"}));
    let redaction = json!({"type": "m.room.redaction", "event_id": "$r1", "redacts": "$e1"});
    let ts = at("2026-10-24T08:00:00Z")?;
    let record = room()
        .removed(&original, &redaction, "Alice", ts)
        .context("a removal is a record")?;
    assert_eq!(record.pointer, "matrix:!dmroom:example.org/$e1");
    assert_eq!(record.text, REMOVED);
    assert_eq!(record.raw, redaction);
    Ok(())
}

#[test]
fn an_event_id_is_a_dollar_and_at_most_255_bytes_of_id_bytes() {
    for id in [
        "$Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg",
        "$abc+def/ghi=",
        "$legacy1234:example.org",
    ] {
        assert!(is_event_id(id), "{id}");
    }
    let longest = format!("${}", "a".repeat(254));
    assert!(is_event_id(&longest));
    for id in [
        "",
        "$",
        "event",
        "!room:example.org",
        "$a\nb",
        "$a b",
        "$trailing.",
        "$trailing:",
        &format!("${}", "a".repeat(255)),
    ] {
        assert!(!is_event_id(id), "{id:?}");
    }
    assert!(is_room_id("!room:example.org:8448"));
    assert!(is_room_id("!-BooksClubRoomV12"));
    assert!(!is_room_id("!room:[::1]"));
    assert!(!is_room_id("room:example.org"));
}

#[test]
fn a_relation_that_names_no_event_id_is_no_relation() {
    let long = format!("${}", "a".repeat(3_000));
    let reply = message(
        "$reply",
        &json!({"msgtype": "m.text", "body": "hi",
                "m.relates_to": {"rel_type": "m.thread", "event_id": long}}),
    );
    assert_eq!(thread_root(&reply), None);
    assert_eq!(kind(&reply), Kind::Message);
    let edit = message(
        "$edit",
        &json!({"msgtype": "m.text", "body": "* hi",
                "m.new_content": {"msgtype": "m.text", "body": "hi"},
                "m.relates_to": {"rel_type": "m.replace", "event_id": "$x\ny"}}),
    );
    assert_eq!(kind(&edit), Kind::Other);
    let redaction = json!({"type": "m.room.redaction", "event_id": "$r", "redacts": long});
    assert_eq!(kind(&redaction), Kind::Other);
}

#[test]
fn a_name_is_one_line_cut_short_and_an_author_shows_their_matrix_id() {
    assert_eq!(
        one_line("Book club\n## Heading\t- item", 256),
        "Book club ## Heading - item"
    );
    assert_eq!(one_line("a\u{1b}[2Jb", 256), "a [2Jb");
    assert_eq!(one_line("abcdef", 3), "abc…");
    assert_eq!(
        author_line(Some("Ada\nLovelace"), "@ada:example.org"),
        "Ada Lovelace (@ada:example.org)"
    );
    assert_eq!(author_line(None, "@ada:example.org"), "@ada:example.org");
    assert_eq!(
        author_line(Some("@ada:example.org"), "@ada:example.org"),
        "@ada:example.org"
    );
    assert_eq!(
        author_line(Some(" \n "), "@ada:example.org"),
        "@ada:example.org"
    );
}
