//! What `ctx pending` leaves out: the messages the deployment excludes, and what would take
//! one answer past its size.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{DateTime, TimeZone as _, Utc};

use super::Pending;
use crate::config::DataDir;
use crate::delta::{Delta, NewMessage, OpenFlag, ReplyRef};
use crate::exclude::Exclusion;
use crate::windows::{Manifest, Status};

/// [`super::pending_in`] with no window cap and no Slack source, which these tests do not
/// exercise.
fn pending_in(
    data: &DataDir,
    exclusion: &Exclusion,
    budget: usize,
    offset: usize,
) -> Result<Pending> {
    super::pending_in(data, None, exclusion, budget, offset, None)
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-pending-size-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn time(minute: u32) -> Result<DateTime<Utc>> {
    Utc.with_ymd_and_hms(2026, 10, 30, 2, minute, 0)
        .single()
        .context("a valid test time")
}

fn message(pointer: &str, minute: u32) -> Result<NewMessage> {
    Ok(NewMessage {
        pointer: pointer.to_owned(),
        label: None,
        ts: time(minute)?,
        author: None,
        is_reply: false,
        thread: None,
        excerpt: "an excerpt as long as a real one, which a delta cuts at 160 characters"
            .to_owned(),
        matches: Vec::new(),
    })
}

/// Writes one completed window `id` whose delta lists `messages`, as a fetch writes it.
fn window(data: &DataDir, id: &str, messages: Vec<NewMessage>) -> Result<PathBuf> {
    window_with(data, id, messages, Vec::new(), Vec::new())
}

/// Writes one completed window `id` whose delta lists `messages`, `unreviewed` flags and
/// `opened` status changes, as a fetch writes it.
fn window_with(
    data: &DataDir,
    id: &str,
    messages: Vec<NewMessage>,
    unreviewed: Vec<OpenFlag>,
    opened: Vec<String>,
) -> Result<PathBuf> {
    let end = time(0)?;
    let dir = data.windows().join(id);
    std::fs::create_dir_all(&dir)?;
    let manifest = Manifest {
        version: 1,
        window: id.to_owned(),
        generated_at: end,
        window_end: end,
        sources: BTreeMap::new(),
        warnings: Vec::new(),
        exit_status: Status::Ok,
    };
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
    std::fs::write(dir.join("records.jsonl"), b"")?;
    std::fs::write(dir.join("rendered.md"), b"# window\n")?;
    let delta = Delta {
        version: 1,
        window: id.to_owned(),
        window_end: end,
        generated_at: end,
        previous_window: None,
        new_messages: messages,
        unreviewed,
        closed_since_previous: Vec::new(),
        opened_since_previous: opened,
        omitted_messages: None,
        omitted_changes: None,
    };
    std::fs::write(dir.join("delta.json"), serde_json::to_vec(&delta)?)?;
    Ok(dir)
}

fn messages(prefix: &str, count: u32) -> Result<Vec<NewMessage>> {
    (0..count)
        .map(|n| message(&format!("{prefix}/{n}"), n % 60))
        .collect()
}

fn remove(root: &Path) -> Result<()> {
    std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

/// An exclusion set after a window was fetched leaves its messages out of the answer, and the
/// window's files are not touched.
#[test]
fn an_excluded_stream_never_appears_in_a_pending_delta() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let mut mixed = messages("github:example-org/busy@c", 5)?;
    mixed.extend(messages("github:example-org/busy#1/comment", 2)?);
    mixed.extend(messages("slack:C0EXAMPLE01", 3)?);
    let dir = window(&data, "20261030T020000Z-0000", mixed)?;
    let before = std::fs::read(dir.join("delta.json"))?;

    let exclusion = Exclusion::new(&["github:example-org/busy@*".to_owned()])?;
    let pending = pending_in(&data, &exclusion, usize::MAX, 0)?;
    let after = std::fs::read(dir.join("delta.json"))?;
    remove(&root)?;

    let delta = pending.delta.context("one window, one delta")?;
    assert!(
        delta
            .new_messages
            .iter()
            .all(|message| !message.pointer.starts_with("github:example-org/busy@")),
        "no excluded commit is listed"
    );
    assert_eq!(
        delta.new_messages.len(),
        5,
        "the comments and Slack messages stay"
    );
    assert_eq!(pending.windows.first().map(|w| w.new_messages), Some(5));
    assert_eq!(
        before, after,
        "the window's delta.json is as the fetch wrote it"
    );
    Ok(())
}

/// Windows whose deltas together pass the answer's size wait for a later answer, oldest first.
#[test]
fn windows_past_the_answer_size_wait_for_the_next_answer() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let ids = [
        "20261030T020000Z-0000",
        "20261030T021500Z-0000",
        "20261030T023000Z-0000",
    ];
    for (n, id) in ids.iter().enumerate() {
        window(&data, id, messages(&format!("slack:C0EXAMPLE0{n}"), 40)?)?;
    }
    let whole = pending_in(&data, &Exclusion::default(), usize::MAX, 0)?;
    let one = whole
        .delta
        .as_ref()
        .context("three windows, one delta")?
        .answer_len()
        / 3;
    // Room for two windows' deltas and not three.
    let pending = pending_in(&data, &Exclusion::default(), one * 2 + one / 2, 0)?;
    remove(&root)?;

    assert_eq!(whole.later_windows, 0);
    let listed = pending
        .windows
        .iter()
        .map(|window| window.window.as_str())
        .collect::<Vec<_>>();
    assert_eq!(listed, ids[..2]);
    assert_eq!(pending.later_windows, 1);
    let delta = pending.delta.context("two windows, one delta")?;
    assert_eq!(delta.new_messages.len(), 80);
    assert_eq!(
        delta.omitted_messages, None,
        "whole windows fit, so none is cut"
    );
    assert!(pending
        .windows
        .iter()
        .all(|window| window.listed_messages == window.new_messages));
    Ok(())
}

