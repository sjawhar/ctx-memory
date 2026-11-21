//! Mail the deployment withholds: by its `List-Id`, and by the content net's keys.

use super::*;

/// GitHub's notification mail about a repository the deployment excludes, and mail from no list.
const LISTED: &str = "dddddddddddddddd";
const UNLISTED: &str = "aaaaaaaaaaaaaaaa";
/// The List-ID GitHub gives that repository's mail, `<repo>.<owner>.github.com`.
const PRIVATE_LIST: &str = "private.example-org.github.com";

/// A message that carries `header` (its name and value) beside its subject.
fn with_header(mut message: Value, name: &str, value: &str) -> Value {
    if let Some(headers) = message
        .pointer_mut("/payload/headers")
        .and_then(Value::as_array_mut)
    {
        headers.push(json!({"name": name, "value": value}));
    }
    message
}

/// The repository's notification mail, as GitHub writes its `List-Id`
/// (docs.github.com/en/subscriptions-and-notifications/reference/email-notification-headers,
/// read 2026-11-21: `OWNER/REPOSITORY <REPOSITORY.OWNER.github.com>`).
fn listed() -> Value {
    with_header(
        message(LISTED, "1792944000000", "Zmlyc3Q="),
        "List-ID",
        "example-org/private <private.example-org.github.com>",
    )
}

/// The source's configuration with `PRIVATE_LIST` excluded, and without: the control.
fn with_and_without_the_list() -> [(Value, bool); 2] {
    [
        (json!({"exclude_list_ids": [PRIVATE_LIST]}), true),
        (json!({}), false),
    ]
}

/// Mail from an excluded list is never a record on the history path, where no Gmail query
/// applies, and the cursor moves past it as it moves past a promotion. Control: without the list
/// configured, the same history stores both messages.
#[test]
fn mail_from_an_excluded_list_is_not_stored_from_the_history() -> Result<()> {
    for (config, excluded) in with_and_without_the_list() {
        let api = FakeGmailApi {
            history: Some(History {
                entries: vec![HistoryEntry {
                    id: "900".to_owned(),
                    added: vec![LISTED.to_owned(), UNLISTED.to_owned()],
                }],
                latest: Some("1000".to_owned()),
            }),
            messages: BTreeMap::from([
                (LISTED.to_owned(), listed()),
                (
                    UNLISTED.to_owned(),
                    message(UNLISTED, "1792944000000", "Zmlyc3Q="),
                ),
            ]),
            ..FakeGmailApi::default()
        };
        let mut request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
        request.config = config;
        let messages = run_fetch(&api, &request)?;

        let stored = if excluded {
            vec![format!("gmail:{UNLISTED}")]
        } else {
            vec![format!("gmail:{LISTED}"), format!("gmail:{UNLISTED}")]
        };
        assert_eq!(pointers(&messages), stored, "excluded: {excluded}");
        assert_eq!(
            stream_end(&messages),
            Some((
                StreamStatus::Ok,
                Some("1000@2026-10-25T18:00:00Z".to_owned())
            ))
        );
    }
    Ok(())
}

/// Mail from an excluded list is never a record on the listing path either, a first fetch's or
/// a relisting's, though the listing's query does not name the list. Control: without the list
/// configured, the same listing stores both messages.
#[test]
fn mail_from_an_excluded_list_is_not_stored_from_a_listing() -> Result<()> {
    for (config, excluded) in with_and_without_the_list() {
        let api = FakeGmailApi {
            candidates: vec![json!({"id": LISTED}), json!({"id": UNLISTED})],
            messages: BTreeMap::from([
                (LISTED.to_owned(), listed()),
                (
                    UNLISTED.to_owned(),
                    message(UNLISTED, "1792944000000", "Zmlyc3Q="),
                ),
            ]),
            latest_history_id: "4242".to_owned(),
            ..FakeGmailApi::default()
        };
        let mut request = fetch_request(None)?;
        request.config = config;
        let messages = run_fetch(&api, &request)?;

        let stored = if excluded {
            vec![format!("gmail:{UNLISTED}")]
        } else {
            vec![format!("gmail:{LISTED}"), format!("gmail:{UNLISTED}")]
        };
        assert_eq!(pointers(&messages), stored, "excluded: {excluded}");
        assert_eq!(
            stream_end(&messages),
            Some((
                StreamStatus::Ok,
                Some("4242@2026-10-25T18:00:00Z".to_owned())
            ))
        );
    }
    Ok(())
}

