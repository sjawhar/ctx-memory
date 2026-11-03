//! The agent marker: the newest window whose material an agent has committed, and how far it has
//! worked the window after it when that window holds more than one page.
//!
//! One `ctx pending` answer lists at most one page of new messages: what fits in one answer's
//! bytes, and at most the tree's `[ingest] page_messages` (500 unless it sets another). A window
//! with more is worked a page at a time, across cycles: the answer lists its next page,
//! `ctx marker page` records that page once its records are written, and the marker passes the
//! window only once an answer has listed the window's last page. A cycle that stops partway
//! through a page has recorded nothing for it, so the next cycle reads that page again, and no
//! page before it.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::atomic::write_replace;
use crate::config::DataDir;
use crate::context::Context;
use crate::delta::{self, Delta, NewMessage, ANSWER_DELTA_BYTES};
use crate::exclude::Exclusion;
use crate::output::printable;
use crate::source::slack::{slack_schemes, Mentions};
use crate::store::Store;
use crate::windows::{self, Status, Window, WindowId};

/// The newest window an agent has turned into committed records.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub version: u32,
    pub window: String,
    pub advanced_at: DateTime<Utc>,
    pub by: String,
}

/// How far the agent has worked the oldest window after the marker, a page at a time: every new
/// message of `window` up to `last`, in the order an answer lists them, has its records written.
///
/// It is kept in a file of its own beside the marker, so the marker's file and the answers that
/// carry the marker keep their shape. A page of a window the marker has reached is spent, and is
/// read as no page at all.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub version: u32,
    pub window: String,
    /// How many of the window's new messages, oldest first, the recorded pages hold, as counted
    /// when the newest of them was recorded.
    pub worked: usize,
    /// The newest message the recorded pages hold. The next page starts after it, so a message
    /// an exclusion leaves out later moves no message from one page to another.
    pub last: PageMessage,
    pub recorded_at: DateTime<Utc>,
    pub by: String,
}

/// One message, by its place in the order an answer lists a window's messages: oldest first, and
/// by pointer among messages of one time ([`delta::oldest_first`]).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PageMessage {
    pub ts: DateTime<Utc>,
    pub pointer: String,
}

impl Page {
    /// How many of `messages`, which are oldest first, the recorded pages hold.
    fn worked_of(&self, messages: &[NewMessage]) -> usize {
        messages.partition_point(|message| {
            (message.ts, message.pointer.as_str()) <= (self.last.ts, self.last.pointer.as_str())
        })
    }
}

/// One completed window the marker has not reached.
#[derive(Clone, Debug, Serialize)]
pub struct PendingWindow {
    pub window: String,
    /// The window's directory under the data directory, such as `windows/<window id>`.
    ///
    /// Relative, because the agent that reads the files and the server that lists them each
    /// mount the share where their own container puts it.
    pub dir: String,
    pub window_end: DateTime<Utc>,
    pub status: Status,
    /// The window's new messages, less those the deployment excludes.
    pub new_messages: usize,
    /// How many of them the answer's delta lists: every one no recorded page holds, unless the
    /// answer lists only a page of them (`page`), and then the rest are in the window's
    /// `delta.json`.
    pub listed_messages: usize,
    /// The page of the window this answer lists, when the window is worked in pages: it holds
    /// more new messages than one answer lists, or a page of it is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<PageView>,
}

/// The page of a window one answer lists.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PageView {
    /// How many of the window's new messages, oldest first, the pages already recorded hold.
    /// The answer leaves them out.
    pub worked: usize,
    /// `worked` and the messages this page lists. Once the page's records are written,
    /// `ctx marker page <window> <through>` records it.
    pub through: usize,
    /// Whether this page holds the window's newest message. Then no page is recorded: once its
    /// records are written, `ctx marker advance <window>` moves the marker past the window.
    pub last: bool,
}

impl PageView {
    /// A page whose fields print as wide as any page's can, so the bytes an answer keeps for a
    /// window's page are never fewer than the page takes.
    const WIDEST: Self = Self {
        worked: usize::MAX,
        through: usize::MAX,
        last: false,
    };
}

/// The windows an agent still has to handle, with their combined delta.
#[derive(Debug, Serialize)]
pub struct Pending {
    pub marker: Option<Marker>,
    pub windows: Vec<PendingWindow>,
    pub delta: Option<Delta>,
    /// Completed windows after these that this answer leaves for a later one, because their
    /// deltas would take it past one answer's size or their messages past one page. Once the
    /// marker passes the windows listed here, the next answer starts with them.
    pub later_windows: usize,
}

/// Reads the marker without changing it.
///
/// # Errors
///
/// Returns an error when the marker file exists but cannot be decoded or uses another version.
pub fn show(context: &Context) -> Result<Option<Marker>> {
    load(&context.data()?.marker())
}

/// Reads the page record of the oldest window after the marker without changing it: `None`
/// when no page of that window is recorded.
///
/// # Errors
///
/// Returns an error when the marker or the page record exists but cannot be decoded or uses
/// another version.
pub fn show_page(context: &Context) -> Result<Option<Page>> {
    let data = context.data()?;
    let (_, held) = held(data)?;
    page_after(data, held.as_ref())
}

