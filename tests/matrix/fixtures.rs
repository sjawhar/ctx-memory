//! The runners and readers the Matrix checks share.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use ctx_memory::pointer::Schemes;
use ctx_memory::record::Record;
use ctx_memory::source::{
    FetchRequest, MatrixSource, Message, Request, ResolveRequest, Session, Source, SourceRef,
    StreamStatus,
};
use serde_json::{json, Value};

use crate::support::fake_matrix::{decode, FakeMatrix, TOKEN};

/// Environment variables are process-wide, so these checks take turns.
static ENVIRONMENT: Mutex<()> = Mutex::new(());
static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

pub(super) const ALICE: &str = "@alice:example.org";
pub(super) const BOB: &str = "@bob:example.org";
/// A room id with a server name, as room versions up to 11 write it.
pub(super) const FAMILY: &str = "!familychat:example.org";
/// A room id without one, as room version 12 writes it.
pub(super) const BOOKS: &str = "!-BooksClubRoomV12xxxxxxxxxxxxxxxxxxxxxxxxxxx";
pub(super) const BANK: &str = "!bankalerts:example.org";
pub(super) const CODES: &str = "!logincodes:example.org";

/// The window end most checks fetch to.
pub(super) const WINDOW_END: &str = "2026-10-25T18:00:00Z";

pub(super) fn guard() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn scratch(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "ctx-matrix-it-{label}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub(super) fn remove(dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

pub(super) fn at(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

/// Milliseconds since the epoch of the window end moved by `minutes`.
pub(super) fn ms(minutes: i64) -> Result<i64> {
    Ok((at(WINDOW_END)? + Duration::minutes(minutes)).timestamp_millis())
}

/// A homeserver with the family room: Alice and Bob are its members.
pub(super) fn homeserver() -> Result<FakeMatrix> {
    let server = FakeMatrix::start()?;
    server.room(FAMILY, Some("Family"))?;
    server.member(FAMILY, ALICE, "Alice")?;
    server.member(FAMILY, BOB, "Bob")?;
    Ok(server)
}

/// One fetch, as `ctx fetch` would ask for it.
pub(super) struct Fetch<'a> {
    pub(super) config: Value,
    pub(super) state_dir: &'a Path,
    pub(super) cursors: BTreeMap<String, String>,
    pub(super) window_end: &'a str,
    pub(super) backfill_hours: u32,
    pub(super) exclude: Vec<String>,
}

impl<'a> Fetch<'a> {
    pub(super) fn new(server: &FakeMatrix, state_dir: &'a Path) -> Self {
        Self {
            config: json!({"homeserver": server.url()}),
            state_dir,
            cursors: BTreeMap::new(),
            window_end: WINDOW_END,
            backfill_hours: 24,
            exclude: Vec::new(),
        }
    }

    /// Sets one configuration key.
    pub(super) fn with(mut self, key: &str, value: Value) -> Self {
        if let Some(config) = self.config.as_object_mut() {
            config.insert(key.to_owned(), value);
        }
        self
    }

    pub(super) fn cursors(mut self, cursors: &BTreeMap<String, String>) -> Self {
        self.cursors.clone_from(cursors);
        self
    }

    pub(super) fn until(mut self, window_end: &'a str) -> Self {
        self.window_end = window_end;
        self
    }

    /// Runs the source with the homeserver's token in the environment, and checks every message
    /// it sent against the protocol a fetch validates.
    pub(super) fn run(&self) -> Result<Vec<Message>> {
        let request = Request::Fetch(FetchRequest {
            protocol: 1,
            source: SourceRef {
                id: "matrix".to_owned(),
                kind: "matrix".to_owned(),
                scheme: "matrix".to_owned(),
            },
            config: self.config.clone(),
            window_end: at(self.window_end)?,
            backfill_hours: self.backfill_hours,
            cursors: self.cursors.clone(),
            discovered: Vec::new(),
            state_dir: self.state_dir.to_path_buf(),
            exclude: self.exclude.clone(),
        });
        run_validated(&request)
    }
}

/// Runs a resolve of `pointers`.
pub(super) fn resolve(
    server: &FakeMatrix,
    config: Value,
    pointers: &[String],
) -> Result<Vec<Message>> {
    let mut config = config;
    if let Some(object) = config.as_object_mut() {
        object.insert("homeserver".to_owned(), json!(server.url()));
    }
    let state_dir = scratch("resolve")?;
    let request = Request::Resolve(ResolveRequest {
        protocol: 1,
        source: SourceRef {
            id: "matrix".to_owned(),
            kind: "matrix".to_owned(),
            scheme: "matrix".to_owned(),
        },
        config,
        pointers: pointers.to_vec(),
        state_dir: state_dir.clone(),
    });
    let outcome = run_validated(&request);
    remove(&state_dir)?;
    outcome
}

fn run_validated(request: &Request) -> Result<Vec<Message>> {
    std::env::set_var("CTX_CREDENTIAL_MATRIX", TOKEN);
    let mut messages = Vec::new();
    let run = MatrixSource::new().run(request, &mut |message| {
        messages.push(message);
        Ok(())
    });
    std::env::remove_var("CTX_CREDENTIAL_MATRIX");
    run?;
    let schemes = Schemes::new(["matrix".to_owned()]);
    let mut session = Session::new(request, &schemes);
    for message in &messages {
        session
            .accept(message.clone())
            .map_err(|error| anyhow!("the source broke the protocol: {error:#}"))?;
    }
    let outcome = session.finish(Ok(()));
    if let Some(error) = outcome.error {
        return Err(anyhow!("the source run failed: {error}"));
    }
    Ok(messages)
}

/// Each stream's end: its status, cursor and error.
pub(super) fn ends(
    messages: &[Message],
) -> BTreeMap<String, (StreamStatus, Option<String>, Option<String>)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::StreamEnd {
                stream,
                status,
                cursor,
                error,
            } => Some((stream.clone(), (*status, cursor.clone(), error.clone()))),
            _ => None,
        })
        .collect()
}

