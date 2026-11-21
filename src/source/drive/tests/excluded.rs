//! Files the deployment never stores because they name a key of the content net.

use super::fixtures::*;

/// A document whose name names an excluded issue, one whose exported text links into the
/// excluded project, and one that names neither.
const NAMED: &str = "1SynthRolloutPlanExample0000";
const INSIDE: &str = "1WeeklyNotesExample000000000";
const KEPT: &str = "1QuarterlyPlanExample0000000";

fn files() -> Vec<Value> {
    vec![
        document(NAMED, "SYNTH-12 rollout plan"),
        document(INSIDE, "Weekly notes"),
        document(KEPT, "Quarterly plan"),
    ]
}

fn exports() -> BTreeMap<String, (String, bool)> {
    BTreeMap::from([
        (NAMED.to_owned(), ("the rollout".to_owned(), false)),
        // Truncated, so reading it raises a warning that names the file.
        (
            INSIDE.to_owned(),
            ("see dispatch://SYNTH/artifact/plan".to_owned(), true),
        ),
        (KEPT.to_owned(), ("ship in March".to_owned(), false)),
    ])
}

/// A Drive that serves `files()` on the stream `path` names: the changes feed, the listing by
/// time a first fetch makes, or a meeting folder of the Meet folder.
fn drive_for(path: &str) -> (FakeDriveApi, Value, Option<(&'static str, &'static str)>) {
    match path {
        "changes feed" => (
            FakeDriveApi {
                changes: Changes {
                    files: files(),
                    next_page_token: "token-2".to_owned(),
                    ..Changes::default()
                },
                exports: exports(),
                ..FakeDriveApi::default()
            },
            json!({"meet": false}),
            Some((STREAM_CHANGES, "token-1")),
        ),
        "listing" => (
            FakeDriveApi {
                start_page_token: "token-1".to_owned(),
                files: vec![("modifiedTime >".to_owned(), files())],
                exports: exports(),
                ..FakeDriveApi::default()
            },
            json!({"meet": false}),
            None,
        ),
        _ => (
            FakeDriveApi {
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
                        files(),
                    ),
                ],
                exports: exports(),
                ..FakeDriveApi::default()
            },
            json!({"changes": false}),
            None,
        ),
    }
}

/// A file that names an excluded key is stored on no stream: the changes feed, the listing by
/// time and the Meet folder. One whose name names the key is not even exported, and one whose
/// exported text does leaves no warning naming it in the window. Each stream's cursor moves as
/// before. Control: with no key configured, every stream stores all three files and warns of
/// the truncated export.
#[test]
fn a_file_that_names_an_excluded_key_is_stored_on_no_stream() -> Result<()> {
    for path in ["changes feed", "listing", "meet"] {
        for excluded in [true, false] {
            let (api, mut config, cursor) = drive_for(path);
            if excluded {
                if let Some(fields) = config.as_object_mut() {
                    fields.insert("exclude_keys".to_owned(), json!(["SYNTH"]));
                }
            }
            let messages = run_fetch(&api, &request(config, cursor)?)?;
            let mut stored = records(&messages)
                .into_iter()
                .map(|record| record.pointer)
                .collect::<Vec<_>>();
            stored.sort();
            let named_inside = warnings(&messages)
                .iter()
                .any(|warning| warning.contains(INSIDE));
            let exported_named = api
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with(&format!("export {NAMED}")));
            if excluded {
                assert_eq!(stored, [format!("drive:{KEPT}")], "{path}");
                assert!(!named_inside, "{path}: {:?}", warnings(&messages));
                assert!(!exported_named, "{path}: {:?}", api.calls.borrow());
            } else {
                let mut all = [NAMED, INSIDE, KEPT].map(|id| format!("drive:{id}"));
                all.sort();
                assert_eq!(stored, all, "{path}");
                assert!(named_inside, "{path}: {:?}", warnings(&messages));
                assert!(exported_named, "{path}");
            }
            let stream = if path == "meet" {
                STREAM_MEET
            } else {
                STREAM_CHANGES
            };
            assert!(
                stream_cursor(&messages, stream).is_some(),
                "{path}: the stream proposes a cursor"
            );
        }
    }
    Ok(())
}