/// A marker move or a page record the caller asked for and may not have: a window that does
/// not name a completed window, one the marker has already passed, one whose last page no answer
/// has listed yet, or a page other than the one an answer lists.
///
/// Typed, so the server can tell the caller's mistake from its own trouble: anything else that
/// stops a move — a marker or a window it cannot read, a marker it cannot write — is the
/// machine's, not the caller's.
#[derive(Debug)]
pub struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(why: String) -> anyhow::Error {
    anyhow::Error::new(Refused(why))
}

/// What one answer is held to, and the names it serves Slack mentions with.
struct Serving<'a> {
    exclusion: &'a Exclusion,
    /// The most bytes its delta and its window list take ([`ANSWER_DELTA_BYTES`]).
    budget: usize,
    /// The most new messages it lists: the tree's `[ingest] page_messages`.
    page_messages: usize,
    mentions: Option<&'a Mentions<'a>>,
}

/// Runs `work` over the tree's data directory with what the tree's answers are held to.
fn serving<T>(
    context: &Context,
    work: impl FnOnce(&DataDir, &Serving<'_>) -> Result<T>,
) -> Result<T> {
    let data = context.data()?;
    let exclusion = Exclusion::configured(&context.config)?;
    let page_messages = context.config.ingest.page_messages;
    let schemes = slack_schemes(&context.config.sources)?;
    if schemes.is_empty() {
        return work(
            data,
            &Serving {
                exclusion: &exclusion,
                budget: ANSWER_DELTA_BYTES,
                page_messages,
                mentions: None,
            },
        );
    }
    let store = Store::load(data, context.state()?)?;
    let mentions = Mentions::new(&store, schemes);
    work(
        data,
        &Serving {
            exclusion: &exclusion,
            budget: ANSWER_DELTA_BYTES,
            page_messages,
            mentions: Some(&mentions),
        },
    )
}

/// Moves the marker to a completed window, after the commit that handled it.
///
/// `window` is a completed window identifier or `newest`. A window the marker has already
/// passed is refused, so a repeated or out-of-order cycle cannot hide material; moving the
/// marker to the window it already holds changes only who moved it and when. So is a move past
/// a window whose last page no answer lists yet: a window larger than one page is worked a page
/// at a time ([`page`]), and the marker passes it only once what is left of it fits in one.
///
/// # Errors
///
/// Returns a [`Refused`] error when the window is not complete, is older than the current
/// marker, or would pass a window with more than one page left, and any other error when marker
/// state or a window cannot be read or the marker cannot be persisted.
pub fn advance(context: &Context, window: &str, by: &str) -> Result<Marker> {
    serving(context, |data, serving| {
        advance_in(data, window, by, serving)
    })
}

/// [`advance`] over a data directory, with what an answer is held to.
fn advance_in(data: &DataDir, window: &str, by: &str, serving: &Serving<'_>) -> Result<Marker> {
    let window = windows::resolve_arg(window, &data.windows())
        .and_then(Window::open)
        .map_err(|error| refused(format!("{error:#}")))?;
    let (_, held) = held(data)?;
    if let Some(held) = held.as_ref() {
        if *held > window.id {
            return Err(refused(format!(
                "window {} is older than the marker at {held}",
                window.id
            )));
        }
    }
    let page = page_after(data, held.as_ref())?;
    for passed in windows::after(&data.windows(), held.as_ref())? {
        if passed.id > window.id {
            break;
        }
        let id = passed.id.clone();
        let listed = answer(data, vec![passed], serving, page.as_ref())?;
        let Some(entry) = listed.windows.first() else {
            continue;
        };
        if let Some(view) = entry.page.filter(|view| !view.last) {
            return Err(refused(format!(
                "window {id} holds {} new message(s) no recorded page holds, more than one page \
                 ({} listed): the marker passes it once its last page is worked. Work it a page at \
                 a time as `ctx pending` lists it, and record each page with \
                 `ctx marker page {id} <through>`",
                entry.new_messages - view.worked,
                entry.listed_messages
            )));
        }
    }
    let marker = Marker {
        version: 1,
        window: window.id.to_string(),
        advanced_at: Utc::now(),
        by: by.to_owned(),
    };
    write_json(&data.marker(), &marker, "the marker")?;
    // A page of a window the marker now covers is spent.
    if let Some(page) = page {
        if WindowId::parse(&page.window)? <= window.id {
            remove_page(&data.marker_page())?;
        }
    }
    Ok(marker)
}

/// Records that the agent has worked the oldest window after the marker through its `through`
/// oldest new messages, after the commit that wrote their records.
///
/// `through` is the `page.through` of the window in a `ctx pending` answer: the window's
/// messages up to the end of the page that answer lists. A smaller one records part of that page.
/// The next answer lists the page after it, so a cycle that stops partway through a page reads
/// that page again and no page before it. Recording the page already recorded changes only who
/// recorded it and when. A page is recorded for the oldest window after the marker alone, and the
/// last page of a window is not recorded: the marker moves past the window instead ([`advance`]).
///
/// # Errors
///
/// Returns a [`Refused`] error when the window is not the oldest window after the marker, or
/// `through` is behind the pages already recorded or past the page an answer lists, and any
/// other error when marker state or the window cannot be read or the page cannot be persisted.
pub fn page(context: &Context, window: &str, through: usize, by: &str) -> Result<Page> {
    serving(context, |data, serving| {
        page_in(data, window, through, by, serving)
    })
}

/// [`page`] over a data directory, with what an answer is held to.
fn page_in(
    data: &DataDir,
    window: &str,
    through: usize,
    by: &str,
    serving: &Serving<'_>,
) -> Result<Page> {
    let window = windows::resolve_arg(window, &data.windows())
        .and_then(Window::open)
        .map_err(|error| refused(format!("{error:#}")))?;
    let (_, held) = held(data)?;
    let recorded = page_after(data, held.as_ref())?;
    let oldest = match windows::after(&data.windows(), held.as_ref())?
        .into_iter()
        .next()
    {
        Some(oldest) if oldest.id == window.id => oldest,
        Some(oldest) if oldest.id < window.id => {
            return Err(refused(format!(
                "window {} is not the oldest window after the marker: {} is, and a page is \
                 recorded for that window alone. Move the marker past the windows before {} first",
                window.id, oldest.id, window.id
            )))
        }
        _ => {
            return Err(refused(format!(
                "the marker has already passed window {}",
                window.id
            )))
        }
    };
    let listed = answer(data, vec![oldest], serving, recorded.as_ref())?;
    let entry = listed
        .windows
        .first()
        .context("an answer over one window lists it")?;
    let worked = entry.page.map_or(0, |view| view.worked);
    let end = worked + entry.listed_messages;
    if through == 0 {
        return Err(refused(
            "a page holds at least one message: record the `page.through` that `ctx pending` \
             lists for the window"
                .to_owned(),
        ));
    }
    if through < worked {
        return Err(refused(format!(
            "the pages recorded for window {} already hold its {worked} oldest new messages, so \
             a page through message {through} is behind them",
            window.id
        )));
    }
    if through > end {
        return Err(refused(format!(
            "the page `ctx pending` lists for window {} ends at message {end} of its {}: record \
             at most that, once its records are written",
            window.id, entry.new_messages
        )));
    }
    let last = if through == worked {
        recorded
            .map(|page| page.last)
            .context("a window with worked messages has a recorded page")?
    } else {
        let message = listed
            .delta
            .as_ref()
            .and_then(|delta| delta.new_messages.get(through - worked - 1))
            .context("the page lists the message it ends with")?;
        PageMessage {
            ts: message.ts,
            pointer: message.pointer.clone(),
        }
    };
    let page = Page {
        version: 1,
        window: window.id.to_string(),
        worked: through,
        last,
        recorded_at: Utc::now(),
        by: by.to_owned(),
    };
    write_json(&data.marker_page(), &page, "the page record")?;
    Ok(page)
}

/// Lists the completed windows after the marker with the delta they combine into.
///
/// `limit` caps how many of those windows, oldest first, the answer holds: a cycle handles
/// the capped list and leaves the rest for the next one, so one run never has to read the
/// whole of a backlog at once. `None` lists every pending window. The delta combines only the
/// windows the answer holds.
///
/// The answer leaves out every message the deployment excludes, and lists at most one page:
/// it stays within one answer's size ([`ANSWER_DELTA_BYTES`]) and within the tree's
/// `[ingest] page_messages`. It takes windows oldest first while they fit, and counts the rest,
/// and those past `limit`, in `later_windows`. A window that does not fit is taken anyway when
/// the windows before it hold no message, so empty windows never hold a large one back; its
/// delta then lists the oldest messages of one page, `omitted_messages` counts the rest, and its
/// `page` says which page that is. The messages of the pages recorded for the oldest window
/// ([`page`]) are left out.
///
/// Each message's excerpt is cut from the text ctx serves for it now ([`Delta::serve`]), so a
/// Slack message reads with the people it mentions named by the names the store holds.
///
/// # Errors
///
/// Returns an error when the marker, the page record, a window manifest, a window delta, or
/// the store cannot be read.
pub fn pending(context: &Context, limit: Option<usize>) -> Result<Pending> {
    serving(context, |data, serving| pending_in(data, limit, serving))
}

/// [`pending`] over a data directory, with what an answer is held to.
fn pending_in(data: &DataDir, limit: Option<usize>, serving: &Serving<'_>) -> Result<Pending> {
    let (marker, held) = held(data)?;
    let page = page_after(data, held.as_ref())?;
    let mut after = windows::after(&data.windows(), held.as_ref())?;
    let waiting = after.len();
    if let Some(limit) = limit {
        after.truncate(limit);
    }
    let Answer { windows, delta } = answer(data, after, serving, page.as_ref())?;
    Ok(Pending {
        marker,
        later_windows: waiting - windows.len(),
        windows,
        delta,
    })
}

/// The pointers of the messages that the page `ctx pending` lists for `window` holds, when the
/// window is worked in pages; `None` when an answer lists every new message of the window.
///
/// The page is the one an answer would list with `window` the oldest window it holds, which it is
/// whenever an answer lists only a page of it.
///
/// # Errors
///
/// Returns an error when the marker, the page record, the window or the store cannot be read.
pub fn page_pointers(context: &Context, window: &Window) -> Result<Option<BTreeSet<String>>> {
    serving(context, |data, serving| {
        let (_, held) = held(data)?;
        let page = page_after(data, held.as_ref())?;
        let listed = answer(data, vec![window.clone()], serving, page.as_ref())?;
        if listed
            .windows
            .first()
            .and_then(|entry| entry.page)
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(
            listed
                .delta
                .map(|delta| {
                    delta
                        .new_messages
                        .into_iter()
                        .map(|message| message.pointer)
                        .collect()
                })
                .unwrap_or_default(),
        ))
    })
}

