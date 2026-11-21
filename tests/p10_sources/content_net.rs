//! `CTX_EXCLUDE_KEYS` in the environment of the process that runs the Gmail, Drive and Calendar
//! sources: each drops an item whose text names an excluded key before it is a record.

use anyhow::Result;
use ctx_memory::source::{self, Request};
use serde_json::{json, Value};

use crate::fixtures::{fetch_request, remove_scratch, run_conformance, scratch, source_spec};
use crate::support::fake_google::{clear_google_environment, environment_lock, FakeGoogle};

/// One built-in source's run: its kind, scheme, `[[sources]]` settings and request
/// configuration, the stream and cursor it starts from, and what the fake Google answers.
struct Run<'a> {
    kind: &'a str,
    scheme: &'a str,
    settings: &'a str,
    config: Value,
    cursor: Option<(&'a str, &'a str)>,
    routes: Vec<(&'static str, Value)>,
}

impl Run<'_> {
    /// Runs the source once, with `CTX_EXCLUDE_KEYS=OTHER,SYNTH` set when `excluded`, and
    /// returns the pointers it stored, sorted.
    fn stored(self, excluded: bool) -> Result<Vec<String>> {
        let root = scratch(&format!("content-net-{}-{excluded}", self.kind))?;
        let server = FakeGoogle::start(self.routes)?;
        server.use_in_environment("ya29.content-net-test");
        if excluded {
            std::env::set_var("CTX_EXCLUDE_KEYS", "OTHER,SYNTH");
        }
        let outcome = (|| {
            let spec = source_spec(self.kind, self.settings)?;
            let mut request = fetch_request(&root, self.kind, self.scheme, self.config)?;
            if let (Request::Fetch(fetch), Some((stream, at))) = (&mut request, self.cursor) {
                fetch.cursors.insert(stream.to_owned(), at.to_owned());
            }
            let source = source::open(&spec, self.scheme)?;
            anyhow::Ok(run_conformance(source.as_ref(), &request))
        })();
        std::env::remove_var("CTX_EXCLUDE_KEYS");
        clear_google_environment();
        remove_scratch(&root)?;
        let outcome = outcome?;
        assert!(!outcome.has_errors(), "{}: {:#?}", self.kind, outcome.error);
        let mut pointers = outcome
            .records
            .iter()
            .map(|record| record.pointer.clone())
            .collect::<Vec<_>>();
        pointers.sort();
        Ok(pointers)
    }
}

/// A Gmail message in `full` format with `subject`.
fn mail(id: &str, subject: &str) -> Value {
    json!({
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": "1792944000000",
        "payload": {
            "headers": [{"name": "Subject", "value": subject}],
            "mimeType": "text/plain",
            "body": {"data": "Zmlyc3Q="}
        }
    })
}

/// The Gmail source drops, from its history, a message whose subject names an excluded issue.
/// Control: without the variable it stores it.
#[test]
fn gmail_drops_mail_that_names_a_key_the_environment_excludes() -> Result<()> {
    let _environment = environment_lock();
    for excluded in [true, false] {
        let stored = Run {
            kind: "gmail",
            scheme: "gmail",
            settings: "",
            config: json!({}),
            cursor: Some(("me", "850@2026-10-24T00:00:00Z")),
            routes: vec![
                (
                    "/gmail/v1/users/me/history",
                    json!({
                        "history": [{"id": "900", "messagesAdded": [
                            {"message": {"id": "dddddddddddddddd"}},
                            {"message": {"id": "aaaaaaaaaaaaaaaa"}}
                        ]}],
                        "historyId": "1000"
                    }),
                ),
                (
                    "/gmail/v1/users/me/messages/dddddddddddddddd",
                    mail("dddddddddddddddd", "Re: SYNTH-12 rollout"),
                ),
                (
                    "/gmail/v1/users/me/messages/aaaaaaaaaaaaaaaa",
                    mail("aaaaaaaaaaaaaaaa", "Lunch"),
                ),
            ],
        }
        .stored(excluded)?;
        let want: &[&str] = if excluded {
            &["gmail:aaaaaaaaaaaaaaaa"]
        } else {
            &["gmail:aaaaaaaaaaaaaaaa", "gmail:dddddddddddddddd"]
        };
        assert_eq!(stored, want, "excluded: {excluded}");
    }
    Ok(())
}

/// The Drive source drops, from a first fetch's listing, a document whose exported text links
/// into an excluded project. Control: without the variable it stores it.
#[test]
fn drive_drops_a_file_that_names_a_key_the_environment_excludes() -> Result<()> {
    let _environment = environment_lock();
    let document = |id: &str, name: &str| {
        json!({
            "id": id,
            "name": name,
            "mimeType": "application/vnd.google-apps.document",
            "modifiedTime": "2026-10-25T17:00:00Z"
        })
    };
    for excluded in [true, false] {
        let stored = Run {
            kind: "drive",
            scheme: "drive",
            settings: "meet = false\n",
            config: json!({ "meet": false }),
            cursor: None,
            routes: vec![
                (
                    "/drive/v3/changes/startPageToken",
                    json!({"startPageToken": "token-1"}),
                ),
                (
                    "/drive/v3/files/1WeeklyNotesExample000000000/export",
                    json!("see dispatch://SYNTH/artifact/plan"),
                ),
                (
                    "/drive/v3/files/1QuarterlyPlanExample0000000/export",
                    json!("ship in March"),
                ),
                (
                    "/drive/v3/files",
                    json!({"files": [
                        document("1WeeklyNotesExample000000000", "Weekly notes"),
                        document("1QuarterlyPlanExample0000000", "Quarterly plan"),
                    ]}),
                ),
            ],
        }
        .stored(excluded)?;
        let want: &[&str] = if excluded {
            &["drive:1QuarterlyPlanExample0000000"]
        } else {
            &[
                "drive:1QuarterlyPlanExample0000000",
                "drive:1WeeklyNotesExample000000000",
            ]
        };
        assert_eq!(stored, want, "excluded: {excluded}");
    }
    Ok(())
}

/// The Calendar source drops, from a first fetch's listing by time, an event whose description
/// names an excluded issue. Control: without the variable it stores it.
#[test]
fn calendar_drops_an_event_that_names_a_key_the_environment_excludes() -> Result<()> {
    let _environment = environment_lock();
    let event = |id: &str, description: &str| {
        json!({
            "id": id,
            "summary": "Sync",
            "description": description,
            "start": {"dateTime": "2026-10-25T09:00:00Z"},
            "end": {"dateTime": "2026-10-25T10:00:00Z"}
        })
    };
    for excluded in [true, false] {
        let stored = Run {
            kind: "calendar",
            scheme: "gcal",
            settings: "calendars = [\"primary\"]\n",
            config: json!({ "calendars": ["primary"] }),
            cursor: None,
            routes: vec![(
                "/calendar/v3/calendars/primary/events",
                json!({
                    "items": [
                        event("EVENT-SYNTH", "Agenda: SYNTH-12"),
                        event("EVENT-OTHER", "Agenda"),
                    ],
                    "nextSyncToken": "sync-1"
                }),
            )],
        }
        .stored(excluded)?;
        let want: &[&str] = if excluded {
            &["gcal:primary/EVENT-OTHER"]
        } else {
            &["gcal:primary/EVENT-OTHER", "gcal:primary/EVENT-SYNTH"]
        };
        assert_eq!(stored, want, "excluded: {excluded}");
    }
    Ok(())
}
