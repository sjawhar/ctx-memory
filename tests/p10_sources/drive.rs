//! The Drive source: start tokens, the changes feed, relistings, export refusals and verbatim
//! text.

use anyhow::{Context as _, Result};
use ctx_memory::source::{self, Request, SourceOutcome};
use serde_json::{json, Value};

use crate::fixtures::{
    fetch_from, fetch_request, remove_scratch, run_conformance, scratch, source_spec, timestamp,
};
use crate::support::fake_google::{clear_google_environment, environment_lock, FakeGoogle};

#[test]
fn drive_first_fetch_takes_a_start_page_token_and_exports_text() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-first-fetch")?;
    let server = FakeGoogle::start(vec![
        (
            "/drive/v3/changes/startPageToken",
            json!({"startPageToken": "token-1"}),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            json!("The plan is to ship in March."),
        ),
        (
            "/drive/v3/files",
            json!({"files": [{
                "id": "1QuarterlyPlanExample0000000",
                "name": "Quarterly plan",
                "mimeType": "application/vnd.google-apps.document",
                "modifiedTime": "2026-10-25T17:00:00Z"
            }]}),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");

    let spec = source_spec("drive", "meet = false\n")?;
    let request = fetch_request(&root, "drive", "drive", json!({ "meet": false }))?;
    let source = source::open(&spec, "drive")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let requests = server.requests()?;

    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    assert!(
        requests
            .iter()
            .any(|request| request.contains("/drive/v3/changes/startPageToken")),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("mimeType=text%2Fplain")),
        "{requests:?}"
    );
    // The token was taken before the listing, so the listing carries no upper bound: a file
    // changed between the window's end and that read is in neither bound otherwise.
    let listing = requests
        .iter()
        .find(|request| request.contains("/drive/v3/files?"))
        .context("the first fetch listed files")?;
    assert!(listing.contains("modifiedTime+%3E"), "{listing}");
    assert!(!listing.contains("%3C"), "{listing}");
    let record = outcome
        .records
        .first()
        .context("the fetch stored a record")?;
    assert_eq!(record.pointer, "drive:1QuarterlyPlanExample0000000");
    // The export route answers with a JSON string, so its bytes carry the quotes.
    assert!(
        record.text.contains("The plan is to ship in March."),
        "{}",
        record.text
    );
    assert_eq!(
        outcome
            .streams
            .get("drive")
            .and_then(|stream| stream.cursor.clone()),
        Some("token-1@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

/// A Drive error reply as Google writes it, with its reason beside the message.
fn drive_error(status: u16, reason: &str, message: &str) -> String {
    json!({"error": {
        "code": status,
        "errors": [{"domain": "global", "reason": reason, "message": message}],
        "message": message
    }})
    .to_string()
}

/// A Google document as a Drive listing or the changes feed names it.
fn google_document(id: &str, name: &str) -> Value {
    google_document_at(id, name, "2026-10-25T17:00:00Z")
}

/// A Google document modified at `modified`, as a Drive listing names it.
fn google_document_at(id: &str, name: &str, modified: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "application/vnd.google-apps.document",
        "modifiedTime": modified
    })
}

/// Export refusals that are about the file itself: the document, its name, its export route,
/// and Drive's reason and message.
const FILE_REFUSALS: [(&str, &str, &str, &str, &str); 4] = [
    (
        "1RestrictedPlanExample000000",
        "Restricted plan",
        "/drive/v3/files/1RestrictedPlanExample000000/export",
        "cannotExportFile",
        "This file cannot be exported by the user.",
    ),
    (
        "1HugeSheetExample00000000000",
        "Huge sheet",
        "/drive/v3/files/1HugeSheetExample00000000000/export",
        "exportSizeLimitExceeded",
        "This file is too large to be exported.",
    ),
    (
        "1UnreadableNotesExample00000",
        "Unreadable notes",
        "/drive/v3/files/1UnreadableNotesExample00000/export",
        "insufficientFilePermissions",
        "The user does not have sufficient permissions for file 1UnreadableNotesExample00000.",
    ),
    (
        // Drive's live answer on 2026-10-28 to an export of a file that is not a Docs Editors
        // file.
        "1VidsExplainerExample0000000",
        "Vids explainer",
        "/drive/v3/files/1VidsExplainerExample0000000/export",
        "fileNotExportable",
        "Export only supports Docs Editors files.",
    ),
];

#[test]
fn a_drive_file_whose_export_is_refused_is_stored_as_metadata() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-refused-export")?;
    // Each refusal is about the file itself. The deleted document's export has no route, so
    // this server answers it 404 `notFound`, as Drive does for a file deleted after the listing
    // named it.
    let refused = FILE_REFUSALS;
    let mut routes = vec![
        (
            "/drive/v3/changes/startPageToken",
            200,
            json!({"startPageToken": "token-1"}).to_string(),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            200,
            json!("The plan is to ship in March.").to_string(),
        ),
    ];
    let mut listed = vec![
        google_document("1QuarterlyPlanExample0000000", "Quarterly plan"),
        google_document("1DeletedNotesExample00000000", "Deleted notes"),
    ];
    for (id, name, route, reason, message) in refused {
        routes.push((route, 403, drive_error(403, reason, message)));
        listed.push(google_document(id, name));
    }
    routes.push((
        "/drive/v3/files?",
        200,
        json!({"files": listed}).to_string(),
    ));
    let server = FakeGoogle::start_answering(routes)?;
    server.use_in_environment("ya29.drive-test");

    let spec = source_spec("drive", "meet = false\n")?;
    let request = fetch_request(&root, "drive", "drive", json!({ "meet": false }))?;
    let source = source::open(&spec, "drive")?;
    let outcome = run_conformance(source.as_ref(), &request);

    clear_google_environment();
    remove_scratch(&root)?;

    // A file Drive will not export does not stop the stream.
    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    let stored = |id: &str| {
        outcome
            .records
            .iter()
            .find(|record| record.pointer == format!("drive:{id}"))
            .map(|record| record.text.clone())
    };
    let warned = |id: &str, status: &str| {
        outcome.warnings.iter().any(|warning| {
            warning.message.contains(&format!("drive:{id}")) && warning.message.contains(status)
        })
    };
    assert_eq!(
        stored("1DeletedNotesExample00000000").as_deref(),
        Some("Deleted notes (application/vnd.google-apps.document)")
    );
    assert!(
        warned("1DeletedNotesExample00000000", "404"),
        "{:?}",
        outcome.warnings
    );
    for (id, name, _, reason, _) in refused {
        assert_eq!(
            stored(id),
            Some(format!("{name} (application/vnd.google-apps.document)")),
            "{reason}"
        );
        assert!(warned(id, "403"), "{reason}: {:?}", outcome.warnings);
    }
    assert!(stored("1QuarterlyPlanExample0000000")
        .is_some_and(|text| text.contains("The plan is to ship in March.")));
    assert_eq!(
        outcome
            .streams
            .get("drive")
            .and_then(|stream| stream.cursor.clone()),
        Some("token-1@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn a_drive_export_refused_for_the_caller_ends_the_stream_and_holds_its_cursor() -> Result<()> {
    let _environment = environment_lock();
    // Each refusal says nothing about the file: the project's daily limit is spent, the token
    // lacks the Drive scope, or a proxy answered with its own page. Storing the file as its name
    // would move the cursor past text a later run can export.
    for (case, body) in [
        (
            "dailyLimitExceeded",
            drive_error(403, "dailyLimitExceeded", "Daily Limit Exceeded"),
        ),
        (
            "insufficientPermissions",
            drive_error(
                403,
                "insufficientPermissions",
                "Request had insufficient authentication scopes.",
            ),
        ),
        (
            "a proxy's page",
            "<html><body><h1>403 Forbidden</h1></body></html>".to_owned(),
        ),
    ] {
        let root = scratch("drive-caller-refusal")?;
        let server = FakeGoogle::start_answering(vec![
            (
                "/drive/v3/changes?pageToken=token-1&",
                200,
                json!({
                    "changes": [{"file": google_document("1QuarterlyPlanExample0000000", "Quarterly plan")}],
                    "newStartPageToken": "token-2"
                })
                .to_string(),
            ),
            (
                "/drive/v3/files/1QuarterlyPlanExample0000000/export",
                403,
                body,
            ),
        ])?;
        server.use_in_environment("ya29.drive-test");

        let spec = source_spec("drive", "meet = false\n")?;
        let Request::Fetch(mut fetch) =
            fetch_request(&root, "drive", "drive", json!({ "meet": false }))?
        else {
            anyhow::bail!("fetch_request builds a fetch");
        };
        fetch
            .cursors
            .insert("drive".to_owned(), "token-1".to_owned());
        let request = Request::Fetch(fetch);
        let source = source::open(&spec, "drive")?;
        let outcome = run_conformance(source.as_ref(), &request);

        clear_google_environment();
        remove_scratch(&root)?;

        assert!(outcome.records.is_empty(), "{case}: {:?}", outcome.records);
        let stream = outcome
            .streams
            .get("drive")
            .with_context(|| format!("{case}: the drive stream ended"))?;
        assert_eq!(stream.cursor, None, "{case}");
        assert!(
            stream
                .error
                .as_deref()
                .is_some_and(|error| error.contains("403")
                    && error.contains("drive:1QuarterlyPlanExample0000000")),
            "{case}: {stream:?}"
        );
    }
    Ok(())
}

#[test]
fn a_drive_changes_token_drive_refuses_is_listed_again_from_the_stored_time() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-refused-token")?;
    let refused = refused_page_token();
    let server = FakeGoogle::start_answering(vec![
        ("/drive/v3/changes?pageToken=token-9", 400, refused),
        (
            "/drive/v3/changes/startPageToken",
            200,
            json!({"startPageToken": "token-10"}).to_string(),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            200,
            json!("The plan is to ship in March.").to_string(),
        ),
        (
            "/drive/v3/files?",
            200,
            json!({"files": [google_document("1QuarterlyPlanExample0000000", "Quarterly plan")]})
                .to_string(),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");

    let spec = source_spec("drive", "meet = false\n")?;
    let request = fetch_from(
        &root,
        "drive",
        json!({ "meet": false }),
        &[("drive", "token-9@2026-10-24T00:00:00Z")],
        "2026-10-25T18:00:00Z",
        24,
    )?;
    let source = source::open(&spec, "drive")?;
    let outcome = run_conformance(source.as_ref(), &request);
    let requests = server.requests()?;

    clear_google_environment();
    remove_scratch(&root)?;

    // The stream recovers on its own: it lists from the time the cursor carries, under a token
    // taken before that listing.
    assert!(!outcome.has_errors(), "{:#?}", outcome.streams);
    assert!(outcome
        .records
        .iter()
        .any(|record| record.pointer == "drive:1QuarterlyPlanExample0000000"));
    assert_eq!(
        outcome
            .streams
            .get("drive")
            .and_then(|stream| stream.cursor.clone()),
        Some("token-10@2026-10-25T18:00:00Z".to_owned())
    );
    let listing = requests
        .iter()
        .position(|request| request.starts_with("GET /drive/v3/files?"))
        .context("the stream listed Drive")?;
    let token = requests
        .iter()
        .position(|request| request.contains("/drive/v3/changes/startPageToken"))
        .context("the stream took a new token")?;
    assert!(token < listing, "{requests:?}");
    assert!(
        requests
            .get(listing)
            .is_some_and(|request| request.contains("2026-10-24T00%3A00%3A00Z")),
        "{requests:?}"
    );
    Ok(())
}

/// Drive's answer to a changes token it does not accept, as it gave on 2026-10-28 for a
/// malformed one.
fn refused_page_token() -> String {
    json!({"error": {
        "code": 400,
        "message": "Invalid Value",
        "errors": [{
            "message": "Invalid Value",
            "domain": "global",
            "reason": "invalid",
            "location": "pageToken",
            "locationType": "parameter"
        }]
    }})
    .to_string()
}

/// The cursor a fetch proposed for the `drive` stream.
fn drive_cursor(outcome: &SourceOutcome) -> Option<String> {
    outcome
        .streams
        .get("drive")
        .and_then(|stream| stream.cursor.clone())
}

/// Each record a fetch stored, with whether it is context.
fn carried(outcome: &SourceOutcome) -> Vec<(String, bool)> {
    outcome
        .records
        .iter()
        .map(|record| (record.pointer.clone(), record.context))
        .collect()
}

/// A relisting after a refused token takes its token before it lists, so the feed that token
/// opens never names a file changed between the window's end and the listing. Such a file must
/// reach a later window as a record of its own, not as context that no delta shows.
#[test]
fn a_drive_file_changed_after_the_window_during_a_relisting_reaches_the_next_window() -> Result<()>
{
    let _environment = environment_lock();
    let root = scratch("drive-relisting-after-window")?;
    let spec = source_spec("drive", "meet = false\n")?;
    let later = google_document_at(
        "1LaterEditExample00000000000",
        "Later edit",
        "2026-10-25T18:30:00Z",
    );
    let export = (
        "/drive/v3/files/1LaterEditExample00000000000/export",
        200,
        json!("Edited after the window ended.").to_string(),
    );

    let server = FakeGoogle::start_answering(vec![
        (
            "/drive/v3/changes?pageToken=token-9",
            400,
            refused_page_token(),
        ),
        (
            "/drive/v3/changes/startPageToken",
            200,
            json!({"startPageToken": "token-10"}).to_string(),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            200,
            json!("The plan is to ship in March.").to_string(),
        ),
        export.clone(),
        (
            "/drive/v3/files?",
            200,
            json!({"files": [
                google_document("1QuarterlyPlanExample0000000", "Quarterly plan"),
                later.clone()
            ]})
            .to_string(),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");
    let request = fetch_from(
        &root,
        "drive",
        json!({ "meet": false }),
        &[("drive", "token-9@2026-10-24T00:00:00Z")],
        "2026-10-25T18:00:00Z",
        24,
    )?;
    let first = run_conformance(source::open(&spec, "drive")?.as_ref(), &request);
    clear_google_environment();

    let server = FakeGoogle::start_answering(vec![
        export,
        (
            "/drive/v3/files?",
            200,
            json!({"files": [later]}).to_string(),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");
    let first_cursor = drive_cursor(&first).unwrap_or_default();
    let request = fetch_from(
        &root,
        "drive",
        json!({ "meet": false }),
        &[("drive", &first_cursor)],
        "2026-10-25T19:00:00Z",
        24,
    )?;
    let next = run_conformance(source::open(&spec, "drive")?.as_ref(), &request);
    let requests = server.requests()?;
    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!first.has_errors(), "{:#?}", first.streams);
    assert_eq!(
        carried(&first),
        vec![("drive:1QuarterlyPlanExample0000000".to_owned(), false)]
    );
    // The cursor holds at the window end and carries the token taken before the listing.
    assert_eq!(first_cursor, "2026-10-25T18:00:00Z#token-10");

    assert!(!next.has_errors(), "{:#?}", next.streams);
    assert_eq!(
        carried(&next),
        vec![("drive:1LaterEditExample00000000000".to_owned(), false)]
    );
    assert_eq!(
        drive_cursor(&next),
        Some("token-10@2026-10-25T19:00:00Z".to_owned())
    );
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /drive/v3/files?")
                && request.contains("2026-10-25T18%3A00%3A00Z")),
        "{requests:?}"
    );
    Ok(())
}

/// A file or a message dated 2100-01-01: a date this far ahead can only come from a wrong clock,
/// and waiting for the clock to pass it would hold every later change behind it. The window
/// stores it at its end, which the host accepts, and the cursor moves on.
#[test]
fn a_drive_file_dated_in_the_future_is_stored_and_the_cursor_moves() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-future-dated")?;
    let server = FakeGoogle::start(vec![
        (
            "/drive/v3/changes?pageToken=token-1&",
            json!({
                "changes": [
                    {"file": google_document_at("1SkewedClockExample000000000", "Skewed clock", "2100-01-01T00:00:00Z")},
                    {"file": google_document("1QuarterlyPlanExample0000000", "Quarterly plan")}
                ],
                "newStartPageToken": "token-2"
            }),
        ),
        (
            "/drive/v3/files/1SkewedClockExample000000000/export",
            json!("Dated by a wrong clock."),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            json!("The plan is to ship in March."),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");

    let spec = source_spec("drive", "meet = false\n")?;
    let request = fetch_from(
        &root,
        "drive",
        json!({ "meet": false }),
        &[("drive", "token-1@2026-10-24T00:00:00Z")],
        "2026-10-25T18:00:00Z",
        24,
    )?;
    let outcome = run_conformance(source::open(&spec, "drive")?.as_ref(), &request);
    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.streams);
    let window_end = timestamp("2026-10-25T18:00:00Z")?;
    let skewed = outcome
        .records
        .iter()
        .find(|record| record.pointer == "drive:1SkewedClockExample000000000")
        .context("the future-dated file is stored")?;
    assert_eq!((skewed.ts, skewed.context), (window_end, false));
    assert!(outcome
        .records
        .iter()
        .any(|record| record.pointer == "drive:1QuarterlyPlanExample0000000"));
    assert_eq!(
        outcome
            .streams
            .get("drive")
            .and_then(|stream| stream.cursor.clone()),
        Some("token-2@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn a_drive_changes_feed_is_followed_page_by_page_to_its_new_start_token() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-changes-pages")?;
    let pdf = |id: &str| {
        json!({"file": {
            "id": id,
            "name": format!("{id}.pdf"),
            "mimeType": "application/pdf",
            "modifiedTime": "2026-10-25T17:00:00Z"
        }})
    };
    let server = FakeGoogle::start(vec![
        (
            "/drive/v3/changes?pageToken=token-1&",
            json!({"changes": [pdf("1FirstPagePdfExample00000000")], "nextPageToken": "page-2"}),
        ),
        (
            "/drive/v3/changes?pageToken=page-2&",
            json!({"changes": [pdf("1SecondPagePdfExample0000000")], "newStartPageToken": "token-3"}),
        ),
    ])?;
    server.use_in_environment("ya29.drive-test");

    let spec = source_spec("drive", "meet = false\n")?;
    let Request::Fetch(mut fetch) =
        fetch_request(&root, "drive", "drive", json!({ "meet": false }))?
    else {
        anyhow::bail!("fetch_request builds a fetch");
    };
    fetch
        .cursors
        .insert("drive".to_owned(), "token-1".to_owned());
    let request = Request::Fetch(fetch);
    let source = source::open(&spec, "drive")?;
    let outcome = run_conformance(source.as_ref(), &request);

    clear_google_environment();
    remove_scratch(&root)?;

    assert!(!outcome.has_errors(), "{:#?}", outcome.error);
    let pointers: Vec<&str> = outcome
        .records
        .iter()
        .map(|record| record.pointer.as_str())
        .collect();
    assert_eq!(
        pointers,
        vec![
            "drive:1FirstPagePdfExample00000000",
            "drive:1SecondPagePdfExample0000000"
        ]
    );
    assert_eq!(
        outcome
            .streams
            .get("drive")
            .and_then(|stream| stream.cursor.clone()),
        Some("token-3@2026-10-25T18:00:00Z".to_owned())
    );
    Ok(())
}

#[test]
fn a_drive_fetch_declares_verbatim_and_cite_verify_checks_its_quote() -> Result<()> {
    let _environment = environment_lock();
    let root = scratch("drive-verbatim")?;
    let tree = root.join("tree");
    let data = root.join("data");
    std::fs::create_dir(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"drive\"\nkind = \"drive\"\nmeet = false\n",
    )?;
    std::fs::write(
        tree.join("decision.md"),
        "---\ntype: decision\nstatus: open\nquote: \"ship in March\"\nsource: \"drive:1QuarterlyPlanExample0000000\"\n---\n",
    )?;
    let server = FakeGoogle::start(vec![
        (
            "/drive/v3/changes/startPageToken",
            json!({"startPageToken": "token-1"}),
        ),
        (
            "/drive/v3/files/1QuarterlyPlanExample0000000/export",
            json!("The plan is to ship in March."),
        ),
        (
            "/drive/v3/files",
            json!({"files": [{
                "id": "1QuarterlyPlanExample0000000",
                "name": "Quarterly plan",
                "mimeType": "application/vnd.google-apps.document",
                "modifiedTime": "2026-10-25T17:00:00Z"
            }]}),
        ),
    ])?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;

    let fetched = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree_text, "fetch"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_DATA_DIR", &data)
        .env("CTX_GOOGLE_API_BASE", server.base())
        .env("CTX_CREDENTIAL_GOOGLE", "ya29.drive-verbatim")
        .output()
        .context("running the Drive fetch")?;
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let sources: Value = serde_json::from_slice(&std::fs::read(data.join("sources.json"))?)?;

    let verified = std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree_text, "cite", "verify"])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_DATA_DIR", &data)
        .output()
        .context("running citation verification")?;
    remove_scratch(&root)?;

    assert_eq!(
        sources
            .pointer("/sources/drive/verbatim")
            .and_then(Value::as_bool),
        Some(true),
        "{sources}"
    );
    assert_eq!(verified.status.code(), Some(0));
    let response: Value = serde_json::from_slice(&verified.stdout)?;
    let report = response.get("data").context("verification has data")?;
    assert_eq!(report.get("checked").and_then(Value::as_u64), Some(1));
    assert_eq!(
        report
            .get("skipped")
            .and_then(|skipped| skipped.get("drive")),
        None,
        "{report}"
    );
    assert_eq!(
        report.get("broken").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
    Ok(())
}