/// What one answer lists: its windows, oldest first, and the delta they combine into.
struct Answer {
    windows: Vec<PendingWindow>,
    delta: Option<Delta>,
}

/// One window as an answer would list it whole: its entry, and its delta less the messages the
/// recorded pages hold.
struct Unworked {
    entry: PendingWindow,
    delta: Option<Delta>,
    /// How many of its new messages the recorded pages hold.
    worked: usize,
}

/// Reads `window` as an answer lists it: its delta less what the deployment excludes, oldest
/// first, less the messages `page` holds when it is that window's, and with each excerpt served
/// as ctx serves the message now.
fn unworked(
    data: &DataDir,
    window: &Window,
    serving: &Serving<'_>,
    page: Option<&Page>,
) -> Result<Unworked> {
    let manifest = window.manifest()?;
    let mut delta = delta::read(window, serving.exclusion)?;
    let new_messages = delta.as_ref().map_or(0, |delta| delta.new_messages.len());
    let mut worked = 0;
    if let Some(delta) = delta.as_mut() {
        delta::oldest_first(&mut delta.new_messages);
        if let Some(page) = page.filter(|page| page.window == window.id.as_str()) {
            worked = page.worked_of(&delta.new_messages);
            delta.skip(worked);
        }
        if let Some(mentions) = serving.mentions {
            delta.serve(mentions)?;
        }
    }
    let dir = window.dir.strip_prefix(data.root()).with_context(|| {
        format!(
            "{} is not under the data directory {}",
            window.dir.display(),
            data.root().display()
        )
    })?;
    Ok(Unworked {
        entry: PendingWindow {
            window: window.id.to_string(),
            dir: dir.to_string_lossy().into_owned(),
            window_end: manifest.window_end,
            status: manifest.exit_status,
            new_messages,
            listed_messages: new_messages - worked,
            page: None,
        },
        delta,
        worked,
    })
}

