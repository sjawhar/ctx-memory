//! Drive changes feed, cursor and Meet collection tests.

use super::fixtures::*;
#[test]
fn a_file_changed_after_the_window_but_before_the_listing_reaches_the_next_window() -> Result<()> {
    let quarterly = document_at(
        "1QuarterlyPlanExample0000000",
        "Quarterly plan",
        "2026-10-25T17:00:00Z",
    );
    let later = document_at(
        "1LaterEditExample00000000000",
        "Later edit",
        "2026-10-25T18:30:00Z",
    );
    let exports = BTreeMap::from([
        (
            "1QuarterlyPlanExample0000000".to_owned(),
            ("The plan is to ship in March.".to_owned(), false),
        ),
        (
            "1LaterEditExample00000000000".to_owned(),
            ("Edited after the window ended.".to_owned(), false),
        ),
    ]);
    let first = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![("modifiedTime >".to_owned(), vec![quarterly, later.clone()])],
        exports: exports.clone(),
        ..FakeDriveApi::default()
    };
    let messages = run_fetch(&first, &request(json!({"meet": false}), None)?)?;

    // The start page token was read before the listing, so the feed it opens never names a
    // file changed between the window's end and the listing. This window must not carry it,
    // so the cursor names the window's end, carrying the token, and the next run lists from
    // there.
    let stored = records(&messages);
    assert_eq!(
        stored
            .iter()
            .map(|record| (record.pointer.as_str(), record.context))
            .collect::<Vec<_>>(),
        vec![("drive:1QuarterlyPlanExample0000000", false)]
    );
    let cursor =
        stream_cursor(&messages, STREAM_CHANGES).context("the first run proposed a cursor")?;
    assert_eq!(cursor, "2026-10-25T18:00:00Z#token-1");
    // The listing carries no upper bound, or the later file would never be seen at all.
    assert!(
        first
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("files ") && !call.contains("modifiedTime <=")),
        "{:?}",
        first.calls.borrow()
    );

    let next = FakeDriveApi {
        files: vec![(
            "modifiedTime > '2026-10-25T18:00:00Z' and trashed = false".to_owned(),
            vec![later],
        )],
        exports,
        ..FakeDriveApi::default()
    };
    let mut request = request(json!({"meet": false}), Some((STREAM_CHANGES, &cursor)))?;
    request.window_end = DateTime::parse_from_rfc3339("2026-10-25T19:00:00Z")?.with_timezone(&Utc);
    let messages = run_fetch(&next, &request)?;

    // The next window carries the file as a record of its own, and the stream adopts the
    // token the first run took.
    assert_eq!(
        records(&messages)
            .iter()
            .map(|record| (record.pointer.as_str(), record.context))
            .collect::<Vec<_>>(),
        vec![("drive:1LaterEditExample00000000000", false)]
    );
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1@2026-10-25T19:00:00Z".to_owned())
    );
    assert!(
        !next
            .calls
            .borrow()
            .iter()
            .any(|call| call == "start_page_token"),
        "{:?}",
        next.calls.borrow()
    );
    Ok(())
}

#[test]
fn drive_holds_the_meet_cursor_when_the_folder_is_missing() -> Result<()> {
    // No folder answers the name, so nothing of the window was read for this stream.
    let api = FakeDriveApi::default();
    let request = request(
        json!({"changes": false}),
        Some((STREAM_MEET, "2026-10-24T09:00:00Z")),
    )?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 0);
    assert_eq!(
        stream_cursor(&messages, STREAM_MEET),
        Some("2026-10-24T09:00:00Z".to_owned())
    );
    let warnings = warnings(&messages);
    let warning = warnings.first().context("the missing folder warns")?;
    assert!(warning.contains("Google Meet"), "{warning}");
    Ok(())
}

