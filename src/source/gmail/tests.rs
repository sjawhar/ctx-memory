//! Unit tests for the Gmail source.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use super::super::{FetchRequest, ResolveRequest, StreamStatus, PROTOCOL_VERSION};
use super::fetch::{
    excluded_labels, fetch_messages, GmailApi, History, HistoryEntry, Reach, MAX_HISTORY_MESSAGES,
};
use super::*;

#[derive(Default)]
struct FakeGmailApi {
    calls: RefCell<Vec<String>>,
    candidates: Vec<Value>,
    messages: BTreeMap<String, Value>,
    threads: BTreeMap<String, Vec<Value>>,
    history: Option<History>,
    /// The history each start ID opens, where a test names one; any other ID opens
    /// `history`.
    histories: BTreeMap<String, History>,
    /// History IDs Gmail no longer holds.
    expired: BTreeSet<String>,
    latest_history_id: String,
}

impl GmailApi for FakeGmailApi {
    fn list_messages(&self, query: &str) -> Result<Vec<Value>> {
        self.calls.borrow_mut().push(format!("list {query}"));
        Ok(self.candidates.clone())
    }

    fn message(&self, id: &str) -> Result<Option<Value>> {
        self.calls.borrow_mut().push(format!("messages get {id}"));
        // A message the mailbox no longer holds is what Gmail answers 404 for.
        Ok(self.messages.get(id).cloned())
    }

    fn thread(&self, id: &str) -> Result<Vec<Value>> {
        self.calls.borrow_mut().push(format!("threads get {id}"));
        Ok(self.threads.get(id).cloned().unwrap_or_default())
    }

    fn history(&self, start: &str) -> Result<Option<History>> {
        self.calls.borrow_mut().push(format!("history {start}"));
        Ok(self
            .histories
            .get(start)
            .cloned()
            .or_else(|| self.history.clone()))
    }

    fn holds_history(&self, start: &str) -> Result<bool> {
        self.calls.borrow_mut().push(format!("holds {start}"));
        Ok(!self.expired.contains(start))
    }

    fn latest_history_id(&self) -> Result<String> {
        self.calls.borrow_mut().push("profile".to_owned());
        Ok(self.latest_history_id.clone())
    }
}

fn message(id: &str, internal_date: &str, body: &str) -> Value {
    labelled_message(id, internal_date, body, &[])
}

fn labelled_message(id: &str, internal_date: &str, body: &str, labels: &[&str]) -> Value {
    json!({
        "labelIds": labels,
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": internal_date,
        "payload": {
            "headers": [{"name": "Subject", "value": id}],
            "mimeType": "text/plain",
            "body": {"data": body}
        }
    })
}

fn fetch_request(cursor: Option<&str>) -> Result<FetchRequest> {
    let mut cursors = BTreeMap::new();
    if let Some(cursor) = cursor {
        cursors.insert(STREAM.to_owned(), cursor.to_owned());
    }
    Ok(FetchRequest {
        protocol: PROTOCOL_VERSION,
        source: super::super::SourceRef {
            id: "gmail".to_owned(),
            kind: "gmail".to_owned(),
            scheme: "gmail".to_owned(),
        },
        config: json!({}),
        window_end: DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc),
        backfill_hours: 24,
        cursors,
        discovered: Vec::new(),
        state_dir: std::env::temp_dir(),
        exclude: Vec::new(),
    })
}

fn run_fetch(api: &FakeGmailApi, request: &FetchRequest) -> Result<Vec<Message>> {
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    fetch_with_api(request, &config, api, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(messages)
}

fn pointers(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some(record.pointer.clone()),
            _ => None,
        })
        .collect()
}

fn stream_end(messages: &[Message]) -> Option<(StreamStatus, Option<String>)> {
    messages.iter().find_map(|message| match message {
        Message::StreamEnd { status, cursor, .. } => Some((*status, cursor.clone())),
        _ => None,
    })
}

fn warnings(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Warning { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn gmail_uses_history_id_cursor() -> Result<()> {
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["aaaaaaaaaaaaaaaa".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
        )]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), vec!["gmail:aaaaaaaaaaaaaaaa"]);
    // The cursor carries the time through which the mailbox is stored beside the history
    // ID, so a run whose history Gmail no longer holds knows where to list from.
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("1000@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    assert_eq!(
        api.calls.into_inner(),
        vec!["history 850", "messages get aaaaaaaaaaaaaaaa"]
    );
    Ok(())
}

