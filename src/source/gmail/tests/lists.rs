//! Mail the deployment withholds by its `List-Id`.

use super::fixtures::*;

/// GitHub's notification mail about a repository the deployment excludes, and mail from no list.
const LISTED: &str = "dddddddddddddddd";
const UNLISTED: &str = "aaaaaaaaaaaaaaaa";
/// The List-ID GitHub gives that repository's mail, `<repo>.<owner>.github.com`.
const PRIVATE_LIST: &str = "private.example-org.github.com";

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

/// A List-ID is at most 255 octets (RFC 2919 §2): one of 255 is taken, one of 256 is refused.
#[test]
fn a_list_id_longer_than_255_octets_is_refused() {
    let id = |length: usize| format!("{}.github.com", "a".repeat(length - ".github.com".len()));
    assert!(ExcludedLists::from_parts(&[id(255)], None).is_ok());
    assert!(ExcludedLists::from_parts(&[id(256)], None).is_err());
}