/// A Drive whose first fetch lists `files`, with `exports` and the exports it `refused`.
fn listing_of(
    files: Vec<Value>,
    exports: BTreeMap<String, (String, bool)>,
    refused: BTreeMap<String, u16>,
) -> FakeDriveApi {
    FakeDriveApi {
        start_page_token: "token-1".to_owned(),
        files: vec![("modifiedTime >".to_owned(), files)],
        exports,
        refused,
        ..FakeDriveApi::default()
    }
}

/// The configuration of a first fetch by listing, with `extra` fields.
fn listing_config(extra: Value) -> Value {
    let mut config = json!({"meet": false});
    if let (Some(fields), Value::Object(extra)) = (config.as_object_mut(), extra) {
        fields.extend(extra);
    }
    config
}

/// Whether the fetch asked Drive to export `id`.
fn exported(api: &FakeDriveApi, id: &str) -> bool {
    api.calls
        .borrow()
        .iter()
        .any(|call| call.starts_with(&format!("export {id}")))
}

/// A file whose description names an excluded key is not exported and not stored, though the
/// record never stores a description. Control: under a description that names none, the file is
/// stored, and its record holds no description, as before the net asked for one.
#[test]
fn a_file_whose_description_names_a_key_is_not_exported_or_stored() -> Result<()> {
    for (description, withheld) in [("SYNTH-12 rollout plan", true), ("rollout plan", false)] {
        let mut file = document(KEPT, "Quarterly plan");
        if let Some(fields) = file.as_object_mut() {
            fields.insert("description".to_owned(), json!(description));
        }
        let api = listing_of(vec![file], exports(), BTreeMap::new());
        let config = listing_config(json!({"exclude_keys": ["SYNTH"]}));
        let messages = run_fetch(&api, &request(config, None)?)?;
        let stored = records(&messages);
        if withheld {
            assert!(stored.is_empty(), "{description}: {stored:?}");
            assert!(!exported(&api, KEPT), "{:?}", api.calls.borrow());
        } else {
            let [record] = stored.as_slice() else {
                bail!("{description}: want one record, got {stored:?}");
            };
            assert!(record.raw.get("description").is_none(), "{}", record.raw);
        }
    }
    Ok(())
}

/// A file whose name names a private repository, or whose export names the project's name, is
/// not stored, and the one named by its name is not even exported. A public repository is
/// kept. Control: with no term configured, all three are stored.
#[test]
fn a_file_that_names_a_repository_or_the_projects_name_is_not_stored() -> Result<()> {
    let files = vec![
        document(NAMED, "theorem-labs/synth design notes"),
        document(INSIDE, "Weekly notes"),
        document(KEPT, "theorem-labs/public-repo design notes"),
    ];
    let exports = BTreeMap::from([
        (NAMED.to_owned(), ("notes".to_owned(), false)),
        (
            INSIDE.to_owned(),
            ("the project falcon plan".to_owned(), false),
        ),
        (KEPT.to_owned(), ("notes".to_owned(), false)),
    ]);
    for excluded in [true, false] {
        let api = listing_of(files.clone(), exports.clone(), BTreeMap::new());
        let config = listing_config(if excluded {
            json!({"exclude_repositories": ["theorem-labs/synth"], "exclude_terms": ["Project Falcon"]})
        } else {
            json!({})
        });
        let messages = run_fetch(&api, &request(config, None)?)?;
        let mut stored = records(&messages)
            .into_iter()
            .map(|record| record.pointer)
            .collect::<Vec<_>>();
        stored.sort();
        if excluded {
            assert_eq!(stored, [format!("drive:{KEPT}")]);
            assert!(!exported(&api, NAMED), "{:?}", api.calls.borrow());
        } else {
            assert_eq!(stored.len(), 3, "{stored:?}");
        }
    }
    Ok(())
}

