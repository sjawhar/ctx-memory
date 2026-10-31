//! Local SQLite index of normalized source records, rebuilt from the fetch windows.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::atomic::write_replace;
use crate::config::{DataDir, StateDir};
use crate::pointer::Pointer;
use crate::record::Record;
use crate::windows::{self, Window, WindowId};

/// The schema this build reads. A store written by another one is rebuilt from the files.
const SCHEMA_VERSION: u32 = 4;

/// Numbers the staging files of rebuilds that run at the same time in one process.
static NEXT_REBUILD: AtomicUsize = AtomicUsize::new(0);

/// How long a rebuild keeps trying to copy its replay into a store other connections keep
/// locked. One batch holds the store for well under a second.
const PUBLISH_PATIENCE: Duration = Duration::from_secs(60);

/// The staging database of one rebuild, which that rebuild removes on every way out.
///
/// The file belongs to the call that made it: its name carries this process's id and a counter,
/// so no other run can name it, and no other run may remove it — the process that made one may
/// still be writing it. That leaves this call as the only one able to collect the file a failed
/// replay wrote, and nothing else in the tree ever does. A successful rebuild has copied the
/// file into the store by then, so it is removed the same way.
struct Staging(PathBuf);

impl Staging {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Nowhere to report a failure here, and the rebuild's own error is the one that
        // matters; a file left by a failed removal is collected by the next rebuild that
        // draws this pid and counter.
        let _ = remove_database(&self.0);
    }
}

/// SQLite-backed local record index.
pub struct Store {
    conn: Connection,
    sources: PathBuf,
    resolved: PathBuf,
}

/// Each source's verbatim declaration, kept with the windows so a rebuild restores it.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    version: u32,
    sources: BTreeMap<String, SourceEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceEntry {
    verbatim: bool,
}

/// One record a live resolution obtained, as one line of a file in the state directory.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Resolved {
    fetched_at: DateTime<Utc>,
    source: String,
    verbatim: Option<bool>,
    record: Record,
}

impl Store {
    /// Opens the tree's index, rebuilding it from the files when it is missing or stale.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be opened, replayed, or replaced.
    pub fn load(data: &DataDir, state: &StateDir) -> Result<Self> {
        if usable(&state.store())? {
            return Self::open(&state.store(), &data.sources(), &state.resolved());
        }
        Self::rebuild(data, state)
    }