/// One window larger than an answer is still listed, with the oldest messages that fit; the
/// answer says how many it left out, and stays within its size.
#[test]
fn a_window_larger_than_an_answer_lists_what_fits_and_counts_the_rest() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    window(
        &data,
        "20261030T020000Z-0000",
        messages("github:example-org/busy@c", 2_000)?,
    )?;
    window(
        &data,
        "20261030T021500Z-0000",
        messages("slack:C0EXAMPLE01", 3)?,
    )?;
    let budget = 64 * 1024;
    let pending = pending_in(&data, &Exclusion::default(), budget, 0)?;
    remove(&root)?;

    let delta = pending.delta.as_ref().context("one delta")?;
    assert!(delta.answer_len() <= budget, "the answer's delta fits");
    let first = pending
        .windows
        .first()
        .context("the oldest window is listed")?;
    assert_eq!(pending.windows.len(), 1);
    assert_eq!(pending.later_windows, 1);
    assert_eq!(first.new_messages, 2_000);
    assert!(first.listed_messages > 0 && first.listed_messages < 2_000);
    assert_eq!(first.listed_messages, delta.new_messages.len());
    assert_eq!(
        delta.omitted_messages,
        Some(2_000 - first.listed_messages),
        "the answer counts what it left out"
    );
    assert_eq!(pending.next_offset, Some(first.listed_messages));
    let oldest = delta.new_messages.windows(2).all(|pair| {
        pair.first().map(|m| (m.ts, &m.pointer)) <= pair.get(1).map(|m| (m.ts, &m.pointer))
    });
    assert!(oldest, "the messages kept are the oldest, in order");
    Ok(())
}

/// Following `next_offset` lists every message of a window larger than an answer, each once,
/// and every part stays within the answer's size.
#[test]
fn next_offset_lists_every_message_of_a_large_window_once() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let all = messages("github:example-org/busy@c", 2_000)?;
    let pointers = all
        .iter()
        .map(|message| message.pointer.clone())
        .collect::<std::collections::BTreeSet<_>>();
    window(&data, "20261030T020000Z-0000", all)?;
    let budget = 64 * 1024;
    let mut offset = 0;
    let mut listed = Vec::new();
    let mut parts = 0;
    loop {
        let pending = pending_in(&data, &Exclusion::default(), budget, offset)?;
        let delta = pending.delta.context("a delta")?;
        assert!(delta.answer_len() <= budget, "part {parts} fits");
        listed.extend(
            delta
                .new_messages
                .into_iter()
                .map(|message| message.pointer),
        );
        parts += 1;
        match pending.next_offset {
            Some(next) => offset = next,
            None => break,
        }
        assert!(parts < 1_000, "the parts end");
    }
    remove(&root)?;

    assert!(parts > 1, "the window takes more than one answer");
    assert_eq!(listed.len(), 2_000, "each message is listed once");
    assert_eq!(
        listed
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        pointers
    );
    Ok(())
}