#[test]
fn gmail_without_a_cursor_lists_the_window_and_keeps_the_history_id() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![json!({"id": "aaaaaaaaaaaaaaaa"})],
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
        )]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(None)?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), vec!["gmail:aaaaaaaaaaaaaaaa"]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("4242@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    let calls = api.calls.into_inner();
    assert_eq!(calls.first().map(String::as_str), Some("profile"));
    assert!(
        calls
            .iter()
            .any(|call| call.contains("-category:promotions -category:social")),
        "{calls:?}"
    );
    Ok(())
}

#[test]
fn gmail_skips_promotions_and_spam() -> Result<()> {
    let promoted = labelled_message(
        "bbbbbbbbbbbbbbbb",
        "1792944000000",
        "Zmlyc3Q=",
        &["CATEGORY_PROMOTIONS"],
    );
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["bbbbbbbbbbbbbbbb".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([("bbbbbbbbbbbbbbbb".to_owned(), promoted)]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), [] as [String; 0]);
    // A cursor an earlier build wrote carries no time; the first finished run adds one.
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("1000@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    Ok(())
}

#[test]
fn gmail_holds_the_cursor_for_a_message_newer_than_the_window() -> Result<()> {
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["cccccccccccccccc".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([(
            "cccccccccccccccc".to_owned(),
            // 2026-10-25T18:30:00Z, after the window end.
            message("cccccccccccccccc", "1792989000000", "Zmlyc3Q="),
        )]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), [] as [String; 0]);
    assert_eq!(
        stream_end(&messages),
        Some((StreamStatus::Ok, Some("850".to_owned())))
    );
    Ok(())
}

#[test]
fn gmail_falls_back_to_a_listing_when_the_history_id_is_too_old() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![json!({"id": "aaaaaaaaaaaaaaaa"})],
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
        )]),
        latest_history_id: "4242".to_owned(),
        history: None,
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("1@2026-10-20T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), vec!["gmail:aaaaaaaaaaaaaaaa"]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("4242@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    assert_eq!(warnings(&messages).len(), 1);
    // The listing starts at the time the cursor names, not at the backfill window, so the
    // days between the two are not stepped over.
    let calls = api.calls.into_inner();
    assert!(
        calls.iter().any(|call| call.contains("after:2026/10/19")),
        "{calls:?}"
    );
    Ok(())
}

#[test]
fn gmail_lists_the_window_again_when_mail_arrives_after_it() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![
            json!({"id": "aaaaaaaaaaaaaaaa"}),
            json!({"id": "dddddddddddddddd"}),
        ],
        messages: BTreeMap::from([
            (
                "aaaaaaaaaaaaaaaa".to_owned(),
                message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
            ),
            (
                // 2026-10-25T18:30:00Z, after the window end.
                "dddddddddddddddd".to_owned(),
                message("dddddddddddddddd", "1792989000000", "bGF0ZXI="),
            ),
        ]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(None)?;
    let messages = run_fetch(&api, &request)?;

    // The history ID was read before the listing, so it may already cover the mail that
    // arrived after the window ended and this window must not carry. Naming it would skip
    // that mail for good, so the stream names the window's end, carries the ID, and lists
    // again next run.
    assert_eq!(pointers(&messages), vec!["gmail:aaaaaaaaaaaaaaaa"]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("2026-10-24T18:00:00Z/2026-10-25T18:00:00Z#4242".to_owned())
        ))
    );
    Ok(())
}

/// 2100-01-01T00:00:00Z: an arrival date this far ahead can only come from a wrong clock.
const FUTURE_MILLIS: &str = "4102444800000";

