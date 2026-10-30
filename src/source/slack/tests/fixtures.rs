//! Unit tests for the Slack source.

pub(super) use std::cell::RefCell;
pub(super) use std::collections::BTreeMap;
pub(super) use std::io::{BufRead as _, BufReader, Write as _};
pub(super) use std::net::TcpListener;
pub(super) use std::path::Path;
pub(super) use std::sync::atomic::{AtomicUsize, Ordering};
pub(super) use std::sync::{Arc, LazyLock, Mutex};
pub(super) use std::thread::JoinHandle;

pub(super) use anyhow::{anyhow, bail, Context as _, Result};
pub(super) use chrono::{DateTime, Duration, SecondsFormat, Utc};
pub(super) use serde_json::{json, Value};

pub(super) use crate::api::slack::{AppCredential, AppKind, ClaudeCodeCredential, SlackCredential};
pub(super) use crate::record::Record;

pub(super) use super::super::super::{
    DiscoveredStream, FetchRequest, Op, ResolveRequest, SourceRef, StreamStatus, PROTOCOL_VERSION,
};
pub(super) use super::super::auth::Authenticator;
pub(super) use super::super::fetch::{
    discover_streams, fetch_with_api, flag, is_channel, read_thread_index, resolve_with_api,
    update_thread_activity, write_thread_index, SlackApi, ThreadIndex, THREAD_INDEX_FILE,
};
pub(super) use super::super::records::{message_timestamp, record_from_message, slack_timestamp};
pub(super) use super::super::{
    emit_resolve_hello, parse_config, run_fetch, Message, Request, SlackConfig, SlackSource, Source,
};

pub(super) type RecordingApi = (String, Arc<Mutex<Vec<String>>>, JoinHandle<Result<()>>);

pub(super) static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
pub(super) static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(super) fn fake_http_api(responses: Vec<String>) -> Result<RecordingApi> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = std::thread::spawn(move || -> Result<()> {
        for body in responses {
            let (mut stream, _) = listener.accept()?;
            let mut request = BufReader::new(stream.try_clone()?);
            let mut first_line = String::new();
            request.read_line(&mut first_line)?;
            captured
                .lock()
                .map_err(|error| anyhow!("locking requests: {error}"))?
                .push(first_line);
            loop {
                let mut line = String::new();
                let bytes_read = request.read_line(&mut line)?;
                if bytes_read == 0 || line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes())?;
        }
        Ok(())
    });
    Ok((format!("http://{address}"), requests, server))
}

#[derive(Default)]
pub(super) struct FakeSlackApi {
    pub(super) channels: BTreeMap<String, Value>,
    /// Each channel's messages. `history` answers with those inside its inclusive
    /// `oldest..=latest` range, as `conversations.history` does with `inclusive=true`.
    pub(super) histories: BTreeMap<String, Vec<Value>>,
    pub(super) replies_by_thread: BTreeMap<(String, String), Vec<Value>>,
    pub(super) users: BTreeMap<String, Option<Value>>,
    pub(super) reply_calls: RefCell<Vec<(String, String)>>,
    pub(super) conversations: Vec<Value>,
    pub(super) list_calls: std::cell::Cell<usize>,
    /// When set, `history` overwrites this file with text that is not JSON, as a damaged
    /// file or another writer would leave it for the thread index's merge read.
    pub(super) corrupt_index: Option<std::path::PathBuf>,
    /// Every history call the fetch made.
    pub(super) history_calls: RefCell<Vec<HistoryCall>>,
}

/// One `history` call: the channel, and its `oldest` and `latest` bounds.
pub(super) type HistoryCall = (String, DateTime<Utc>, DateTime<Utc>);

impl SlackApi for FakeSlackApi {
    fn history(
        &self,
        channel: &str,
        oldest: DateTime<Utc>,
        latest: DateTime<Utc>,
    ) -> Result<Vec<Value>> {
        self.history_calls
            .borrow_mut()
            .push((channel.to_owned(), oldest, latest));
        if let Some(path) = &self.corrupt_index {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, "not json")?;
        }
        let mut found = Vec::new();
        for message in self.histories.get(channel).into_iter().flatten() {
            let sent = slack_timestamp(message_timestamp(message)?)?;
            if sent >= oldest && sent <= latest {
                found.push(message.clone());
            }
        }
        Ok(found)
    }

    fn thread_messages(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
        self.reply_calls
            .borrow_mut()
            .push((channel.to_owned(), thread_ts.to_owned()));
        Ok(self
            .replies_by_thread
            .get(&(channel.to_owned(), thread_ts.to_owned()))
            .cloned()
            .unwrap_or_default())
    }

    fn channel_info(&self, channel: &str) -> Result<Value> {
        Ok(self
            .channels
            .get(channel)
            .cloned()
            .unwrap_or_else(|| json!({"name": "general"})))
    }

    fn user_info(&self, user: &str) -> Result<Option<Value>> {
        Ok(self.users.get(user).cloned().flatten())
    }

    fn list_conversations(&self, _types: &str, exclude_archived: bool) -> Result<Vec<Value>> {
        self.list_calls.set(self.list_calls.get() + 1);
        Ok(self
            .conversations
            .iter()
            .filter(|conversation| !(exclude_archived && flag(conversation, "is_archived")))
            .cloned()
            .collect())
    }
}

pub(super) fn scratch(label: &str) -> Result<std::path::PathBuf> {
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "ctx-memory-slack-{label}-{}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

pub(super) fn at(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .with_context(|| format!("parsing test timestamp {value}"))
}

pub(super) fn slack_ts(timestamp: DateTime<Utc>) -> String {
    format!("{}.000000", timestamp.timestamp())
}

pub(super) fn fetch_request(
    root: &Path,
    window_end: DateTime<Utc>,
    channels: Vec<String>,
    discovered: Vec<DiscoveredStream>,
) -> FetchRequest {
    let channels = Value::Array(channels.into_iter().map(Value::String).collect());
    FetchRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "slack".to_owned(),
            kind: "slack".to_owned(),
            scheme: "slack".to_owned(),
        },
        config: json!({
            "workspace": "acme",
            "channels": channels,
            "thread_poll_days": 30,
        }),
        window_end,
        backfill_hours: 24,
        cursors: BTreeMap::new(),
        discovered,
        state_dir: root.join("state"),
        exclude: Vec::new(),
    }
}

/// Runs the fetch logic over explicit streams, the way the discovery step would hand them
/// in. Discovery itself is covered by `discovers_public_channels_and_listed_private_only`.
pub(super) fn fetch_messages(request: &FetchRequest, api: &FakeSlackApi) -> Result<Vec<Message>> {
    let config = parse_config(&request.config)?;
    let streams = config.channels.clone();
    let mut messages = vec![Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: streams.clone(),
    }];
    fetch_with_api(request, &config, &streams, api, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(messages)
}

pub(super) fn records(messages: &[Message]) -> Vec<&Record> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some(record.as_ref()),
            _ => None,
        })
        .collect()
}

pub(super) fn warnings(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Warning { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn write_index(root: &Path, index: &ThreadIndex) -> Result<()> {
    let state = root.join("state");
    std::fs::create_dir_all(&state)?;
    std::fs::write(state.join(THREAD_INDEX_FILE), serde_json::to_string(index)?)?;
    Ok(())
}
