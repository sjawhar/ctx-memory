//! Built-in Slack source.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::atomic::write_replace;

use crate::api::http::env_token;
use crate::api::slack::{html_unescape, SlackClient};
use crate::record::Record;

use super::{
    window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source, StreamStatus,
    PROTOCOL_VERSION,
};

const DEFAULT_TOKEN_ENV: &str = "SLACK_TOKEN";
const DEFAULT_THREAD_POLL_DAYS: u32 = 30;
const THREAD_INDEX_FILE: &str = "threads.json";

/// Built-in source for Slack conversations.
pub struct SlackSource {
    api: Option<String>,
}

impl SlackSource {
    /// Creates a Slack source using Slack's production API endpoint.
    #[must_use]
    pub const fn new() -> Self {
        Self { api: None }
    }

    /// Creates a Slack source using an alternate API endpoint.
    #[must_use]
    pub fn with_api(api: impl Into<String>) -> Self {
        Self {
            api: Some(api.into()),
        }
    }
}

impl Default for SlackSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for SlackSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                let streams = emit_fetch_hello(request, &config, sink)?;
                let token = env_token(&config.token_env, "ctx fetch")?;
                if let Some(api) = &self.api {
                    let client = SlackClient::with_api(&token, api);
                    fetch_with_api(request, &config, &streams, &client, sink)
                } else {
                    let client = SlackClient::new(&token);
                    fetch_with_api(request, &config, &streams, &client, sink)
                }
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)?;
                let token = env_token(&config.token_env, "ctx resolve")?;
                if let Some(api) = &self.api {
                    let client = SlackClient::with_api(&token, api);
                    resolve_with_api(request, &config, &client, sink)
                } else {
                    let client = SlackClient::new(&token);
                    resolve_with_api(request, &config, &client, sink)
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackConfig {
    #[serde(default = "default_token_env")]
    token_env: String,
    workspace: String,
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default = "default_thread_poll_days")]
    thread_poll_days: u32,
}

fn default_token_env() -> String {
    DEFAULT_TOKEN_ENV.to_owned()
}

const fn default_thread_poll_days() -> u32 {
    DEFAULT_THREAD_POLL_DAYS
}

trait SlackApi {
    fn history(
        &self,
        channel: &str,
        oldest: DateTime<Utc>,
        latest: DateTime<Utc>,
    ) -> Result<Vec<Value>>;
    fn thread_messages(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>>;
    fn channel_info(&self, channel: &str) -> Result<Value>;
    fn user_info(&self, user: &str) -> Result<Option<Value>>;
}

impl SlackApi for SlackClient<'_> {
    fn history(
        &self,
        channel: &str,
        oldest: DateTime<Utc>,
        latest: DateTime<Utc>,
    ) -> Result<Vec<Value>> {
        Self::history(self, channel, oldest, latest)
    }

    fn thread_messages(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
        Self::thread_messages(self, channel, thread_ts)
    }

    fn channel_info(&self, channel: &str) -> Result<Value> {
        Self::channel_info(self, channel)
    }

    fn user_info(&self, user: &str) -> Result<Option<Value>> {
        match self.call("users.info", &[("user", user.to_owned())]) {
            Ok(body) => body
                .get("user")
                .cloned()
                .map(Some)
                .ok_or_else(|| anyhow!("slack users.info {user}: missing 'user' in response")),
            Err(error) if is_absent_user(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn is_absent_user(error: &anyhow::Error) -> bool {
    matches!(
        error.to_string().as_str(),
        r#"slack users.info: "user_not_found""# | r#"slack users.info: "users_not_found""#
    )
}

fn parse_config(value: &Value) -> Result<SlackConfig> {
    let config: SlackConfig =
        serde_json::from_value(value.clone()).context("parsing Slack source configuration")?;
    if config.token_env.is_empty() {
        bail!("Slack source token_env must not be empty");
    }
    if config.workspace.is_empty() {
        bail!("Slack source workspace must not be empty");
    }
    Ok(config)
}

fn emit_fetch_hello(
    request: &FetchRequest,
    config: &SlackConfig,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<Vec<String>> {
    let (streams, warnings) = fetch_streams(config, &request.discovered);
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: streams.clone(),
    })?;
    for warning in warnings {
        sink(Message::Warning {
            stream: None,
            message: warning,
        })?;
    }
    Ok(streams)
}

fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch, Op::Resolve],
        verbatim: true,
        streams: Vec::new(),
    })
}