/// A resolve, of the message or of its thread, answers no message from an excluded list, so a
/// live resolve stores and prints none of it. Control: without the list, both are answered.
#[test]
fn a_resolve_answers_no_mail_from_an_excluded_list() -> Result<()> {
    for (config, excluded) in with_and_without_the_list() {
        let api = FakeGmailApi {
            messages: BTreeMap::from([(LISTED.to_owned(), listed())]),
            threads: BTreeMap::from([(
                "cccccccccccccccc".to_owned(),
                vec![listed(), message(UNLISTED, "1792947600000", "c2Vjb25k")],
            )]),
            ..FakeGmailApi::default()
        };
        let request = ResolveRequest {
            protocol: PROTOCOL_VERSION,
            source: crate::source::SourceRef {
                id: "gmail".to_owned(),
                kind: "gmail".to_owned(),
                scheme: "gmail".to_owned(),
            },
            config,
            pointers: vec![
                format!("gmail:{LISTED}"),
                "gmail:thread/cccccccccccccccc".to_owned(),
            ],
            state_dir: std::env::temp_dir(),
        };
        let mut output = Vec::new();
        resolve_with_api(
            &request,
            &withheld_by(&request.config)?,
            &api,
            &mut |message| {
                output.push(message);
                Ok(())
            },
        )?;

        let answered = if excluded {
            vec![format!("gmail:{UNLISTED}")]
        } else {
            vec![
                format!("gmail:{LISTED}"),
                format!("gmail:{LISTED}"),
                format!("gmail:{UNLISTED}"),
            ]
        };
        assert_eq!(pointers(&output), answered, "excluded: {excluded}");
    }
    Ok(())
}

/// A `List-Id` header names its list whatever the case of the header's name or of the id, with
/// or without a description and angle brackets, and names that list alone: not one whose id
/// merely ends or starts like it. A message with no such header, or with no headers, is from no
/// list. Control: with no list excluded, no message is.
#[test]
fn a_list_id_header_names_its_list_in_any_case_and_no_other_list() -> Result<()> {
    let lists = ExcludedLists::from_parts(&[PRIVATE_LIST.to_owned()], None)?;
    let plain = message(UNLISTED, "1792944000000", "Zmlyc3Q=");
    let named = |name: &str, value: &str| with_header(plain.clone(), name, value);
    for message in [
        named(
            "List-ID",
            "example-org/private <private.example-org.github.com>",
        ),
        named(
            "list-id",
            "Example-Org/Private <Private.Example-Org.GitHub.com>",
        ),
        named("List-Id", "  private.example-org.github.com  "),
        named("List-Id", "<private.example-org.github.com>"),
    ] {
        assert!(lists.names(&message), "{message}");
        assert!(!ExcludedLists::default().names(&message), "{message}");
    }
    for message in [
        plain.clone(),
        json!({"id": UNLISTED, "threadId": "cccccccccccccccc"}),
        named(
            "List-Id",
            "example-org/private-two <private-two.example-org.github.com>",
        ),
        named("List-Id", "<x-private.example-org.github.com>"),
        named("List-Id", "<private.example-org.github.com.evil>"),
        named("X-List-Id", "<private.example-org.github.com>"),
    ] {
        assert!(!lists.names(&message), "{message}");
    }
    Ok(())
}

/// `CTX_GMAIL_EXCLUDE_LIST_IDS` adds List-IDs to the configuration's, so neither list takes a
/// list off the other, and an entry that is not an id alone is an error rather than one that
/// matches nothing.
#[test]
fn the_environment_adds_list_ids_to_the_configurations() -> Result<()> {
    let configured = vec!["Private.Example-Org.github.com".to_owned()];
    let both = ExcludedLists::from_parts(
        &configured,
        Some(" other.example-org.github.com ,, private.example-org.github.com"),
    )?;
    let other = with_header(
        message(UNLISTED, "1792944000000", "Zmlyc3Q="),
        "List-Id",
        "<other.example-org.github.com>",
    );
    assert!(both.names(&listed()));
    assert!(both.names(&other));
    let configured_alone = ExcludedLists::from_parts(&configured, None)?;
    assert!(configured_alone.names(&listed()));
    assert!(!configured_alone.names(&other));
    assert_eq!(
        ExcludedLists::from_parts(&[], Some(""))?,
        ExcludedLists::default()
    );
    for refused in [
        ExcludedLists::from_parts(&[" ".to_owned()], None),
        ExcludedLists::from_parts(
            &[],
            Some("example-org/private <private.example-org.github.com>"),
        ),
        ExcludedLists::from_parts(&["<private.example-org.github.com>".to_owned()], None),
        ExcludedLists::from_parts(&[], Some("private.example-org.github.com;")),
        ExcludedLists::from_parts(&[], Some("private..example-org.github.com")),
        ExcludedLists::from_parts(&[], Some("not-a-list-id")),
        ExcludedLists::from_parts(&[], Some(".example-org.github.com")),
    ] {
        assert!(refused.is_err(), "{refused:?}");
    }
    Ok(())
}