#[test]
fn drive_skips_folders_and_shortcuts() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![
                json!({
                    "id": "1MeetingFolderExample0000000",
                    "name": "Weekly sync - 2026/10/25 09:00 PDT",
                    "mimeType": "application/vnd.google-apps.folder",
                    "modifiedTime": "2026-10-25T17:00:00Z"
                }),
                json!({
                    "id": "1NotesShortcutExample000000",
                    "name": "Weekly sync - Notes by Gemini",
                    "mimeType": "application/vnd.google-apps.shortcut",
                    "modifiedTime": "2026-10-25T17:00:00Z"
                }),
            ],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 0);
    assert_eq!(warnings(&messages), [] as [String; 0]);
    Ok(())
}

#[test]
fn a_meet_document_is_not_repeated_in_the_changes_stream() -> Result<()> {
    let mut notes = document(
        "1WeeklySyncTranscript0000000",
        "Weekly sync - Notes by Gemini",
    );
    if let Some(fields) = notes.as_object_mut() {
        fields.insert(
            "parents".to_owned(),
            json!(["1WeeklySyncMeetingFolder0000"]),
        );
    }
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![notes.clone()],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        files: vec![
            (
                "name = 'Google Meet'".to_owned(),
                vec![json!({
                    "id": "1GoogleMeetFolderExample0000",
                    "name": "Google Meet",
                    "mimeType": "application/vnd.google-apps.folder",
                    "modifiedTime": "2026-10-25T17:30:00Z"
                })],
            ),
            (
                "'1GoogleMeetFolderExample0000' in parents".to_owned(),
                vec![json!({
                    "id": "1WeeklySyncMeetingFolder0000",
                    "name": "Weekly sync - 2026/10/25 09:00 PDT",
                    "mimeType": "application/vnd.google-apps.folder",
                    "modifiedTime": "2026-10-25T17:30:00Z"
                })],
            ),
            (
                "'1WeeklySyncMeetingFolder0000' in parents".to_owned(),
                vec![notes],
            ),
        ],
        exports: BTreeMap::from([(
            "1WeeklySyncTranscript0000000".to_owned(),
            ("Sami: let us ship it.".to_owned(), false),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    let records = records(&messages);
    assert_eq!(records.len(), 1, "{records:#?}");
    let record = records
        .first()
        .context("the Meet document is recorded once")?;
    assert_eq!(record.stream, STREAM_MEET);
    Ok(())
}

#[test]
fn drive_without_a_cursor_takes_a_start_page_token_before_listing() -> Result<()> {
    let api = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![(
            "modifiedTime >".to_owned(),
            vec![document("1QuarterlyPlanExample0000000", "Quarterly plan")],
        )],
        exports: BTreeMap::from([(
            "1QuarterlyPlanExample0000000".to_owned(),
            ("The plan is to ship in March.".to_owned(), false),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), None)?;
    let messages = run_fetch(&api, &request)?;

    let calls = api.calls.borrow().clone();
    assert_eq!(calls.first().map(String::as_str), Some("start_page_token"));
    assert_eq!(records(&messages).len(), 1);
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn drive_holds_the_cursor_for_a_file_newer_than_the_window() -> Result<()> {
    let newer = document_at(
        "1LaterEditExample00000000000",
        "Later edit",
        "2026-10-25T18:30:00Z",
    );
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![newer],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 0);
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1".to_owned())
    );
    Ok(())
}

/// A modified time this far ahead can only come from a wrong clock: nothing changed it yet.
const FUTURE: &str = "2100-01-01T00:00:00Z";

#[test]
fn a_file_dated_in_the_future_is_stored_and_does_not_hold_the_cursor() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![
                document_at("1SkewedClockExample000000000", "Skewed clock", FUTURE),
                document("1QuarterlyPlanExample0000000", "Quarterly plan"),
            ],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        exports: BTreeMap::from([
            (
                "1SkewedClockExample000000000".to_owned(),
                ("Dated by a wrong clock.".to_owned(), false),
            ),
            (
                "1QuarterlyPlanExample0000000".to_owned(),
                ("The plan is to ship in March.".to_owned(), false),
            ),
        ]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    // Waiting for the clock to pass the date would hold every later change behind it. The
    // window stores the file at its end, keeping its own date in the raw data.
    let records = records(&messages);
    let skewed = records
        .iter()
        .find(|record| record.pointer == "drive:1SkewedClockExample000000000")
        .context("the future-dated file is stored")?;
    assert_eq!(skewed.ts, request.window_end);
    assert!(!skewed.context);
    assert_eq!(
        skewed.raw.get("modifiedTime").and_then(Value::as_str),
        Some(FUTURE)
    );
    assert!(records
        .iter()
        .any(|record| record.pointer == "drive:1QuarterlyPlanExample0000000"));
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-2@2026-10-25T18:00:00Z".to_owned())
    );
    assert!(
        warnings(&messages).iter().any(|warning| warning
            .contains("drive:1SkewedClockExample000000000")
            && warning.contains(FUTURE)),
        "{:?}",
        warnings(&messages)
    );
    Ok(())
}

#[test]
fn a_file_changed_minutes_after_a_window_that_ends_now_still_holds_the_cursor() -> Result<()> {
    // The run reads the file moments after the window ends: its date is believable, so it
    // belongs to the next window and the cursor holds.
    let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).context("the time now")?;
    let later = (now + chrono::Duration::minutes(5)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![document_at(
                "1LaterEditExample00000000000",
                "Later edit",
                &later,
            )],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        ..FakeDriveApi::default()
    };
    let mut request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    request.window_end = now;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 0);
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1".to_owned())
    );
    Ok(())
}