fn fetch_streams(
    config: &SlackConfig,
    discovered: &[super::DiscoveredStream],
) -> (Vec<String>, Vec<String>) {
    let mut streams = BTreeSet::new();
    let mut warnings = Vec::new();
    for channel in &config.channels {
        if is_channel(channel) {
            streams.insert(channel.clone());
        } else {
            warnings.push(format!(
                "ignoring malformed configured Slack channel {channel:?}"
            ));
        }
    }
    for channel in discovered {
        if is_channel(&channel.stream) {
            streams.insert(channel.stream.clone());
        } else {
            warnings.push(format!(
                "ignoring malformed discovered Slack channel {:?}",
                channel.stream
            ));
        }
    }
    (streams.into_iter().collect(), warnings)
}

fn is_channel(channel: &str) -> bool {
    let mut bytes = channel.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    channel.len() >= 9
        && matches!(first, b'C' | b'D' | b'G')
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn fetch_with_api<C: SlackApi + ?Sized>(
    request: &FetchRequest,
    config: &SlackConfig,
    streams: &[String],
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut thread_index = read_thread_index(&request.state_dir)?;
    let mut skipped_threads = 0_usize;

    for channel in streams {
        let result = window_start(
            request.cursors.get(channel).map(String::as_str),
            request.window_end,
            request.backfill_hours,
        )
        .and_then(|start| {
            fetch_channel(
                api,
                channel,
                start,
                request.window_end,
                config,
                &request.source.scheme,
                &mut thread_index,
            )
        });
        match result {
            Ok((records, skipped)) => {
                skipped_threads += skipped;
                for record in records {
                    sink(Message::Record(Box::new(record)))?;
                }
                sink(Message::StreamEnd {
                    stream: channel.clone(),
                    status: StreamStatus::Ok,
                    cursor: Some(
                        request
                            .window_end
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                    ),
                    error: None,
                })?;
            }
            Err(error) => {
                sink(Message::StreamEnd {
                    stream: channel.clone(),
                    status: StreamStatus::Error,
                    cursor: None,
                    error: Some(format!("{error:#}")),
                })?;
            }
        }
    }

    if skipped_threads > 0 {
        sink(Message::Warning {
            stream: None,
            message: format!(
                "{skipped_threads} tracked thread(s) idle over {} days not polled",
                config.thread_poll_days
            ),
        })?;
    }
    write_thread_index(&request.state_dir, &thread_index)
}

fn fetch_channel<C: SlackApi + ?Sized>(
    api: &C,
    channel: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    config: &SlackConfig,
    scheme: &str,
    thread_index: &mut ThreadIndex,
) -> Result<(Vec<Record>, usize)> {
    let info = api
        .channel_info(channel)
        .with_context(|| format!("reading Slack channel {channel}"))?;
    let channel_name = info.get("name").and_then(Value::as_str).unwrap_or(channel);
    let label = format!("#{channel_name}");
    let history = api
        .history(channel, start, end)
        .with_context(|| format!("reading Slack history for {channel}"))?;
    let horizon = end - Duration::days(i64::from(config.thread_poll_days));
    let mut reply_parents = BTreeSet::new();
    let mut skipped_threads = 0_usize;
    let mut messages = Vec::new();
    let mut emitted = BTreeSet::new();

    if let Some(tracked) = thread_index.get(channel) {
        for (parent, latest) in tracked {
            let latest_at = slack_timestamp(latest)
                .with_context(|| format!("parsing tracked Slack thread {channel}/{parent}"))?;
            if latest_at >= horizon {
                reply_parents.insert(parent.clone());
            } else {
                skipped_threads += 1;
            }
        }
    }

    for message in history {
        let timestamp = message_timestamp(&message)?;
        if is_thread_parent(&message) {
            if let Some(latest) = message.get("latest_reply").and_then(Value::as_str) {
                update_thread_activity(thread_index, channel, timestamp, latest)?;
                if slack_timestamp(latest)? >= horizon {
                    reply_parents.insert(timestamp.to_owned());
                }
            }
        }
        if inside_window(&message, start, end)? && emitted.insert(timestamp.to_owned()) {
            messages.push(message);
        }
    }

    let mut replies = Vec::new();
    let mut contexts = Vec::new();
    for parent in reply_parents {
        let found = api
            .thread_messages(channel, &parent)
            .with_context(|| format!("reading Slack replies for {channel}/{parent}"))?;
        let mut parent_message = None;
        let mut emitted_reply = false;
        for reply in found {
            let timestamp = message_timestamp(&reply)?;
            if timestamp == parent {
                parent_message = Some(reply);
                continue;
            }
            update_thread_activity(thread_index, channel, &parent, timestamp)?;
            if inside_window(&reply, start, end)? && emitted.insert(timestamp.to_owned()) {
                emitted_reply = true;
                replies.push(reply);
            }
        }
        if emitted_reply && !emitted.contains(&parent) {
            contexts.push(parent_message.ok_or_else(|| {
                anyhow!("Slack thread {channel}/{parent} did not return its parent message")
            })?);
        }
    }

    let names = resolve_user_names(
        api,
        contexts.iter().chain(messages.iter()).chain(replies.iter()),
    )?;
    let mut records = Vec::with_capacity(contexts.len() + messages.len() + replies.len());
    for message in contexts {
        let mut record = record_from_message(&message, channel, &label, &names, &config.workspace)?;
        record.context = true;
        apply_scheme(&mut record, scheme);
        records.push(record);
    }
    for message in messages.into_iter().chain(replies) {
        let mut record = record_from_message(&message, channel, &label, &names, &config.workspace)?;
        apply_scheme(&mut record, scheme);
        records.push(record);
    }
    Ok((records, skipped_threads))
}

fn is_thread_parent(message: &Value) -> bool {
    let has_replies = message
        .get("reply_count")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0)
        || message
            .get("latest_reply")
            .and_then(Value::as_str)
            .is_some();
    let timestamp = message.get("ts").and_then(Value::as_str);
    let thread = message.get("thread_ts").and_then(Value::as_str);
    has_replies && (thread.is_none() || thread == timestamp)
}