/// A file an excluded address owns, or holds a permission on, is not exported and not stored,
/// though it names no key; the drop is logged by pointer, never by name or address. A file only
/// public addresses hold is kept, and its record stores no permission. Control: with no address
/// configured, all three files are stored.
#[test]
fn a_file_an_excluded_address_owns_or_is_shared_with_is_not_stored() -> Result<()> {
    let held_by = |id: &str, field: &str, address: &str| {
        let mut file = document(id, "Quarterly plan");
        if let Some(fields) = file.as_object_mut() {
            fields.insert(field.to_owned(), json!([{"emailAddress": address}]));
        }
        file
    };
    let files = vec![
        held_by(NAMED, "owners", "Outside@Partner.example"),
        held_by(INSIDE, "permissions", "outside@partner.example"),
        held_by(KEPT, "permissions", "ada@theorem.example"),
    ];
    let exports = BTreeMap::from([
        (NAMED.to_owned(), ("plan".to_owned(), false)),
        (INSIDE.to_owned(), ("plan".to_owned(), false)),
        (KEPT.to_owned(), ("plan".to_owned(), false)),
    ]);
    for excluded in [true, false] {
        let api = listing_of(files.clone(), exports.clone(), BTreeMap::new());
        let config = listing_config(if excluded {
            json!({"exclude_emails": ["outside@partner.example"]})
        } else {
            json!({})
        });
        crate::source::content_net::tests::take_logged();
        let messages = run_fetch(&api, &request(config, None)?)?;
        let logged = crate::source::content_net::tests::take_logged();
        let mut stored = records(&messages)
            .into_iter()
            .map(|record| {
                assert!(record.raw.get("permissions").is_none(), "{}", record.raw);
                record.pointer
            })
            .collect::<Vec<_>>();
        stored.sort();
        if excluded {
            assert_eq!(stored, [format!("drive:{KEPT}")]);
            assert!(!exported(&api, NAMED) && !exported(&api, INSIDE));
            let mut want = [NAMED, INSIDE].map(|id| {
                format!(
                    r#"{{"event":"withheld","reason":"private-address","pointer":"drive:{id}"}}"#
                )
            });
            want.sort();
            let mut logged = logged;
            logged.sort();
            assert_eq!(logged, want);
        } else {
            let mut all = [NAMED, INSIDE, KEPT].map(|id| format!("drive:{id}"));
            all.sort();
            assert_eq!(stored, all);
            assert!(logged.is_empty(), "{logged:?}");
        }
    }
    Ok(())
}

/// A document with the given extra fields.
fn document_with(id: &str, extra: Value) -> Value {
    let mut file = document(id, "Quarterly plan");
    if let (Some(fields), Value::Object(extra)) = (file.as_object_mut(), extra) {
        fields.extend(extra);
    }
    file
}