#[test]
fn a_listing_stores_a_file_dated_in_the_future_and_names_the_token() -> Result<()> {
    let api = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![(
            "modifiedTime >".to_owned(),
            vec![document_at(
                "1SkewedClockExample000000000",
                "Skewed clock",
                FUTURE,
            )],
        )],
        exports: BTreeMap::from([(
            "1SkewedClockExample000000000".to_owned(),
            ("Dated by a wrong clock.".to_owned(), false),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), None)?;
    let messages = run_fetch(&api, &request)?;

    // A file whose date no clock has reached yet is not a change after the window: holding
    // the cursor at the window end for it would list it again on every run until 2100.
    assert_eq!(
        records(&messages)
            .iter()
            .map(|record| (record.pointer.as_str(), record.ts, record.context))
            .collect::<Vec<_>>(),
        vec![(
            "drive:1SkewedClockExample000000000",
            request.window_end,
            false
        )]
    );
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn a_run_that_stops_inside_the_feed_keeps_the_time_its_cursor_carried() -> Result<()> {
    // The feed is not read to its end, so Drive is stored no further than before.
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![document_at(
                "1LaterEditExample00000000000",
                "Later edit",
                "2026-10-25T18:30:00Z",
            )],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        ..FakeDriveApi::default()
    };
    let request = request(
        json!({"meet": false}),
        Some((STREAM_CHANGES, "token-1@2026-10-24T18:00:00Z")),
    )?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1@2026-10-24T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn a_refused_changes_token_lists_again_from_the_stored_time_under_a_new_token() -> Result<()> {
    let api = FakeDriveApi {
        refused_tokens: BTreeSet::from(["token-9".to_owned()]),
        start_page_token: "token-10".to_owned(),
        files: vec![(
            "modifiedTime > '2026-10-24T00:00:00Z' and trashed = false".to_owned(),
            vec![pdf(0, "2026-10-24T12:00:00Z")],
        )],
        ..FakeDriveApi::default()
    };
    let request = request(
        json!({"meet": false}),
        Some((STREAM_CHANGES, "token-9@2026-10-24T00:00:00Z")),
    )?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 1);
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::StreamEnd { stream, status: StreamStatus::Ok, .. } if stream == STREAM_CHANGES
    )));
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-10@2026-10-25T18:00:00Z".to_owned())
    );
    assert!(
        warnings(&messages)
            .iter()
            .any(|warning| warning.contains("token-9") && warning.contains("2026-10-24T00:00:00Z")),
        "{:?}",
        warnings(&messages)
    );
    // The new token is taken before the listing it follows.
    assert_eq!(
        api.calls.borrow().get(..3),
        Some(
            &[
                "changes token-9".to_owned(),
                "start_page_token".to_owned(),
                "files modifiedTime > '2026-10-24T00:00:00Z' and trashed = false".to_owned(),
            ][..]
        )
    );
    Ok(())
}

