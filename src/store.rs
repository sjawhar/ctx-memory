//! Durable SQLite storage for normalized source records.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::pointer::Pointer;
use crate::record::Record;

/// SQLite-backed local record store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens a store, creating its parent directory and schema when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent cannot be created, SQLite cannot open or configure the
    /// database, or the schema cannot be created.
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling SQLite write-ahead logging")?;
        conn.pragma_update(None, "synchronous", "FULL")
            .context("enabling full SQLite synchronization")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS record (
                pointer TEXT PRIMARY KEY,
                scheme TEXT NOT NULL,
                source TEXT NOT NULL,
                stream TEXT NOT NULL,
                thread TEXT,
                ts TEXT NOT NULL,
                json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS record_thread ON record(thread);
            CREATE TABLE IF NOT EXISTS source_metadata (
                source TEXT PRIMARY KEY,
                verbatim INTEGER NOT NULL CHECK (verbatim IN (0, 1))
            );
            PRAGMA user_version = 1;
            ",
        )
        .context("creating record store schema")?;
        Ok(Self { conn })
    }

    /// Inserts records from one source, preserving an already-stored record for every pointer.
    ///
    /// # Errors
    ///
    /// Returns an error when a record has no canonical pointer scheme, cannot be serialized, or
    /// SQLite cannot write the transaction.
    pub fn put_all(&self, source: &str, records: &[Record]) -> Result<usize> {
        let transaction = self
            .conn
            .unchecked_transaction()
            .context("starting record store transaction")?;
        let inserted = {
            let mut statement = transaction
                .prepare(
                    "INSERT OR IGNORE INTO record(pointer, scheme, source, stream, thread, ts, json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .context("preparing record insertion")?;
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
                inserted += statement
                    .execute(params![
                        record.pointer,
                        scheme,
                        source,
                        record.stream,
                        record.thread,
                        record.ts.to_rfc3339(),
                        json,
                    ])
                    .with_context(|| format!("storing record {}", record.pointer))?;
            }
            inserted
        };
        transaction
            .commit()
            .context("committing record store transaction")?;
        Ok(inserted)
    }

    /// Stores whether a source's records are verbatim, as declared in its protocol hello.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot persist the source metadata.
    pub fn set_source_verbatim(&self, source: &str, verbatim: bool) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO source_metadata(source, verbatim) VALUES (?1, ?2)
                 ON CONFLICT(source) DO UPDATE SET verbatim = excluded.verbatim",
                params![source, verbatim],
            )
            .with_context(|| format!("storing metadata for source {source}"))?;
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

    /// Looks up one record by its canonical pointer.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read the record or its stored JSON is invalid.
    pub fn get(&self, pointer: &Pointer) -> Result<Option<Record>> {
        let pointer = pointer.to_string();
        let json = self
            .conn
            .query_row(
                "SELECT json FROM record WHERE pointer = ?1",
                [pointer],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading record from store")?;
        json.map(|record| decode_record(&record)).transpose()
    }

    /// Returns the stored parent and replies for a thread, ordered by occurrence time.
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
                "SELECT json FROM record
                 WHERE pointer = ?1 OR thread = ?1
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
}

fn decode_record(json: &str) -> Result<Record> {
    serde_json::from_str(json).context("decoding record stored in SQLite")
}