fn resolve_with_api<C: SlackApi + ?Sized>(
    request: &ResolveRequest,
    config: &SlackConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut found: Vec<(String, String, Value)> = Vec::new();
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for pointer in &request.pointers {
        let Some((channel, timestamp)) = parse_pointer(pointer, &request.source.scheme) else {
            continue;
        };
        let messages = resolve_messages(api, channel, timestamp)?;
        if messages.is_empty() {
            continue;
        }
        let label = if let Some(label) = labels.get(channel) {
            label.clone()
        } else {
            let info = api
                .channel_info(channel)
                .with_context(|| format!("reading Slack channel {channel}"))?;
            let name = info.get("name").and_then(Value::as_str).unwrap_or(channel);
            let label = format!("#{name}");
            labels.insert(channel.to_owned(), label.clone());
            label
        };
        for message in messages {
            found.push((channel.to_owned(), label.clone(), message));
        }
    }
    let names = resolve_user_names(api, found.iter().map(|(_, _, message)| message))?;
    for (channel, label, message) in found {
        let mut record =
            record_from_message(&message, &channel, &label, &names, &config.workspace)?;
        apply_scheme(&mut record, &request.source.scheme);
        sink(Message::Record(Box::new(record)))?;
    }
    Ok(())
}

fn parse_pointer<'a>(pointer: &'a str, scheme: &str) -> Option<(&'a str, &'a str)> {
    let locator = pointer.strip_prefix(scheme)?.strip_prefix(':')?;
    let (channel, timestamp) = locator.split_once('/')?;
    (is_channel(channel) && is_timestamp(timestamp)).then_some((channel, timestamp))
}

