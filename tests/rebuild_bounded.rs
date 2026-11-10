//! `Store::rebuild` must keep a bounded number of records in memory.
//!
//! The rebuild reads every completed window of a share. A share grows one window at a time and
//! never discards one, so a rebuild that holds all of the windows at the same time grows with
//! the age of the share. This test measures the memory of two rebuilds of the same shape. The
//! second share holds four times the windows of the first. Each window holds the same number
//! of records. If the rebuild holds one window at a time, the two peaks are near each other.
//! If the rebuild holds all of the windows, the second peak is near four times the first.
//!
//! # How the measurement works
//!
//! This file declares its own `#[global_allocator]`. The allocator counts the live bytes of
//! Rust's heap and keeps the highest count it saw. A `#[global_allocator]` applies to one test
//! binary, so this counter reaches no other test and no part of the shipped program. This file
//! holds one `#[test]`, so no other test thread adds to the count.
//!
//! The index is a SQLite database, and the bundled SQLite allocates through C and not through
//! Rust's allocator. The counter therefore does not see the pages of the index. It sees the
//! records that the rebuild holds in Rust memory, which is the quantity this test is about.
//!
//! # The numbers
//!
//! The counts are 20 windows against 80 windows, and 50 records in each window. The larger
//! share holds 4000 records, enough that a rebuild which keeps every window of it stands
//! megabytes above the smaller share's rebuild, and the whole test writes the windows and
//! rebuilds three times in about 2 seconds. The allowance is 262144 bytes (256 KiB).
//!
//! Measured on 2026-11-10, this tree's rebuild gave peaks of 58759 and 79218 bytes. The
//! difference is 20459 bytes, which is 8 percent of the allowance, so the allowance leaves
//! this rebuild much room. The rebuild that held every window at the same time gave 641211
//! and 2503992 bytes on the same day. That difference is 1862781 bytes, which is 7.1 times
//! the allowance, so this test fails for that rebuild by a wide margin.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context as _, Result};
use chrono::{DateTime, TimeZone as _, Utc};
use ctx_memory::config::{DataDir, StateDir};
use ctx_memory::record::Record;
use ctx_memory::store::Store;
use ctx_memory::windows::{Manifest, SourcedRecord, Status};

/// The live bytes of Rust's heap now.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The highest value of `LIVE` since the last write to this counter.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, with a counter of the live bytes and of the highest live bytes.
struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Adds one allocation to the live bytes and lifts the peak to the new total.
fn added(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every method gives the pointer, the layout and the size to the system allocator
// unchanged, and only adds counters around it. The counters allocate nothing themselves.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc_zeroed(layout);
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, new_size);
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            added(new_size);
        }
        moved
    }
}

/// How many more bytes than the smaller share's rebuild the larger share's rebuild may hold.
const ALLOWANCE: usize = 262_144;

/// The number of windows of the smaller share.
const FEW_WINDOWS: usize = 20;

/// The number of windows of the larger share.
const MANY_WINDOWS: usize = 80;

/// The number of records in each window of both shares.
const RECORDS_PER_WINDOW: usize = 50;

/// The text of each record. A message of this length makes the difference between the two
/// peaks large enough to read, and keeps the files small enough to write in a moment.
const TEXT: &str = "The fetch job read this message from the channel and wrote it into the \
     window, with the author, the time and the thread it belongs to. The index keeps one row \
     for each version of it that a fetch returned.";