/// Each record's pointer, time, and whether it is context.
fn dated(messages: &[Message]) -> Vec<(String, DateTime<Utc>, bool)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some((record.pointer.clone(), record.ts, record.context)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_message_dated_in_the_future_is_stored_and_does_not_hold_the_history_cursor() -> Result<()> {
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![
                HistoryEntry {
                    id: "900".to_owned(),
                    added: vec!["eeeeeeeeeeeeeeee".to_owned()],
                },
                HistoryEntry {
                    id: "950".to_owned(),
                    added: vec!["aaaaaaaaaaaaaaaa".to_owned()],
                },
            ],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([
            (
                "eeeeeeeeeeeeeeee".to_owned(),
                message("eeeeeeeeeeeeeeee", FUTURE_MILLIS, "c2tld2Vk"),
            ),
            (
                "aaaaaaaaaaaaaaaa".to_owned(),
                message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
            ),
        ]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    // Waiting for the clock to pass the date would hold every later message behind it. The
    // window stores the message at its end, and its raw data keeps its own date.
    assert_eq!(
        dated(&messages),
        vec![
            (
                "gmail:eeeeeeeeeeeeeeee".to_owned(),
                request.window_end,
                false
            ),
            (
                "gmail:aaaaaaaaaaaaaaaa".to_owned(),
                DateTime::parse_from_rfc3339("2026-10-25T16:00:00Z")?.with_timezone(&Utc),
                false
            ),
        ]
    );
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("1000@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("eeeeeeeeeeeeeeee")
                && warning.contains("2100-01-01T00:00:00Z")),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}

#[test]
fn a_message_minutes_after_a_window_that_ends_now_still_holds_the_history_cursor() -> Result<()> {
    // The run reads the message moments after the window ends: its date is believable, so
    // it belongs to the next window and the cursor holds.
    let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).context("the time now")?;
    let later = (now + Duration::minutes(5)).timestamp_millis().to_string();
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["cccccccccccccccc".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([(
            "cccccccccccccccc".to_owned(),
            message("cccccccccccccccc", &later, "Zmlyc3Q="),
        )]),
        ..FakeGmailApi::default()
    };
    let mut request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    request.window_end = now;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), [] as [String; 0]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("850@2026-10-24T00:00:00Z".to_owned())
        ))
    );
    Ok(())
}

#[test]
fn a_listing_stores_a_message_dated_in_the_future_and_names_the_history_id() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![
            json!({"id": "aaaaaaaaaaaaaaaa"}),
            json!({"id": "eeeeeeeeeeeeeeee"}),
        ],
        messages: BTreeMap::from([
            (
                "aaaaaaaaaaaaaaaa".to_owned(),
                message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
            ),
            (
                "eeeeeeeeeeeeeeee".to_owned(),
                message("eeeeeeeeeeeeeeee", FUTURE_MILLIS, "c2tld2Vk"),
            ),
        ]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(None)?;
    let messages = run_fetch(&api, &request)?;

    // A message no clock has reached yet did not arrive after the window: holding the
    // cursor at the window end for it would list it again on every run until 2100.
    assert_eq!(
        pointers(&messages),
        vec!["gmail:aaaaaaaaaaaaaaaa", "gmail:eeeeeeeeeeeeeeee"]
    );
    assert!(dated(&messages)
        .iter()
        .all(|(_, ts, context)| *ts <= request.window_end && !context));
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("4242@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    // The listing that reaches the window end has no upper bound, so it names mail dated
    // any time after its start.
    let calls = api.calls.into_inner();
    let listing = calls
        .iter()
        .find(|call| call.starts_with("list "))
        .context("the run listed the mailbox")?;
    assert!(!listing.contains("before:"), "{listing}");
    Ok(())
}

#[test]
fn gmail_errors_when_an_expired_history_names_no_stored_time() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![json!({"id": "aaaaaaaaaaaaaaaa"})],
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
        )]),
        latest_history_id: "4242".to_owned(),
        history: None,
        ..FakeGmailApi::default()
    };
    // A cursor an earlier build wrote names no time, so nothing says how far back the
    // listing must reach. The stream stops rather than write a cursor over that gap.
    let request = fetch_request(Some("1"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), [] as [String; 0]);
    assert_eq!(stream_end(&messages), Some((StreamStatus::Error, None)));
    Ok(())
}

#[test]
fn gmail_skips_a_message_the_history_names_and_gmail_no_longer_holds() -> Result<()> {
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["eeeeeeeeeeeeeeee".to_owned(), "aaaaaaaaaaaaaaaa".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        // The first message was deleted after the history named it, so Gmail answers 404.
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
        )]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), vec!["gmail:aaaaaaaaaaaaaaaa"]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("1000@2026-10-25T18:00:00Z".to_owned())
        ))
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("1 message")),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}

