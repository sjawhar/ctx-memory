//! The server and the remote client share one set of routes and JSON bodies. Each body is
//! pinned here against the bytes the code before the shared module wrote, so a client and a
//! server on either side of that change still understand each other.

use anyhow::Result;
use chrono::{TimeZone, Utc};
use ctx_memory::marker::Marker;
use ctx_memory::protocol::{
    Captured, CommandRequest, Committed, DeleteRequest, Draft, Finding, MarkerRequest,
    MarkerResponse, PutRequest, Refused, COMMAND_ROUTE, DELETE_ROUTE, MARKER_ROUTE, PUT_ROUTE,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

/// Asserts `value` writes exactly `old`, the bytes the route carried before, and reads back to
/// a value that writes them again.
fn same_bytes<T>(value: &T, old: &str) -> Result<()>
where
    T: DeserializeOwned + Serialize,
{
    let written = serde_json::to_string(value)?;
    assert_eq!(written, old);
    let read: T = serde_json::from_str(&written)?;
    assert_eq!(serde_json::to_string(&read)?, old);
    Ok(())
}

fn draft() -> Draft {
    Draft {
        path: "decisions/draft.md".to_owned(),
        content: "---\ntype: decision\n---\nA draft.\n".to_owned(),
    }
}

fn finding() -> Finding {
    Finding {
        kind: "quote".to_owned(),
        path: "decisions/renewal.md".to_owned(),
        detail: "slack:C1/1.2 — the quote is not in the source".to_owned(),
    }
}

fn marker() -> Result<Marker> {
    Ok(Marker {
        version: 1,
        window: "20261028T000000Z-0001".to_owned(),
        advanced_at: Utc
            .with_ymd_and_hms(2026, 10, 28, 1, 2, 3)
            .single()
            .ok_or_else(|| anyhow::anyhow!("no such time"))?,
        by: "ada@example.com (session-7)".to_owned(),
    })
}

#[test]
fn the_routes_are_the_paths_the_server_has_always_answered() {
    assert_eq!(COMMAND_ROUTE, "/v1/command");
    assert_eq!(PUT_ROUTE, "/v1/records/put");
    assert_eq!(DELETE_ROUTE, "/v1/records/delete");
    assert_eq!(MARKER_ROUTE, "/v1/marker/advance");
}

/// The client wrote `json!({ "argv": argv, "drafts": drafts })`.
#[test]
fn a_command_request_keeps_its_bytes() -> Result<()> {
    let argv = vec!["cite".to_owned(), "verify".to_owned(), "x.md".to_owned()];
    let drafts = vec![draft()];
    let old = json!({ "argv": argv, "drafts": drafts }).to_string();
    same_bytes(&CommandRequest { argv, drafts }, &old)?;
    // A caller that sends no drafts is still understood.
    let bare: CommandRequest = serde_json::from_str(r#"{"argv":["rules"]}"#)?;
    assert!(bare.drafts.is_empty());
    Ok(())
}

/// A draft travels inside a command request, which the client built as a JSON value.
#[test]
fn a_draft_keeps_its_bytes() -> Result<()> {
    let old = json!({ "path": draft().path, "content": draft().content }).to_string();
    same_bytes(&draft(), &old)
}

/// The server answered with the typed `Captured` itself.
#[test]
fn a_captured_run_keeps_its_bytes() -> Result<()> {
    same_bytes(
        &Captured {
            stdout: "checked 1\n".to_owned(),
            stderr: "warning: \u{1b}\n".to_owned(),
            exit_code: 1,
        },
        r#"{"stdout":"checked 1\n","stderr":"warning: \u001b\n","exit_code":1}"#,
    )
}

/// The client wrote `json!({ "path": path, "content": content, "session": session })`.
#[test]
fn a_put_request_keeps_its_bytes() -> Result<()> {
    let (path, content, session) = ("decisions/renewal.md", "body", "session-7");
    let old = json!({ "path": path, "content": content, "session": session }).to_string();
    same_bytes(
        &PutRequest {
            path: path.to_owned(),
            content: content.to_owned(),
            session: Some(session.to_owned()),
        },
        &old,
    )?;
    let bare: PutRequest = serde_json::from_str(r#"{"path":"a.md","content":"b"}"#)?;
    assert_eq!(bare.session, None);
    Ok(())
}

/// The client wrote `json!({ "path": path, "session": session })`.
#[test]
fn a_delete_request_keeps_its_bytes() -> Result<()> {
    let (path, session) = ("decisions/renewal.md", "session-7");
    let old = json!({ "path": path, "session": session }).to_string();
    same_bytes(
        &DeleteRequest {
            path: path.to_owned(),
            session: Some(session.to_owned()),
        },
        &old,
    )?;
    let bare: DeleteRequest = serde_json::from_str(r#"{"path":"a.md"}"#)?;
    assert_eq!(bare.session, None);
    Ok(())
}

/// The client wrote `json!({ "window": window, "session": session })`.
#[test]
fn a_marker_request_keeps_its_bytes() -> Result<()> {
    let (window, session) = ("20261028T000000Z-0001", "session-7");
    let old = json!({ "window": window, "session": session }).to_string();
    same_bytes(
        &MarkerRequest {
            window: window.to_owned(),
            session: Some(session.to_owned()),
        },
        &old,
    )?;
    let bare: MarkerRequest = serde_json::from_str(r#"{"window":"w"}"#)?;
    assert_eq!(bare.session, None);
    Ok(())
}

/// A finding travels inside a refusal, which the server built as a JSON value.
#[test]
fn a_finding_keeps_its_bytes() -> Result<()> {
    let found = finding();
    let old = json!({ "kind": found.kind, "path": found.path, "detail": found.detail }).to_string();
    same_bytes(&finding(), &old)
}

/// The server answered `json!({ "commit": commit })`.
#[test]
fn a_committed_write_keeps_its_bytes() -> Result<()> {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let old = json!({ "commit": commit }).to_string();
    same_bytes(
        &Committed {
            commit: commit.to_owned(),
        },
        &old,
    )
}

/// The server answered `json!({ "findings": findings })`.
#[test]
fn a_refused_write_keeps_its_bytes() -> Result<()> {
    let findings = vec![finding(), finding()];
    let old = json!({ "findings": findings }).to_string();
    same_bytes(&Refused { findings }, &old)
}

/// The server answered `json!({ "marker": marker })`, which sorts the marker's keys; the marker
/// file keeps the marker's own order.
#[test]
fn a_marker_answer_keeps_its_bytes() -> Result<()> {
    let old = json!({ "marker": marker()? }).to_string();
    same_bytes(&MarkerResponse { marker: marker()? }, &old)?;
    assert_eq!(
        old,
        r#"{"marker":{"advanced_at":"2026-10-28T01:02:03Z","by":"ada@example.com (session-7)","version":1,"window":"20261028T000000Z-0001"}}"#
    );
    assert!(serde_json::to_string(&marker()?)?.starts_with(r#"{"version":1,"window":"#));
    Ok(())
}