/// An empty window before one larger than an answer does not hold it back: both are listed,
/// the large one cut, so the agent can move past the empty one and on through the large one.
#[test]
fn an_empty_window_does_not_hold_back_a_large_one() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    window(&data, "20261030T020000Z-0000", Vec::new())?;
    window(
        &data,
        "20261030T021500Z-0000",
        messages("github:example-org/busy@c", 2_000)?,
    )?;
    let pending = pending_in(&data, &Exclusion::default(), 64 * 1024, 0)?;
    remove(&root)?;

    assert_eq!(pending.windows.len(), 2);
    assert_eq!(pending.later_windows, 0);
    assert!(pending.next_offset.is_some());
    Ok(())
}

/// The size an answer is held to is what goes on the wire: the printed JSON, with its control
/// characters escaped, escaped once more into the server's JSON string.
#[test]
fn the_answer_size_is_what_goes_on_the_wire() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let heavy = (0..2_000)
        .map(|n| {
            let mut message = message(&format!("slack:C0EXAMPLE01/{n}"), n % 60)?;
            message.excerpt = "\u{85}\"\\".repeat(50);
            Ok(message)
        })
        .collect::<Result<Vec<_>>>()?;
    window(&data, "20261030T020000Z-0000", heavy)?;
    let budget = 64 * 1024;
    let pending = pending_in(&data, &Exclusion::default(), budget, 0)?;
    remove(&root)?;

    let mut printed = Vec::new();
    crate::output::write_json(&mut printed, &pending)?;
    let wire = serde_json::to_string(&String::from_utf8(printed)?)?;
    assert!(
        wire.len() <= budget + 1024,
        "the answer on the wire is {} bytes",
        wire.len()
    );
    Ok(())
}

/// Messages that each make a different record stale page through like any others: a flag's
/// own fields count only on the page that lists one of its replies, and every page lists at
/// least one message, so `next_offset` always moves on.
#[test]
fn flagged_messages_page_through_and_every_page_moves_on() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let all = messages("slack:C0EXAMPLE01", 2_000)?;
    let flags = all
        .iter()
        .enumerate()
        .map(|(n, message)| OpenFlag {
            record: format!("questions/q{n}.md"),
            kind: "question".to_owned(),
            status: "open".to_owned(),
            title: "t".repeat(1_000),
            owner: None,
            last_reviewed: None,
            replies: vec![ReplyRef {
                pointer: message.pointer.clone(),
                ts: message.ts,
                author: None,
                how: "pointer".to_owned(),
                excerpt: message.excerpt.clone(),
            }],
        })
        .collect();
    window_with(&data, "20261030T020000Z-0000", all, flags, Vec::new())?;
    let budget = 256 * 1024;
    let mut offset = 0;
    let mut listed = 0;
    let mut parts = 0;
    loop {
        let pending = pending_in(&data, &Exclusion::default(), budget, offset)?;
        let delta = pending.delta.context("a delta")?;
        assert!(delta.answer_len() <= budget, "part {parts} fits");
        assert!(
            !delta.new_messages.is_empty(),
            "part {parts} lists a message"
        );
        assert_eq!(delta.unreviewed.len(), delta.new_messages.len());
        listed += delta.new_messages.len();
        parts += 1;
        match pending.next_offset {
            Some(next) => {
                assert!(next > offset, "next_offset moves on");
                offset = next;
            }
            None => break,
        }
        assert!(parts < 1_000, "the parts end");
    }
    remove(&root)?;

    assert!(parts > 1);
    assert_eq!(listed, 2_000);
    Ok(())
}

/// A window with no new message but more status changes than an answer holds does not hold
/// back the window after it.
#[test]
fn a_window_of_status_changes_alone_does_not_hold_back_the_next() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let opened = (0..20_000)
        .map(|n| format!("questions/a-question-with-a-long-name-{n}.md"))
        .collect();
    window_with(
        &data,
        "20261030T020000Z-0000",
        Vec::new(),
        Vec::new(),
        opened,
    )?;
    window(
        &data,
        "20261030T021500Z-0000",
        messages("slack:C0EXAMPLE01", 3)?,
    )?;
    let budget = 64 * 1024;
    let pending = pending_in(&data, &Exclusion::default(), budget, 0)?;
    remove(&root)?;

    assert_eq!(pending.windows.len(), 2, "both windows are listed");
    assert_eq!(pending.later_windows, 0);
    let delta = pending.delta.context("a delta")?;
    assert_eq!(delta.new_messages.len(), 3);
    assert!(
        delta.omitted_changes.is_some(),
        "the status changes are cut and counted"
    );
    assert!(delta.answer_len() <= budget);
    Ok(())
}