#[test]
fn gmail_lists_a_long_gap_one_slice_at_a_time() -> Result<()> {
    let early = "00000000000000a1";
    let late = "00000000000000a2";
    let api = FakeGmailApi {
        candidates: vec![json!({"id": early}), json!({"id": late})],
        messages: BTreeMap::from([
            // 2026-10-03T12:00:00Z, inside the first slice.
            (
                early.to_owned(),
                message(early, "1791028800000", "Zmlyc3Q="),
            ),
            // 2026-10-20T12:00:00Z, in a later slice.
            (late.to_owned(), message(late, "1792497600000", "bGF0ZXI=")),
        ]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    // The mailbox is stored through October 1st, more than three weeks before the window
    // ends, so this run lists the first seven days and the next one lists on from there. A
    // bare time is what an earlier build wrote, so the run reads a history ID first.
    let request = fetch_request(Some("2026-10-01T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages), vec![format!("gmail:{early}")]);
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("2026-10-01T00:00:00Z/2026-10-08T00:00:00Z#4242".to_owned())
        ))
    );
    let calls = api.calls.into_inner();
    assert_eq!(calls.first().map(String::as_str), Some("profile"));
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("list after:2026/09/30 before:2026/10/09")),
        "{calls:?}"
    );
    Ok(())
}

/// Runs one fetch at `window_end` from `cursor`, returning its records' pointers and the
/// cursor it proposed.
fn run_at(
    api: &FakeGmailApi,
    cursor: Option<&str>,
    window_end: &str,
    backfill_hours: u32,
) -> Result<(Vec<String>, String)> {
    let mut request = fetch_request(cursor)?;
    request.window_end = DateTime::parse_from_rfc3339(window_end)?.with_timezone(&Utc);
    request.backfill_hours = backfill_hours;
    let messages = run_fetch(api, &request)?;
    let Some((StreamStatus::Ok, Some(cursor))) = stream_end(&messages) else {
        bail!("the run proposed no cursor: {messages:?}");
    };
    Ok((pointers(&messages), cursor))
}