#[test]
fn a_refused_token_with_no_stored_time_stops_the_stream() -> Result<()> {
    // A bare token, as an earlier build wrote, says nothing of how far back a listing must
    // reach, so the stream stops rather than step over the gap.
    let api = FakeDriveApi {
        refused_tokens: BTreeSet::from(["token-9".to_owned()]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-9")))?;
    let messages = run_fetch(&api, &request)?;

    assert!(messages.iter().any(|message| matches!(
        message,
        Message::StreamEnd { stream, status: StreamStatus::Error, cursor: None, .. }
            if stream == STREAM_CHANGES
    )));
    Ok(())
}

fn pdf(index: usize, modified: &str) -> Value {
    json!({
        "id": format!("1ChangedPdfExample{index:010}"),
        "name": format!("Changed {index}.pdf"),
        "mimeType": "application/pdf",
        "modifiedTime": modified
    })
}

#[test]
fn a_long_changes_feed_is_read_a_bounded_number_of_files_at_a_time() -> Result<()> {
    // Six pages of a hundred changed files each: the run reads pages until it has read
    // `MAX_CHANGED_FILES`, and names the page after the last one it read.
    let pages: BTreeMap<String, Changes> = (0..6)
        .map(|page| {
            let token = format!("page-{page}");
            let files = (0..100)
                .map(|file| pdf(page * 100 + file, "2026-10-25T17:00:00Z"))
                .collect();
            let changes = if page < 5 {
                Changes {
                    files,
                    next_page_token: format!("page-{}", page + 1),
                    more: true,
                }
            } else {
                Changes {
                    files,
                    next_page_token: "token-after".to_owned(),
                    more: false,
                }
            };
            (token, changes)
        })
        .collect();
    let api = FakeDriveApi {
        pages,
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "page-0")))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), MAX_CHANGED_FILES);
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some(format!("page-{}", MAX_CHANGED_FILES / 100))
    );
    assert!(!api
        .calls
        .borrow()
        .contains(&format!("changes page-{}", MAX_CHANGED_FILES / 100)));
    Ok(())
}

#[test]
fn a_file_changed_after_the_window_holds_the_cursor_at_its_own_page() -> Result<()> {
    let api = FakeDriveApi {
        pages: BTreeMap::from([
            (
                "page-0".to_owned(),
                Changes {
                    files: vec![pdf(0, "2026-10-25T17:00:00Z")],
                    next_page_token: "page-1".to_owned(),
                    more: true,
                },
            ),
            (
                "page-1".to_owned(),
                Changes {
                    files: vec![
                        pdf(1, "2026-10-25T17:10:00Z"),
                        pdf(2, "2026-10-25T18:30:00Z"),
                    ],
                    next_page_token: "token-after".to_owned(),
                    more: false,
                },
            ),
        ]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "page-0")))?;
    let messages = run_fetch(&api, &request)?;

    // The first page is stored, so the next run starts at the page holding the later file.
    let pointers: Vec<String> = records(&messages)
        .into_iter()
        .map(|record| record.pointer)
        .collect();
    assert!(pointers.contains(&"drive:1ChangedPdfExample0000000000".to_owned()));
    assert!(!pointers.contains(&"drive:1ChangedPdfExample0000000002".to_owned()));
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("page-1".to_owned())
    );
    Ok(())
}

