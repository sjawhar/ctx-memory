//! Unit tests for the Drive source.

pub(super) use std::cell::RefCell;
pub(super) use std::collections::{BTreeMap, BTreeSet};

pub(super) use anyhow::{anyhow, Context as _, Result};
pub(super) use chrono::{DateTime, SecondsFormat, Utc};
pub(super) use serde_json::{json, Value};

pub(super) use crate::api::google::Text;
pub(super) use crate::record::Record;

pub(super) use super::super::super::{FetchRequest, SourceRef, StreamStatus, PROTOCOL_VERSION};
pub(super) use super::super::api::{Changes, DriveApi};
pub(super) use super::super::fetch::{MAX_CHANGED_FILES, STREAM_CHANGES, STREAM_MEET};
pub(super) use super::super::{emit_fetch_hello, fetch_with_api, parse_config, Message};

#[derive(Default)]
pub(super) struct FakeDriveApi {
    pub(super) calls: RefCell<Vec<String>>,
    pub(super) start_page_token: String,
    pub(super) changes: Changes,
    /// The page of the changes feed each token opens, where a test names one; any other
    /// token opens `changes`.
    pub(super) pages: BTreeMap<String, Changes>,
    /// Changes tokens Drive refuses.
    pub(super) refused_tokens: BTreeSet<String>,
    /// Query substring to the files it answers with.
    pub(super) files: Vec<(String, Vec<Value>)>,
    pub(super) exports: BTreeMap<String, (String, bool)>,
    /// Files Drive will not export, with the status it answers.
    pub(super) refused: BTreeMap<String, u16>,
    /// Files whose export fails, with the error the client returns: a refusal that is not
    /// about the file, which `google::text_reply` turns into an error.
    pub(super) failed: BTreeMap<String, String>,
    pub(super) export_limits: RefCell<Vec<usize>>,
}

impl DriveApi for FakeDriveApi {
    fn start_page_token(&self) -> Result<String> {
        self.calls.borrow_mut().push("start_page_token".to_owned());
        Ok(self.start_page_token.clone())
    }

    fn changes(&self, page_token: &str) -> Result<Option<Changes>> {
        self.calls
            .borrow_mut()
            .push(format!("changes {page_token}"));
        if self.refused_tokens.contains(page_token) {
            return Ok(None);
        }
        Ok(Some(
            self.pages.get(page_token).unwrap_or(&self.changes).clone(),
        ))
    }

    fn files(&self, query: &str) -> Result<Vec<Value>> {
        self.calls.borrow_mut().push(format!("files {query}"));
        Ok(self
            .files
            .iter()
            .find(|(pattern, _)| query.contains(pattern.as_str()))
            .map(|(_, files)| files.clone())
            .unwrap_or_default())
    }

    fn export(&self, id: &str, mime: &str, limit: usize) -> Result<Text> {
        self.calls.borrow_mut().push(format!("export {id} {mime}"));
        self.export_limits.borrow_mut().push(limit);
        if let Some(error) = self.failed.get(id) {
            return Err(anyhow!("{error}"));
        }
        if let Some(status) = self.refused.get(id) {
            return Ok(Text::Refused {
                status: *status,
                message: "refused by the fake".to_owned(),
            });
        }
        self.exports
            .get(id)
            .cloned()
            .map(|(text, truncated)| Text::Body { text, truncated })
            .ok_or_else(|| anyhow!("missing fake Drive export {id}"))
    }
}

pub(super) fn document(id: &str, name: &str) -> Value {
    document_at(id, name, "2026-10-25T17:00:00Z")
}

pub(super) fn document_at(id: &str, name: &str, modified: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "application/vnd.google-apps.document",
        "modifiedTime": modified,
        "webViewLink": format!("https://docs.google.com/document/d/{id}"),
        "lastModifyingUser": {"displayName": "Sami Jawhar"}
    })
}

pub(super) fn request(config: Value, cursor: Option<(&str, &str)>) -> Result<FetchRequest> {
    let mut cursors = BTreeMap::new();
    if let Some((stream, cursor)) = cursor {
        cursors.insert(stream.to_owned(), cursor.to_owned());
    }
    Ok(FetchRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "drive".to_owned(),
            kind: "drive".to_owned(),
            scheme: "drive".to_owned(),
        },
        config,
        window_end: DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc),
        backfill_hours: 24,
        cursors,
        discovered: Vec::new(),
        state_dir: std::env::temp_dir(),
    })
}

pub(super) fn run_fetch(api: &FakeDriveApi, request: &FetchRequest) -> Result<Vec<Message>> {
    let config = parse_config(&request.config)?;
    let mut messages = Vec::new();
    emit_fetch_hello(request, &config, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    fetch_with_api(request, &config, api, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(messages)
}

pub(super) fn records(messages: &[Message]) -> Vec<Record> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some((**record).clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn warnings(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Warning { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn stream_cursor(messages: &[Message], wanted: &str) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::StreamEnd { stream, cursor, .. } if stream == wanted => cursor.clone(),
        _ => None,
    })
}
