//! Mail the deployment withholds by the content net: its keys, terms and addresses.

use super::fixtures::*;

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

/// In mail that is HTML alone, a block or line-break tag reads as a line break and an inline tag
/// as nothing: a key in the paragraph after `Notes`, or after a `<br>`, `<center>`, `<section>`,
/// `<dt>` or `<header>`, and a phrase split over a `<br>` or two `<article>`s, are named, so the
/// message is stored on no path. Control: the same mail naming another key or phrase is stored
/// on every path.
#[test]
fn html_mail_whose_blocks_name_a_key_or_phrase_is_not_stored_on_any_path() -> Result<()> {
    let config = json!({"exclude_keys": ["SYNTH"], "exclude_terms": ["Project Falcon"]});
    for (key, phrase, withheld) in [("SYNTH", "Falcon", true), ("SYNTHB", "Falconer", false)] {
        let split = |tag: &str| format!("<{tag}>Notes</{tag}><{tag}><b>{key}</b>-12</{tag}>");
        let messages = [
            html_only(
                "00000000000000h1",
                &format!("<p>Notes</p><p><b>{key}</b>-12</p>"),
            ),
            html_only("00000000000000h2", &format!("Notes<br><b>{key}</b>-12")),
            html_only("00000000000000h3", &format!("Project<br>{phrase} weekly")),
            html_only("00000000000000h4", &split("center")),
            html_only("00000000000000h5", &split("section")),
            html_only("00000000000000h6", &split("dt")),
            html_only(
                "00000000000000h7",
                &format!("<header>Notes</header><footer><b>{key}</b>-12</footer>"),
            ),
            html_only(
                "00000000000000h8",
                &format!("<article>Project</article><article>{phrase} weekly</article>"),
            ),
        ];
        let stored = stored_on_each_path(&config, &messages)?;
        let all = [
            "00000000000000h1",
            "00000000000000h2",
            "00000000000000h3",
            "00000000000000h4",
            "00000000000000h5",
            "00000000000000h6",
            "00000000000000h7",
            "00000000000000h8",
        ];
        assert_each_path_stores(&stored, if withheld { &[] } else { &all }, key);
    }
    Ok(())
}

/// Text encoded as Gmail sends a part's body, base64url.
fn encoded(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
}

/// A text part whose declared charset is not UTF-8 is also read as UTF-8, lossily: a sender
/// that declares ISO-8859-1 over UTF-8 bytes (`SYNTH‑12`, a non-breaking hyphen) hides the key
/// from the declared decoding alone. Such a message is stored on no path. Control: the same
/// bytes naming another key are stored on every path.
#[test]
fn a_part_declared_in_another_charset_is_also_read_as_utf8() -> Result<()> {
    let declared = |id: &str, text: &str| {
        json!({
            "id": id,
            "threadId": "cccccccccccccccc",
            "internalDate": "1792944000000",
            "payload": {
                "headers": [
                    {"name": "Subject", "value": "Notes"},
                    {"name": "Content-Type", "value": "text/plain; charset=ISO-8859-1"}
                ],
                "mimeType": "text/plain",
                "body": {"data": encoded(text)}
            }
        })
    };
    let messages = [
        declared("00000000000000g1", "agenda: SYNTH\u{2011}12"),
        declared("00000000000000g2", "agenda: SYNTHB\u{2011}12"),
    ];
    let stored = stored_on_each_path(&json!({"exclude_keys": ["SYNTH"]}), &messages)?;
    assert_each_path_stores(&stored, &["00000000000000g2"], "SYNTH excluded");
    Ok(())
}

/// An HTML alternative whose key markup (`SYNTH<b>-12</b>`, `<a href=…>SYNTH</a>-12`) or an
/// entity (`SYNTH&#45;12`, beside a bare `&` in `Q&A`) splits is read as a reader sees it,
/// though the record takes the plain alternative, which names nothing: the message is stored
/// on no path. Control: with no key configured, every path stores them all.
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
        multipart(
            "00000000000000f3",
            "Notes",
            "plan",
            r#"<a href="https://example.com/x?a=1&b=2">SYNTH</a>-12"#,
        ),
        multipart(
            "00000000000000f4",
            "Notes",
            "plan",
            "<p>Q&A on SYNTH&#45;12</p>",
        ),
    ];
    let ids = [
        "00000000000000f1",
        "00000000000000f2",
        "00000000000000f3",
        "00000000000000f4",
    ];
    let stored = stored_on_each_path(&json!({"exclude_keys": ["SYNTH"]}), &messages)?;
    assert_each_path_stores(&stored, &[], "SYNTH excluded");
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(&stored, &ids, "no key excluded");
    Ok(())
}