#[test]
fn a_first_fetch_with_a_long_backfill_lists_one_slice_at_a_time() -> Result<()> {
    let api = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![(
            "modifiedTime >".to_owned(),
            vec![pdf(0, "2026-09-28T12:00:00Z")],
        )],
        ..FakeDriveApi::default()
    };
    // Thirty days of backfill: the first run lists the oldest seven.
    let mut request = request(json!({"meet": false}), None)?;
    request.backfill_hours = 30 * 24;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 1);
    // The cursor names the time listed through and carries the changes token, which the
    // run took before it listed the first slice.
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("2026-10-02T18:00:00Z#token-1".to_owned())
    );
    let calls = api.calls.borrow();
    assert_eq!(
        calls.first().map(String::as_str),
        Some("start_page_token"),
        "{calls:?}"
    );
    assert!(
            calls.iter().any(|call| call
                == "files modifiedTime > '2026-09-25T18:00:00Z' and modifiedTime <= '2026-10-02T18:00:00Z' and trashed = false"),
            "{calls:?}"
        );
    Ok(())
}

/// Runs one fetch of the changes stream at `window_end` from `cursor`, returning its
/// records' pointers and the cursor it proposed.
fn run_changes(
    api: &FakeDriveApi,
    cursor: Option<&str>,
    window_end: &str,
    backfill_hours: u32,
) -> Result<(Vec<String>, String)> {
    let mut request = request(
        json!({"meet": false}),
        cursor.map(|cursor| (STREAM_CHANGES, cursor)),
    )?;
    request.window_end = DateTime::parse_from_rfc3339(window_end)?.with_timezone(&Utc);
    request.backfill_hours = backfill_hours;
    let messages = run_fetch(api, &request)?;
    let cursor = stream_cursor(&messages, STREAM_CHANGES)
        .with_context(|| format!("the run proposed no cursor: {messages:?}"))?;
    let pointers = records(&messages)
        .into_iter()
        .map(|record| record.pointer)
        .collect();
    Ok((pointers, cursor))
}

#[test]
fn a_file_uploaded_during_a_sliced_catch_up_with_an_old_time_is_read_by_the_next_run() -> Result<()>
{
    // Ten days of backfill: the first run lists the oldest seven days.
    let first = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![(
            "modifiedTime > '2026-10-15T18:00:00Z' and modifiedTime <= '2026-10-22T18:00:00Z'"
                .to_owned(),
            vec![pdf(1, "2026-10-16T12:00:00Z")],
        )],
        ..FakeDriveApi::default()
    };
    let (_, cursor) = run_changes(&first, None, "2026-10-25T18:00:00Z", 10 * 24)?;

    // Then a file is uploaded that keeps its old modified time, behind the slice the next
    // run lists. Drive's changes feed names it after the first token, and a start token
    // taken now opens the feed after it.
    let uploaded = pdf(2, "2026-10-20T09:00:00Z");
    let last = FakeDriveApi {
        start_page_token: "token-2".to_owned(),
        files: vec![(
            "modifiedTime > '2026-10-22T18:00:00Z'".to_owned(),
            vec![pdf(3, "2026-10-24T12:00:00Z")],
        )],
        ..FakeDriveApi::default()
    };
    let (listed, cursor) = run_changes(&last, Some(&cursor), "2026-10-25T19:00:00Z", 10 * 24)?;
    assert_eq!(listed, vec!["drive:1ChangedPdfExample0000000003"]);

    let feed = FakeDriveApi {
        pages: BTreeMap::from([
            (
                "token-1".to_owned(),
                Changes {
                    files: vec![uploaded, pdf(3, "2026-10-24T12:00:00Z")],
                    next_page_token: "token-3".to_owned(),
                    more: false,
                },
            ),
            (
                "token-2".to_owned(),
                Changes {
                    files: Vec::new(),
                    next_page_token: "token-3".to_owned(),
                    more: false,
                },
            ),
        ]),
        ..FakeDriveApi::default()
    };
    let (followed, cursor) = run_changes(&feed, Some(&cursor), "2026-10-25T20:00:00Z", 10 * 24)?;
    assert!(
        followed.contains(&"drive:1ChangedPdfExample0000000002".to_owned()),
        "the uploaded file was never read: {followed:?}"
    );
    assert_eq!(cursor, "token-3@2026-10-25T20:00:00Z");
    Ok(())
}

