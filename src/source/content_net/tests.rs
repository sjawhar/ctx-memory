//! The content net's matching, its entries and its drop log.

use std::cell::RefCell;

use anyhow::Result;
use serde_json::json;

use super::{withheld, ContentNet, Hit, NetLists};

thread_local! {
    /// The drop log lines this test thread wrote, beside standard error.
    pub(in crate::source) static LOGGED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The drop log lines this test thread wrote since it last asked.
pub(in crate::source) fn take_logged() -> Vec<String> {
    LOGGED.with(|logged| std::mem::take(&mut *logged.borrow_mut()))
}

fn net(keys: &[&str]) -> Result<ContentNet> {
    let keys = keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
    ContentNet::from_parts(&keys, None)
}

/// Each key matches as an issue, `<KEY>-<number>`, as a link, `dispatch://<KEY>`, and as a
/// Dispatch web link, `/projects/<KEY>`, in any case: a key is bounded by anything but an
/// ASCII letter or digit, so `_` and a letter outside ASCII separate it too. An issue of
/// another key that starts or ends like it, a number that runs into a word, and the key
/// alone do not match. Control: with no key, nothing does.
#[test]
fn a_key_matches_as_an_issue_or_a_link_and_nothing_else() -> Result<()> {
    let synth = net(&["SYNTH"])?;
    for text in [
        "see SYNTH-12.",
        "SYNTH-1",
        "(SYNTH-3040)",
        "branch INFRA-1-SYNTH-7-fix",
        "open dispatch://SYNTH/artifact/plan",
        "dispatch://SYNTH",
        "notes_SYNTH-12.pdf",
        "SYNTH-12_draft",
        "中SYNTH-12",
        "SYNTH-12é",
        "synth-12",
        "dispatch://synth/ask/1",
        "https://dispatch.example.com/projects/SYNTH/issues",
        "https://dispatch.example.com/projects/synth",
    ] {
        assert!(synth.matches(text), "{text}");
        assert!(!ContentNet::default().matches(text), "{text}");
    }
    for text in [
        "SYNTHB-12",
        "XSYNTH-12",
        "1SYNTH-12",
        "SYNTH-12a",
        "SYNTH-",
        "SYNTH",
        "dispatch://SYNTHB",
        "/projects/SYNTHB/issues",
        "projects/SYNTH",
        "the synth project",
    ] {
        assert!(!synth.matches(text), "{text}");
    }
    Ok(())
}

/// A key is matched in text as a reader of it would see it: HTML tags taken out (a comment,
/// and a `>` inside a quoted attribute, do not end them early), entities decoded (a bare `&`
/// that starts none is kept), Unicode dashes read as `-`, soft hyphens and zero-width
/// characters dropped, and folded iCalendar lines unfolded. Control: the same text naming
/// another key does not match.
#[test]
fn a_key_is_matched_through_markup_entities_dashes_and_folding() -> Result<()> {
    let synth = net(&["SYNTH"])?;
    for (text, other) in [
        ("SYNTH<b>-12</b>", "SYNTHB<b>-12</b>"),
        ("SYNTH&#45;12", "SYNTHB&#45;12"),
        ("SYNTH&#x2d;12", "SYNTHB&#x2d;12"),
        ("SYNTH&ndash;12", "SYNTHB&ndash;12"),
        (
            r#"<a href="x">SYNTH</a>-12"#,
            r#"<a href="x">SYNTHB</a>-12"#,
        ),
        (
            r#"<a title="a > b">SYNTH</a>-12"#,
            r#"<a title="a > b">SYNTHB</a>-12"#,
        ),
        ("SYNTH<!-- a > b -->-12", "SYNTHB<!-- a > b -->-12"),
        ("Q&A on SYNTH&#45;12", "Q&A on SYNTHB&#45;12"),
        ("SYNTH\u{2013}12", "SYNTHB\u{2013}12"),
        ("SYNTH\u{2011}12", "SYNTHB\u{2011}12"),
        ("SYN\u{00AD}TH-12", "SYN\u{00AD}THB-12"),
        ("SYNTH\u{200B}-12", "SYNTHB\u{200B}-12"),
        ("DESCRIPTION:SYN\r\n TH-12", "DESCRIPTION:SYN\r\n THB-12"),
        ("DESCRIPTION:SYNTH-\r\n\t12", "DESCRIPTION:SYNTHB-\r\n\t12"),
    ] {
        assert!(synth.matches(text), "{text:?}");
        assert!(!synth.matches(other), "{other:?}");
    }
    assert!(!synth.matches("a < b and SYNTHB > c, Q&A"));
    Ok(())
}

/// The reading is hardened past what markup alone hides: an unterminated `<` keeps the text
/// after it; percent-escapes are decoded; every format character (`\p{Cf}`) is dropped; text
/// is folded to NFKC, so fullwidth letters read as ASCII; a numeric entity may carry any
/// number of leading zeros; and a line break after a hyphen inside a key is joined. Control:
/// the same text naming another key does not match.
#[test]
fn a_key_is_matched_through_escapes_format_characters_and_compatibility_forms() -> Result<()> {
    let synth = net(&["SYNTH"])?;
    for (text, other) in [
        ("<b SYNTH&#45;12", "<b SYNTHB&#45;12"),
        ("SYNTH%2D12", "SYNTHB%2D12"),
        (
            "https://x.example/%53YNTH-12",
            "https://x.example/%53YNTHB-12",
        ),
        ("SYNTH\u{2066}-12\u{2069}", "SYNTHB\u{2066}-12\u{2069}"),
        ("SYNTH\u{200E}-12", "SYNTHB\u{200E}-12"),
        (
            "\u{FF33}\u{FF39}\u{FF2E}\u{FF34}\u{FF28}-12",
            "\u{FF33}\u{FF39}\u{FF2E}\u{FF34}\u{FF28}\u{FF22}-12",
        ),
        ("SYNTH&#0000000045;12", "SYNTHB&#0000000045;12"),
        ("SYNTH&#x000000002d;12", "SYNTHB&#x000000002d;12"),
        ("SYNTH-\n12", "SYNTHB-\n12"),
        ("SYNTH-\r\n12", "SYNTHB-\r\n12"),
    ] {
        assert!(synth.matches(text), "{text:?}");
        assert!(!synth.matches(other), "{other:?}");
    }
    Ok(())
}

/// A value names a key when any string it holds, at any depth, does; a record when its
/// title, text, author, label, link or raw data does. The key named is the configured one.
#[test]
fn a_value_or_a_record_names_a_key_through_any_of_its_strings() -> Result<()> {
    let synth = net(&["SYNTH", "OTHER"])?;
    let key = |key: &str| Some(Hit::Key(key.to_owned()));
    assert_eq!(
        synth.hit_in_value(&json!({"a": [1, {"b": "fix synth-4"}]})),
        key("SYNTH")
    );
    assert_eq!(
        synth.hit_in_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "SYNTH-4": 1})),
        key("SYNTH")
    );
    assert_eq!(
        synth.hit_in_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "INFRA-4": 1})),
        None
    );
    let record = |title: &str, raw: serde_json::Value| crate::record::Record {
        pointer: "gmail:a".to_owned(),
        stream: "me".to_owned(),
        ts: chrono::DateTime::UNIX_EPOCH,
        label: None,
        author: None,
        author_id: None,
        thread: None,
        title: Some(title.to_owned()),
        url: None,
        text: "nothing here".to_owned(),
        aliases: Vec::new(),
        context: false,
        raw,
    };
    assert_eq!(
        synth.hit_in_record(&record("OTHER-9 plan", json!({}))),
        Some(Hit::Key("OTHER".to_owned()))
    );
    assert!(synth
        .hit_in_record(&record("plan", json!({"description": "dispatch://SYNTH"})))
        .is_some());
    assert!(synth
        .hit_in_record(&record("plan", json!({"description": "INFRA-9"})))
        .is_none());
    Ok(())
}