/// Every part Gmail sends inline is read and checked, whatever its type: a JSON part, a
/// delivery status, an octet stream, an AMP alternative, and an invitation whose iCalendar
/// folds a key across two lines. Each such message is stored on no path. Control: with no key
/// configured, every path stores them all.
#[test]
fn every_inline_part_is_checked_whatever_its_type() -> Result<()> {
    let parts = [
        (
            "00000000000000c1",
            "application/json",
            r#"{"issue":"SYNTH-12"}"#,
        ),
        (
            "00000000000000c2",
            "message/delivery-status",
            "Final-Recipient: rfc822; x@example.com\nDiagnostic: SYNTH-12",
        ),
        (
            "00000000000000c3",
            "application/octet-stream",
            "blob SYNTH-12 blob",
        ),
        ("00000000000000c4", "text/x-amp-html", "<b>SYNTH</b>-12"),
        (
            "00000000000000c5",
            "text/calendar",
            "BEGIN:VEVENT\r\nSUMMARY:Review SYN\r\n TH-12\r\nEND:VEVENT",
        ),
    ];
    let messages = parts
        .iter()
        .map(|(id, mime, text)| with_part(id, "see the attachment", &part(mime, text)))
        .collect::<Vec<_>>();
    let ids = parts.iter().map(|(id, ..)| *id).collect::<Vec<_>>();
    let stored = stored_on_each_path(&json!({"exclude_keys": ["SYNTH"]}), &messages)?;
    assert_each_path_stores(&stored, &[], "SYNTH excluded");
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(&stored, &ids, "no key excluded");
    Ok(())
}

/// Mail from, to, cc or reply-to an excluded address, named whole in any case beside a display
/// name, is stored on no path, though it names no key. Mail between public addresses is kept.
/// Control: with no address configured, every path stores all five.
#[test]
fn mail_an_excluded_address_takes_part_in_is_not_stored_on_any_path() -> Result<()> {
    let plain = |id: &str| message(id, "1792944000000", "Zmlyc3Q=");
    let messages = [
        with_header(
            plain("00000000000000d1"),
            "From",
            "\"Out Side\" <Outside@Partner.example>",
        ),
        with_header(
            plain("00000000000000d2"),
            "To",
            "ada@theorem.example, outside@partner.example",
        ),
        with_header(plain("00000000000000d3"), "Cc", "<outside@partner.example>"),
        with_header(
            plain("00000000000000d4"),
            "Reply-To",
            "outside@partner.example",
        ),
        with_header(
            plain("00000000000000d5"),
            "From",
            "Ada <ada@theorem.example>",
        ),
    ];
    let config = json!({"exclude_emails": ["outside@partner.example"]});
    let stored = stored_on_each_path(&config, &messages)?;
    assert_each_path_stores(&stored, &["00000000000000d5"], "the address excluded");
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(
        &stored,
        &[
            "00000000000000d1",
            "00000000000000d2",
            "00000000000000d3",
            "00000000000000d4",
            "00000000000000d5",
        ],
        "no address excluded",
    );
    Ok(())
}

/// Mail that names no key or link, but a private repository as `owner/name` (or in a GitHub
/// URL) or the project's name, is stored on no path. A public repository of the same owner and
/// a public phrase are kept, and so is mail that names the repository's bare name. Control: with
/// no term configured, every path stores them all.
#[test]
fn mail_that_names_a_repository_or_the_projects_name_is_not_stored_on_any_path() -> Result<()> {
    let plain = |id: &str, subject: &str| {
        with_header(message(id, "1792944000000", "Zmlyc3Q="), "Subject", subject)
    };
    let messages = [
        plain(
            "00000000000000t1",
            "Review theorem-labs/synth before Friday",
        ),
        plain(
            "00000000000000t2",
            "https://github.com/Theorem-Labs/Synth/pull/3",
        ),
        plain("00000000000000t3", "Project Falcon kickoff"),
        plain("00000000000000t4", "Review theorem-labs/public-repo"),
        plain("00000000000000t5", "Quarterly plan"),
        plain("00000000000000t6", "the synth notes"),
    ];
    let config = json!({
        "exclude_repositories": ["theorem-labs/synth"],
        "exclude_terms": ["Project Falcon"]
    });
    let stored = stored_on_each_path(&config, &messages)?;
    assert_each_path_stores(
        &stored,
        &["00000000000000t4", "00000000000000t5", "00000000000000t6"],
        "the terms excluded",
    );
    let stored = stored_on_each_path(&json!({}), &messages)?;
    assert_each_path_stores(
        &stored,
        &[
            "00000000000000t1",
            "00000000000000t2",
            "00000000000000t3",
            "00000000000000t4",
            "00000000000000t5",
            "00000000000000t6",
        ],
        "no term excluded",
    );
    Ok(())
}