/// The cursor of each stream that ended well.
pub(super) fn cursors(messages: &[Message]) -> BTreeMap<String, String> {
    ends(messages)
        .into_iter()
        .filter_map(|(stream, (status, cursor, _))| {
            (status == StreamStatus::Ok)
                .then_some(cursor)
                .flatten()
                .map(|cursor| (stream, cursor))
        })
        .collect()
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

/// Each record's pointer and text, in the order sent.
pub(super) fn texts(messages: &[Message]) -> Vec<(String, String)> {
    records(messages)
        .into_iter()
        .map(|record| (record.pointer.clone(), record.text.clone()))
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

/// The streams the hello planned.
pub(super) fn planned(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Hello { streams, .. } => Some(streams.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The pointer of an event in a room.
pub(super) fn pointer(room: &str, event: &str) -> String {
    format!("matrix:{room}/{event}")
}

/// The decoded path of every request the homeserver received.
pub(super) fn paths(server: &FakeMatrix) -> Result<Vec<String>> {
    Ok(server
        .requests()?
        .iter()
        .map(|request| decode(&request.path))
        .collect())
}

/// The decoded paths that read `room`.
pub(super) fn room_reads(server: &FakeMatrix, room: &str) -> Result<Vec<String>> {
    Ok(paths(server)?
        .into_iter()
        .filter(|path| path.contains(&format!("/rooms/{room}/")))
        .collect())
}

/// Fails unless the run ended `count` streams, every one of them well.
pub(super) fn every_stream_ok(messages: &[Message], count: usize) {
    let ended = ends(messages);
    assert_eq!(ended.len(), count, "{ended:?}");
    for (stream, (status, _, error)) in &ended {
        assert_eq!(*status, StreamStatus::Ok, "{stream}: {error:?}");
    }
}

/// Fails unless the run sent no request that reads `room`.
pub(super) fn untouched(server: &FakeMatrix, room: &str) -> Result<()> {
    assert_eq!(
        room_reads(server, room)?,
        Vec::<String>::new(),
        "{room} was read"
    );
    Ok(())
}

/// Fails unless the run read `room`.
pub(super) fn touched(server: &FakeMatrix, room: &str) -> Result<()> {
    assert_ne!(
        room_reads(server, room)?,
        Vec::<String>::new(),
        "{room} was not read"
    );
    Ok(())
}

/// A cursor naming `event`, sent `minutes` after the window end.
pub(super) fn cursor_at(event: &str, minutes: i64) -> Result<String> {
    Ok(format!(
        "{event}@{}",
        (at(WINDOW_END)? + Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ))
}