/// An excluded address is named whole and in any case, alone or in a header beside a display
/// name and other addresses; an address that only contains it, or shares its domain, is
/// not. A malformed entry is an error. Control: with no address, nothing is named.
#[test]
fn an_address_is_named_whole_in_any_case() -> Result<()> {
    let addresses = ContentNet::default().with_addresses(
        &["Outside@Partner.example".to_owned()],
        Some(" ,b@x.example"),
    )?;
    for text in [
        "outside@partner.example",
        "\"Out Side\" <OUTSIDE@partner.example>, ada@theorem.example",
        "mailto:outside@partner.example",
        "B@X.example",
    ] {
        assert!(addresses.names_address(text), "{text}");
        assert!(!ContentNet::default().names_address(text), "{text}");
    }
    for text in [
        "someone@partner.example",
        "notoutside@partner.example",
        "outside@partner.example.org",
        "ada@theorem.example",
    ] {
        assert!(!addresses.names_address(text), "{text}");
    }
    for refused in [
        "outside",
        "Out <outside@partner.example>",
        "a@b@partner.example",
        "a@localhost",
        "@partner.example",
    ] {
        assert!(
            ContentNet::default()
                .with_addresses(&[refused.to_owned()], None)
                .is_err(),
            "{refused}"
        );
    }
    Ok(())
}