/// An excluded address is named however an address header writes it, read as RFC 5322 reads a
/// mailbox list: a quoted local part, a comment or folding inside the address, a group, an
/// encoded word in a display name (base64 or quoted-printable); and in the headers a list or a
/// relay carries the original sender and recipients in (`X-Original-From`, `X-Original-Sender`,
/// `Resent-From`, `Resent-Sender`, `Resent-To`, `Resent-Cc`). Each such message is stored on no
/// path. Control: the same forms naming a public address are stored on every path.
#[test]
fn an_excluded_address_is_named_however_a_header_writes_it() -> Result<()> {
    let forms = |address: &str| {
        use base64::Engine as _;
        let (local, domain) = address.split_once('@').unwrap_or((address, ""));
        let b = base64::engine::general_purpose::STANDARD.encode(address);
        vec![
            ("From", format!("\"{local}\"@{domain}")),
            ("To", format!("Side <{local}(the partner)@{domain}>")),
            ("Cc", format!("{local} @\r\n {domain}")),
            ("To", format!("Partners: ada@theorem.example, {address};")),
            ("From", format!("=?UTF-8?B?{b}?= <list@lists.example>")),
            (
                "Reply-To",
                format!(
                    "=?utf-8?Q?{}?= <list@lists.example>",
                    address.replace('@', "=40")
                ),
            ),
            ("X-Original-From", address.to_owned()),
            ("X-Original-Sender", address.to_owned()),
            ("Resent-From", address.to_owned()),
            ("Resent-Sender", address.to_owned()),
            ("Resent-To", address.to_owned()),
            ("Resent-Cc", address.to_owned()),
        ]
    };
    let config = json!({"exclude_emails": ["outside@partner.example"]});
    for (address, excluded) in [
        ("Outside@Partner.example", true),
        ("ada@theorem.example", false),
    ] {
        let messages = forms(address)
            .into_iter()
            .enumerate()
            .map(|(index, (name, value))| {
                with_header(
                    message(&format!("{index:016x}"), "1792944000000", "Zmlyc3Q="),
                    name,
                    &value,
                )
            })
            .collect::<Vec<_>>();
        let ids = (0..messages.len())
            .map(|index| format!("{index:016x}"))
            .collect::<Vec<_>>();
        let stored = stored_on_each_path(&config, &messages)?;
        if excluded {
            assert_each_path_stores(&stored, &[], address);
        } else {
            let ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
            assert_each_path_stores(&stored, &ids, address);
        }
    }
    Ok(())
}

/// An excluded address is named in an address header however malformed it is, since the raw
/// value is scanned beside the parsed mailboxes: an unclosed `<`, an unquoted address in a display
/// name, with and without a comment, and an unclosed quote. Each such message is stored on no
/// path. Control: the same malformed headers naming a public address are stored on every path.
#[test]
fn an_excluded_address_is_named_in_a_malformed_header() -> Result<()> {
    let config = json!({"exclude_emails": ["outside@partner.example"]});
    for (address, excluded) in [
        ("Outside@Partner.example", true),
        ("ada@theorem.example", false),
    ] {
        let forms = [
            ("From", format!("Out Side <{address}")),
            ("From", format!("{address} via Busy <list@lists.example>")),
            (
                "Reply-To",
                format!("{address} (Busy) via list <list@lists.example>"),
            ),
            ("To", format!("\"Out Side <{address}>")),
            (
                "From",
                format!("\"'{address}' via Synth\" <synth@theorem.example>"),
            ),
        ];
        let messages = forms
            .iter()
            .enumerate()
            .map(|(index, (name, value))| {
                with_header(
                    message(
                        &format!("{:016x}", index + 0x40),
                        "1792944000000",
                        "Zmlyc3Q=",
                    ),
                    name,
                    value,
                )
            })
            .collect::<Vec<_>>();
        let ids = (0..forms.len())
            .map(|index| format!("{:016x}", index + 0x40))
            .collect::<Vec<_>>();
        let ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
        let stored = stored_on_each_path(&config, &messages)?;
        assert_each_path_stores(&stored, if excluded { &[] } else { &ids }, address);
    }
    Ok(())
}