/// What each path that reads a message stores of `messages` under `config`: a history walk, a
/// listing, a resolve of each message, and a resolve of their thread.
fn stored_on_each_path(
    config: &Value,
    messages: &[Value],
) -> Result<Vec<(&'static str, Vec<String>)>> {
    stored_on_each_path_with(config, messages, BTreeMap::new())
}

/// [`stored_on_each_path`], with Gmail keeping `attachments` apart from the messages.
fn stored_on_each_path_with(
    config: &Value,
    messages: &[Value],
    attachments: BTreeMap<(String, String), String>,
) -> Result<Vec<(&'static str, Vec<String>)>> {
    let ids = messages
        .iter()
        .map(|message| required_string(message, "id", "test message").map(str::to_owned))
        .collect::<Result<Vec<_>>>()?;
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: ids.clone(),
            }],
            latest: Some("1000".to_owned()),
        }),
        candidates: ids.iter().map(|id| json!({ "id": id })).collect(),
        messages: ids.iter().cloned().zip(messages.iter().cloned()).collect(),
        threads: BTreeMap::from([("cccccccccccccccc".to_owned(), messages.to_vec())]),
        latest_history_id: "4242".to_owned(),
        attachments,
        ..FakeGmailApi::default()
    };
    let mut stored = Vec::new();
    for (path, cursor) in [
        ("history", Some("850@2026-10-24T00:00:00Z")),
        ("listing", None),
    ] {
        let mut request = fetch_request(cursor)?;
        request.config = config.clone();
        let messages = run_fetch(&api, &request)?;
        assert!(
            matches!(stream_end(&messages), Some((StreamStatus::Ok, Some(_)))),
            "{path}: {:?}",
            stream_end(&messages)
        );
        stored.push((path, pointers(&messages)));
    }
    let resolve = |pointers: Vec<String>| -> Result<Vec<String>> {
        let request = ResolveRequest {
            protocol: PROTOCOL_VERSION,
            source: crate::source::SourceRef {
                id: "gmail".to_owned(),
                kind: "gmail".to_owned(),
                scheme: "gmail".to_owned(),
            },
            config: config.clone(),
            pointers,
            state_dir: std::env::temp_dir(),
        };
        let mut output = Vec::new();
        resolve_with_api(&request, &withheld_by(config)?, &api, &mut |message| {
            output.push(message);
            Ok(())
        })?;
        Ok(self::pointers(&output))
    };
    stored.push((
        "resolve",
        resolve(ids.iter().map(|id| format!("gmail:{id}")).collect())?,
    ));
    stored.push((
        "thread resolve",
        resolve(vec!["gmail:thread/cccccccccccccccc".to_owned()])?,
    ));
    Ok(stored)
}

/// Asserts that every path stores exactly `want` of the messages.
fn assert_each_path_stores(stored: &[(&str, Vec<String>)], want: &[&str], case: &str) {
    let mut want = want
        .iter()
        .map(|id| format!("gmail:{id}"))
        .collect::<Vec<_>>();
    want.sort();
    for (path, pointers) in stored {
        let mut got = pointers.clone();
        got.sort();
        got.dedup();
        assert_eq!(got, want, "{case}: {path}");
    }
}