/// A drop is logged as one JSON line naming the reason, the key when a key withheld the item,
/// and the pointer, in that order, and nothing else.
#[test]
fn a_drop_is_logged_by_reason_key_and_pointer_alone() {
    take_logged();
    withheld(&Hit::Key("SYNTH".to_owned()), "gmail:abc");
    withheld(&Hit::Address, "gcal:team@example.com/E1");
    assert_eq!(
        take_logged(),
        [
            r#"{"event":"withheld","reason":"private-content-net","key":"SYNTH","pointer":"gmail:abc"}"#,
            r#"{"event":"withheld","reason":"private-address","pointer":"gcal:team@example.com/E1"}"#,
        ]
    );
}

/// A term is matched whole, in any case, as a reader sees it: a repository as `owner/name`,
/// so also inside `github.com/owner/name` and the paths after it, and a phrase across any
/// run of whitespace or markup. A term is bounded by anything but an ASCII letter, digit, `_`
/// or `-`, so `owner/name-proofs` and a longer word are other terms, and a bare repository
/// name matches nothing ctx was not given. A drop by a term is logged without the term,
/// which may be confidential. Control: a public repository and a public phrase are not
/// matched, and with no term nothing is.
#[test]
fn a_term_is_matched_whole_in_any_case() -> Result<()> {
    let terms = ContentNet::build(
        &NetLists {
            terms: &[" Project  Falcon ".to_owned()],
            repositories: &["theorem-labs/synth".to_owned()],
            ..NetLists::default()
        },
        &|_| Ok(None),
    )?;
    for text in [
        "see theorem-labs/synth for the plan",
        "https://github.com/Theorem-Labs/Synth/pull/3",
        "git@github.com:theorem-labs/synth.git",
        "theorem-labs/<b>synth</b>",
        "the project falcon\nkickoff",
        "PROJECT FALCON",
        "<b>Project</b>&nbsp;Falcon",
    ] {
        assert_eq!(terms.hit_in(text), Some(Hit::Term), "{text:?}");
        assert_eq!(ContentNet::default().hit_in(text), None, "{text:?}");
    }
    for text in [
        "theorem-labs/synth-proofs",
        "theorem-labs/synthb",
        "xtheorem-labs/synth",
        "theorem-labs/public-repo",
        "the synth repository",
        "Project Falconer",
        "Quarterly plan",
    ] {
        assert_eq!(terms.hit_in(text), None, "{text:?}");
    }
    take_logged();
    withheld(&Hit::Term, "gmail:abc");
    assert_eq!(
        take_logged(),
        [r#"{"event":"withheld","reason":"private-term","pointer":"gmail:abc"}"#]
    );
    Ok(())
}

/// A term is bounded as a key is, by anything but an ASCII letter or digit, so `_` and `-`
/// separate it: `Falcon_plan_v2.docx` and `Falcon-team sync` name `Falcon`. A repository keeps
/// the stricter bound, where `_` and `-` continue its name, so `owner/name-proofs` and
/// `owner/name_tools` are other repositories. Control: `Falconer` and `Falcons` are other
/// words, and with no term nothing is named.
#[test]
fn a_term_is_bounded_as_a_key_is_and_a_repository_more_strictly() -> Result<()> {
    let net = ContentNet::build(
        &NetLists {
            terms: &["Falcon".to_owned()],
            repositories: &["theorem-labs/synth".to_owned()],
            ..NetLists::default()
        },
        &|_| Ok(None),
    )?;
    for text in [
        "Falcon_plan_v2.docx",
        "Falcon-team sync",
        "the falcon.",
        "theorem-labs/synth",
    ] {
        assert_eq!(net.hit_in(text), Some(Hit::Term), "{text:?}");
        assert_eq!(ContentNet::default().hit_in(text), None, "{text:?}");
    }
    for text in [
        "Falconer",
        "Falcons",
        "theorem-labs/synth-proofs",
        "theorem-labs/synth_tools",
    ] {
        assert_eq!(net.hit_in(text), None, "{text:?}");
    }
    Ok(())
}

/// A block or line-break tag reads as a line break when it is taken out, and an inline tag as
/// nothing: a key in the paragraph after `Notes` is a word of its own, and a phrase split over
/// a `<br>` is still the phrase. Control: the same text naming another key or phrase is not
/// named.
#[test]
fn a_block_tag_reads_as_a_line_break_and_an_inline_tag_as_nothing() -> Result<()> {
    let net = ContentNet::build(
        &NetLists {
            keys: &["SYNTH".to_owned()],
            terms: &["Project Falcon".to_owned()],
            ..NetLists::default()
        },
        &|_| Ok(None),
    )?;
    for (text, other) in [
        (
            "<p>Notes</p><p><b>SYNTH</b>-12</p>",
            "<p>Notes</p><p><b>SYNTHB</b>-12</p>",
        ),
        ("Notes<br><b>SYNTH</b>-12", "Notes<br><b>SYNTHB</b>-12"),
        ("Notes<BR/>SYNTH-12", "Notes<BR/>SYNTHB-12"),
        (
            "<ul><li>Notes</li><li>SYNTH-12</li></ul>",
            "<ul><li>Notes</li><li>SYNTHB-12</li></ul>",
        ),
        (
            "<td>Notes</td><td>SYNTH-12</td>",
            "<td>Notes</td><td>SYNTHB-12</td>",
        ),
        ("<h2>Notes</h2>SYNTH-12", "<h2>Notes</h2>SYNTHB-12"),
        ("Project<br>Falcon weekly", "Project<br>Falconer weekly"),
        (
            "<div>Project</div><div>Falcon</div>",
            "<div>Project</div><div>Falconer</div>",
        ),
    ] {
        assert!(net.hit_in(text).is_some(), "{text:?}");
        assert_eq!(net.hit_in(other), None, "{other:?}");
    }
    Ok(())
}

/// A repository entry is `owner/name`, and a bare name is refused: `rocq` alone would drop
/// every item that mentions the word. A term shorter than three characters is refused too.
/// `CTX_EXCLUDE_TERMS` and `CTX_EXCLUDE_REPOSITORIES` add to the configuration's lists.
#[test]
fn a_repository_is_owner_slash_name_and_a_term_is_three_characters_or_more() -> Result<()> {
    for refused in [
        "rocq",
        "theorem-labs/",
        "/synth",
        "a/b/c",
        "theorem labs/synth",
    ] {
        let lists = NetLists {
            repositories: &[refused.to_owned()],
            ..NetLists::default()
        };
        assert!(
            ContentNet::build(&lists, &|_| Ok(None)).is_err(),
            "{refused}"
        );
    }
    for refused in ["", "ab", " x "] {
        let lists = NetLists {
            terms: &[refused.to_owned()],
            ..NetLists::default()
        };
        assert!(
            ContentNet::build(&lists, &|_| Ok(None)).is_err(),
            "{refused:?}"
        );
    }
    let both = ContentNet::build(&NetLists::default(), &|_| Ok(None))?
        .with_terms(&[], Some(" Project Falcon ,, "))?
        .with_repositories(&[], Some("theorem-labs/synth"))?;
    assert_eq!(both.hit_in("Project Falcon"), Some(Hit::Term));
    assert_eq!(both.hit_in("theorem-labs/synth"), Some(Hit::Term));
    assert!(both.has_text_net());
    Ok(())
}

/// `CTX_EXCLUDE_KEYS` adds keys to the configuration's, and a key in another form is an
/// error rather than one that matches nothing.
#[test]
fn the_environment_adds_keys_to_the_configurations() -> Result<()> {
    let both = ContentNet::from_parts(&["SYNTH".to_owned()], Some(" SYNTHB ,, SYNTH"))?;
    assert!(both.matches("SYNTH-1") && both.matches("SYNTHB-1"));
    let configured = ContentNet::from_parts(&["SYNTH".to_owned()], None)?;
    assert!(!configured.matches("SYNTHB-1"));
    assert!(!ContentNet::from_parts(&[], Some(""))?.matches("SYNTH-1"));
    for refused in [
        ContentNet::from_parts(&[" ".to_owned()], None),
        ContentNet::from_parts(&["synth".to_owned()], None),
        ContentNet::from_parts(&[], Some("SYNTH-1")),
        ContentNet::from_parts(&[], Some("SYN TH")),
        ContentNet::from_parts(&[], Some("1SYNTH")),
    ] {
        assert!(refused.is_err(), "{refused:?}");
    }
    Ok(())
}