#[test]
fn mail_imported_during_a_sliced_catch_up_with_an_old_date_is_read_by_the_next_run() -> Result<()> {
    let early = "00000000000000b1";
    let imported = "00000000000000b2";
    let late = "00000000000000b3";
    // Ten days of backfill: the first run lists the oldest seven days.
    let first = FakeGmailApi {
        candidates: vec![json!({"id": early})],
        // 2026-10-16T12:00:00Z, inside the first slice.
        messages: BTreeMap::from([(
            early.to_owned(),
            message(early, "1792152000000", "Zmlyc3Q="),
        )]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    let (_, cursor) = run_at(&first, None, "2026-10-25T18:00:00Z", 10 * 24)?;

    // Then mail is imported with its old date, 2026-10-18, behind the slice the next run
    // lists. Gmail's history names it after the first ID, and an ID read now follows it.
    let last = FakeGmailApi {
        candidates: vec![json!({"id": late})],
        // 2026-10-24T12:00:00Z, inside the last slice.
        messages: BTreeMap::from([(late.to_owned(), message(late, "1792843200000", "bGF0ZQ=="))]),
        latest_history_id: "4400".to_owned(),
        ..FakeGmailApi::default()
    };
    let (listed, cursor) = run_at(&last, Some(&cursor), "2026-10-25T19:00:00Z", 10 * 24)?;
    assert_eq!(listed, vec![format!("gmail:{late}")]);

    let walk = FakeGmailApi {
        histories: BTreeMap::from([
            (
                "4242".to_owned(),
                History {
                    entries: vec![HistoryEntry {
                        id: "4300".to_owned(),
                        added: vec![imported.to_owned()],
                    }],
                    latest: Some("4500".to_owned()),
                },
            ),
            (
                "4400".to_owned(),
                History {
                    entries: Vec::new(),
                    latest: Some("4500".to_owned()),
                },
            ),
        ]),
        messages: BTreeMap::from([(
            imported.to_owned(),
            message(imported, "1792324800000", "aW1wb3J0ZWQ="),
        )]),
        ..FakeGmailApi::default()
    };
    let (walked, cursor) = run_at(&walk, Some(&cursor), "2026-10-25T20:00:00Z", 10 * 24)?;
    assert_eq!(
        walked,
        vec![format!("gmail:{imported}")],
        "the imported mail was never read"
    );
    assert_eq!(cursor, "4500@2026-10-25T20:00:00Z");
    Ok(())
}

#[test]
fn an_expired_history_id_mid_catch_up_lists_again_from_the_start_and_loses_no_mail() -> Result<()> {
    let early = "00000000000000c1";
    let imported = "00000000000000c2";
    let middle = "00000000000000c3";
    // Twenty days of backfill, from 2026-10-05T18:00Z: three slices. The first run lists
    // the oldest seven days and reads history ID 4242.
    let first = FakeGmailApi {
        candidates: vec![json!({"id": early})],
        // 2026-10-06T12:00:00Z, inside the first slice.
        messages: BTreeMap::from([(
            early.to_owned(),
            message(early, "1791288000000", "Zmlyc3Q="),
        )]),
        latest_history_id: "4242".to_owned(),
        ..FakeGmailApi::default()
    };
    let (_, cursor) = run_at(&first, None, "2026-10-25T18:00:00Z", 20 * 24)?;
    assert_eq!(
        cursor, "2026-10-05T18:00:00Z/2026-10-12T18:00:00Z#4242",
        "the cursor carries where the catch-up began"
    );

    // Then mail is imported with its old date, 2026-10-08, behind the next slice, and Gmail
    // drops history ID 4242, so no history will ever name that mail.
    let all = BTreeMap::from([
        (
            early.to_owned(),
            message(early, "1791288000000", "Zmlyc3Q="),
        ),
        (
            imported.to_owned(),
            message(imported, "1791460800000", "aW1wb3J0ZWQ="),
        ),
        // 2026-10-14T12:00:00Z, inside the second slice.
        (
            middle.to_owned(),
            message(middle, "1791979200000", "bWlkZGxl"),
        ),
    ]);
    let candidates = vec![
        json!({"id": early}),
        json!({"id": imported}),
        json!({"id": middle}),
    ];
    let restart = FakeGmailApi {
        candidates: candidates.clone(),
        messages: all.clone(),
        expired: BTreeSet::from(["4242".to_owned()]),
        latest_history_id: "4400".to_owned(),
        ..FakeGmailApi::default()
    };
    let mut request = fetch_request(Some(&cursor))?;
    request.window_end = DateTime::parse_from_rfc3339("2026-10-25T19:00:00Z")?.with_timezone(&Utc);
    request.backfill_hours = 20 * 24;
    let messages = run_fetch(&restart, &request)?;
    assert_eq!(
        pointers(&messages),
        vec![format!("gmail:{early}"), format!("gmail:{imported}")],
        "the listing starts again from where the catch-up began"
    );
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some("2026-10-05T18:00:00Z/2026-10-12T18:00:00Z#4400".to_owned())
        ))
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("4242") && warning.contains("2026-10-05T18:00:00Z")),
        "{:?}",
        warnings(&messages)
    );
    // The new ID is read before the listing it follows.
    let calls = restart.calls.into_inner();
    assert_eq!(
        calls.get(..2),
        Some(&["holds 4242".to_owned(), "profile".to_owned()][..])
    );

    // The catch-up then goes on under the new ID, which Gmail still holds.
    let on = FakeGmailApi {
        candidates: candidates.clone(),
        messages: all.clone(),
        ..FakeGmailApi::default()
    };
    let (listed, cursor) = run_at(
        &on,
        Some("2026-10-05T18:00:00Z/2026-10-12T18:00:00Z#4400"),
        "2026-10-25T20:00:00Z",
        20 * 24,
    )?;
    assert_eq!(listed, vec![format!("gmail:{middle}")]);
    assert_eq!(cursor, "2026-10-05T18:00:00Z/2026-10-19T18:00:00Z#4400");
    let (listed, cursor) = run_at(&on, Some(&cursor), "2026-10-25T21:00:00Z", 20 * 24)?;
    assert_eq!(listed, [] as [String; 0]);
    assert_eq!(cursor, "4400@2026-10-25T21:00:00Z");
    Ok(())
}

#[test]
fn gmail_walks_a_long_history_a_bounded_number_of_messages_at_a_time() -> Result<()> {
    let count = MAX_HISTORY_MESSAGES + 1;
    let ids: Vec<String> = (0..count).map(|index| format!("{index:016x}")).collect();
    let api = FakeGmailApi {
        history: Some(History {
            entries: ids
                .iter()
                .enumerate()
                .map(|(index, id)| HistoryEntry {
                    id: (1000 + index).to_string(),
                    added: vec![id.clone()],
                })
                .collect(),
            latest: Some("9999".to_owned()),
        }),
        messages: ids
            .iter()
            .map(|id| (id.clone(), message(id, "1792944000000", "Zmlyc3Q=")))
            .collect(),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(pointers(&messages).len(), MAX_HISTORY_MESSAGES);
    // The cursor names the last change the run finished, with the time the mailbox was
    // stored through before: the walk did not reach the window end.
    assert_eq!(
        stream_end(&messages),
        Some((
            StreamStatus::Ok,
            Some(format!(
                "{}@2026-10-24T00:00:00Z",
                1000 + MAX_HISTORY_MESSAGES - 1
            ))
        ))
    );
    let last = ids.last().context("the history names messages")?;
    assert!(!api.calls.borrow().contains(&format!("messages get {last}")));
    Ok(())
}

#[test]
fn gmail_decodes_non_utf8_charset() -> Result<()> {
    // "Café" in ISO-8859-1, base64url encoded.
    let record = record_from_message(&json!({
        "id": "aaaaaaaaaaaaaaaa",
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": {
            "mimeType": "text/plain",
            "headers": [
                {"name": "Content-Type", "value": "text/plain; charset=\"ISO-8859-1\""}
            ],
            "body": {"data": "Q2Fm6Q=="}
        }
    }))?;

    assert!(record.text.contains("Café"), "{}", record.text);
    Ok(())
}