    /// Opens a store at `path`, creating its parent directory and schema when needed, and
    /// takes the share's verbatim declarations into its cache.
    ///
    /// `sources` is the file that keeps each source's verbatim declaration beside the windows;
    /// `resolved` is the local directory holding the records live resolution obtained. An open
    /// store answers with the declarations that file holds now, so a source the fetch job
    /// declared after this machine built its index is not treated as one nothing has declared.
    /// A source the file does not name keeps what the store learned, which is what a live
    /// resolve wrote. This call writes the schema version straight away, so a caller that
    /// wants the store at `path` to hold a complete replay builds it elsewhere and copies it
    /// in, as [`Store::rebuild`] does.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent cannot be created, SQLite cannot open or configure the
    /// database, the schema cannot be created, or the declarations cannot be read or cached.
    pub fn open(path: &Path, sources: &Path, resolved: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling SQLite write-ahead logging")?;
        conn.pragma_update(None, "synchronous", "FULL")
            .context("enabling full SQLite synchronization")?;
        conn.execute_batch(&format!(
            "
            CREATE TABLE IF NOT EXISTS record (
                pointer TEXT NOT NULL,
                version TEXT NOT NULL,
                scheme TEXT NOT NULL,
                source TEXT NOT NULL,
                stream TEXT NOT NULL,
                thread TEXT,
                ts TEXT NOT NULL,
                fetched_at TEXT NOT NULL,
                last_seen TEXT NOT NULL,
                json TEXT NOT NULL,
                PRIMARY KEY (pointer, version)
            );
            CREATE INDEX IF NOT EXISTS record_thread ON record(thread);
            CREATE INDEX IF NOT EXISTS record_author
                ON record(json_extract(json, '$.author_id'), ts);
            CREATE TABLE IF NOT EXISTS source_metadata (
                source TEXT PRIMARY KEY,
                verbatim INTEGER NOT NULL CHECK (verbatim IN (0, 1))
            );
            CREATE TABLE IF NOT EXISTS delivered (
                pointer TEXT PRIMARY KEY,
                version TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS indexed (
                key TEXT PRIMARY KEY,
                window TEXT NOT NULL
            );
            PRAGMA user_version = {SCHEMA_VERSION};
            "
        ))
        .context("creating record store schema")?;
        let store = Self {
            conn,
            sources: sources.to_path_buf(),
            resolved: resolved.to_path_buf(),
        };
        store.take_declarations()?;
        Ok(store)
    }

    /// Replaces the tree's store with one replayed from every file that holds records.
    ///
    /// Those files are the completed windows in the data directory and the records live
    /// resolution kept in the state directory; the store is an index of them and carries
    /// nothing they do not. The replay runs in a staging file and is copied into the store's
    /// own file, in one SQLite transaction, only once it is complete: an interrupted rebuild
    /// leaves the old index and is retried rather than answering from half the history, and a
    /// connection that already holds the store open reads the old index or the new one in that
    /// same file. Opening the replay takes the share's verbatim declarations, so what a live
    /// resolve learned here survives only for a source the share does not declare.
    ///
    /// # Errors
    ///
    /// Returns an error when a window or resolved file cannot be read, or the replay cannot be
    /// written or copied into place.
    pub fn rebuild(data: &DataDir, state: &StateDir) -> Result<Self> {
        let path = state.store();
        // One rebuild per caller: no lock covers a read command, and two of them racing must
        // not unlink each other's replay. The guard removes this call's own file on every way
        // out, so a replay that fails leaves nothing behind.
        let staging = Staging(path.with_extension(format!(
            "rebuilding-{}-{}",
            std::process::id(),
            NEXT_REBUILD.fetch_add(1, Ordering::Relaxed)
        )));
        remove_database(staging.path())?;
        {
            let store = Self::open(staging.path(), &data.sources(), &state.resolved())?;
            let mut batches: Vec<(DateTime<Utc>, String, Vec<Record>)> = Vec::new();
            let mut through = None;
            for window in windows::after(&data.windows(), None)? {
                let (fetched_at, by_source) = window.records_by_source()?;
                store.deliver(by_source.values().flatten(), &window.id)?;
                through = Some(window.id.clone());
                batches.extend(
                    by_source
                        .into_iter()
                        .map(|(source, records)| (fetched_at, source, records)),
                );
            }
            for resolved in read_resolved(&state.resolved())? {
                if let Some(verbatim) = resolved.verbatim {
                    store.cache_source_verbatim(&resolved.source, verbatim)?;
                }
                batches.push((resolved.fetched_at, resolved.source, vec![resolved.record]));
            }
            batches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
            for (fetched_at, source, records) in batches {
                store.put_all(&source, &records, fetched_at)?;
            }
            if let Some(through) = through {
                store.mark_indexed(&through)?;
            }
            publish(&store.conn, &path)?;
        }
        Self::open(&path, &data.sources(), &state.resolved())
    }

    /// Inserts the versions of `records` the store does not hold yet, and notes that this fetch
    /// saw the ones it does.
    ///
    /// A pointer keeps every version a fetch saw and `get` answers with the version the newest
    /// fetch returned, so an edit that is undone resolves to the restored text while the
    /// intermediate one still verifies a quote taken from it. The count returned is the number
    /// of versions this call added.
    ///
    /// # Errors
    ///
    /// Returns an error when a record has no canonical pointer scheme, cannot be serialized, or
    /// SQLite cannot write the transaction.
    pub fn put_all(
        &self,
        source: &str,
        records: &[Record],
        fetched_at: DateTime<Utc>,
    ) -> Result<usize> {
        let transaction = self
            .conn
            .unchecked_transaction()
            .context("starting record store transaction")?;
        let inserted = {
            let mut insert = transaction
                .prepare(
                    "INSERT OR IGNORE INTO record(
                         pointer, version, scheme, source, stream, thread, ts,
                         fetched_at, last_seen, json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
                )
                .context("preparing record insertion")?;
            let mut touch = transaction
                .prepare(
                    "UPDATE record SET last_seen = ?3
                     WHERE pointer = ?1 AND version = ?2 AND last_seen < ?3",
                )
                .context("preparing record refresh")?;
            let mut inserted = 0;
            for record in records {
                let scheme = record
                    .pointer
                    .split_once(':')
                    .filter(|(scheme, locator)| !scheme.is_empty() && !locator.is_empty())
                    .map(|(scheme, _)| scheme)
                    .ok_or_else(|| anyhow!("record pointer {} is not canonical", record.pointer))?;
                let json = serde_json::to_string(record)
                    .with_context(|| format!("serializing record {}", record.pointer))?;
                let version = version(record)?;
                let added = insert
                    .execute(params![
                        record.pointer,
                        version,
                        scheme,
                        source,
                        record.stream,
                        record.thread,
                        record.ts.to_rfc3339(),
                        fetched_at.to_rfc3339(),
                        json,
                    ])
                    .with_context(|| format!("storing record {}", record.pointer))?;
                if added == 0 {
                    touch
                        .execute(params![record.pointer, version, fetched_at.to_rfc3339()])
                        .with_context(|| {
                            format!("recording this fetch of record {}", record.pointer)
                        })?;
                }
                inserted += added;
            }
            inserted
        };
        transaction
            .commit()
            .context("committing record store transaction")?;
        Ok(inserted)
    }

    /// Whether each record is the version of its message the newest window that delivered the
    /// message as new delivered, in `records`' order: then a source that sends it again sends
    /// nothing new.
    ///
    /// Only windows count. A record a live resolve obtained, or one a window carried only as
    /// context, was never handed to the agent as new, so it hides nothing. And only the newest
    /// delivered version counts, so a message edited back to an earlier text is new again.
    ///
    /// # Errors
    ///
    /// Returns an error when a record cannot be serialized or SQLite cannot answer.
    pub fn unchanged(&self, records: &[Record]) -> Result<Vec<bool>> {
        let mut statement = self
            .conn
            .prepare("SELECT version FROM delivered WHERE pointer = ?1")
            .context("preparing the delivered version lookup")?;
        records
            .iter()
            .map(|record| {
                let delivered: Option<String> = statement
                    .query_row(params![record.pointer], |row| row.get(0))
                    .optional()
                    .with_context(|| format!("looking up the delivered {}", record.pointer))?;
                Ok(match delivered {
                    Some(delivered) => version(record)? == delivered,
                    None => false,
                })
            })
            .collect()
    }

    /// Adds a published window to the store: every record it holds, and, for each message it
    /// delivered as new, the version it delivered. The store then counts the window as indexed,
    /// so [`Store::catch_up`] does not read it again.
    ///
    /// # Errors
    ///
    /// Returns an error when the window cannot be read or SQLite cannot write.
    pub fn index_window(&self, window: &Window) -> Result<()> {
        let (fetched_at, by_source) = window.records_by_source()?;
        for (source, records) in &by_source {
            self.put_all(source, records, fetched_at)?;
        }
        self.deliver(by_source.values().flatten(), &window.id)?;
        self.mark_indexed(&window.id)
    }

    /// Indexes every completed window after the last one the store indexed, oldest first, so a
    /// store that another machine's fetches left behind knows what they delivered.
    ///
    /// # Errors
    ///
    /// Returns an error when a window cannot be read or SQLite cannot write.
    pub fn catch_up(&self, data: &DataDir) -> Result<()> {
        let indexed: Option<String> = self
            .conn
            .query_row(
                "SELECT window FROM indexed WHERE key = 'windows'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("reading the newest indexed window")?;
        let indexed = indexed.map(|window| WindowId::parse(&window)).transpose()?;
        for window in windows::after(&data.windows(), indexed.as_ref())? {
            self.index_window(&window)?;
        }
        Ok(())
    }

    /// Notes, for each record a window delivered as new, the version it delivered. Context
    /// records are not new, so they leave the delivered version as it was.
    fn deliver<'a>(
        &self,
        records: impl IntoIterator<Item = &'a Record>,
        window: &WindowId,
    ) -> Result<()> {
        let transaction = self
            .conn
            .unchecked_transaction()
            .context("starting the delivered versions transaction")?;
        {
            let mut upsert = transaction
                .prepare(
                    "INSERT INTO delivered(pointer, version) VALUES (?1, ?2)
                     ON CONFLICT(pointer) DO UPDATE SET version = excluded.version",
                )
                .context("preparing the delivered version update")?;
            for record in records.into_iter().filter(|record| !record.context) {
                upsert
                    .execute(params![record.pointer, version(record)?])
                    .with_context(|| {
                        format!(
                            "noting the version window {window} delivered of {}",
                            record.pointer
                        )
                    })?;
            }
        }
        transaction
            .commit()
            .context("committing the delivered versions")
    }

    fn mark_indexed(&self, window: &WindowId) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO indexed(key, window) VALUES ('windows', ?1)
                 ON CONFLICT(key) DO UPDATE SET window = excluded.window",
                params![window.to_string()],
            )
            .with_context(|| format!("noting window {window} as indexed"))?;
        Ok(())
    }

    /// Keeps the records one live resolution obtained, so a rebuilt index still holds them.
    ///
    /// They are written to this machine's state directory, never to the shared data directory:
    /// `ctx resolve` and `ctx cite verify` are read commands, and only the fetch job writes the
    /// share.
    ///
    /// # Errors
    ///
    /// Returns an error when the resolved file cannot be written or SQLite cannot be updated.
    pub fn put_resolved(
        &self,
        source: &str,
        verbatim: Option<bool>,
        records: &[Record],
        fetched_at: DateTime<Utc>,
    ) -> Result<usize> {
        if let Some(verbatim) = verbatim {
            self.cache_source_verbatim(source, verbatim)?;
        }
        if !records.is_empty() {
            let mut jsonl = Vec::new();
            for record in records {
                serde_json::to_writer(
                    &mut jsonl,
                    &Resolved {
                        fetched_at,
                        source: source.to_owned(),
                        verbatim,
                        record: record.clone(),
                    },
                )
                .with_context(|| format!("serializing resolved record {}", record.pointer))?;
                jsonl.push(b'\n');
            }
            let name = format!(
                "{}-{}-{source}.jsonl",
                fetched_at.format("%Y%m%dT%H%M%S%.6fZ"),
                std::process::id()
            );
            write_replace(&self.resolved.join(name), &jsonl)?;
        }
        self.put_all(source, records, fetched_at)
    }

    /// Stores whether a source's records are verbatim, as declared in its protocol hello.
    ///
    /// The declaration is written beside the windows as well, so a rebuilt store keeps it and
    /// `ctx cite verify` still checks that source's quotes. Only the fetch path calls this: it
    /// writes the shared data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite or the source file cannot be written.
    pub fn set_source_verbatim(&self, source: &str, verbatim: bool) -> Result<()> {
        self.cache_source_verbatim(source, verbatim)?;
        let mut sources = read_sources(&self.sources)?;
        if sources.get(source) == Some(&verbatim) {
            return Ok(());
        }
        sources.insert(source.to_owned(), verbatim);
        write_sources(&self.sources, &sources)
    }

    /// Notes a source's verbatim declaration in this machine's index only.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot persist the source metadata.
    pub fn cache_source_verbatim(&self, source: &str, verbatim: bool) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO source_metadata(source, verbatim) VALUES (?1, ?2)
                 ON CONFLICT(source) DO UPDATE SET verbatim = excluded.verbatim",
                params![source, verbatim],
            )
            .with_context(|| format!("storing metadata for source {source}"))?;
        Ok(())
    }

    /// Takes the share's verbatim declarations into the cache, so an open store answers with
    /// what `sources.json` holds now rather than only what it held when the index was built.
    ///
    /// A source the share declares overrides the cache: the fetch job writes that file, and its
    /// declaration is the authority on whether a source's records are its own words. A source
    /// the share does not name keeps the cache, which is what a live resolve on this machine
    /// learned and wrote to `resolved/`.
    ///
    /// # Errors
    ///
    /// Returns an error when the declarations cannot be read or SQLite cannot cache them.
    fn take_declarations(&self) -> Result<()> {
        for (source, verbatim) in read_sources(&self.sources)? {
            self.cache_source_verbatim(&source, verbatim)?;
        }
        Ok(())
    }

    /// Returns the stored verbatim declaration for one source.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the source metadata.
    pub fn source_verbatim(&self, source: &str) -> Result<Option<bool>> {
        self.conn
            .query_row(
                "SELECT verbatim FROM source_metadata WHERE source = ?1",
                [source],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("reading metadata for source {source}"))
    }

    /// Looks up the version of one canonical pointer that the newest fetch returned.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the record or its stored JSON is invalid.
    pub fn get(&self, pointer: &Pointer) -> Result<Option<Record>> {
        let pointer = pointer.to_string();
        let json = self
            .conn
            .query_row(
                "SELECT json FROM record WHERE pointer = ?1
                 ORDER BY last_seen DESC, rowid DESC
                 LIMIT 1",
                [pointer],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading record from store")?;
        json.map(|record| decode_record(&record)).transpose()
    }

    /// Returns every stored version of one pointer with the fetch time that first saw it,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the versions or a stored value is invalid.
    pub fn versions(&self, pointer: &Pointer) -> Result<Vec<(DateTime<Utc>, Record)>> {
        let pointer = pointer.to_string();
        let mut statement = self
            .conn
            .prepare(
                "SELECT fetched_at, json FROM record WHERE pointer = ?1
                 ORDER BY fetched_at ASC, rowid ASC",
            )
            .context("preparing version lookup")?;
        let rows = statement
            .query_map([pointer], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("reading record versions from store")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collecting record versions")?
            .into_iter()
            .map(|(fetched_at, json)| Ok((decode_time(&fetched_at)?, decode_record(&json)?)))
            .collect()
    }

    /// Returns the newest version of the parent and replies for a thread, ordered by
    /// occurrence time.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the thread or a stored record JSON value is
    /// invalid.
    pub fn thread(&self, thread: &Pointer) -> Result<Vec<Record>> {
        let thread = thread.to_string();
        let mut statement = self
            .conn
            .prepare(
                "SELECT json FROM record AS newest
                 WHERE (pointer = ?1 OR thread = ?1)
                   AND rowid = (
                       SELECT rowid FROM record
                       WHERE pointer = newest.pointer
                       ORDER BY last_seen DESC, rowid DESC
                       LIMIT 1)
                 ORDER BY ts ASC, pointer ASC",
            )
            .context("preparing thread lookup")?;
        let rows = statement
            .query_map([thread], |row| row.get::<_, String>(0))
            .context("reading thread from store")?;
        let json = rows
            .collect::<rusqlite::Result<Vec<String>>>()
            .context("collecting thread records")?;
        json.into_iter()
            .map(|record| decode_record(&record))
            .collect()
    }

    /// The newest text the records `author_id` wrote, under one of `schemes`, hold at `path`, a
    /// JSON path into the stored record such as `$.author`: the newest that is neither empty nor
    /// the author's id, which a source writes when it could not learn the name.
    ///
    /// The index on `author_id` answers it from that author's own records, newest first, so it
    /// reads only the ones the answer passes over, never the whole store.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the records.
    pub fn authored(
        &self,
        schemes: &[String],
        author_id: &str,
        path: &str,
    ) -> Result<Option<String>> {
        if schemes.is_empty() {
            return Ok(None);
        }
        let mut statement = self
            .conn
            .prepare(&authored_query(
                schemes.len(),
                "ORDER BY ts DESC, rowid DESC LIMIT 1",
            ))
            .context("preparing the author name lookup")?;
        let parameters = [author_id, path]
            .into_iter()
            .chain(schemes.iter().map(String::as_str));
        let value: Option<String> = statement
            .query_row(rusqlite::params_from_iter(parameters), |row| row.get(0))
            .optional()
            .with_context(|| format!("reading what {author_id}'s records hold at {path}"))?;
        Ok(value.map(|value| value.trim().to_owned()))
    }

    /// Every text the records `author_id` wrote, under one of `schemes`, hold at `path`, as
    /// [`Store::authored`] reads one: each distinct one that is neither empty nor the author's
    /// id, in no particular order. The index on `author_id` answers it from that author's own
    /// records.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the records.
    pub fn authored_all(
        &self,
        schemes: &[String],
        author_id: &str,
        path: &str,
    ) -> Result<Vec<String>> {
        if schemes.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .conn
            .prepare(&authored_query(schemes.len(), ""))
            .context("preparing the author names lookup")?;
        let parameters = [author_id, path]
            .into_iter()
            .chain(schemes.iter().map(String::as_str));
        let values = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(0)
            })
            .with_context(|| format!("reading what {author_id}'s records hold at {path}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("collecting what {author_id}'s records hold at {path}"))?;
        let mut values = values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        Ok(values)
    }
}

/// The query [`Store::authored`] and [`Store::authored_all`] run for `schemes` schemes, with
/// `order` after its conditions: `?1` the author's id, `?2` the JSON path, and `?3` onward the
/// schemes. It names `author_id` exactly as the `record_author` index does, which is what lets
/// SQLite answer it from that index.
fn authored_query(schemes: usize, order: &str) -> String {
    let in_schemes = (0..schemes)
        .map(|index| format!("?{}", index + 3))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT json_extract(json, ?2) FROM record
         WHERE json_extract(json, '$.author_id') = ?1
           AND scheme IN ({in_schemes})
           AND typeof(json_extract(json, ?2)) = 'text'
           AND trim(json_extract(json, ?2)) NOT IN ('', ?1)
         {order}"
    )
}