/// A `List-Id` folded across lines inside its brackets, as RFC 2919 §3 has a reader take it
/// whatever whitespace a transport puts there, names its list on every path: the history walk,
/// the listing, and a resolve of the message and of its thread. Control: without the list
/// configured, every path stores it.
#[test]
fn a_folded_list_id_is_excluded_on_every_path() -> Result<()> {
    let folded = with_header(
        message(LISTED, "1792944000000", "Zmlyc3Q="),
        "List-Id",
        "example-org/private <private.\r\n\texample-org.github.com>",
    );
    let plain = message(UNLISTED, "1792944000000", "Zmlyc3Q=");
    let messages = [folded, plain];
    for (config, excluded) in with_and_without_the_list() {
        let stored = stored_on_each_path(&config, &messages)?;
        if excluded {
            assert_each_path_stores(&stored, &[UNLISTED], "the list excluded");
        } else {
            assert_each_path_stores(&stored, &[LISTED, UNLISTED], "no list excluded");
        }
    }
    Ok(())
}

/// A message part encoded as Gmail sends it, base64url.
fn part(mime: &str, text: &str) -> Value {
    use base64::Engine as _;
    json!({
        "mimeType": mime,
        "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)}
    })
}

/// A message whose subject is `subject` and whose plain and HTML parts say `plain` and `html`.
fn multipart(id: &str, subject: &str, plain: &str, html: &str) -> Value {
    json!({
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": {
            "headers": [{"name": "Subject", "value": subject}],
            "mimeType": "multipart/alternative",
            "parts": [part("text/plain", plain), part("text/html", html)]
        }
    })
}

/// A message whose one part is HTML saying `html`, so its record's text is that HTML with its
/// tags taken out and its entities decoded.
fn html_only(id: &str, html: &str) -> Value {
    let mut payload = part("text/html", html);
    if let Some(fields) = payload.as_object_mut() {
        fields.insert(
            "headers".to_owned(),
            json!([{"name": "Subject", "value": "Notes"}]),
        );
    }
    json!({
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": payload
    })
}

/// Mail whose text names an excluded key is stored on no path (the history walk, the listing,
/// and a resolve of the message and of its thread): an issue (`SYNTH-12`) in its subject, a link
/// (`dispatch://SYNTH`) in its HTML part alone, which the record's text never takes, and, in mail
/// that is HTML alone, an issue that markup (`SYNTH<b>-12</b>`) or an entity (`SYNTH&#45;12`)
/// splits, which the record's text joins. An issue of another key is kept. Control: with no key
/// configured, every path stores all five.
#[test]
fn mail_that_names_an_excluded_key_is_not_stored_on_any_path() -> Result<()> {
    let subject = multipart(
        "00000000000000e1",
        "Re: SYNTH-12 rollout",
        "plan",
        "<p>plan</p>",
    );
    let html_link = multipart(
        "00000000000000e2",
        "Weekly notes",
        "see the plan",
        r#"<a href="dispatch://SYNTH/artifact/plan">the plan</a>"#,
    );
    let other = multipart(
        "00000000000000e3",
        "INFRA-12 rollout",
        "SYNTHB-4",
        "SYNTHB-4",
    );
    let markup = html_only("00000000000000e4", "<p>SYNTH<b>-12</b> rollout</p>");
    let entity = html_only("00000000000000e5", "<p>SYNTH&#45;12 rollout</p>");
    let messages = [subject, html_link, other, markup, entity];
    let stored = stored_on_each_path(&json!({"exclude_keys": ["SYNTH"]}), &messages)?;
    assert_each_path_stores(&stored, &["00000000000000e3"], "SYNTH excluded");
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(
        &stored,
        &[
            "00000000000000e1",
            "00000000000000e2",
            "00000000000000e3",
            "00000000000000e4",
            "00000000000000e5",
        ],
        "no key excluded",
    );
    Ok(())
}

/// A List-ID is at most 255 octets (RFC 2919 §2): one of 255 is taken, one of 256 is refused.
#[test]
fn a_list_id_longer_than_255_octets_is_refused() {
    let id = |length: usize| format!("{}.github.com", "a".repeat(length - ".github.com".len()));
    assert!(ExcludedLists::from_parts(&[id(255)], None).is_ok());
    assert!(ExcludedLists::from_parts(&[id(256)], None).is_err());
}

/// Text encoded as Gmail sends a part's body, base64url.
fn encoded(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
}