#[test]
fn resolving_a_thread_uses_the_threads_endpoint_and_sorts_messages() -> Result<()> {
    let api = FakeGmailApi {
        threads: BTreeMap::from([(
            "cccccccccccccccc".to_owned(),
            vec![
                message("bbbbbbbbbbbbbbbb", "1792947600000", "c2Vjb25k"),
                message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
            ],
        )]),
        ..FakeGmailApi::default()
    };
    let request = ResolveRequest {
        protocol: PROTOCOL_VERSION,
        source: super::super::SourceRef {
            id: "gmail".to_owned(),
            kind: "gmail".to_owned(),
            scheme: "gmail".to_owned(),
        },
        config: json!({}),
        pointers: vec!["gmail:thread/cccccccccccccccc".to_owned()],
        state_dir: std::env::temp_dir(),
    };
    let config = parse_config(&request.config)?;
    let mut output = Vec::new();
    resolve_with_api(&request, &config, &api, &mut |message| {
        output.push(message);
        Ok(())
    })?;

    assert_eq!(api.calls.into_inner(), vec!["threads get cccccccccccccccc"]);
    assert_eq!(
        pointers(&output),
        vec!["gmail:aaaaaaaaaaaaaaaa", "gmail:bbbbbbbbbbbbbbbb"]
    );
    Ok(())
}

#[test]
fn fetch_rejects_a_message_without_a_timestamp() -> Result<()> {
    let api = FakeGmailApi {
        candidates: vec![json!({"id": "aaaaaaaaaaaaaaaa"})],
        messages: BTreeMap::from([(
            "aaaaaaaaaaaaaaaa".to_owned(),
            json!({"id": "aaaaaaaaaaaaaaaa", "threadId": "cccccccccccccccc"}),
        )]),
        ..FakeGmailApi::default()
    };
    let start = DateTime::parse_from_rfc3339("2026-10-25T17:00:00Z")?.with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc);
    let error = fetch_messages(
        &api,
        start,
        Reach::WindowEnd(end),
        "",
        &excluded_labels(""),
        &mut Vec::new(),
    )
    .err()
    .context("Gmail fetch accepted a message without internalDate")?;

    assert!(format!("{error:#}").contains("internalDate"));
    Ok(())
}

#[test]
fn html_body_is_used_when_plain_text_is_absent() -> Result<()> {
    let record = record_from_message(&json!({
        "id": "aaaaaaaaaaaaaaaa",
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": {
            "mimeType": "text/html",
            "body": {"data": "PHA-SGVsbG8gJmFtcDsgZ29vZGJ5ZTwvcD4"}
        }
    }))?;

    assert!(record.text.contains("Hello & goodbye"));
    Ok(())
}

#[test]
fn a_failure_part_way_through_the_history_leaves_the_cursor_alone() -> Result<()> {
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: vec!["aaaaaaaaaaaaaaaa".to_owned(), "bbbbbbbbbbbbbbbb".to_owned()],
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: BTreeMap::from([
            (
                "aaaaaaaaaaaaaaaa".to_owned(),
                message("aaaaaaaaaaaaaaaa", "1792944000000", "Zmlyc3Q="),
            ),
            (
                // Gmail answered without the field the record's timestamp comes from.
                "bbbbbbbbbbbbbbbb".to_owned(),
                json!({"id": "bbbbbbbbbbbbbbbb", "threadId": "cccccccccccccccc"}),
            ),
        ]),
        ..FakeGmailApi::default()
    };
    let request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    let messages = run_fetch(&api, &request)?;

    // The run stopped inside the window, so it proposes no cursor and the stored one
    // stands: the next fetch reads that history again from where it was.
    assert_eq!(pointers(&messages), [] as [String; 0]);
    assert_eq!(stream_end(&messages), Some((StreamStatus::Error, None)));
    Ok(())
}
