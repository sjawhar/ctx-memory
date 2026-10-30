//! Slack channel discovery, thread state and index, history reads, and resolve.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;

use crate::api::slack::SlackClient;
use crate::atomic::write_replace;
use crate::record::Record;
use crate::source::{
    slice_end, window_start, FetchRequest, Message, Op, ResolveRequest, StreamStatus,
    MAX_SLICE_DAYS, PROTOCOL_VERSION,
};

use super::auth::SlackAuth;
use super::records::{
    apply_scheme, is_timestamp, mentions, message_timestamp, record_from_message, slack_timestamp,
    Mention,
};
use super::SlackConfig;

/// How long after it is posted a message nobody has answered is read again, so that its first
/// reply is found and its thread followed. A first reply later than this is not read, and
/// neither is the rest of that thread.
const FIRST_REPLY_HOURS: i64 = 48;
pub(super) const THREAD_INDEX_FILE: &str = "threads.json";

pub(super) trait SlackApi {
    fn history(
        &self,
        channel: &str,
        oldest: DateTime<Utc>,
        latest: DateTime<Utc>,
    ) -> Result<Vec<Value>>;
    fn thread_messages(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>>;
    fn channel_info(&self, channel: &str) -> Result<Value>;
    fn user_info(&self, user: &str) -> Result<Option<Value>>;
    /// Every conversation of `types` (`conversations.list`), used to discover membership.
    fn list_conversations(&self, types: &str, exclude_archived: bool) -> Result<Vec<Value>>;
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

    fn list_conversations(&self, types: &str, exclude_archived: bool) -> Result<Vec<Value>> {
        Self::list_conversations(self, types, exclude_archived)
    }
}

fn is_absent_user(error: &anyhow::Error) -> bool {
    matches!(
        error.to_string().as_str(),
        r#"slack users.info: "user_not_found""# | r#"slack users.info: "users_not_found""#
    )
}

/// Whether a failed `conversations.info` lookup, as `Names` keeps it, says Slack does not
/// know the channel, as it says of a private channel the user is not in.
fn is_absent_channel(error: &str) -> bool {
    error == r#"slack conversations.info: "channel_not_found""#
}

/// Resolves the token, discovers the streams, and fetches them.
pub(super) fn run_fetch(
    request: &FetchRequest,
    config: &SlackConfig,
    api: Option<&str>,
    auth: &dyn SlackAuth,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let token = auth.access_token()?;
    match api {
        Some(api) => fetch_pipeline(request, config, &SlackClient::with_api(&token, api), sink),
        None => fetch_pipeline(request, config, &SlackClient::new(&token), sink),
    }
}

fn fetch_pipeline<C: SlackApi + ?Sized>(
    request: &FetchRequest,
    config: &SlackConfig,
    client: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let (streams, warnings) = discover_streams(client, config, false)?;
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
    fetch_with_api(request, config, &streams, client, sink)
}

pub(super) fn emit_resolve_hello(
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

/// Discovers the streams to fetch: public channels the signed-in user is in, plus the private
/// channels the configuration lists. Direct and group messages are never listed, so they are
/// never fetched; a configured channel that is not a readable public or private channel is warned
/// and skipped.
/// A fetch leaves out archived channels, which take no new messages; a resolve keeps them
/// (`include_archived`), since what a fetch read there before is still in scope.
pub(super) fn discover_streams<C: SlackApi + ?Sized>(
    api: &C,
    config: &SlackConfig,
    include_archived: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let listing = api
        .list_conversations("public_channel,private_channel", !include_archived)
        .context("listing Slack conversations")?;
    let mut by_id: BTreeMap<String, &Value> = BTreeMap::new();
    for conversation in &listing {
        if let Some(id) = conversation.get("id").and_then(Value::as_str) {
            by_id.insert(id.to_owned(), conversation);
        }
    }
    let mut streams = BTreeSet::new();
    let mut warnings = Vec::new();
    for conversation in &listing {
        let Some(id) = conversation.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !flag(conversation, "is_private") && flag(conversation, "is_member") {
            streams.insert(id.to_owned());
        }
    }
    for channel in &config.channels {
        match by_id.get(channel) {
            None => warnings.push(format!(
                "configured Slack channel {channel:?} is not a public or private channel you can read; skipping"
            )),
            Some(conversation) if flag(conversation, "is_im") || flag(conversation, "is_mpim") => {
                warnings.push(format!(
                    "configured Slack channel {channel:?} is a direct or group message, which ctx never fetches; skipping"
                ));
            }
            Some(_) => {
                streams.insert(channel.clone());
            }
        }
    }
    Ok((streams.into_iter().collect(), warnings))
}

pub(super) fn flag(conversation: &Value, key: &str) -> bool {
    conversation
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether `channel` is an id a fetch can read: `C` for a public or newer private channel, `G`
/// for an older private channel. A `D` direct message never is. A `G` id can also be a group
/// message, which only the conversation listing tells apart (`discover_streams`).
pub(super) fn is_channel(channel: &str) -> bool {
    let mut bytes = channel.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    channel.len() >= 9
        && matches!(first, b'C' | b'G')
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

pub(super) fn fetch_with_api<C: SlackApi + ?Sized>(
    request: &FetchRequest,
    config: &SlackConfig,
    streams: &[String],
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut thread_index = read_thread_index(&request.state_dir)?;
    let mut names = Names::new(streams);
    let mut skipped_threads = 0_usize;
    // A stream that ends Ok moves its cursor past this window, and the threads found in it are
    // polled again only through the index. So no stream ends until the index is saved: a save
    // that fails fails the run, and `Session::finish` ends every stream as an error, its cursor
    // unmoved.
    let mut ends = Vec::with_capacity(streams.len());

    for channel in streams {
        let result = window_start(
            request.cursors.get(channel).map(String::as_str),
            request.window_end,
            request.backfill_hours,
        )
        .and_then(|start| {
            let span = Span {
                start,
                end: slice_end(start, request.window_end),
                window_end: request.window_end,
            };
            fetch_channel(
                api,
                channel,
                span,
                config,
                &request.source.scheme,
                &mut thread_index,
                &mut names,
            )
            .map(|(records, skipped)| (records, skipped, span))
        });
        match result {
            Ok((records, skipped, span)) => {
                skipped_threads += skipped;
                let end = span.end.to_rfc3339_opts(SecondsFormat::Secs, true);
                if span.end < span.window_end {
                    sink(Message::Warning {
                        stream: Some(channel.clone()),
                        message: format!(
                            "{channel} is stored through {}, more than {MAX_SLICE_DAYS} days before the window end; this fetch reads it through {end} and the next reads on from there",
                            span.start.to_rfc3339_opts(SecondsFormat::Secs, true)
                        ),
                    })?;
                }
                for record in records {
                    sink(Message::Record(Box::new(record)))?;
                }
                ends.push(Message::StreamEnd {
                    stream: channel.clone(),
                    status: StreamStatus::Ok,
                    cursor: Some(end),
                    error: None,
                });
            }
            Err(error) => {
                ends.push(Message::StreamEnd {
                    stream: channel.clone(),
                    status: StreamStatus::Error,
                    cursor: None,
                    error: Some(format!("{error:#}")),
                });
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
    if let Some(warning) = names.warning() {
        sink(Message::Warning {
            stream: None,
            message: warning,
        })?;
    }
    write_thread_index(&request.state_dir, &thread_index)?;
    for end in ends {
        sink(end)?;
    }
    Ok(())
}

/// The times one channel's fetch reads between.
#[derive(Clone, Copy)]
struct Span {
    /// The channel's cursor: messages after it are emitted.
    start: DateTime<Utc>,
    /// The end of this run's slice: messages up to it are emitted, and the cursor moves to it.
    end: DateTime<Utc>,
    /// The window's end, which the thread and first-reply horizons count back from.
    window_end: DateTime<Utc>,
}

fn fetch_channel<C: SlackApi + ?Sized>(
    api: &C,
    channel: &str,
    span: Span,
    config: &SlackConfig,
    scheme: &str,
    thread_index: &mut ThreadIndex,
    names: &mut Names<'_>,
) -> Result<(Vec<Record>, usize)> {
    let Span { start, end, .. } = span;
    let label = names.channel_label(api, channel)?;
    let horizon = span.window_end - Duration::days(i64::from(config.thread_poll_days));
    // A thread reply is not in the channel's history unless it was also sent to the channel,
    // and a message nobody had answered when it was read carries no `latest_reply` to track its
    // thread by. Reading the history back `FIRST_REPLY_HOURS` shows each such message again
    // with the `latest_reply` its first reply gave it; a thread with replies is then followed
    // through the index for `thread_poll_days` after its last reply. Only messages inside the
    // cursor's window are emitted.
    let first_reply_horizon = (span.window_end - Duration::hours(FIRST_REPLY_HOURS)).max(horizon);
    let history = api
        .history(channel, start.min(first_reply_horizon), end)
        .with_context(|| format!("reading Slack history for {channel}"))?;
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

    names.learn(
        api,
        contexts.iter().chain(messages.iter()).chain(replies.iter()),
    )?;
    let mut records = Vec::with_capacity(contexts.len() + messages.len() + replies.len());
    for message in contexts {
        let mut record =
            record_from_message(&message, channel, &label, names.named(), &config.workspace)?;
        record.context = true;
        apply_scheme(&mut record, scheme);
        records.push(record);
    }
    for message in messages.into_iter().chain(replies) {
        let mut record =
            record_from_message(&message, channel, &label, names.named(), &config.workspace)?;
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

pub(super) fn resolve_with_api<C: SlackApi + ?Sized>(
    request: &ResolveRequest,
    config: &SlackConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    // A pointer that names no channel is refused before any call to Slack.
    let pointers = request
        .pointers
        .iter()
        .map(|pointer| parse_pointer(pointer, &request.source.scheme))
        .collect::<Result<Vec<_>>>()?;
    // A resolve is reachable from any caller the server admits, so it reads only what a fetch
    // reads: the fetch's own discovery decides, before any message is read, rather than the
    // token's wider scopes.
    let (readable, _warnings) = discover_streams(api, config, true)?;
    let mut names = Names::new(&readable);
    let mut found: Vec<(String, String, Value)> = Vec::new();
    for (channel, timestamp) in pointers {
        if !readable.iter().any(|stream| stream == channel) {
            bail!(
                "Slack conversation {channel} is not one this source fetches: ctx reads the public channels you are in and the private channels `channels` lists, never a direct or group message"
            );
        }
        let messages = resolve_messages(api, channel, timestamp)?;
        if messages.is_empty() {
            continue;
        }
        let label = names.channel_label(api, channel)?;
        for message in messages {
            found.push((channel.to_owned(), label.clone(), message));
        }
    }
    names.learn(api, found.iter().map(|(_, _, message)| message))?;
    for (channel, label, message) in found {
        let mut record =
            record_from_message(&message, &channel, &label, names.named(), &config.workspace)?;
        apply_scheme(&mut record, &request.source.scheme);
        sink(Message::Record(Box::new(record)))?;
    }
    if let Some(warning) = names.warning() {
        sink(Message::Warning {
            stream: None,
            message: warning,
        })?;
    }
    Ok(())
}

fn parse_pointer<'a>(pointer: &'a str, scheme: &str) -> Result<(&'a str, &'a str)> {
    let (channel, timestamp) = pointer
        .strip_prefix(scheme)
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|locator| locator.split_once('/'))
        .filter(|(_, timestamp)| is_timestamp(timestamp))
        .ok_or_else(|| anyhow!("{pointer} is not a Slack message pointer"))?;
    if !is_channel(channel) {
        bail!(
            "{pointer} does not name a public or private channel: ctx never reads a direct or group message"
        );
    }
    Ok((channel, timestamp))
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

/// What one fetch or resolve run has learned from Slack about the ids in the messages it
/// read: the name of each person who wrote or is mentioned in one, of each channel it reads,
/// and of each channel a message mentions without a name. Slack is asked about each id once
/// per run, and the answer, a failure included, holds for the rest of the run. A person's name
/// is the one the author line gives them.
struct Names<'r> {
    /// The channels this run reads. A private channel's name is shown only for one of these.
    readable: &'r [String],
    /// Each id Slack named, as `record_from_message` takes them.
    named: BTreeMap<String, String>,
    /// Each id Slack answered for with no name to show: a user it does not know or gave no
    /// name, and a private channel this run does not read.
    unnamed: BTreeSet<String>,
    /// Each id whose lookup failed, with the error.
    failed: BTreeMap<String, String>,
    /// Each mentioned id that kept its id because its lookup failed, which the run warns of.
    kept: BTreeSet<String>,
}

impl<'r> Names<'r> {
    const fn new(readable: &'r [String]) -> Self {
        Self {
            readable,
            named: BTreeMap::new(),
            unnamed: BTreeSet::new(),
            failed: BTreeMap::new(),
            kept: BTreeSet::new(),
        }
    }

    /// The names learned so far.
    const fn named(&self) -> &BTreeMap<String, String> {
        &self.named
    }

    fn asked(&self, id: &str) -> bool {
        self.named.contains_key(id) || self.unnamed.contains(id) || self.failed.contains_key(id)
    }

    /// The label of a channel this run reads: `#` and its name, or its id when Slack gives no
    /// name. A failed lookup fails the channel's read, as it always has.
    fn channel_label<C: SlackApi + ?Sized>(&mut self, api: &C, channel: &str) -> Result<String> {
        self.look_up_channel(api, channel);
        if let Some(error) = self.failed.get(channel) {
            bail!("reading Slack channel {channel}: {error}");
        }
        Ok(format!(
            "#{}",
            self.named.get(channel).map_or(channel, String::as_str)
        ))
    }

    /// Learns the names `messages` need: each author's, and each mentioned person's and
    /// unnamed channel's. A failed author lookup fails the run's read of these messages, as
    /// it always has; a failed mention lookup leaves that mention its id.
    fn learn<'m, C: SlackApi + ?Sized>(
        &mut self,
        api: &C,
        messages: impl IntoIterator<Item = &'m Value>,
    ) -> Result<()> {
        for message in messages {
            if let Some(author) = message.get("user").and_then(Value::as_str) {
                self.look_up_user(api, author);
                if let Some(error) = self.failed.get(author) {
                    bail!("reading Slack user {author}: {error}");
                }
            }
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            for mention in mentions(text) {
                let id = match mention {
                    Mention::User { id, .. } => {
                        self.look_up_user(api, id);
                        id
                    }
                    Mention::Channel { id, label: None } => {
                        self.look_up_channel(api, id);
                        id
                    }
                    Mention::Channel { .. } | Mention::Group { .. } | Mention::Everyone(_) => {
                        continue
                    }
                };
                if self
                    .failed
                    .get(id)
                    .is_some_and(|error| !is_absent_channel(error))
                {
                    self.kept.insert(id.to_owned());
                }
            }
        }
        Ok(())
    }

    fn look_up_user<C: SlackApi + ?Sized>(&mut self, api: &C, id: &str) {
        if self.asked(id) {
            return;
        }
        match api.user_info(id) {
            Ok(user) => match user.as_ref().and_then(display_name) {
                Some(name) => {
                    self.named.insert(id.to_owned(), name.to_owned());
                }
                None => {
                    self.unnamed.insert(id.to_owned());
                }
            },
            Err(error) => {
                self.failed.insert(id.to_owned(), format!("{error:#}"));
            }
        }
    }

    /// Learns a channel's name when the channel is public or one this run reads. A private
    /// channel the run does not read keeps its id, so no record shows a name its configuration
    /// did not open.
    fn look_up_channel<C: SlackApi + ?Sized>(&mut self, api: &C, id: &str) {
        if self.asked(id) {
            return;
        }
        match api.channel_info(id) {
            Ok(info) => {
                let shown =
                    !flag(&info, "is_private") || self.readable.iter().any(|read| read == id);
                match info.get("name").and_then(Value::as_str).filter(|_| shown) {
                    Some(name) => {
                        self.named.insert(id.to_owned(), name.to_owned());
                    }
                    None => {
                        self.unnamed.insert(id.to_owned());
                    }
                }
            }
            Err(error) => {
                self.failed.insert(id.to_owned(), format!("{error:#}"));
            }
        }
    }

    /// The warning for the mentions that kept their id because a lookup failed, if any did.
    fn warning(&self) -> Option<String> {
        if self.kept.is_empty() {
            return None;
        }
        let failures = self
            .kept
            .iter()
            .filter_map(|id| self.failed.get(id).map(|error| format!("{id} ({error})")))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "Slack could not name {} mentioned id(s), which keep their id: {failures}",
            self.kept.len()
        ))
    }
}

/// The name Slack gives a user, as the author line shows it: the display name, else the real
/// name, else the user name. `None` when Slack gives none of them.
fn display_name(user: &Value) -> Option<&str> {
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
    ]
    .into_iter()
    .flatten()
    .find(|name| !name.is_empty())
}

fn inside_window(message: &Value, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<bool> {
    let timestamp = slack_timestamp(message_timestamp(message)?)?;
    Ok(timestamp > start && timestamp <= end)
}

pub(super) fn update_thread_activity(
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

pub(super) type ThreadIndex = BTreeMap<String, BTreeMap<String, String>>;

pub(super) fn read_thread_index(state_dir: &Path) -> Result<ThreadIndex> {
    let path = state_dir.join(THREAD_INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(ThreadIndex::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the index, keeping every thread the file already holds.
///
/// The index lives beside the windows, which every machine fetching this tree shares, while
/// the fetch lock stays on one machine (`crate::fetch::acquire_lock`), so another fetch can
/// have added a thread since this one read the file. A thread is only ever added or moved
/// forward here, so merging what is on disk hides nothing and loses no reply.
pub(super) fn write_thread_index(state_dir: &Path, index: &ThreadIndex) -> Result<()> {
    let path = state_dir.join(THREAD_INDEX_FILE);
    let mut merged = read_thread_index(state_dir)?;
    for (channel, threads) in index {
        for (parent, latest) in threads {
            update_thread_activity(&mut merged, channel, parent, latest)?;
        }
    }
    let text = serde_json::to_string(&merged)
        .with_context(|| format!("serializing {}", path.display()))?;
    write_replace(&path, text.as_bytes())
}