/// An HTML alternative whose key markup (`SYNTH<b>-12</b>`) or an entity (`SYNTH&#45;12`) splits
/// is read as a record's text reads HTML, tags out and entities decoded, though the record takes
/// the plain alternative, which names nothing: the message is stored on no path. Control: with no
/// key configured, every path stores both.
#[test]
fn an_html_alternative_that_names_a_key_through_markup_is_not_stored_on_any_path() -> Result<()> {
    let messages = [
        multipart(
            "00000000000000f1",
            "Notes",
            "plan",
            "<p>SYNTH<b>-12</b></p>",
        ),
        multipart("00000000000000f2", "Notes", "plan", "<p>SYNTH&#45;12</p>"),
    ];
    let stored = stored_on_each_path(&json!({"exclude_keys": ["SYNTH"]}), &messages)?;
    assert_each_path_stores(&stored, &[], "SYNTH excluded");
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(
        &stored,
        &["00000000000000f1", "00000000000000f2"],
        "no key excluded",
    );
    Ok(())
}

/// A message whose plain part says `plain` and which carries `attached` too.
fn with_part(id: &str, plain: &str, attached: &Value) -> Value {
    json!({
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": {
            "headers": [{"name": "Subject", "value": "Notes"}],
            "mimeType": "multipart/mixed",
            "parts": [part("text/plain", plain), attached]
        }
    })
}

/// A part Gmail keeps apart from its message, under `attachment`.
fn kept_apart(mime: &str, filename: &str, attachment: &str) -> Value {
    json!({
        "mimeType": mime,
        "filename": filename,
        "body": {"attachmentId": attachment, "size": 8}
    })
}

/// A text part Gmail keeps apart from its message, an attached text file, is read and checked:
/// one that names an excluded key withholds its message on every path. Control: one that names
/// none is read and its message kept; with no key configured, no part is read and both are kept.
#[test]
fn a_text_part_gmail_keeps_apart_is_read_and_checked() -> Result<()> {
    let messages = [
        with_part(
            "00000000000000a1",
            "see the notes",
            &kept_apart("text/plain", "notes.txt", "att-1"),
        ),
        with_part(
            "00000000000000a2",
            "see the notes",
            &kept_apart("text/plain", "notes.txt", "att-2"),
        ),
    ];
    let attachments = BTreeMap::from([
        (
            ("00000000000000a1".to_owned(), "att-1".to_owned()),
            encoded("agenda: SYNTH-12"),
        ),
        (
            ("00000000000000a2".to_owned(), "att-2".to_owned()),
            encoded("agenda: INFRA-12"),
        ),
    ]);
    let stored = stored_on_each_path_with(
        &json!({"exclude_keys": ["SYNTH"]}),
        &messages,
        attachments.clone(),
    )?;
    assert_each_path_stores(&stored, &["00000000000000a2"], "SYNTH excluded");
    let stored = stored_on_each_path_with(&json!({}), &messages, BTreeMap::new())?;
    assert_each_path_stores(
        &stored,
        &["00000000000000a1", "00000000000000a2"],
        "no key excluded, so no part is read",
    );
    Ok(())
}

/// A part that is not text, such as a PDF, is not read: whatever it holds, its message is
/// stored, and its record holds the part's file name alone, never its content. A key in that
/// file name, which the record would store, withholds the message. Control: the same PDF under a
/// file name that names no key is stored.
#[test]
fn a_binary_part_is_not_read_and_a_key_in_its_file_name_withholds_the_message() -> Result<()> {
    for (filename, withheld) in [("report.pdf", false), ("SYNTH-12 report.pdf", true)] {
        let pdf = with_part(
            "00000000000000b1",
            "see the report",
            &kept_apart("application/pdf", filename, "att-pdf"),
        );
        let api = FakeGmailApi {
            candidates: vec![json!({"id": "00000000000000b1"})],
            messages: BTreeMap::from([("00000000000000b1".to_owned(), pdf)]),
            latest_history_id: "4242".to_owned(),
            ..FakeGmailApi::default()
        };
        let mut request = fetch_request(None)?;
        request.config = json!({"exclude_keys": ["SYNTH"]});
        let messages = run_fetch(&api, &request)?;
        let stored = records(&messages);
        assert!(
            !api.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("attachments get")),
            "{filename}: {:?}",
            api.calls.borrow()
        );
        if withheld {
            assert!(stored.is_empty(), "{filename}: {stored:?}");
        } else {
            let [record] = stored.as_slice() else {
                bail!("{filename}: want one record, got {stored:?}");
            };
            assert!(!record.text.contains("SYNTH"), "{}", record.text);
            assert!(!record.raw.to_string().contains("SYNTH"), "{}", record.raw);
            assert!(
                record.raw.to_string().contains("report.pdf"),
                "{}",
                record.raw
            );
        }
    }
    Ok(())
}