/// A file an excluded address last modified, or shared with the caller, is not stored. While
/// addresses are set, the record of a file kept stores no address of the person who last modified
/// it, nor of whoever shared it, and that person's address is never its author, though it has no
/// display name. Control (R9): with no address configured, all three are stored as before the
/// net: the record keeps the last modifier's address, and a last modifier with no display name is
/// the author by address; `sharingUser`, which the listing asks for the net alone, is never kept.
#[test]
fn a_file_an_excluded_address_modified_or_shared_is_not_stored() -> Result<()> {
    let by = |address: &str| json!({"displayName": "Someone", "emailAddress": address});
    let files = vec![
        document_with(
            NAMED,
            json!({"lastModifyingUser": by("Outside@Partner.example"), "permissions": []}),
        ),
        document_with(
            INSIDE,
            json!({"sharingUser": by("outside@partner.example"), "permissions": []}),
        ),
        document_with(
            KEPT,
            json!({
                "lastModifyingUser": {"emailAddress": "ada@theorem.example"},
                "sharingUser": by("ada@theorem.example"),
                "permissions": []
            }),
        ),
    ];
    let exports =
        BTreeMap::from([NAMED, INSIDE, KEPT].map(|id| (id.to_owned(), ("plan".to_owned(), false))));
    for excluded in [true, false] {
        let api = listing_of(files.clone(), exports.clone(), BTreeMap::new());
        let config = listing_config(if excluded {
            json!({"exclude_emails": ["outside@partner.example"]})
        } else {
            json!({})
        });
        let messages = run_fetch(&api, &request(config, None)?)?;
        let stored = records(&messages);
        let kept = stored
            .iter()
            .find(|record| record.pointer == format!("drive:{KEPT}"))
            .context("the public file is stored")?;
        let raw = kept.raw.to_string();
        assert!(kept.raw.get("sharingUser").is_none(), "{raw}");
        if excluded {
            assert!(!raw.contains("theorem.example"), "{raw}");
            assert_ne!(kept.author.as_deref(), Some("ada@theorem.example"));
            assert_eq!(stored.len(), 1, "{stored:?}");
        } else {
            assert!(
                raw.contains(r#""emailAddress":"ada@theorem.example""#),
                "{raw}"
            );
            assert_eq!(kept.author.as_deref(), Some("ada@theorem.example"));
            assert_eq!(stored.len(), 3, "{stored:?}");
        }
    }
    Ok(())
}

/// A file a listing gives no permissions for, as Drive lists a shared-drive file or one the
/// caller may only view, is checked by its permissions and its shared drive's members, which the
/// source asks Drive for, each drive once a stream. A file an excluded address holds a permission
/// on, or whose drive it is a member of, is not stored; nor is one whose permissions, or whose
/// drive's members, Drive refuses to list (`403 insufficientFilePermissions`, as Drive answers a
/// reader, or `404 notFound`): that drop is logged as `permissions-unread`, by pointer, and the
/// drive's refusal is asked once for both of its files. Control: with no address configured, no
/// permissions are asked for and all six are stored.
#[test]
fn a_shared_drive_file_is_checked_by_its_own_and_its_drives_permissions() -> Result<()> {
    let files = vec![
        document_with("file-member", json!({"driveId": "drive-outside"})),
        document_with("file-shared", json!({"driveId": "drive-public"})),
        document_with("file-public", json!({"driveId": "drive-public"})),
        document_with("file-unlisted", json!({})),
        document_with("file-drive-unlisted", json!({"driveId": "drive-unlisted"})),
        document_with(
            "file-drive-unlisted-2",
            json!({"driveId": "drive-unlisted"}),
        ),
    ];
    let ids = [
        "file-member",
        "file-shared",
        "file-public",
        "file-unlisted",
        "file-drive-unlisted",
        "file-drive-unlisted-2",
    ];
    let exports = BTreeMap::from(ids.map(|id| (id.to_owned(), ("plan".to_owned(), false))));
    let listed = |address: &str| vec![address.to_owned()];
    let permission_lists = BTreeMap::from([
        ("file-member".to_owned(), listed("ada@theorem.example")),
        (
            "drive-outside".to_owned(),
            listed("Outside@Partner.example"),
        ),
        ("file-shared".to_owned(), listed("outside@partner.example")),
        ("file-public".to_owned(), listed("ada@theorem.example")),
        ("drive-public".to_owned(), listed("ada@theorem.example")),
        (
            "file-drive-unlisted".to_owned(),
            listed("ada@theorem.example"),
        ),
        (
            "file-drive-unlisted-2".to_owned(),
            listed("ada@theorem.example"),
        ),
    ]);
    let permission_refusals =
        BTreeSet::from(["file-unlisted".to_owned(), "drive-unlisted".to_owned()]);
    for excluded in [true, false] {
        let mut api = listing_of(files.clone(), exports.clone(), BTreeMap::new());
        api.permission_lists = permission_lists.clone();
        api.permission_refusals = permission_refusals.clone();
        let config = listing_config(if excluded {
            json!({"exclude_emails": ["outside@partner.example"]})
        } else {
            json!({})
        });
        crate::source::content_net::tests::take_logged();
        let messages = run_fetch(&api, &request(config, None)?)?;
        let mut logged = crate::source::content_net::tests::take_logged();
        logged.sort();
        let stored = records(&messages)
            .into_iter()
            .map(|record| record.pointer)
            .collect::<BTreeSet<_>>();
        let asked = |id: &str| {
            api.calls
                .borrow()
                .iter()
                .filter(|call| **call == format!("permissions {id}"))
                .count()
        };
        if excluded {
            assert_eq!(stored, BTreeSet::from(["drive:file-public".to_owned()]));
            let line = |reason: &str, id: &str| {
                format!(r#"{{"event":"withheld","reason":"{reason}","pointer":"drive:{id}"}}"#)
            };
            let mut want = vec![
                line("private-address", "file-member"),
                line("private-address", "file-shared"),
                line("permissions-unread", "file-unlisted"),
                line("permissions-unread", "file-drive-unlisted"),
                line("permissions-unread", "file-drive-unlisted-2"),
            ];
            want.sort();
            assert_eq!(logged, want);
            assert_eq!(asked("drive-public"), 1, "{:?}", api.calls.borrow());
            assert_eq!(asked("drive-unlisted"), 1, "{:?}", api.calls.borrow());
        } else {
            assert_eq!(stored.len(), 6, "{stored:?}");
            assert!(
                !api.calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("permissions")),
                "{:?}",
                api.calls.borrow()
            );
        }
    }
    Ok(())
}

/// A permissions listing that fails for a reason that is not the file or drive itself (a spent
/// limit after retries, a server error, the network, the token) ends the stream with that error,
/// on the file's own permissions and on its drive's members alike, as a failed export does: the
/// stream proposes no cursor, so the next fetch reads the file again, and nothing of the file is
/// stored. Control: when Drive lists the permissions, the stream ends with its cursor and the
/// file is stored.
#[test]
fn a_failed_permissions_listing_ends_the_stream_before_the_file() -> Result<()> {
    for (file, failing) in [
        (document_with("file-a", json!({})), "file-a"),
        (
            document_with("file-a", json!({"driveId": "drive-a"})),
            "drive-a",
        ),
    ] {
        for fails in [true, false] {
            let mut api = listing_of(
                vec![file.clone()],
                BTreeMap::from([("file-a".to_owned(), ("plan".to_owned(), false))]),
                BTreeMap::new(),
            );
            api.permission_lists = BTreeMap::from([
                ("file-a".to_owned(), vec!["ada@theorem.example".to_owned()]),
                ("drive-a".to_owned(), vec!["ada@theorem.example".to_owned()]),
            ]);
            if fails {
                api.permission_lists.remove(failing);
                api.permission_failures = BTreeMap::from([(
                    failing.to_owned(),
                    "Google drive/v3/files/x/permissions returned 503: Backend Error".to_owned(),
                )]);
            }
            let config = listing_config(json!({"exclude_emails": ["outside@partner.example"]}));
            let messages = run_fetch(&api, &request(config, None)?)?;
            let stored = records(&messages);
            let cursor = stream_cursor(&messages, STREAM_CHANGES);
            if fails {
                assert!(stored.is_empty(), "{failing}: {stored:?}");
                assert_eq!(cursor, None, "{failing}: the stream proposes no cursor");
            } else {
                assert_eq!(stored.len(), 1, "{failing}: {stored:?}");
                assert!(
                    cursor.is_some(),
                    "{failing}: the stream proposes its cursor"
                );
            }
        }
    }
    Ok(())
}

/// An export longer than `max_export_bytes` is read whole, up to Drive's own export limit, while
/// a net is set: a key past the cap withholds the file, and no warning names it. Control: under a
/// net that names another key, the file is stored with the first `max_export_bytes` of its text
/// and the warning that says so; with no net the export is asked for `max_export_bytes` alone, as
/// before.
#[test]
fn an_export_past_the_cap_is_checked_whole_and_cut_for_the_record() -> Result<()> {
    let text = "an introduction that runs past twenty bytes, then SYNTH-12";
    for (keys, withheld) in [(json!(["SYNTH"]), true), (json!(["OTHER"]), false)] {
        let api = listing_of(
            vec![document(KEPT, "Quarterly plan")],
            BTreeMap::from([(KEPT.to_owned(), (text.to_owned(), false))]),
            BTreeMap::new(),
        );
        let config = listing_config(json!({"exclude_keys": keys, "max_export_bytes": 20}));
        let messages = run_fetch(&api, &request(config, None)?)?;
        let stored = records(&messages);
        assert_eq!(*api.export_limits.borrow(), [10 * 1024 * 1024], "{keys}");
        let named = warnings(&messages)
            .iter()
            .any(|warning| warning.contains(KEPT));
        if withheld {
            assert!(stored.is_empty(), "{stored:?}");
            assert!(!named, "{:?}", warnings(&messages));
        } else {
            let [record] = stored.as_slice() else {
                bail!("want one record, got {stored:?}");
            };
            assert_eq!(record.text, &text[..20]);
            assert!(named, "{:?}", warnings(&messages));
        }
    }
    let api = listing_of(
        vec![document(KEPT, "Quarterly plan")],
        BTreeMap::from([(KEPT.to_owned(), (text[..20].to_owned(), true))]),
        BTreeMap::new(),
    );
    run_fetch(
        &api,
        &request(listing_config(json!({"max_export_bytes": 20})), None)?,
    )?;
    assert_eq!(*api.export_limits.borrow(), [20]);
    Ok(())
}

/// A file that is not a Google document is never exported: whatever it holds, it is stored as
/// its metadata alone, its name and type, never its content. A key in that metadata withholds
/// it. Control: the same PDF under a name that names no key is stored.
#[test]
fn a_file_that_is_not_a_google_document_is_stored_as_metadata_alone() -> Result<()> {
    for (name, withheld) in [("report.pdf", false), ("SYNTH-12 report.pdf", true)] {
        let pdf = json!({
            "id": KEPT,
            "name": name,
            "mimeType": "application/pdf",
            "modifiedTime": "2026-10-25T17:00:00Z"
        });
        let api = listing_of(vec![pdf], BTreeMap::new(), BTreeMap::new());
        let config = listing_config(json!({"exclude_keys": ["SYNTH"]}));
        let messages = run_fetch(&api, &request(config, None)?)?;
        let stored = records(&messages);
        assert!(!exported(&api, KEPT), "{name}: {:?}", api.calls.borrow());
        if withheld {
            assert!(stored.is_empty(), "{name}: {stored:?}");
        } else {
            let [record] = stored.as_slice() else {
                bail!("{name}: want one record, got {stored:?}");
            };
            assert_eq!(record.text, "report.pdf (application/pdf)");
        }
    }
    Ok(())
}

/// A document whose export Drive refuses is stored as its metadata alone. A key in that metadata
/// withholds it before any export is asked for. Control: the same document under a name that
/// names no key is asked for, refused and stored as metadata.
#[test]
fn a_refused_export_is_stored_as_metadata_alone() -> Result<()> {
    for (name, withheld) in [("Quarterly plan", false), ("SYNTH-12 plan", true)] {
        let api = listing_of(
            vec![document(KEPT, name)],
            BTreeMap::new(),
            BTreeMap::from([(KEPT.to_owned(), 403)]),
        );
        let config = listing_config(json!({"exclude_keys": ["SYNTH"]}));
        let messages = run_fetch(&api, &request(config, None)?)?;
        let stored = records(&messages);
        if withheld {
            assert!(stored.is_empty(), "{name}: {stored:?}");
            assert!(!exported(&api, KEPT), "{:?}", api.calls.borrow());
        } else {
            let [record] = stored.as_slice() else {
                bail!("{name}: want one record, got {stored:?}");
            };
            assert!(exported(&api, KEPT));
            assert_eq!(
                record.text,
                "Quarterly plan (application/vnd.google-apps.document)"
            );
        }
    }
    Ok(())
}
