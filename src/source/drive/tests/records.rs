//! Drive rendering and export tests.

use super::fixtures::*;
#[test]
fn drive_exports_doc_text() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![document("1QuarterlyPlanExample0000000", "Quarterly plan")],
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

    let verbatim = messages.iter().find_map(|message| match message {
        Message::Hello { verbatim, .. } => Some(*verbatim),
        _ => None,
    });
    assert_eq!(verbatim, Some(true));
    let records = records(&messages);
    let record = records.first().context("the change produced a record")?;
    assert_eq!(record.text, "The plan is to ship in March.");
    assert_eq!(record.pointer, "drive:1QuarterlyPlanExample0000000");
    assert_eq!(record.title.as_deref(), Some("Quarterly plan"));
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-2@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn drive_caps_export_at_5mb_and_warns() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![document("1BigSheetExample000000000000", "Big sheet")],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        exports: BTreeMap::from([(
            "1BigSheetExample000000000000".to_owned(),
            ("x".repeat(64), true),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    assert_eq!(
        api.export_limits.borrow().first().copied(),
        Some(5 * 1024 * 1024)
    );
    let warnings = warnings(&messages);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = warnings.first().context("the capped export warns")?;
    assert!(
        warning.contains("1BigSheetExample000000000000"),
        "{warning}"
    );
    assert!(warning.contains("5242880"), "{warning}");
    Ok(())
}

#[test]
fn drive_skips_non_google_docs_with_warning() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![json!({
                "id": "1ContractPdfExample000000000",
                "name": "Contract.pdf",
                "mimeType": "application/pdf",
                "modifiedTime": "2026-10-25T17:00:00Z"
            })],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    assert!(
        !api.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("export")),
        "{:?}",
        api.calls.borrow()
    );
    let warnings = warnings(&messages);
    let warning = warnings.first().context("a non-document warns")?;
    assert!(warning.contains("Contract.pdf"), "{warning}");
    assert!(warning.contains("application/pdf"), "{warning}");
    let records = records(&messages);
    let record = records
        .first()
        .context("a non-document is still recorded")?;
    assert_eq!(record.title.as_deref(), Some("Contract.pdf"));
    assert!(record.text.contains("application/pdf"), "{}", record.text);
    Ok(())
}

#[test]
fn a_document_drive_will_not_export_is_stored_as_metadata_and_moves_the_cursor() -> Result<()> {
    let api = FakeDriveApi {
        changes: Changes {
            files: vec![
                document("1RestrictedPlanExample000000", "Restricted plan"),
                document("1QuarterlyPlanExample0000000", "Quarterly plan"),
            ],
            next_page_token: "token-2".to_owned(),
            ..Changes::default()
        },
        exports: BTreeMap::from([(
            "1QuarterlyPlanExample0000000".to_owned(),
            ("The plan is to ship in March.".to_owned(), false),
        )]),
        refused: BTreeMap::from([("1RestrictedPlanExample000000".to_owned(), 403)]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    let records = records(&messages);
    let restricted = records
        .iter()
        .find(|record| record.pointer == "drive:1RestrictedPlanExample000000")
        .context("the refused document is still recorded")?;
    assert_eq!(
        restricted.text,
        "Restricted plan (application/vnd.google-apps.document)"
    );
    assert!(records
        .iter()
        .any(|record| record.text == "The plan is to ship in March."));
    assert!(
        warnings(&messages).iter().any(|warning| {
            warning.contains("drive:1RestrictedPlanExample000000") && warning.contains("403")
        }),
        "{:?}",
        warnings(&messages)
    );
    assert_eq!(
        stream_cursor(&messages, STREAM_CHANGES),
        Some("token-2@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn an_export_refused_for_the_caller_stops_the_stream() -> Result<()> {
    // A 401 or a spent daily limit names the caller, not the file: storing every document
    // as its name would hide that, so the stream errors and its cursor stays where it was.
    let api = FakeDriveApi {
            changes: Changes {
                files: vec![document("1QuarterlyPlanExample0000000", "Quarterly plan")],
                next_page_token: "token-2".to_owned(),
                ..Changes::default()
            },
            failed: BTreeMap::from([(
                "1QuarterlyPlanExample0000000".to_owned(),
                "Google drive/v3/files/1QuarterlyPlanExample0000000/export returned 403: Daily Limit Exceeded"
                    .to_owned(),
            )]),
            ..FakeDriveApi::default()
        };
    let request = request(json!({"meet": false}), Some((STREAM_CHANGES, "token-1")))?;
    let messages = run_fetch(&api, &request)?;

    assert!(records(&messages).is_empty());
    let error = messages.iter().find_map(|message| match message {
        Message::StreamEnd {
            stream,
            status: StreamStatus::Error,
            cursor: None,
            error,
        } if stream == STREAM_CHANGES => error.clone(),
        _ => None,
    });
    // The error names the file the run stopped at.
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("drive:1QuarterlyPlanExample0000000")
                && error.contains("Daily Limit Exceeded")),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn meet_transcript_goes_to_drive_meet_stream() -> Result<()> {
    let api = FakeDriveApi {
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
                vec![document(
                    "1WeeklySyncTranscript0000000",
                    "Weekly sync - 2026/10/25 09:00 PDT - Notes by Gemini",
                )],
            ),
        ],
        exports: BTreeMap::from([(
            "1WeeklySyncTranscript0000000".to_owned(),
            ("Sami: let us ship it.".to_owned(), false),
        )]),
        ..FakeDriveApi::default()
    };
    let request = request(json!({"changes": false}), None)?;
    let messages = run_fetch(&api, &request)?;

    let records = records(&messages);
    let record = records
        .first()
        .context("the Meet folder produced a record")?;
    assert_eq!(record.stream, STREAM_MEET);
    assert_eq!(record.pointer, "drive:1WeeklySyncTranscript0000000");
    assert_eq!(record.text, "Sami: let us ship it.");
    assert_eq!(
        stream_cursor(&messages, STREAM_MEET),
        Some("2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}