/// What one answer lists of `candidates`, completed windows after the marker, oldest first.
///
/// It takes windows while their deltas fit within the answer's size and their messages within
/// one page. A window that does not fit is taken anyway when the windows before it hold no
/// message, cut to one page, and nothing after it is taken. The messages of `page`'s window that
/// the recorded pages hold are left out before anything is counted.
fn answer(
    data: &DataDir,
    candidates: Vec<Window>,
    serving: &Serving<'_>,
    page: Option<&Page>,
) -> Result<Answer> {
    let mut windows = Vec::new();
    // Each listed window's delta, by its place in `windows`; a window without one has none.
    let mut deltas = Vec::new();
    let mut delta_of = Vec::new();
    // How many of each listed window's new messages the recorded pages hold.
    let mut worked_of = Vec::new();
    let mut size = 0_usize;
    let mut listing = 0_usize;
    let mut window_bytes = 0_usize;
    let mut cut = false;
    for window in candidates {
        let Unworked {
            entry,
            delta,
            worked,
        } = unworked(data, &window, serving, page)?;
        let remaining = entry.listed_messages;
        // A window worked in pages carries its page, and what the answer keeps for it is the
        // widest a page can print.
        let paged_bytes = delta::answer_len_of(&PendingWindow {
            page: Some(PageView::WIDEST),
            ..entry.clone()
        });
        let entry_bytes = if worked > 0 {
            paged_bytes
        } else {
            delta::answer_len_of(&entry)
        };
        // A window with no message left adds only its entry: its status changes are capped with
        // the combined delta's (`Delta::fit`), so a run of them never holds back a later window.
        let bytes = if remaining == 0 {
            entry_bytes
        } else {
            delta.as_ref().map_or(0, Delta::answer_len) + entry_bytes
        };
        let fits = size + bytes <= serving.budget && listing + remaining <= serving.page_messages;
        if !fits && listing > 0 {
            break;
        }
        size += bytes;
        listing += remaining;
        windows.push(entry);
        worked_of.push(worked);
        delta_of.push(delta.map(|delta| {
            deltas.push(delta);
            deltas.len() - 1
        }));
        if !fits {
            // Taken past the answer's size or page because the windows before it hold nothing:
            // it is cut to one page below, its entry carries that page, and nothing after it is
            // taken.
            window_bytes += paged_bytes;
            cut = true;
            break;
        }
        window_bytes += entry_bytes;
    }
    let mut combined = delta::combine(&deltas, Utc::now());
    if let Some(combined) = combined.as_mut() {
        if cut {
            combined.keep_oldest(serving.page_messages);
        }
        combined.fit(serving.budget.saturating_sub(window_bytes));
        if combined.omitted_messages.is_some() {
            count_listed(&mut windows, &deltas, &delta_of, combined);
        }
    }
    for (window, worked) in windows.iter_mut().zip(worked_of) {
        let through = worked + window.listed_messages;
        if worked > 0 || through < window.new_messages {
            window.page = Some(PageView {
                worked,
                through,
                last: through >= window.new_messages,
            });
        }
    }
    Ok(Answer {
        windows,
        delta: combined,
    })
}