/// Reports whether the store at `path` is one this build can read.
///
/// It reads the schema version, which every store carries from the moment it is created. What
/// keeps a half-replayed store from being read is [`Store::rebuild`]: it replays into a staging
/// file of its own and copies that into `path` in one transaction only once the replay is
/// complete.
fn usable(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .with_context(|| format!("reading the schema version of {}", path.display()))?;
    Ok(version == SCHEMA_VERSION)
}

/// Copies a complete replay into the store at `path`, replacing everything it held.
///
/// SQLite's backup copies every page under the store's own write lock, in one transaction:
/// a connection that holds the store open reads the old index or the new one, and a copy that
/// dies part way leaves the old one. The file is never swapped for another. Renaming a new file
/// over it would leave every open connection on a file no path names, sharing its write-ahead
/// log and shared-memory file with the new one, which SQLite does not survive.
///
/// The copy does not switch the store's journal mode itself: page one carries the replay's
/// write-ahead-log header, so a store this call creates is in that mode once the copy commits.
/// Switching a new, empty file to it here would take an exclusive lock that a second rebuild
/// opening the same file refuses at once rather than waiting for.
fn publish(replay: &Connection, path: &Path) -> Result<()> {
    let mut live = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    let backup = Backup::new(replay, &mut live)
        .with_context(|| format!("starting to copy the rebuilt index into {}", path.display()))?;
    // A negative page count copies every page in one step. The store's busy timeout waits out
    // a connection writing one batch; where waiting could deadlock, SQLite answers busy at once
    // instead, and the step is tried again.
    let deadline = Instant::now() + PUBLISH_PATIENCE;
    loop {
        let step = backup
            .step(-1)
            .with_context(|| format!("copying the rebuilt index into {}", path.display()))?;
        if step == StepResult::Done {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{} stayed locked by another connection for {} seconds, so the rebuilt index was not copied in",
                path.display(),
                PUBLISH_PATIENCE.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Removes a SQLite database and the write-ahead log files beside it.
fn remove_database(path: &Path) -> Result<()> {
    remove_file(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        remove_file(&PathBuf::from(companion))?;
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// Reads every record a live resolution kept, oldest file first.
fn read_resolved(directory: &Path) -> Result<Vec<Resolved>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in
        fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files.sort();
    let mut resolved = Vec::new();
    for path in files {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            resolved.push(
                serde_json::from_str(line)
                    .with_context(|| format!("decoding {} line {}", path.display(), number + 1))?,
            );
        }
    }
    Ok(resolved)
}

/// Identifies one version of a record by the content a reader quotes.
fn version(record: &Record) -> Result<String> {
    #[derive(Serialize)]
    struct Version<'a> {
        text: &'a str,
        title: Option<&'a str>,
        label: Option<&'a str>,
        author: Option<&'a str>,
    }

    let canonical = serde_json::to_vec(&Version {
        text: &record.text,
        title: record.title.as_deref(),
        label: record.label.as_deref(),
        author: record.author.as_deref(),
    })
    .with_context(|| format!("serializing the version of record {}", record.pointer))?;
    Ok(format!("{:x}", Sha256::digest(&canonical)))
}

fn read_sources(path: &Path) -> Result<BTreeMap<String, bool>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let sources: Sources =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    if sources.version != 1 {
        return Err(anyhow!(
            "{} has unsupported source metadata version {}",
            path.display(),
            sources.version
        ));
    }
    Ok(sources
        .sources
        .into_iter()
        .map(|(source, entry)| (source, entry.verbatim))
        .collect())
}

fn write_sources(path: &Path, sources: &BTreeMap<String, bool>) -> Result<()> {
    let sources = Sources {
        version: 1,
        sources: sources
            .iter()
            .map(|(source, verbatim)| {
                (
                    source.clone(),
                    SourceEntry {
                        verbatim: *verbatim,
                    },
                )
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&sources).context("serializing source metadata")?;
    write_replace(path, &bytes)
}

fn decode_time(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("decoding fetch time {value} stored in SQLite"))?
        .with_timezone(&Utc))
}

fn decode_record(json: &str) -> Result<Record> {
    serde_json::from_str(json).context("decoding record stored in SQLite")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context as _, Result};
    use chrono::{DateTime, TimeZone as _, Utc};

    use super::{authored_query, publish, Store};
    use crate::config::{DataDir, StateDir};
    use crate::pointer::Pointer;
    use crate::record::Record;
    use crate::windows::{Manifest, SourcedRecord, Status};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn scratch(label: &str) -> Result<PathBuf> {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-store-{label}-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(root)
    }

    fn remove_scratch(path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    }

    /// The staging databases, and their write-ahead logs, left in a state directory.
    fn staging_files(state: &StateDir) -> Result<Vec<String>> {
        let root = state.root();
        let mut found = Vec::new();
        for entry in
            std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?
        {
            let name = entry
                .with_context(|| format!("reading {}", root.display()))?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if name.starts_with("store.rebuilding") {
                found.push(name);
            }
        }
        found.sort();
        Ok(found)
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, hour, 0, 0)
            .single()
            .unwrap_or(DateTime::UNIX_EPOCH)
    }

    fn record(pointer: &str, text: &str) -> Record {
        Record {
            pointer: pointer.to_owned(),
            stream: "C0EXAMPLE01".to_owned(),
            ts: at(12),
            label: None,
            author: Some("Alice".to_owned()),
            author_id: None,
            thread: None,
            title: None,
            url: None,
            text: text.to_owned(),
            aliases: Vec::new(),
            context: false,
            raw: serde_json::Value::Null,
        }
    }

    fn pointer(value: &str) -> Result<Pointer> {
        let (scheme, locator) = value
            .split_once(':')
            .context("test pointers are canonical")?;
        Ok(Pointer {
            scheme: scheme.to_owned(),
            locator: locator.to_owned(),
        })
    }

    fn window(
        data: &DataDir,
        id: &str,
        generated_at: DateTime<Utc>,
        records: &[Record],
    ) -> Result<()> {
        let dir = data.windows().join(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let manifest = Manifest {
            version: 1,
            window: id.to_owned(),
            generated_at,
            window_end: generated_at,
            sources: std::collections::BTreeMap::new(),
            warnings: Vec::new(),
            exit_status: Status::Ok,
        };
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        let mut jsonl = Vec::new();
        for record in records {
            serde_json::to_writer(
                &mut jsonl,
                &SourcedRecord {
                    source: "slack".to_owned(),
                    record: record.clone(),
                },
            )?;
            jsonl.push(b'\n');
        }
        std::fs::write(dir.join("records.jsonl"), jsonl)?;
        std::fs::write(dir.join("rendered.md"), b"# window\n")?;
        Ok(())
    }

    /// What `unchanged` says of `text` at `pointer`.
    fn unchanged(store: &Store, pointer: &str, text: &str) -> Result<bool> {
        Ok(store
            .unchanged(&[record(pointer, text)])?
            .first()
            .copied()
            .unwrap_or_default())
    }

    /// Only a window that delivered a message as new makes it not new again: a live resolve
    /// and a window that carried it as context do not, and a message edited back to an earlier
    /// text is new again.
    #[test]
    fn only_the_newest_version_a_window_delivered_is_unchanged() -> Result<()> {
        let root = scratch("delivered")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        let store = Store::load(&data, &state)?;
        let resolved = "slack:C0EXAMPLE01/1";
        store.put_resolved(
            "slack",
            Some(true),
            &[record(resolved, "resolved live")],
            at(10),
        )?;
        let parent = "slack:C0EXAMPLE01/2";
        let mut context = record(parent, "a parent");
        context.context = true;
        let edited = "slack:C0EXAMPLE01/3";
        window(
            &data,
            "20261025T110000Z-0000",
            at(11),
            &[context, record(edited, "A")],
        )?;
        window(
            &data,
            "20261025T120000Z-0000",
            at(12),
            &[record(edited, "B")],
        )?;
        store.catch_up(&data)?;
        let caught_up = (
            unchanged(&store, resolved, "resolved live")?,
            unchanged(&store, parent, "a parent")?,
            unchanged(&store, edited, "B")?,
            unchanged(&store, edited, "A")?,
        );
        window(
            &data,
            "20261025T130000Z-0000",
            at(13),
            &[record(edited, "A")],
        )?;
        store.catch_up(&data)?;
        let restored = unchanged(&store, edited, "A")?;
        let rebuilt = Store::rebuild(&data, &state)?;
        let after_rebuild = (
            unchanged(&rebuilt, edited, "A")?,
            unchanged(&rebuilt, edited, "B")?,
        );
        drop((store, rebuilt));
        remove_scratch(&root)?;

        assert_eq!(
            caught_up,
            (false, false, true, false),
            "resolved, context, newest delivered, earlier delivered"
        );
        assert!(restored, "the restored text is the newest delivered");
        assert_eq!(after_rebuild, (true, false), "a rebuild agrees");
        Ok(())
    }

    #[test]
    fn edited_message_keeps_both_versions() -> Result<()> {
        let root = scratch("versions")?;
        let store = Store::open(
            &root.join("store.sqlite3"),
            &root.join("sources.json"),
            &root.join("resolved"),
        )?;
        let inserted = store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the first text")],
            at(12),
        )?;
        let edited = store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the edited text")],
            at(13),
        )?;
        let repeated = store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the edited text")],
            at(14),
        )?;
        let pointer = pointer("slack:C0EXAMPLE01/1")?;
        let newest = store.get(&pointer)?;
        let versions = store.versions(&pointer)?;
        remove_scratch(&root)?;

        assert_eq!((inserted, edited, repeated), (1, 1, 0));
        assert_eq!(
            newest.map(|record| record.text),
            Some("the edited text".to_owned())
        );
        assert_eq!(
            versions
                .iter()
                .map(|(fetched_at, record)| (*fetched_at, record.text.clone()))
                .collect::<Vec<_>>(),
            vec![
                (at(12), "the first text".to_owned()),
                (at(13), "the edited text".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn rebuild_matches_incremental() -> Result<()> {
        let root = scratch("rebuild")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        window(
            &data,
            "20261025T120000Z-0000",
            at(12),
            &[record("slack:C0EXAMPLE01/1", "the first text")],
        )?;
        window(
            &data,
            "20261025T130000Z-0000",
            at(13),
            &[record("slack:C0EXAMPLE01/1", "the edited text")],
        )?;
        let pointer = pointer("slack:C0EXAMPLE01/1")?;

        let incremental = Store::load(&data, &state)?;
        incremental.set_source_verbatim("slack", true)?;
        incremental.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the first text")],
            at(12),
        )?;
        incremental.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the edited text")],
            at(13),
        )?;
        let expected = incremental
            .versions(&pointer)?
            .into_iter()
            .map(|(fetched_at, record)| (fetched_at, record.text))
            .collect::<Vec<_>>();
        drop(incremental);

        std::fs::remove_file(state.store())?;
        let rebuilt = Store::load(&data, &state)?;
        let found = rebuilt
            .versions(&pointer)?
            .into_iter()
            .map(|(fetched_at, record)| (fetched_at, record.text))
            .collect::<Vec<_>>();
        let verbatim = rebuilt.source_verbatim("slack")?;
        remove_scratch(&root)?;

        assert_eq!(found, expected);
        assert_eq!(found.len(), 2);
        assert_eq!(
            verbatim,
            Some(true),
            "a rebuilt store restores each source's verbatim flag"
        );
        Ok(())
    }

    #[test]
    fn an_open_store_reads_a_declaration_written_after_it_was_built() -> Result<()> {
        let root = scratch("late-declaration")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        // This machine builds its index while the share declares nothing.
        let built = Store::load(&data, &state)?;
        let before = built.source_verbatim("echo")?;
        drop(built);

        // The fetch job declares the source beside the windows, as another machine's fetch
        // does, and the share reaches this one.
        std::fs::create_dir_all(root.join("data"))?;
        std::fs::write(
            data.sources(),
            b"{\"version\":1,\"sources\":{\"echo\":{\"verbatim\":true}}}",
        )?;
        let loaded = Store::load(&data, &state)?;
        let after = loaded.source_verbatim("echo")?;
        drop(loaded);
        remove_scratch(&root)?;

        assert_eq!(before, None, "nothing had declared the source yet");
        assert_eq!(
            after,
            Some(true),
            "an index that was already built answers with the share's declaration"
        );
        Ok(())
    }

    #[test]
    fn reverted_message_resolves_to_the_newest_fetch() -> Result<()> {
        let root = scratch("reverted")?;
        let store = Store::open(
            &root.join("store.sqlite3"),
            &root.join("sources.json"),
            &root.join("resolved"),
        )?;
        let pointer = pointer("slack:C0EXAMPLE01/1")?;
        store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the first text")],
            at(11),
        )?;
        store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the edited text")],
            at(12),
        )?;
        let reverted = store.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the first text")],
            at(13),
        )?;
        let newest = store.get(&pointer)?;
        let versions = store.versions(&pointer)?;
        remove_scratch(&root)?;

        assert_eq!(reverted, 0, "the reverted text is a version already held");
        assert_eq!(
            newest.map(|record| record.text),
            Some("the first text".to_owned()),
            "the newest fetch answers the lookup"
        );
        assert_eq!(
            versions
                .iter()
                .map(|(fetched_at, record)| (*fetched_at, record.text.clone()))
                .collect::<Vec<_>>(),
            vec![
                (at(11), "the first text".to_owned()),
                (at(12), "the edited text".to_owned()),
            ],
            "versions keep the fetch that first saw each of them"
        );
        Ok(())
    }

    #[test]
    fn live_resolved_records_survive_a_rebuild() -> Result<()> {
        let root = scratch("live-resolved")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        let pointer = pointer("echo:x")?;
        let store = Store::load(&data, &state)?;
        store.put_resolved(
            "echo",
            Some(true),
            &[record("echo:x", "live resolved text")],
            at(12),
        )?;
        drop(store);

        std::fs::remove_file(state.store())?;
        let rebuilt = Store::load(&data, &state)?;
        let found = rebuilt.get(&pointer)?;
        let verbatim = rebuilt.source_verbatim("echo")?;
        let shared = data.sources().exists();
        remove_scratch(&root)?;

        assert_eq!(
            found.map(|record| record.text),
            Some("live resolved text".to_owned()),
            "a rebuild replays the records a live resolve kept"
        );
        assert_eq!(verbatim, Some(true));
        assert!(
            !shared,
            "a live resolve keeps its records out of the shared data directory"
        );
        Ok(())
    }

    #[test]
    fn an_interrupted_rebuild_is_not_trusted() -> Result<()> {
        let root = scratch("interrupted")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        window(
            &data,
            "20261025T120000Z-0000",
            at(12),
            &[record("slack:C0EXAMPLE01/1", "the window text")],
        )?;
        // A rebuild that died mid-replay: its own staging file holds part of the history and
        // was never copied into the store. Another run never deletes that file, because
        // the process that made it may still be writing it. No process has pid 0, so this
        // name can never be one the test binary itself would draw.
        let staging = state.store().with_extension("rebuilding-0-0");
        let partial = Store::open(&staging, &data.sources(), &state.resolved())?;
        partial.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/2", "a half-replayed record")],
            at(12),
        )?;
        drop(partial);

        let loaded = Store::load(&data, &state)?;
        let window_record = loaded.get(&pointer("slack:C0EXAMPLE01/1")?)?;
        let half = loaded.get(&pointer("slack:C0EXAMPLE01/2")?)?;
        let leftover = staging.exists();
        drop(loaded);

        // The other half of the rule: the file a rebuild makes is that rebuild's own to
        // remove. This window cannot be decoded, so the replay below stops at its manifest.
        let broken = data.windows().join("20261025T130000Z-0001");
        std::fs::create_dir_all(&broken)
            .with_context(|| format!("creating {}", broken.display()))?;
        std::fs::write(broken.join("manifest.json"), b"{ not a manifest")?;
        std::fs::write(broken.join("records.jsonl"), b"")?;
        std::fs::write(broken.join("rendered.md"), b"# window\n")?;
        let failed = Store::rebuild(&data, &state);
        let staging_files = staging_files(&state)?;
        remove_scratch(&root)?;

        assert_eq!(
            window_record.map(|record| record.text),
            Some("the window text".to_owned()),
            "the next run replays the windows instead of trusting the staging file"
        );
        assert_eq!(
            half.map(|record| record.text),
            None,
            "nothing the interrupted rebuild left behind is trusted"
        );
        assert!(
            leftover,
            "another run's staging file is left where it is, not unlinked"
        );
        assert!(
            failed.is_err(),
            "a rebuild that cannot read a window fails instead of publishing a partial index"
        );
        assert_eq!(
            staging_files,
            vec!["store.rebuilding-0-0".to_owned()],
            "a failed rebuild removes the staging files it made and leaves another run's alone"
        );
        Ok(())
    }

    /// A rebuild replaces the index inside the file every open store already holds, so a write
    /// through a store opened before the rebuild lands in the index every later open reads.
    /// Renaming a new file over the old one would leave that store writing into a file no path
    /// names any more, while the two databases share one write-ahead log and one shared-memory
    /// file, which SQLite does not survive.
    #[test]
    fn a_store_opened_before_a_rebuild_writes_into_the_rebuilt_index() -> Result<()> {
        let root = scratch("open-across-rebuild")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        window(
            &data,
            "20261025T120000Z-0000",
            at(12),
            &[record("slack:C0EXAMPLE01/1", "the window text")],
        )?;
        let held = Store::load(&data, &state)?;

        drop(Store::rebuild(&data, &state)?);
        held.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/2", "written after the rebuild")],
            at(13),
        )?;
        let reopened = Store::load(&data, &state)?;
        let window_record = reopened.get(&pointer("slack:C0EXAMPLE01/1")?)?;
        let late = reopened.get(&pointer("slack:C0EXAMPLE01/2")?)?;
        drop(held);
        drop(reopened);
        remove_scratch(&root)?;

        assert_eq!(
            window_record.map(|record| record.text),
            Some("the window text".to_owned())
        );
        assert_eq!(
            late.map(|record| record.text),
            Some("written after the rebuild".to_owned()),
            "a write through a store opened before the rebuild was lost"
        );
        Ok(())
    }

    /// A store a rebuild creates is in write-ahead-log mode as soon as the copy commits, so no
    /// later open has to switch a file other connections may hold to it.
    #[test]
    fn a_copied_replay_leaves_a_new_store_in_write_ahead_log_mode() -> Result<()> {
        let root = scratch("publish-mode")?;
        let replay = Store::open(
            &root.join("replay.sqlite3"),
            &root.join("sources.json"),
            &root.join("resolved"),
        )?;
        replay.put_all(
            "slack",
            &[record("slack:C0EXAMPLE01/1", "the window text")],
            at(12),
        )?;
        let path = root.join("store.sqlite3");

        publish(&replay.conn, &path)?;
        let mode: String =
            rusqlite::Connection::open(&path)?
                .pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        drop(replay);
        remove_scratch(&root)?;

        assert_eq!(mode, "wal");
        Ok(())
    }

    #[test]
    fn concurrent_rebuilds_do_not_collide() -> Result<()> {
        let root = scratch("concurrent-rebuild")?;
        let data = DataDir::new(root.join("data"));
        let state = StateDir::new(root.join("state"));
        window(
            &data,
            "20261025T120000Z-0000",
            at(12),
            &[record("slack:C0EXAMPLE01/1", "the window text")],
        )?;
        let mut failures = Vec::new();
        for _ in 0..8 {
            // Every tree rebuilds once after a schema change, and no lock covers a read.
            let _ = std::fs::remove_file(state.store());
            let outcomes = std::thread::scope(|scope| {
                let handles = (0..2)
                    .map(|_| {
                        scope.spawn(|| -> Result<Option<String>> {
                            let store = Store::load(&data, &state)?;
                            Ok(store
                                .get(&pointer("slack:C0EXAMPLE01/1")?)?
                                .map(|record| record.text))
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(std::thread::ScopedJoinHandle::join)
                    .collect::<Vec<_>>()
            });
            for outcome in outcomes {
                match outcome {
                    Ok(Ok(text)) if text.as_deref() == Some("the window text") => {}
                    Ok(Ok(text)) => failures.push(format!("a rebuild answered {text:?}")),
                    Ok(Err(error)) => failures.push(format!("{error:#}")),
                    Err(_) => failures.push("a rebuild panicked".to_owned()),
                }
            }
        }
        remove_scratch(&root)?;

        assert!(
            failures.is_empty(),
            "two rebuilds at once both answer from the windows: {failures:?}"
        );
        Ok(())
    }

    /// A person's name is read from the records they wrote, newest first, and so is every name
    /// those records hold: a record whose text at the path is empty or the author's own id
    /// names nobody, and a record of a scheme not asked about is not read. SQLite answers
    /// through the index on `author_id`.
    #[test]
    fn an_authors_name_comes_from_their_newest_record_through_the_author_index() -> Result<()> {
        let root = scratch("authored")?;
        let store = Store::open(
            &root.join("store.sqlite3"),
            &root.join("sources.json"),
            &root.join("resolved"),
        )?;
        let by = |pointer: &str, hour: u32, author: &str, full: Option<&str>| {
            let mut written = record(pointer, "a message");
            written.ts = at(hour);
            written.author = Some(author.to_owned());
            written.author_id = Some("U0EXAMPLEA".to_owned());
            if let Some(full) = full {
                written.raw = serde_json::json!({ "user_profile": { "real_name": full } });
            }
            written
        };
        store.put_all(
            "slack",
            &[
                by("slack:C0EXAMPLE01/1", 9, "Kimberly", Some("Kim Lee")),
                by("slack:C0EXAMPLE01/2", 10, "Kim", Some("")),
                by("slack:C0EXAMPLE01/3", 11, "U0EXAMPLEA", None),
            ],
            at(12),
        )?;
        store.put_all(
            "other",
            &[by(
                "other:C0EXAMPLE01/4",
                12,
                "Someone Else",
                Some("Someone Else"),
            )],
            at(12),
        )?;
        let slack = ["slack".to_owned()];
        let full = store.authored(&slack, "U0EXAMPLEA", "$.raw.user_profile.real_name")?;
        let author = store.authored(&slack, "U0EXAMPLEA", "$.author")?;
        let nobody = store.authored(&slack, "U0EXAMPLEB", "$.author")?;
        let no_scheme = store.authored(&[], "U0EXAMPLEA", "$.author")?;
        let every_author = store.authored_all(&slack, "U0EXAMPLEA", "$.author")?;
        let plan = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                authored_query(slack.len(), "")
            ))?
            .query_map(
                rusqlite::params!["U0EXAMPLEA", "$.author", "slack"],
                |row| row.get::<_, String>(3),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(store);
        remove_scratch(&root)?;

        assert_eq!(full.as_deref(), Some("Kim Lee"));
        assert_eq!(author.as_deref(), Some("Kim"));
        assert_eq!(nobody, None);
        assert_eq!(no_scheme, None);
        assert_eq!(every_author, ["Kim", "Kimberly"]);
        assert!(
            plan.iter()
                .any(|step| step.contains("USING INDEX record_author")),
            "the lookup reads the author index: {plan:?}"
        );
        Ok(())
    }
}