fn scratch(label: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-rebuild-bounded-{label}-{}",
        std::process::id()
    ));
    if root.exists() {
        fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(root: &Path) -> Result<()> {
    fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

/// The time of window `index`, which is also the time of every record in it.
fn at(index: usize) -> Result<DateTime<Utc>> {
    let minute = u32::try_from(index / 60).context("the window index fits in a minute")?;
    let second = u32::try_from(index % 60).context("the window index fits in a second")?;
    Utc.with_ymd_and_hms(2026, 10, 25, 12, minute, second)
        .single()
        .context("the window time is a real time")
}

/// Writes `windows` completed windows under `data`, each with `records` records.
fn share(data: &DataDir, windows: usize, records: usize) -> Result<()> {
    for index in 0..windows {
        let generated_at = at(index)?;
        let id = format!("{}-0000", generated_at.format("%Y%m%dT%H%M%SZ"));
        let dir = data.windows().join(&id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let manifest = Manifest {
            version: 1,
            window: id.clone(),
            generated_at,
            window_end: generated_at,
            sources: BTreeMap::new(),
            warnings: Vec::new(),
            exit_status: Status::Ok,
        };
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).context("serializing the test manifest")?,
        )
        .with_context(|| format!("writing the manifest of {id}"))?;
        let mut jsonl = String::new();
        for number in 0..records {
            let sourced = SourcedRecord {
                source: "slack".to_owned(),
                record: Record {
                    pointer: format!("slack:C0EXAMPLE01/{index}.{number}"),
                    stream: "C0EXAMPLE01".to_owned(),
                    ts: generated_at,
                    label: None,
                    author: Some("Bob".to_owned()),
                    author_id: None,
                    thread: None,
                    title: None,
                    url: None,
                    text: format!("{TEXT} ({index}.{number})"),
                    aliases: Vec::new(),
                    context: false,
                    raw: serde_json::Value::Null,
                },
            };
            jsonl.push_str(&serde_json::to_string(&sourced).context("serializing a test record")?);
            jsonl.push('\n');
        }
        fs::write(dir.join("records.jsonl"), jsonl)
            .with_context(|| format!("writing the records of {id}"))?;
        fs::write(dir.join("rendered.md"), "# window\n")
            .with_context(|| format!("writing the rendering of {id}"))?;
    }
    Ok(())
}

/// Rebuilds the index of a share of `windows` windows, and returns the highest live bytes of
/// Rust's heap during that rebuild, above the live bytes it started from.
fn peak_of_rebuild(label: &str, windows: usize, records: usize) -> Result<usize> {
    let root = scratch(label)?;
    let data = DataDir::new(root.join("data"));
    let state = StateDir::new(root.join("state"));
    share(&data, windows, records)?;

    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let store = Store::rebuild(&data, &state).context("rebuilding the index")?;
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    drop(store);

    remove_scratch(&root)?;
    Ok(peak)
}

/// The memory of a rebuild must not grow with the number of windows on the share.
#[test]
fn rebuild_holds_a_bounded_number_of_records() -> Result<()> {
    let started = Instant::now();
    // One small rebuild first, so that the one-time allocations of SQLite and of the JSON
    // reader belong to neither of the two measurements.
    peak_of_rebuild("warm-up", 1, 1)?;

    let few = peak_of_rebuild("few", FEW_WINDOWS, RECORDS_PER_WINDOW)?;
    let many = peak_of_rebuild("many", MANY_WINDOWS, RECORDS_PER_WINDOW)?;
    let hundredths = many.saturating_mul(100) / few.max(1);
    println!(
        "{FEW_WINDOWS} windows: {few} bytes; {MANY_WINDOWS} windows: {many} bytes; \
         ratio {}.{:02}; {RECORDS_PER_WINDOW} records in each window; \
         allowance {ALLOWANCE} bytes; {:.1} s",
        hundredths / 100,
        hundredths % 100,
        started.elapsed().as_secs_f64()
    );

    assert!(
        many <= few.saturating_add(ALLOWANCE),
        "a rebuild of {MANY_WINDOWS} windows held {many} bytes, and one of {FEW_WINDOWS} \
         windows of the same shape held {few} bytes: {}.{:02} times as much, over the \
         allowance of {ALLOWANCE} bytes. Each window holds {RECORDS_PER_WINDOW} records. \
         The rebuild holds every window of the share at the same time, so its memory grows \
         with the share.",
        hundredths / 100,
        hundredths % 100,
    );
    Ok(())
}