/// Sets each window's `listed_messages` to how many of its delta's messages `combined`, an
/// answer's delta cut to its size or page, still lists. `delta_of` gives each window's delta by
/// its place in `deltas`.
fn count_listed(
    windows: &mut [PendingWindow],
    deltas: &[Delta],
    delta_of: &[Option<usize>],
    combined: &Delta,
) {
    let listed = combined
        .new_messages
        .iter()
        .map(|message| message.pointer.as_str())
        .collect::<BTreeSet<_>>();
    for (window, index) in windows.iter_mut().zip(delta_of) {
        window.listed_messages = index
            .and_then(|index| deltas.get(index))
            .map_or(0, |delta| {
                delta
                    .new_messages
                    .iter()
                    .filter(|message| listed.contains(message.pointer.as_str()))
                    .count()
            });
    }
}

/// Prints the pending windows the way a person reads them.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_pending(pending: &Pending, out: &mut dyn Write) -> std::io::Result<()> {
    match pending.marker.as_ref() {
        Some(marker) => writeln!(
            out,
            "marker {} ({} {})",
            printable(&marker.window),
            printable(&marker.by),
            marker.advanced_at.to_rfc3339()
        )?,
        None => writeln!(out, "marker (none)")?,
    }
    if pending.windows.is_empty() {
        writeln!(out, "(no pending windows)")?;
    }
    for window in &pending.windows {
        let Some(page) = window.page else {
            writeln!(
                out,
                "{} {} {} new message(s)",
                window.window,
                window.status.label(),
                window.new_messages
            )?;
            continue;
        };
        if page.through > page.worked {
            writeln!(
                out,
                "{} {} {} new message(s), the page of messages {} to {} listed below",
                window.window,
                window.status.label(),
                window.new_messages,
                page.worked + 1,
                page.through
            )?;
        } else {
            writeln!(
                out,
                "{} {} {} new message(s), all of them in recorded pages",
                window.window,
                window.status.label(),
                window.new_messages
            )?;
        }
        if page.last {
            writeln!(
                out,
                "  its last page: once its records are written, `ctx marker advance {}` moves the \
                 marker past it",
                window.window
            )?;
        } else {
            writeln!(
                out,
                "  once its records are written, `ctx marker page {} {}` records the page, and the \
                 next answer lists the next one",
                window.window, page.through
            )?;
        }
    }
    if pending.later_windows > 0 {
        writeln!(
            out,
            "{} later window(s) wait for the next answer, once the marker passes these",
            pending.later_windows
        )?;
    }
    if let Some(delta) = pending.delta.as_ref() {
        writeln!(out, "{}", delta::summary(delta))?;
    }
    Ok(())
}

/// Prints the marker the way a person reads it.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(marker: Option<&Marker>, out: &mut dyn Write) -> std::io::Result<()> {
    match marker {
        Some(marker) => writeln!(
            out,
            "{} {} {}",
            printable(&marker.window),
            printable(&marker.by),
            marker.advanced_at.to_rfc3339()
        ),
        None => writeln!(out, "(none)"),
    }
}

/// Prints a page record the way a person reads it.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print_page(page: &Page, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "page {} through message {} {} {}",
        printable(&page.window),
        page.worked,
        printable(&page.by),
        page.recorded_at.to_rfc3339()
    )
}

/// The marker, and the window it holds.
fn held(data: &DataDir) -> Result<(Option<Marker>, Option<WindowId>)> {
    let path = data.marker();
    let marker = load(&path)?;
    let held = marker
        .as_ref()
        .map(|marker| {
            WindowId::parse(&marker.window)
                .with_context(|| format!("reading the marker at {}", path.display()))
        })
        .transpose()?;
    Ok((marker, held))
}

/// The page record of the oldest window after the marker at `held`: `None` when no page is
/// recorded, or the one recorded is of a window the marker has reached.
fn page_after(data: &DataDir, held: Option<&WindowId>) -> Result<Option<Page>> {
    let path = data.marker_page();
    let Some(page) = read_json::<Page>(&path)? else {
        return Ok(None);
    };
    if page.version != 1 {
        bail!(
            "{} has unsupported page version {}",
            path.display(),
            page.version
        );
    }
    let window = WindowId::parse(&page.window)
        .with_context(|| format!("reading the page record at {}", path.display()))?;
    Ok(held.is_none_or(|held| window > *held).then_some(page))
}

fn load(path: &Path) -> Result<Option<Marker>> {
    let Some(marker) = read_json::<Marker>(path)? else {
        return Ok(None);
    };
    if marker.version != 1 {
        bail!(
            "{} has unsupported marker version {}",
            path.display(),
            marker.version
        );
    }
    Ok(Some(marker))
}

/// Reads and decodes the JSON file at `path`, or `None` when there is none.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("decoding {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize, what: &str) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).with_context(|| format!("serializing {what}"))?;
    write_replace(path, &bytes)
}