fn resolve_messages<C: SlackApi + ?Sized>(
    api: &C,
    channel: &str,
    timestamp: &str,
) -> Result<Vec<Value>> {
    let mut messages = api.thread_messages(channel, timestamp).unwrap_or_default();
    if !messages
        .iter()
        .any(|message| message.get("ts").and_then(Value::as_str) == Some(timestamp))
    {
        let timestamp_at = slack_timestamp(timestamp)?;
        messages.extend(
            api.history(channel, timestamp_at, timestamp_at)
                .with_context(|| format!("reading Slack message {channel}/{timestamp}"))?,
        );
    }
    if !messages
        .iter()
        .any(|message| message.get("ts").and_then(Value::as_str) == Some(timestamp))
    {
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    Ok(messages
        .into_iter()
        .filter(|message| {
            message
                .get("ts")
                .and_then(Value::as_str)
                .is_some_and(|value| seen.insert(value.to_owned()))
        })
        .collect())
}

fn resolve_user_names<'a, C, I>(api: &C, messages: I) -> Result<BTreeMap<String, String>>
where
    C: SlackApi + ?Sized,
    I: IntoIterator<Item = &'a Value>,
{
    let user_ids = messages
        .into_iter()
        .filter_map(|message| message.get("user").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut names = BTreeMap::new();
    for user_id in user_ids {
        if let Some(user) = api
            .user_info(&user_id)
            .with_context(|| format!("reading Slack user {user_id}"))?
        {
            names.insert(user_id.clone(), display_name(&user, &user_id));
        }
    }
    Ok(names)
}

fn display_name(user: &Value, fallback: &str) -> String {
    let profile = user.get("profile");
    [
        profile
            .and_then(|value| value.get("display_name"))
            .and_then(Value::as_str),
        profile
            .and_then(|value| value.get("real_name"))
            .and_then(Value::as_str),
        user.get("real_name").and_then(Value::as_str),
        user.get("name").and_then(Value::as_str),
        user.get("id").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .find(|name| !name.is_empty())
    .map_or_else(|| fallback.to_owned(), str::to_owned)
}

/// Converts one Slack API message into a normalized source record.
///
/// # Errors
///
/// Returns an error when the message does not contain a valid Slack timestamp.
pub fn record_from_message(
    message: &Value,
    channel: &str,
    label: &str,
    names: &BTreeMap<String, String>,
    workspace: &str,
) -> Result<Record> {
    let timestamp = message_timestamp(message)?;
    let sent_at = slack_timestamp(timestamp)?;
    let author_id = message
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let author = author_id
        .as_deref()
        .map(|id| names.get(id).map_or_else(|| id.to_owned(), Clone::clone));
    let thread_ts = message.get("thread_ts").and_then(Value::as_str);
    let thread = thread_ts
        .filter(|parent| *parent != timestamp)
        .map(|parent| format!("slack:{channel}/{parent}"));
    Ok(Record {
        pointer: format!("slack:{channel}/{timestamp}"),
        stream: channel.to_owned(),
        ts: sent_at,
        label: Some(label.to_owned()),
        author,
        author_id,
        thread,
        title: None,
        url: Some(permalink(workspace, channel, timestamp, thread_ts)),
        text: render_slack_text(
            message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            names,
        ),
        aliases: vec![timestamp.to_owned()],
        context: false,
        raw: message.clone(),
    })
}

fn apply_scheme(record: &mut Record, scheme: &str) {
    if scheme == "slack" {
        return;
    }
    record.pointer = rescheme(&record.pointer, scheme);
    if let Some(thread) = record.thread.as_deref() {
        record.thread = Some(rescheme(thread, scheme));
    }
}

fn rescheme(pointer: &str, scheme: &str) -> String {
    pointer.strip_prefix("slack:").map_or_else(
        || pointer.to_owned(),
        |locator| format!("{scheme}:{locator}"),
    )
}

fn render_slack_text(text: &str, names: &BTreeMap<String, String>) -> String {
    let decoded = html_unescape(text)
        .replace("<!channel>", "@channel")
        .replace("<!here>", "@here");
    let mut rendered = String::with_capacity(decoded.len());
    let mut remaining = decoded.as_str();
    while let Some((prefix, after_marker)) = remaining.split_once("<@") {
        rendered.push_str(prefix);
        let Some((id, rest)) = after_marker.split_once('>') else {
            rendered.push_str("<@");
            rendered.push_str(after_marker);
            return rendered;
        };
        if id.is_empty() {
            rendered.push_str("<@>");
        } else {
            rendered.push('@');
            rendered.push_str(names.get(id).map_or(id, String::as_str));
        }
        remaining = rest;
    }
    rendered.push_str(remaining);
    rendered
}

fn permalink(workspace: &str, channel: &str, timestamp: &str, thread_ts: Option<&str>) -> String {
    let mut url = format!(
        "https://{workspace}.slack.com/archives/{channel}/p{}",
        timestamp.replace('.', "")
    );
    if let Some(parent) = thread_ts.filter(|parent| *parent != timestamp) {
        url.push_str("?thread_ts=");
        url.push_str(parent);
        url.push_str("&cci=1");
    }
    url
}

fn inside_window(message: &Value, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<bool> {
    let timestamp = slack_timestamp(message_timestamp(message)?)?;
    Ok(timestamp > start && timestamp <= end)
}

fn message_timestamp(message: &Value) -> Result<&str> {
    let timestamp = message
        .get("ts")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Slack message is missing ts"))?;
    is_timestamp(timestamp)
        .then_some(timestamp)
        .ok_or_else(|| anyhow!("Slack message timestamp {timestamp:?} is invalid"))
}

fn is_timestamp(timestamp: &str) -> bool {
    timestamp.len() == 17
        && timestamp.bytes().enumerate().all(|(index, byte)| {
            (index == 10 && byte == b'.') || (index != 10 && byte.is_ascii_digit())
        })
}

fn slack_timestamp(timestamp: &str) -> Result<DateTime<Utc>> {
    if !is_timestamp(timestamp) {
        bail!("Slack timestamp {timestamp:?} is invalid");
    }
    let seconds = timestamp
        .get(..10)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?
        .parse::<i64>()
        .with_context(|| format!("parsing Slack timestamp {timestamp:?}"))?;
    let micros = timestamp
        .get(11..)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?
        .parse::<u32>()
        .with_context(|| format!("parsing Slack timestamp {timestamp:?}"))?;
    let nanos = micros
        .checked_mul(1_000)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))?;
    DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| anyhow!("Slack timestamp {timestamp:?} is invalid"))
}

fn update_thread_activity(
    index: &mut ThreadIndex,
    channel: &str,
    parent: &str,
    latest: &str,
) -> Result<()> {
    let latest_at = slack_timestamp(latest)?;
    let replace = match index.get(channel).and_then(|threads| threads.get(parent)) {
        Some(current) => slack_timestamp(current)? < latest_at,
        None => true,
    };
    if replace {
        index
            .entry(channel.to_owned())
            .or_default()
            .insert(parent.to_owned(), latest.to_owned());
    }
    Ok(())
}

type ThreadIndex = BTreeMap<String, BTreeMap<String, String>>;

fn read_thread_index(state_dir: &Path) -> Result<ThreadIndex> {
    let path = state_dir.join(THREAD_INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(ThreadIndex::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_thread_index(state_dir: &Path, index: &ThreadIndex) -> Result<()> {
    let path = state_dir.join(THREAD_INDEX_FILE);
    let text =
        serde_json::to_string(index).with_context(|| format!("serializing {}", path.display()))?;
    write_replace(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, LazyLock, Mutex};
    use std::thread::JoinHandle;

    use super::*;
    use serde_json::json;

    type RecordingApi = (String, Arc<Mutex<Vec<String>>>, JoinHandle<Result<()>>);

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn fake_http_api(responses: Vec<String>) -> Result<RecordingApi> {
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
    struct FakeSlackApi {
        channels: BTreeMap<String, Value>,
        histories: BTreeMap<(String, i64, i64), Vec<Value>>,
        replies_by_thread: BTreeMap<(String, String), Vec<Value>>,
        users: BTreeMap<String, Option<Value>>,
        reply_calls: RefCell<Vec<(String, String)>>,
    }

    impl SlackApi for FakeSlackApi {
        fn history(
            &self,
            channel: &str,
            oldest: DateTime<Utc>,
            latest: DateTime<Utc>,
        ) -> Result<Vec<Value>> {
            Ok(self
                .histories
                .get(&(channel.to_owned(), oldest.timestamp(), latest.timestamp()))
                .cloned()
                .unwrap_or_default())
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
    }

    fn scratch(label: &str) -> Result<std::path::PathBuf> {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ctx-memory-slack-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(path)
    }

    fn at(value: &str) -> Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(value)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .with_context(|| format!("parsing test timestamp {value}"))
    }

    fn slack_ts(timestamp: DateTime<Utc>) -> String {
        format!("{}.000000", timestamp.timestamp())
    }

    fn fetch_request(
        root: &Path,
        window_end: DateTime<Utc>,
        channels: Vec<String>,
        discovered: Vec<super::super::DiscoveredStream>,
    ) -> FetchRequest {
        let channels = Value::Array(channels.into_iter().map(Value::String).collect());
        FetchRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
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
        }
    }

    fn run_fetch(request: &FetchRequest, api: &FakeSlackApi) -> Result<Vec<Message>> {
        let config = parse_config(&request.config)?;
        let mut messages = Vec::new();
        let streams = emit_fetch_hello(request, &config, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        fetch_with_api(request, &config, &streams, api, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    }

    fn records(messages: &[Message]) -> Vec<&Record> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Record(record) => Some(record.as_ref()),
                _ => None,
            })
            .collect()
    }

    fn warnings(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Warning { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    fn write_index(root: &Path, index: &ThreadIndex) -> Result<()> {
        let state = root.join("state");
        std::fs::create_dir_all(&state)?;
        std::fs::write(state.join(THREAD_INDEX_FILE), serde_json::to_string(index)?)?;
        Ok(())
    }

    #[test]
    fn reply_after_window_end_is_left_for_the_next_window() -> Result<()> {
        let root = scratch("reply-after-window")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let parent = slack_ts(end - Duration::days(2));
        let mut index = ThreadIndex::new();
        index.insert(
            "C0EXAMPLE01".to_owned(),
            BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
        );
        write_index(&root, &index)?;
        let future = slack_ts(end + Duration::minutes(1));
        let mut api = FakeSlackApi::default();
        api.replies_by_thread.insert(
            ("C0EXAMPLE01".to_owned(), parent),
            vec![json!({"ts": future, "text": "later", "user": "U0EXAMPLE9"})],
        );

        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let messages = run_fetch(&request, &api)?;
        std::fs::remove_dir_all(&root)?;

        assert!(records(&messages).is_empty());
        Ok(())
    }

    #[test]
    fn a_reply_to_any_tracked_thread_within_the_poll_horizon_is_fetched() -> Result<()> {
        let root = scratch("all-tracked-threads")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let mut parents = Vec::new();
        let mut tracked = BTreeMap::new();
        for offset in 0_i64..150 {
            let parent = slack_ts(end - Duration::days(2) - Duration::seconds(offset));
            tracked.insert(parent.clone(), slack_ts(end - Duration::days(1)));
            parents.push(parent);
        }
        let oldest = parents
            .last()
            .cloned()
            .context("test created tracked parents")?;
        let mut index = ThreadIndex::new();
        index.insert("C0EXAMPLE01".to_owned(), tracked);
        write_index(&root, &index)?;
        let reply = slack_ts(end - Duration::hours(1));
        let mut api = FakeSlackApi::default();
        api.replies_by_thread.insert(
            ("C0EXAMPLE01".to_owned(), oldest.clone()),
            vec![
                json!({"ts": oldest, "text": "parent", "user": "U0EXAMPLE9"}),
                json!({"ts": reply, "text": "new reply", "user": "U0EXAMPLE9"}),
            ],
        );

        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let messages = run_fetch(&request, &api)?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(api.reply_calls.borrow().len(), 150);
        assert!(api
            .reply_calls
            .borrow()
            .iter()
            .any(|(_, parent)| parent == &oldest));
        assert_eq!(records(&messages).len(), 2);
        assert!(records(&messages)
            .iter()
            .any(|record| record.pointer.ends_with(&reply)));
        Ok(())
    }

    #[test]
    fn threads_past_the_horizon_are_counted_in_a_warning() -> Result<()> {
        let root = scratch("thread-horizon")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let parent = slack_ts(end - Duration::days(40));
        let mut index = ThreadIndex::new();
        index.insert(
            "C0EXAMPLE01".to_owned(),
            BTreeMap::from([(parent, slack_ts(end - Duration::days(40)))]),
        );
        write_index(&root, &index)?;

        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let messages = run_fetch(&request, &FakeSlackApi::default())?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(
            warnings(&messages),
            vec!["1 tracked thread(s) idle over 30 days not polled"]
        );
        Ok(())
    }

    #[test]
    fn permalink_uses_the_configured_workspace() -> Result<()> {
        let record = record_from_message(
            &json!({"ts": "1792951800.000100", "text": "hello"}),
            "C0EXAMPLE01",
            "#general",
            &BTreeMap::new(),
            "acme",
        )?;

        let url = record.url.context("Slack record has a permalink")?;
        assert!(url.starts_with("https://acme.slack.com/archives/"));
        Ok(())
    }

    #[test]
    fn pre_window_parent_is_sent_as_a_context_record() -> Result<()> {
        let root = scratch("pre-window-parent")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let parent = slack_ts(end - Duration::days(2));
        let mut index = ThreadIndex::new();
        index.insert(
            "C0EXAMPLE01".to_owned(),
            BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
        );
        write_index(&root, &index)?;
        let reply = slack_ts(end - Duration::hours(1));
        let mut api = FakeSlackApi::default();
        api.replies_by_thread.insert(
            ("C0EXAMPLE01".to_owned(), parent.clone()),
            vec![
                json!({"ts": parent, "text": "parent", "user": "U0EXAMPLE9"}),
                json!({
                    "ts": reply,
                    "thread_ts": parent,
                    "text": "reply",
                    "user": "U0EXAMPLE9"
                }),
            ],
        );

        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let messages = run_fetch(&request, &api)?;
        std::fs::remove_dir_all(&root)?;

        let parent_record = records(&messages)
            .into_iter()
            .find(|record| record.pointer.ends_with(&parent))
            .context("fetched reply has its parent context record")?;
        assert!(parent_record.context);
        Ok(())
    }

    #[test]
    fn malformed_discovered_channel_is_a_warning_not_a_stream() -> Result<()> {
        let root = scratch("bad-discovered-stream")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let request = fetch_request(
            &root,
            end,
            Vec::new(),
            vec![super::super::DiscoveredStream {
                stream: "not-a-channel".to_owned(),
                owner: "person.md".to_owned(),
            }],
        );

        let messages = run_fetch(&request, &FakeSlackApi::default())?;
        std::fs::remove_dir_all(&root)?;

        let hello = messages.first().context("Slack source emitted hello")?;
        let Message::Hello { streams, .. } = hello else {
            bail!("Slack source did not emit hello first");
        };
        assert_eq!(streams.as_slice(), [] as [String; 0]);
        assert_eq!(
            warnings(&messages),
            vec!["ignoring malformed discovered Slack channel \"not-a-channel\""]
        );
        Ok(())
    }

    #[test]
    fn missing_token_names_the_variable() -> Result<()> {
        let _environment = ENVIRONMENT_LOCK
            .lock()
            .map_err(|error| anyhow!("locking test environment: {error}"))?;
        let end = at("2026-10-25T18:00:00Z")?;
        let root = scratch("missing-token")?;
        let mut request = fetch_request(&root, end, Vec::new(), Vec::new());
        request
            .config
            .as_object_mut()
            .context("test Slack configuration is an object")?
            .insert(
                "token_env".to_owned(),
                Value::String("ACME_SLACK".to_owned()),
            );
        let previous = std::env::var_os("ACME_SLACK");
        std::env::remove_var("ACME_SLACK");
        let result = SlackSource::new().run(&Request::Fetch(request), &mut |_| Ok(()));
        if let Some(value) = previous {
            std::env::set_var("ACME_SLACK", value);
        }
        std::fs::remove_dir_all(&root)?;

        let error = result
            .err()
            .context("missing Slack token should fail source execution")?;
        assert!(format!("{error:#}").contains("ACME_SLACK"));
        Ok(())
    }

    #[test]
    fn fetched_parent_and_reply_update_the_thread_index() -> Result<()> {
        let root = scratch("thread-index")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let start = end - Duration::hours(24);
        let parent = slack_ts(end - Duration::hours(2));
        let latest_parent = slack_ts(end - Duration::hours(1));
        let latest_reply = slack_ts(end - Duration::minutes(30));
        let mut api = FakeSlackApi::default();
        api.histories.insert(
            ("C0EXAMPLE01".to_owned(), start.timestamp(), end.timestamp()),
            vec![json!({
                "ts": parent,
                "reply_count": 1,
                "latest_reply": latest_parent,
                "text": "parent",
            })],
        );
        api.replies_by_thread.insert(
            ("C0EXAMPLE01".to_owned(), parent.clone()),
            vec![json!({
                "ts": latest_reply,
                "thread_ts": parent,
                "text": "reply",
            })],
        );

        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let _messages = run_fetch(&request, &api)?;
        let index = read_thread_index(&root.join("state"))?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(
            index
                .get("C0EXAMPLE01")
                .and_then(|threads| threads.get(&parent))
                .map(String::as_str),
            Some(latest_reply.as_str())
        );
        Ok(())
    }

    #[test]
    fn resolve_uses_thread_replies_and_message_history() -> Result<()> {
        let end = at("2026-10-25T18:00:00Z")?;
        let parent = slack_ts(end - Duration::hours(2));
        let parent_at = slack_timestamp(&parent)?;
        let reply = slack_ts(end - Duration::hours(1));
        let mut api = FakeSlackApi::default();
        api.replies_by_thread.insert(
            ("C0EXAMPLE01".to_owned(), parent.clone()),
            vec![json!({
                "ts": reply,
                "thread_ts": parent,
                "text": "reply",
            })],
        );
        api.histories.insert(
            (
                "C0EXAMPLE01".to_owned(),
                parent_at.timestamp(),
                parent_at.timestamp(),
            ),
            vec![json!({"ts": parent, "text": "parent"})],
        );
        let request = ResolveRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "slack".to_owned(),
                kind: "slack".to_owned(),
                scheme: "slack".to_owned(),
            },
            config: json!({"workspace": "acme"}),
            pointers: vec![format!("slack:C0EXAMPLE01/{parent}")],
            state_dir: std::env::temp_dir(),
        };
        let config = parse_config(&request.config)?;
        let mut messages = Vec::new();
        emit_resolve_hello(&request, &mut |message| {
            messages.push(message);
            Ok(())
        })?;
        resolve_with_api(&request, &config, &api, &mut |message| {
            messages.push(message);
            Ok(())
        })?;

        assert_eq!(records(&messages).len(), 2);
        assert!(records(&messages)
            .iter()
            .any(|record| record.pointer.ends_with(&parent)));
        assert!(records(&messages)
            .iter()
            .any(|record| record.pointer.ends_with(&reply)));
        Ok(())
    }

    #[test]
    fn configured_scheme_is_used_for_fetched_records() -> Result<()> {
        let root = scratch("configured-scheme")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let start = end - Duration::hours(24);
        let message = slack_ts(end - Duration::hours(1));
        let expected = format!("acme-slack:C0EXAMPLE01/{message}");
        let mut api = FakeSlackApi::default();
        api.histories.insert(
            ("C0EXAMPLE01".to_owned(), start.timestamp(), end.timestamp()),
            vec![json!({"ts": message, "text": "message"})],
        );
        let mut request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        request.source.scheme = "acme-slack".to_owned();

        let messages = run_fetch(&request, &api)?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(records(&messages).len(), 1);
        assert_eq!(
            records(&messages)
                .first()
                .map(|record| record.pointer.as_str()),
            Some(expected.as_str())
        );
        Ok(())
    }

    #[test]
    fn fractional_parent_is_retained_for_fetch_and_live_resolution() -> Result<()> {
        let _environment = ENVIRONMENT_LOCK
            .lock()
            .map_err(|error| anyhow!("locking test environment: {error}"))?;
        let root = scratch("fractional-parent")?;
        let end = at("2026-10-25T18:00:00Z")?;
        let parent = format!("{}.123456", (end - Duration::days(2)).timestamp());
        let reply = format!("{}.654321", (end - Duration::hours(1)).timestamp());
        let thread = format!(
            r#"{{"ok":true,"messages":[{{"ts":"{parent}","text":"parent"}},{{"ts":"{reply}","thread_ts":"{parent}","text":"reply"}}]}}"#
        );
        let mut index = ThreadIndex::new();
        index.insert(
            "C0EXAMPLE01".to_owned(),
            BTreeMap::from([(parent.clone(), slack_ts(end - Duration::days(1)))]),
        );
        write_index(&root, &index)?;
        let request = fetch_request(&root, end, vec!["C0EXAMPLE01".to_owned()], Vec::new());
        let (fetch_api, fetch_requests, fetch_server) = fake_http_api(vec![
            r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
            r#"{"ok":true,"messages":[]}"#.to_owned(),
            thread.clone(),
        ])?;
        let previous = std::env::var_os("SLACK_TOKEN");
        std::env::set_var("SLACK_TOKEN", "test-token");
        let source = SlackSource::with_api(fetch_api);
        let mut fetched = Vec::new();
        source.run(&Request::Fetch(request), &mut |message| {
            fetched.push(message);
            Ok(())
        })?;
        fetch_server
            .join()
            .map_err(|_| anyhow!("fetch fake API server panicked"))??;
        let fetch_request = fetch_requests
            .lock()
            .map_err(|error| anyhow!("locking fetch requests: {error}"))?
            .iter()
            .find(|request| request.contains("conversations.replies"))
            .cloned()
            .context("fetch called conversations.replies")?;
        assert!(fetch_request.contains(&format!("ts={parent}")));
        assert_eq!(records(&fetched).len(), 2);
        assert!(records(&fetched)
            .iter()
            .any(|record| record.pointer.ends_with(&parent) && record.context));

        let resolve_request = Request::Resolve(ResolveRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "slack".to_owned(),
                kind: "slack".to_owned(),
                scheme: "slack".to_owned(),
            },
            config: json!({"workspace": "acme"}),
            pointers: vec![format!("slack:C0EXAMPLE01/{parent}")],
            state_dir: root.join("state"),
        });
        let (resolve_api, resolve_requests, resolve_server) = fake_http_api(vec![
            thread,
            r#"{"ok":true,"channel":{"name":"general"}}"#.to_owned(),
        ])?;
        let source = SlackSource::with_api(resolve_api);
        let mut resolved = Vec::new();
        source.run(&resolve_request, &mut |message| {
            resolved.push(message);
            Ok(())
        })?;
        if let Some(value) = previous {
            std::env::set_var("SLACK_TOKEN", value);
        } else {
            std::env::remove_var("SLACK_TOKEN");
        }
        resolve_server
            .join()
            .map_err(|_| anyhow!("resolve fake API server panicked"))??;
        let resolve_request = resolve_requests
            .lock()
            .map_err(|error| anyhow!("locking resolve requests: {error}"))?
            .iter()
            .find(|request| request.contains("conversations.replies"))
            .cloned()
            .context("resolve called conversations.replies")?;
        std::fs::remove_dir_all(&root)?;

        assert!(resolve_request.contains(&format!("ts={parent}")));
        assert_eq!(records(&resolved).len(), 2);
        assert!(records(&resolved)
            .iter()
            .any(|record| record.pointer.ends_with(&parent)));
        assert!(records(&resolved)
            .iter()
            .any(|record| record.pointer.ends_with(&reply)));
        Ok(())
    }
}