#[test]
fn a_sliced_cursor_from_an_earlier_build_takes_a_token_before_it_lists() -> Result<()> {
    // A bare time is what the previous build wrote between slices: the run reads it as the
    // time listed through and takes the token it will carry.
    let api = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        ..FakeDriveApi::default()
    };
    let (_, cursor) = run_changes(
        &api,
        Some("2026-10-01T00:00:00Z"),
        "2026-10-25T18:00:00Z",
        24,
    )?;

    assert_eq!(cursor, "2026-10-08T00:00:00Z#token-1");
    assert_eq!(
        api.calls.borrow().first().map(String::as_str),
        Some("start_page_token")
    );
    Ok(())
}

#[test]
fn a_time_cursor_within_a_slice_of_the_window_end_takes_a_changes_token() -> Result<()> {
    let api = FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![(
            "modifiedTime >".to_owned(),
            vec![pdf(0, "2026-10-24T12:00:00Z")],
        )],
        ..FakeDriveApi::default()
    };
    let request = request(
        json!({"meet": false}),
        Some((STREAM_CHANGES, "2026-10-22T00:00:00Z")),
    )?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(records(&messages).len(), 1);
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-1@2026-10-25T18:00:00Z".to_owned())
    );
    let calls = api.calls.borrow();
    assert_eq!(
        calls.first().map(String::as_str),
        Some("start_page_token"),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == "files modifiedTime > '2026-10-22T00:00:00Z' and trashed = false"),
        "{calls:?}"
    );
    Ok(())
}

#[test]
fn a_long_gap_in_the_meet_folder_is_read_one_slice_at_a_time() -> Result<()> {
    let api = FakeDriveApi {
        files: vec![(
            "name = 'Google Meet'".to_owned(),
            vec![json!({
                "id": "1GoogleMeetFolderExample0000",
                "name": "Google Meet",
                "mimeType": "application/vnd.google-apps.folder",
                "modifiedTime": "2026-10-25T17:30:00Z"
            })],
        )],
        ..FakeDriveApi::default()
    };
    let request = request(
        json!({"changes": false}),
        Some((STREAM_MEET, "2026-10-05T18:00:00Z")),
    )?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(
        stream_cursor(&messages, STREAM_MEET),
        Some("2026-10-12T18:00:00Z".to_owned())
    );
    assert!(
            api.calls.borrow().iter().any(|call| call.ends_with(
                "modifiedTime > '2026-10-05T18:00:00Z' and modifiedTime <= '2026-10-12T18:00:00Z' and trashed = false"
            )),
            "{:?}",
            api.calls.borrow()
        );
    Ok(())
}

#[test]
fn a_failure_part_way_through_the_changes_leaves_the_cursor_alone() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![
                document("1QuarterlyPlanExample0000000", "Quarterly plan"),
                // Drive answered without the field the record's timestamp comes from.
                json!({
                    "id": "1MalformedExample00000000000",
                    "name": "Malformed",
                    "mimeType": "application/vnd.google-apps.document"
                }),
            ],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        exports: BTreeMap::from([(
            "1QuarterlyPlanExample0000000".to_owned(),
            ("The plan is to ship in March.".to_owned(), false),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    // The run stopped inside the feed, so it proposes no cursor and the stored page token
    // stands: the next fetch reads that page again.
    assert_eq!(records(&messages).len(), 0);
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::StreamEnd {
                stream,
                status: StreamStatus::Error,
                cursor: None,
                ..
            } if stream == STREAM_CHANGES
        )),
        "{messages:?}"
    );
    Ok(())
}