fn remove_page(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context as _, Result};
    use chrono::{TimeZone as _, Utc};

    use super::{
        advance_in, load, page_in, pending_in, write_json, Marker, Pending, Refused, Serving,
    };
    use crate::config::{DataDir, DEFAULT_PAGE_MESSAGES};
    use crate::delta::Delta;
    use crate::exclude::Exclusion;
    use crate::windows::{Manifest, Status};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn scratch(label: &str) -> Result<PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "ctx-memory-marker-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;
        Ok(root)
    }

    /// Writes one completed window `id` under `data`, carrying `new_messages` new messages, each
    /// named `echo:<id>/<n>` with `n` written in four digits, so they sort as they count.
    ///
    /// A window counts as completed only when it holds `manifest.json`, `records.jsonl`,
    /// `rendered.md` and `delta.json` (`windows::completed`), so this writes all four.
    fn write_window(data: &DataDir, id: &str, new_messages: usize) -> Result<()> {
        let end = Utc.with_ymd_and_hms(2026, 10, 30, 2, 0, 0).unwrap();
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
            // Written newest first: an answer lists them oldest first whatever the file's order.
            new_messages: (0..new_messages)
                .rev()
                .map(|n| crate::delta::NewMessage {
                    pointer: pointer(id, n),
                    label: None,
                    ts: end,
                    author: None,
                    is_reply: false,
                    thread: None,
                    excerpt: String::new(),
                    matches: Vec::new(),
                })
                .collect(),
            unreviewed: Vec::new(),
            closed_since_previous: Vec::new(),
            opened_since_previous: Vec::new(),
            omitted_messages: None,
            omitted_changes: None,
        };
        std::fs::write(dir.join("delta.json"), serde_json::to_vec(&delta)?)?;
        Ok(())
    }

    fn pointer(window: &str, n: usize) -> String {
        format!("echo:{window}/{n:04}")
    }

    fn set_marker(data: &DataDir, window: &str) -> Result<()> {
        write_json(
            &data.marker(),
            &Marker {
                version: 1,
                window: window.to_owned(),
                advanced_at: Utc.with_ymd_and_hms(2026, 10, 30, 2, 0, 0).unwrap(),
                by: "test".to_owned(),
            },
            "the marker",
        )
    }

    /// Answers held to no size, with pages of `page_messages` messages.
    fn paged(exclusion: &Exclusion, page_messages: usize) -> Serving<'_> {
        Serving {
            exclusion,
            budget: usize::MAX,
            page_messages,
            mentions: None,
        }
    }

    fn ids(pending: &Pending) -> Vec<String> {
        pending.windows.iter().map(|w| w.window.clone()).collect()
    }

    fn listed(pending: &Pending) -> Vec<String> {
        pending
            .delta
            .as_ref()
            .map(|delta| {
                delta
                    .new_messages
                    .iter()
                    .map(|message| message.pointer.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn held(data: &DataDir) -> Result<Option<String>> {
        Ok(load(&data.marker())?.map(|marker| marker.window))
    }

    fn is_refused(outcome: Result<impl std::fmt::Debug>) -> bool {
        outcome.is_err_and(|error| error.downcast_ref::<Refused>().is_some())
    }

    const WINDOWS: [&str; 5] = [
        "20261030T020000Z-0000",
        "20261030T021500Z-0000",
        "20261030T023000Z-0000",
        "20261030T024500Z-0000",
        "20261030T030000Z-0000",
    ];

    /// A session that reads `ctx pending --limit N` sees exactly the first N windows, oldest
    /// first; the rest stay pending and the next cycle, after the marker advances past the
    /// cap, reads them. This is the mechanism that keeps one failed cycle from handing the next
    /// an ever-larger backlog: each run handles at most the cap, never the whole backlog.
    #[test]
    fn a_cap_leaves_the_windows_past_it_pending() -> Result<()> {
        let root = scratch("cap")?;
        let data = DataDir::new(root.clone());
        // Five completed windows after no marker, oldest first by id.
        for (n, id) in WINDOWS.iter().enumerate() {
            write_window(&data, id, n + 1)?;
        }
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, usize::MAX);

        // No cap lists every pending window.
        assert_eq!(ids(&pending_in(&data, None, &serving)?), WINDOWS);

        // A cap of two lists only the two oldest, and the delta combines only those.
        let capped = pending_in(&data, Some(2), &serving)?;
        assert_eq!(ids(&capped), &WINDOWS[..2]);
        assert_eq!(
            listed(&capped).len(),
            3,
            "delta combines windows 1 (1 msg) and 2 (2 msgs) only"
        );

        // The session handles the cap and advances the marker to the newest it handled; the
        // next cycle's capped read is the remainder, never the windows already done.
        set_marker(&data, WINDOWS[1])?;
        assert_eq!(ids(&pending_in(&data, Some(2), &serving)?), &WINDOWS[2..4]);

        // A cap larger than the backlog lists all of it and nothing waits.
        set_marker(&data, WINDOWS[3])?;
        assert_eq!(ids(&pending_in(&data, Some(10), &serving)?), &WINDOWS[4..]);

        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    /// A window larger than a page is worked over several cycles: each cycle's answer lists the
    /// next page, oldest messages first, the cycle records it, and the marker passes the window
    /// only once the answer has listed its last page. Every message is listed once.
    #[test]
    fn a_window_larger_than_a_page_is_worked_a_page_per_cycle() -> Result<()> {
        let root = scratch("pages")?;
        let data = DataDir::new(root.clone());
        let id = WINDOWS[0];
        write_window(&data, id, 7)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, 3);
        let mut cycles = Vec::new();
        let mut early = Vec::new();
        loop {
            let pending = pending_in(&data, None, &serving)?;
            let window = pending.windows.first().context("the window waits")?;
            let page = window.page.context("the window is worked in pages")?;
            assert_eq!(window.listed_messages, page.through - page.worked);
            cycles.push((page.worked, page.through, page.last, listed(&pending)));
            if page.last {
                advance_in(&data, id, "cycle", &serving)?;
                break;
            }
            early.push(is_refused(advance_in(&data, id, "cycle", &serving)));
            page_in(&data, id, page.through, "cycle", &serving)?;
            assert!(cycles.len() < 10, "the pages end");
        }
        let after = pending_in(&data, None, &serving)?;
        let spent = data.marker_page().exists();
        let marker = held(&data)?;
        std::fs::remove_dir_all(&root)?;

        let pages: Vec<_> = cycles
            .iter()
            .map(|(worked, through, last, _)| (*worked, *through, *last))
            .collect();
        assert_eq!(pages, vec![(0, 3, false), (3, 6, false), (6, 7, true)]);
        for (worked, through, _, listed) in &cycles {
            let expected: Vec<String> = (*worked..*through).map(|n| pointer(id, n)).collect();
            assert_eq!(
                listed, &expected,
                "the page lists its own messages, oldest first"
            );
        }
        assert_eq!(
            early,
            vec![true, true],
            "the marker moved past the window before its last page"
        );
        assert_eq!(marker.as_deref(), Some(id));
        assert!(after.windows.is_empty());
        assert!(
            !spent,
            "the page record outlived the window the marker passed"
        );
        Ok(())
    }

    /// A cycle that stops partway through a page records nothing for it: the next cycle's
    /// answer lists that same page again, and none of the pages recorded before it.
    #[test]
    fn a_page_not_recorded_is_listed_again_and_no_page_before_it() -> Result<()> {
        let root = scratch("crash")?;
        let data = DataDir::new(root.clone());
        let id = WINDOWS[0];
        write_window(&data, id, 7)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, 3);
        page_in(&data, id, 3, "first cycle", &serving)?;
        // The second cycle reads its page, writes some of its records and stops.
        let interrupted = pending_in(&data, None, &serving)?;
        let again = pending_in(&data, None, &serving)?;
        std::fs::remove_dir_all(&root)?;

        let expected: Vec<String> = (3..6).map(|n| pointer(id, n)).collect();
        assert_eq!(listed(&interrupted), expected);
        assert_eq!(
            listed(&again),
            expected,
            "the next cycle reads the same page"
        );
        let page = again
            .windows
            .first()
            .and_then(|window| window.page)
            .context("the window is worked in pages")?;
        assert_eq!((page.worked, page.through, page.last), (3, 6, false));
        Ok(())
    }

    /// Windows no larger than a page, alone or together, are listed whole and with no page, and
    /// the marker passes them as it always has; so is a window within the default page.
    #[test]
    fn windows_within_a_page_are_listed_whole_as_before() -> Result<()> {
        let root = scratch("small")?;
        let data = DataDir::new(root.clone());
        write_window(&data, WINDOWS[0], 2)?;
        write_window(&data, WINDOWS[1], 1)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, 3);
        let small = pending_in(&data, None, &serving)?;
        let moved = advance_in(&data, WINDOWS[1], "cycle", &serving)?;
        write_window(&data, WINDOWS[2], 40)?;
        let unpaged = pending_in(&data, None, &paged(&exclusion, DEFAULT_PAGE_MESSAGES))?;
        let recorded = data.marker_page().exists();
        std::fs::remove_dir_all(&root)?;

        assert_eq!(ids(&small), &WINDOWS[..2]);
        assert_eq!(small.later_windows, 0);
        assert_eq!(listed(&small).len(), 3);
        assert!(small
            .windows
            .iter()
            .all(|window| window.page.is_none() && window.listed_messages == window.new_messages));
        assert_eq!(moved.window, WINDOWS[1]);
        assert!(!recorded, "no page was recorded");
        assert_eq!(ids(&unpaged), &WINDOWS[2..3]);
        assert_eq!(listed(&unpaged).len(), 40);
        assert!(unpaged.windows.iter().all(|window| window.page.is_none()));
        let json = serde_json::to_value(&unpaged)?;
        assert!(
            json.pointer("/windows/0/page").is_none(),
            "a window listed whole carries no page field: {json}"
        );
        Ok(())
    }

    /// A tree that sets no page size pages a window of more than 500 new messages, 500 at a
    /// time: a first backfill of 1,200 messages is worked over three cycles.
    #[test]
    fn the_default_page_is_500_messages() -> Result<()> {
        let root = scratch("default-page")?;
        let data = DataDir::new(root.clone());
        let id = WINDOWS[0];
        write_window(&data, id, 1_200)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, DEFAULT_PAGE_MESSAGES);
        let mut pages = Vec::new();
        loop {
            let pending = pending_in(&data, None, &serving)?;
            let page = pending
                .windows
                .first()
                .and_then(|window| window.page)
                .context("the window is worked in pages")?;
            pages.push((page.worked, page.through, page.last, listed(&pending).len()));
            if page.last {
                break;
            }
            page_in(&data, id, page.through, "cycle", &serving)?;
            assert!(pages.len() < 10, "the pages end");
        }
        std::fs::remove_dir_all(&root)?;

        assert_eq!(
            pages,
            vec![
                (0, 500, false, 500),
                (500, 1_000, false, 500),
                (1_000, 1_200, true, 200)
            ]
        );
        Ok(())
    }

    /// One answer lists at most a page of messages across its windows: the windows past it
    /// wait for a later answer, as windows past the answer's size do.
    #[test]
    fn windows_past_one_page_wait_for_the_next_answer() -> Result<()> {
        let root = scratch("page-of-windows")?;
        let data = DataDir::new(root.clone());
        for id in &WINDOWS[..3] {
            write_window(&data, id, 2)?;
        }
        let exclusion = Exclusion::default();
        let pending = pending_in(&data, None, &paged(&exclusion, 3))?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(ids(&pending), &WINDOWS[..1]);
        assert_eq!(pending.later_windows, 2);
        assert!(pending.windows.iter().all(|window| window.page.is_none()));
        Ok(())
    }

    /// A page is recorded for the oldest window after the marker alone, and only as far as the
    /// page an answer lists for it reaches; recording the page already recorded changes nothing
    /// but who recorded it.
    #[test]
    fn a_page_is_recorded_only_as_an_answer_lists_it() -> Result<()> {
        let root = scratch("record")?;
        let data = DataDir::new(root.clone());
        write_window(&data, WINDOWS[0], 2)?;
        write_window(&data, WINDOWS[1], 7)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, 3);
        let behind_an_older_window = page_in(&data, WINDOWS[1], 3, "cycle", &serving);
        advance_in(&data, WINDOWS[0], "cycle", &serving)?;
        let of_no_message = page_in(&data, WINDOWS[1], 0, "cycle", &serving);
        let past_the_page = page_in(&data, WINDOWS[1], 4, "cycle", &serving);
        let first = page_in(&data, WINDOWS[1], 3, "cycle", &serving)?;
        let behind = page_in(&data, WINDOWS[1], 2, "cycle", &serving);
        let again = page_in(&data, WINDOWS[1], 3, "retry", &serving)?;
        let passed = page_in(&data, WINDOWS[0], 1, "cycle", &serving);
        let missing = page_in(&data, "20261030T050000Z-0000", 1, "cycle", &serving);
        let next = pending_in(&data, None, &serving)?;
        std::fs::remove_dir_all(&root)?;

        assert!(is_refused(behind_an_older_window));
        assert!(is_refused(of_no_message));
        assert!(is_refused(past_the_page));
        assert!(is_refused(behind));
        assert!(is_refused(passed));
        assert!(is_refused(missing));
        assert_eq!(
            (first.worked, first.last.pointer.as_str()),
            (3, pointer(WINDOWS[1], 2).as_str())
        );
        assert_eq!(again.worked, 3);
        assert_eq!(again.last.pointer, first.last.pointer);
        assert_eq!(again.by, "retry");
        assert_eq!(
            listed(&next),
            (3..6).map(|n| pointer(WINDOWS[1], n)).collect::<Vec<_>>()
        );
        Ok(())
    }

    /// A move of the marker that would pass a window larger than a page whole is refused, the
    /// oldest window or one past it alike; once only its last page is left, the marker passes it.
    #[test]
    fn the_marker_passes_a_window_larger_than_a_page_only_after_its_pages() -> Result<()> {
        let root = scratch("skip")?;
        let data = DataDir::new(root.clone());
        write_window(&data, WINDOWS[0], 1)?;
        write_window(&data, WINDOWS[1], 7)?;
        write_window(&data, WINDOWS[2], 1)?;
        let exclusion = Exclusion::default();
        let serving = paged(&exclusion, 3);
        let over_it = advance_in(&data, WINDOWS[2], "cycle", &serving);
        let marker_after_refusal = held(&data)?;
        advance_in(&data, WINDOWS[0], "cycle", &serving)?;
        let onto_it = advance_in(&data, WINDOWS[1], "cycle", &serving);
        page_in(&data, WINDOWS[1], 3, "cycle", &serving)?;
        let one_page_left = advance_in(&data, WINDOWS[2], "cycle", &serving);
        page_in(&data, WINDOWS[1], 6, "cycle", &serving)?;
        let last_page_left = advance_in(&data, WINDOWS[2], "cycle", &serving)?;
        let spent = data.marker_page().exists();
        std::fs::remove_dir_all(&root)?;

        let message = over_it
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            is_refused(over_it),
            "the move over the large window went through"
        );
        assert!(
            message.contains(WINDOWS[1]) && message.contains("ctx marker page"),
            "{message}"
        );
        assert_eq!(
            marker_after_refusal, None,
            "a refused move moved the marker"
        );
        assert!(is_refused(onto_it));
        assert!(is_refused(one_page_left));
        assert_eq!(last_page_left.window, WINDOWS[2]);
        assert!(!spent);
        Ok(())
    }

    /// A page ends at a message, not at a count: an exclusion set after a page was recorded, which
    /// leaves out a message that page held, leaves no message of the next page unlisted.
    #[test]
    fn an_exclusion_set_after_a_page_moves_no_message_between_pages() -> Result<()> {
        let root = scratch("exclusion")?;
        let data = DataDir::new(root.clone());
        let id = WINDOWS[0];
        write_window(&data, id, 7)?;
        let none = Exclusion::default();
        page_in(&data, id, 3, "cycle", &paged(&none, 3))?;
        let excluded = Exclusion::new(&[pointer(id, 1)])?;
        let pending = pending_in(&data, None, &paged(&excluded, 3))?;
        std::fs::remove_dir_all(&root)?;

        assert_eq!(
            listed(&pending),
            (3..6).map(|n| pointer(id, n)).collect::<Vec<_>>()
        );
        let window = pending.windows.first().context("the window waits")?;
        let page = window.page.context("the window is worked in pages")?;
        assert_eq!(window.new_messages, 6);
        assert_eq!((page.worked, page.through), (2, 5));
        Ok(())
    }
}

#[cfg(test)]
mod pending_size;