/// One message that makes very many records stale is still listed, alone, within the answer's
/// size: the open flags that do not fit are left out and counted.
#[test]
fn one_message_with_more_flags_than_an_answer_holds_stays_within_it() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let only = message("slack:C0EXAMPLE01/1", 0)?;
    let flags = (0..5_000)
        .map(|n| OpenFlag {
            record: format!("questions/q{n}.md"),
            kind: "question".to_owned(),
            status: "open".to_owned(),
            title: "t".repeat(1_000),
            owner: None,
            last_reviewed: None,
            replies: vec![ReplyRef {
                pointer: only.pointer.clone(),
                ts: only.ts,
                author: None,
                how: "pointer".to_owned(),
                excerpt: only.excerpt.clone(),
            }],
        })
        .collect();
    window_with(
        &data,
        "20261030T020000Z-0000",
        vec![only],
        flags,
        Vec::new(),
    )?;
    let budget = 256 * 1024;
    let pending = pending_in(&data, &Exclusion::default(), budget, 0)?;
    remove(&root)?;

    let delta = pending.delta.context("a delta")?;
    assert_eq!(delta.new_messages.len(), 1, "the message is listed");
    assert!(delta.answer_len() <= budget, "and the answer fits");
    assert!(
        delta.omitted_changes.is_some_and(|left_out| left_out > 0),
        "the flags left out are counted"
    );
    assert!(!delta.unreviewed.is_empty(), "the flags that fit are kept");
    Ok(())
}

/// A message whose author a source wrote with millions of characters, each escaped on the wire,
/// is still listed within the answer's size, its author cut to an excerpt.
#[test]
fn one_message_with_a_huge_author_stays_within_an_answer() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let mut only = message("slack:C0EXAMPLE01/1", 0)?;
    only.author = Some("\u{7f}".repeat(1024 * 1024));
    window(&data, "20261030T020000Z-0000", vec![only])?;
    let budget = 256 * 1024;
    let pending = pending_in(&data, &Exclusion::default(), budget, 0)?;
    remove(&root)?;

    let delta = pending.delta.context("a delta")?;
    assert_eq!(delta.new_messages.len(), 1, "the message is listed");
    assert!(delta.answer_len() <= budget, "and the answer fits");
    Ok(())
}

/// A delta written before sources were held to the pointer bound is held to it when read: a
/// message with a longer pointer is refused with what to do, rather than left out where the
/// marker could pass it unseen, and a longer thread pointer is dropped from its message.
#[test]
fn a_persisted_pointer_past_the_bound_is_held_to_it_when_read() -> Result<()> {
    let root = scratch()?;
    let data = DataDir::new(root.clone());
    let long = "x".repeat(crate::source::MAX_POINTER_BYTES + 1);
    let mut threaded = message("slack:C0EXAMPLE01/2", 1)?;
    threaded.thread = Some(format!("slack:C0EXAMPLE01/{long}"));
    window(&data, "20261030T020000Z-0000", vec![threaded])?;
    let threads = pending_in(&data, &Exclusion::default(), usize::MAX, 0)?;
    let named = message(&format!("slack:C0EXAMPLE01/{long}"), 0)?;
    window(&data, "20261030T021500Z-0000", vec![named])?;
    let refused = pending_in(&data, &Exclusion::default(), usize::MAX, 0);
    remove(&root)?;

    let delta = threads.delta.context("a delta")?;
    let pointers = delta
        .new_messages
        .iter()
        .map(|message| (message.pointer.as_str(), message.thread.is_some()))
        .collect::<Vec<_>>();
    assert_eq!(pointers, vec![("slack:C0EXAMPLE01/2", false)]);
    let error = refused
        .err()
        .context("a message past the bound is refused")?
        .to_string();
    assert!(error.contains("--write"), "{error}");
    Ok(())
}
