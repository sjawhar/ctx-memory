//! Fixtures the Gmail source's unit tests share.

pub(super) use std::cell::RefCell;
pub(super) use std::collections::{BTreeMap, BTreeSet};

pub(super) use anyhow::bail;
pub(super) use chrono::{DateTime, Duration, Utc};
pub(super) use serde_json::json;

pub(super) use super::super::super::content_net::ContentNet;
pub(super) use super::super::super::{
    FetchRequest, ResolveRequest, StreamStatus, PROTOCOL_VERSION,
};
pub(super) use super::super::fetch::{
    excluded_labels, fetch_messages, Excluded, GmailApi, History, HistoryEntry, Reach, Withheld,
    MAX_HISTORY_MESSAGES,
};
pub(super) use super::super::lists::ExcludedLists;
pub(super) use super::super::render::required_string;
pub(super) use super::super::*;

#[derive(Default)]
pub(super) struct FakeGmailApi {
    pub(super) calls: RefCell<Vec<String>>,
    pub(super) candidates: Vec<Value>,
    pub(super) messages: BTreeMap<String, Value>,
    pub(super) threads: BTreeMap<String, Vec<Value>>,
    pub(super) history: Option<History>,
    /// The history each start ID opens, where a test names one; any other ID opens
    /// `history`.
    pub(super) histories: BTreeMap<String, History>,
    /// History IDs Gmail no longer holds.
    pub(super) expired: BTreeSet<String>,
    pub(super) latest_history_id: String,
    /// The body of each part Gmail keeps apart, base64url, by message and attachment id.
    pub(super) attachments: BTreeMap<(String, String), String>,
}

impl GmailApi for FakeGmailApi {
    fn list_messages(&self, query: &str) -> Result<Vec<Value>> {
        self.calls.borrow_mut().push(format!("list {query}"));
        Ok(self.candidates.clone())
    }

    fn message(&self, id: &str) -> Result<Option<Value>> {
        self.calls.borrow_mut().push(format!("messages get {id}"));
        // A message the mailbox no longer holds is what Gmail answers 404 for.
        Ok(self.messages.get(id).cloned())
    }

    fn thread(&self, id: &str) -> Result<Vec<Value>> {
        self.calls.borrow_mut().push(format!("threads get {id}"));
        Ok(self.threads.get(id).cloned().unwrap_or_default())
    }

    fn history(&self, start: &str) -> Result<Option<History>> {
        self.calls.borrow_mut().push(format!("history {start}"));
        Ok(self
            .histories
            .get(start)
            .cloned()
            .or_else(|| self.history.clone()))
    }

    fn holds_history(&self, start: &str) -> Result<bool> {
        self.calls.borrow_mut().push(format!("holds {start}"));
        Ok(!self.expired.contains(start))
    }

    fn latest_history_id(&self) -> Result<String> {
        self.calls.borrow_mut().push("profile".to_owned());
        Ok(self.latest_history_id.clone())
    }

    fn attachment(&self, message: &str, attachment: &str) -> Result<String> {
        self.calls
            .borrow_mut()
            .push(format!("attachments get {message} {attachment}"));
        self.attachments
            .get(&(message.to_owned(), attachment.to_owned()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing fake attachment {attachment} of {message}"))
    }
}

pub(super) fn message(id: &str, internal_date: &str, body: &str) -> Value {
    labelled_message(id, internal_date, body, &[])
}

pub(super) fn labelled_message(
    id: &str,
    internal_date: &str,
    body: &str,
    labels: &[&str],
) -> Value {
    json!({
        "labelIds": labels,
        "id": id,
        "threadId": "cccccccccccccccc",
        "internalDate": internal_date,
        "payload": {
            "headers": [{"name": "Subject", "value": id}],
            "mimeType": "text/plain",
            "body": {"data": body}
        }
    })
}

pub(super) fn fetch_request(cursor: Option<&str>) -> Result<FetchRequest> {
    let mut cursors = BTreeMap::new();
    if let Some(cursor) = cursor {
        cursors.insert(STREAM.to_owned(), cursor.to_owned());
    }
    Ok(FetchRequest {
        protocol: PROTOCOL_VERSION,
        source: crate::source::SourceRef {
            id: "gmail".to_owned(),
            kind: "gmail".to_owned(),
            scheme: "gmail".to_owned(),
        },
        config: json!({}),
        window_end: DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc),
        backfill_hours: 24,
        cursors,
        discovered: Vec::new(),
        state_dir: std::env::temp_dir(),
        exclude: Vec::new(),
    })
}

/// The mail the source withholds under `config`, read with no environment.
pub(super) fn withheld_by(config: &Value) -> Result<Withheld> {
    let config = parse_config(config)?;
    Ok(Withheld {
        lists: ExcludedLists::from_parts(&config.exclude_list_ids, None)?,
        net: ContentNet::build(&config.net_lists(), &|_| Ok(None))?,
    })
}

/// Runs a fetch with the request's configuration, `exclude_list_ids` and `exclude_keys` among
/// it, and no environment.
pub(super) fn run_fetch(api: &FakeGmailApi, request: &FetchRequest) -> Result<Vec<Message>> {
    let config = parse_config(&request.config)?;
    let withheld = withheld_by(&request.config)?;
    let mut messages = Vec::new();
    fetch_with_api(request, &config, &withheld, api, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(messages)
}

pub(super) fn pointers(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some(record.pointer.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn records(messages: &[Message]) -> Vec<crate::record::Record> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some((**record).clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn stream_end(messages: &[Message]) -> Option<(StreamStatus, Option<String>)> {
    messages.iter().find_map(|message| match message {
        Message::StreamEnd { status, cursor, .. } => Some((*status, cursor.clone())),
        _ => None,
    })
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

/// A message that carries `header` (its name and value) beside its subject.
pub(super) fn with_header(mut message: Value, name: &str, value: &str) -> Value {
    if let Some(headers) = message
        .pointer_mut("/payload/headers")
        .and_then(Value::as_array_mut)
    {
        headers.push(json!({"name": name, "value": value}));
    }
    message
}

/// What each path that reads a message stores of `messages` under `config`: a history walk, a
/// listing, a resolve of each message, and a resolve of their thread.
pub(super) fn stored_on_each_path(
    config: &Value,
    messages: &[Value],
) -> Result<Vec<(&'static str, Vec<String>)>> {
    stored_on_each_path_with(config, messages, BTreeMap::new())
}

/// [`stored_on_each_path`], with Gmail keeping `attachments` apart from the messages.
pub(super) fn stored_on_each_path_with(
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
pub(super) fn assert_each_path_stores(stored: &[(&str, Vec<String>)], want: &[&str], case: &str) {
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