/// Each message a fetch withholds is logged to standard error by its reason, its key when a key
/// withheld it, and its pointer, and never by its subject, file name or address; no warning of
/// the window names it. Control: a message the net does not withhold is logged by nothing.
#[test]
fn a_withheld_message_is_logged_by_key_and_pointer_and_never_warned_of() -> Result<()> {
    let titled = with_header(
        with_part(
            "00000000000000e1",
            "plan",
            &kept_apart("application/pdf", "secret-roadmap.pdf", "att-1"),
        ),
        "Subject",
        "SYNTH-12 secret roadmap",
    );
    let addressed = with_header(
        message("00000000000000e2", "1792944000000", "Zmlyc3Q="),
        "From",
        "Out Side <outside@partner.example>",
    );
    let kept = message("00000000000000e3", "1792944000000", "Zmlyc3Q=");
    let ids = ["00000000000000e1", "00000000000000e2", "00000000000000e3"];
    let api = FakeGmailApi {
        history: Some(History {
            entries: vec![HistoryEntry {
                id: "900".to_owned(),
                added: ids.iter().map(|id| (*id).to_owned()).collect(),
            }],
            latest: Some("1000".to_owned()),
        }),
        messages: ids
            .iter()
            .map(|id| (*id).to_owned())
            .zip([titled, addressed, kept])
            .collect(),
        ..FakeGmailApi::default()
    };
    let mut request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
    request.config = json!({
        "exclude_keys": ["SYNTH"],
        "exclude_emails": ["outside@partner.example"]
    });
    crate::source::content_net::tests::take_logged();
    let messages = run_fetch(&api, &request)?;
    let logged = crate::source::content_net::tests::take_logged();
    assert_eq!(pointers(&messages), ["gmail:00000000000000e3"]);
    assert_eq!(
        logged,
        [
            r#"{"event":"withheld","reason":"private-content-net","key":"SYNTH","pointer":"gmail:00000000000000e1"}"#,
            r#"{"event":"withheld","reason":"private-address","pointer":"gmail:00000000000000e2"}"#,
        ]
    );
    for line in &logged {
        for private in ["secret", "roadmap", "outside", "partner"] {
            assert!(!line.contains(private), "{line}");
        }
    }
    let warned = messages.iter().any(|message| {
        matches!(message, Message::Warning { message, .. }
            if message.contains("00000000000000e1") || message.contains("00000000000000e2"))
    });
    assert!(!warned, "{messages:?}");
    Ok(())
}

/// A text part Gmail keeps apart and does not answer for stops the stream before its message,
/// which could not be checked, with a warning that names the message's pointer and the cause and
/// nothing of its content; the stream keeps no cursor, so the next fetch reads it again.
/// Control: when Gmail answers, the stream ends with its cursor and no such warning.
#[test]
fn a_text_part_gmail_does_not_answer_for_stops_the_stream_with_a_warning() -> Result<()> {
    for answered in [false, true] {
        let attached = with_header(
            with_part(
                "00000000000000a9",
                "see the notes",
                &kept_apart("text/plain", "notes.txt", "att-9"),
            ),
            "Subject",
            "Quarterly roadmap",
        );
        let api = FakeGmailApi {
            history: Some(History {
                entries: vec![HistoryEntry {
                    id: "900".to_owned(),
                    added: vec!["00000000000000a9".to_owned()],
                }],
                latest: Some("1000".to_owned()),
            }),
            messages: BTreeMap::from([("00000000000000a9".to_owned(), attached)]),
            attachments: if answered {
                BTreeMap::from([(
                    ("00000000000000a9".to_owned(), "att-9".to_owned()),
                    encoded("agenda"),
                )])
            } else {
                BTreeMap::new()
            },
            ..FakeGmailApi::default()
        };
        let mut request = fetch_request(Some("850@2026-10-24T00:00:00Z"))?;
        request.config = json!({"exclude_keys": ["SYNTH"]});
        let messages = run_fetch(&api, &request)?;
        let warnings = messages
            .iter()
            .filter_map(|message| match message {
                Message::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let named = warnings
            .iter()
            .filter(|warning| warning.contains("gmail:00000000000000a9"))
            .collect::<Vec<_>>();
        if answered {
            assert!(named.is_empty(), "{warnings:?}");
            assert!(matches!(
                stream_end(&messages),
                Some((StreamStatus::Ok, Some(_)))
            ));
        } else {
            let [warning] = named.as_slice() else {
                bail!("want one warning naming the message, got {warnings:?}");
            };
            assert!(
                warning.contains("missing fake attachment att-9"),
                "{warning}"
            );
            assert!(
                !warning.contains("roadmap") && !warning.contains("notes"),
                "{warning}"
            );
            assert_eq!(stream_end(&messages), Some((StreamStatus::Error, None)));
        }
    }
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
