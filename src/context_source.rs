//! The context source: an optional program that says what a system the graph does not ingest,
//! such as an issue tracker, knows about the subjects one window names.
//!
//! `[context_source]` in `ctx.toml` names the program. It gets one JSON request on standard
//! input, `{"window": <id>, "subjects": [<subject>…]}`, and answers on standard output with
//! `{"items": [{"ref", "title", "status", "updated_at", "links"}…], "unavailable": <reason>}`.
//! A subject is a key the configured pattern matches or a web address under a configured
//! prefix, found in the window's messages and in the `tracked_by` of every open record. Of a
//! window worked in pages, only the messages of the page `ctx pending` lists are read.
//!
//! What it answers reaches an agent that reads text outside parties wrote, so every field is
//! cut and cleaned here, whatever the program did: control characters go, a title is at most
//! [`TITLE_CHARS`] characters, an item at most [`ITEM_BYTES`] bytes and the whole answer,
//! subjects included, at most [`ANSWER_BYTES`]. A link is kept only when it is a whole
//! `https://` address or one pointer of a configured source. A program that is missing,
//! fails, outlives its time or prints anything but that JSON makes the answer `unavailable`,
//! never an error: context is optional, and the cycle goes on without it. Each run is one line
//! on standard error.

use std::collections::{BTreeSet, HashSet};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::ContextSourceConfig;
use crate::exclude::Exclusion;
use crate::pointer::{is_web_address, Schemes};
use crate::proc;
use crate::redact::one_line;
use crate::tree::Tree;
use crate::windows::{Window, WindowId};

/// The most seconds one run of the program may take, and the default.
pub const TOTAL_SECONDS: u64 = 60;
/// The most subjects one request names.
pub const MAX_SUBJECTS: usize = 30;
/// The most bytes the subjects of one request take together.
pub const SUBJECT_BYTES: usize = 8 * 1024;
/// The most bytes one answer takes, as JSON, subjects and all.
pub const ANSWER_BYTES: usize = 48 * 1024;
/// The most bytes one item takes, as JSON.
pub const ITEM_BYTES: usize = 1536;
/// The most characters of a title.
pub const TITLE_CHARS: usize = 200;
/// The most bytes of standard output read from the program; more makes the answer unavailable.
pub const OUTPUT_BYTES: usize = 256 * 1024;
/// The most bytes of standard error read from the program.
const STDERR_BYTES: usize = 16 * 1024;
/// The most characters of an item's `ref`, `status` or `updated_at`, and of a subject.
const FIELD_CHARS: usize = 200;
/// The most characters of one link; a longer link is dropped, never cut.
const LINK_CHARS: usize = 500;
/// The most links one item keeps.
const MAX_LINKS: usize = 10;
/// The most characters of the reason a program gives for being unavailable.
const REASON_CHARS: usize = 200;

/// What the program is sent on standard input.
#[derive(Serialize)]
struct Request<'a> {
    window: &'a str,
    subjects: &'a [String],
}

/// What the program prints, before it is cleaned. Unknown fields are ignored, so a program
/// may say more than this reads; an answer with neither `items` nor `unavailable` is malformed.
#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    items: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    unavailable: Option<serde_json::Value>,
}

/// One item as the program printed it: `ref`, `title` and `status` are required.
#[derive(Deserialize)]
struct RawItem {
    #[serde(rename = "ref")]
    reference: String,
    title: String,
    status: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    links: Vec<String>,
}

/// One thing the context source knows about, cleaned.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// The item's links, each a whole `https://` address or one pointer of a configured source.
    #[serde(default)]
    pub links: Vec<String>,
}

/// The context of one window, as `GET /v1/windows/<id>/context` and `ctx context` answer it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowContext {
    pub window: String,
    pub subjects: Vec<String>,
    pub items: Vec<Item>,
    /// Items left out because they were malformed, too large, or past the answer's size.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped: usize,
    /// Why there is no context: the program failed, or said it could not answer. No change
    /// may rest on context while this is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl WindowContext {
    /// The answer for a tree with no context source: nothing, and nothing wrong.
    #[must_use]
    pub fn empty(window: &str) -> Self {
        Self {
            window: window.to_owned(),
            subjects: Vec::new(),
            items: Vec::new(),
            dropped: 0,
            unavailable: None,
        }
    }
}

/// Checks a `[context_source]` table.
///
/// # Errors
///
/// Returns an error when the command is empty, the key pattern does not compile or matches
/// the empty string, a link prefix is not an http or https address, or the timeout is zero or
/// over [`TOTAL_SECONDS`].
pub fn validate(config: &ContextSourceConfig) -> Result<()> {
    if config
        .command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        bail!("command must name a program");
    }
    crate::server::identity::Allowed::callers(&config.callers)
        .context("callers must name who may read a window's context")?;
    if let Some(pattern) = config.key_pattern.as_deref() {
        let compiled = Regex::new(pattern).context("key_pattern")?;
        if compiled.is_match("") {
            bail!("key_pattern {pattern:?} matches empty text, so every message would match it");
        }
    }
    for prefix in &config.link_prefixes {
        if !is_web_address(prefix) {
            bail!("link_prefixes entry {prefix:?} is not an http or https address");
        }
    }
    if let Some(seconds) = config.timeout_secs {
        if seconds == 0 || seconds > TOTAL_SECONDS {
            bail!("timeout_secs must be 1 to {TOTAL_SECONDS}, got {seconds}");
        }
    }
    Ok(())
}

/// Why a window's context cannot be asked for: the caller named it wrongly, or it is not there.
#[derive(Debug)]
pub enum Refused {
    /// Not a window id.
    Malformed(String),
    /// No completed window by that id.
    Missing(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(reason) | Self::Missing(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for Refused {}

/// What one call may spend, at most [`MAX_SUBJECTS`] subjects and [`TOTAL_SECONDS`]: a caller
/// that reads several windows in one cycle passes what is left of the cycle's budget, so the
/// budget holds for the cycle and not for each window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_subjects: usize,
    pub seconds: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_subjects: MAX_SUBJECTS,
            seconds: TOTAL_SECONDS,
        }
    }
}

impl Limits {
    /// These limits, each at most the default.
    #[must_use]
    pub fn capped(max_subjects: Option<usize>, seconds: Option<u64>) -> Self {
        Self {
            max_subjects: max_subjects.unwrap_or(MAX_SUBJECTS).min(MAX_SUBJECTS),
            seconds: seconds.unwrap_or(TOTAL_SECONDS).min(TOTAL_SECONDS),
        }
    }
}

/// What one window's context needs, read from the tree: the program, the subjects, the
/// pointer schemes the tree's sources own and the time the call may take. `None` when the tree
/// has no context source.
pub struct Prepared {
    pub window: String,
    pub config: ContextSourceConfig,
    pub subjects: Vec<String>,
    pub schemes: Schemes,
    pub seconds: u64,
}

/// Reads what asking for `window`'s context needs, while the tree is held, so the program
/// itself runs without it. At most `limits.max_subjects` subjects are kept.
///
/// # Errors
///
/// Returns [`Refused`] when `window` is not a completed window id, and another error when the
/// window's records or the data directory cannot be read.
pub fn prepare(
    context: &crate::context::Context,
    window: &str,
    limits: Limits,
) -> Result<Option<Prepared>> {
    let id = WindowId::parse(window)
        .map_err(|error| Refused::Malformed(one_line(&format!("{error:#}"))))?;
    let Some(config) = context.config.context_source.clone() else {
        return Ok(None);
    };
    let dir = context.data()?.windows().join(id.as_str());
    let window = Window::open(dir)
        .map_err(|_| Refused::Missing(format!("window {id} does not exist or is incomplete")))?;
    let exclusion = Exclusion::configured(&context.config)?;
    // A window worked in pages has the context of the page an answer lists now, so a cycle on a
    // later page is not handed what the window's first page named.
    let page = crate::marker::page_pointers(context, &window)?;
    let mut subjects = subjects(&config, &window, &context.tree, &exclusion, page.as_ref())?;
    subjects.truncate(limits.max_subjects);
    let schemes = crate::source::configured_schemes(&context.config.sources)?;
    let seconds = config
        .timeout_secs
        .unwrap_or(TOTAL_SECONDS)
        .min(limits.seconds)
        .min(TOTAL_SECONDS);
    Ok(Some(Prepared {
        window: id.as_str().to_owned(),
        config,
        subjects,
        schemes,
        seconds,
    }))
}

/// The subjects of one window: what the key pattern matches and the web addresses under a link
/// prefix, in the window's messages (less those the deployment excludes, and, when `only` names
/// the messages of the page an answer lists, less every other) and then in the `tracked_by` of
/// every open record, each once, at most [`MAX_SUBJECTS`].
///
/// # Errors
///
/// Returns an error when the window's records cannot be read or the key pattern does not
/// compile.
pub fn subjects(
    config: &ContextSourceConfig,
    window: &Window,
    tree: &Tree,
    exclusion: &Exclusion,
    only: Option<&BTreeSet<String>>,
) -> Result<Vec<String>> {
    let pattern = config
        .key_pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .context("key_pattern")?;
    let mut found = Found::default();
    for sourced in window.records()? {
        let record = &sourced.record;
        if exclusion.matches(&record.pointer)
            || only.is_some_and(|only| !only.contains(&record.pointer))
        {
            continue;
        }
        for text in [
            Some(record.text.as_str()),
            record.title.as_deref(),
            record.url.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            found.scan(text, pattern.as_ref(), &config.link_prefixes);
        }
    }
    for item in crate::open::open_items(tree) {
        for entry in &item.tracked_by {
            found.scan(entry, pattern.as_ref(), &config.link_prefixes);
        }
    }
    Ok(found.subjects)
}

/// The subjects found so far, in the order they were found.
#[derive(Default)]
struct Found {
    subjects: Vec<String>,
    seen: HashSet<String>,
    bytes: usize,
}

impl Found {
    fn scan(&mut self, text: &str, pattern: Option<&Regex>, prefixes: &[String]) {
        if !prefixes.is_empty() {
            for word in text.split(|c: char| {
                c.is_whitespace()
                    || matches!(c, '<' | '>' | '|' | '(' | ')' | '[' | ']' | '"' | '\'')
            }) {
                let word = word.trim_end_matches(['.', ',', ';', ':', '!', '?']);
                if prefixes
                    .iter()
                    .any(|prefix| word.starts_with(prefix.as_str()))
                {
                    self.add(word);
                }
            }
        }
        if let Some(pattern) = pattern {
            for found in pattern.find_iter(text) {
                self.add(found.as_str());
            }
        }
    }

    fn add(&mut self, subject: &str) {
        if self.subjects.len() >= MAX_SUBJECTS
            || subject.is_empty()
            || subject.chars().count() > FIELD_CHARS.max(LINK_CHARS)
            || subject.chars().any(char::is_control)
            || self.bytes.saturating_add(subject.len()) > SUBJECT_BYTES
        {
            return;
        }
        if self.seen.insert(subject.to_owned()) {
            self.bytes += subject.len();
            self.subjects.push(subject.to_owned());
        }
    }
}

/// Runs the context source for one window and answers with what it said, cleaned, or with
/// why it could not. Never fails: a program that fails makes the answer `unavailable`. One
/// line on standard error says what the run did.
#[must_use]
pub fn read(prepared: Prepared) -> WindowContext {
    let started = Instant::now();
    let Prepared {
        window,
        config,
        subjects,
        schemes,
        seconds,
    } = prepared;
    // What the items may take: the answer's cap less everything else it holds, with room for
    // the largest `dropped` count and `unavailable` reason it can carry.
    let envelope = serde_json::to_vec(&WindowContext {
        window: window.clone(),
        subjects: subjects.clone(),
        items: Vec::new(),
        dropped: usize::MAX,
        unavailable: Some("x".repeat(REASON_CHARS)),
    })
    .map_or(ANSWER_BYTES, |json| json.len());
    let budget = ANSWER_BYTES.saturating_sub(envelope);
    let (items, dropped, unavailable, detail) = if subjects.is_empty() {
        (Vec::new(), 0, None, None)
    } else if seconds == 0 {
        let reason = "the cycle's time for context ran out".to_owned();
        (Vec::new(), 0, Some(reason.clone()), Some(reason))
    } else {
        match call(&config, &window, &subjects, &schemes, budget, seconds) {
            Ok((items, dropped, unavailable)) => (items, dropped, unavailable.clone(), unavailable),
            Err(failure) => (Vec::new(), 0, Some(failure.reason), Some(failure.detail)),
        }
    };
    let answer = WindowContext {
        window,
        subjects,
        items,
        dropped,
        unavailable,
    };
    eprintln!(
        "{}",
        log_line(&answer, started.elapsed(), detail.as_deref())
    );
    answer
}

/// The one line a run writes to standard error: counts and times, never an item's text.
fn log_line(answer: &WindowContext, elapsed: Duration, detail: Option<&str>) -> String {
    let bytes = serde_json::to_vec(&answer.items).map_or(0, |items| items.len());
    let mut line = format!(
        "context source: window {}: {} subjects, {} items, {} dropped, {bytes} bytes, {} ms",
        answer.window,
        answer.subjects.len(),
        answer.items.len(),
        answer.dropped,
        elapsed.as_millis()
    );
    if let Some(detail) = detail {
        line.push_str(", unavailable: ");
        line.push_str(detail);
    }
    one_line(&line)
}

/// Why a run gave no context: what the answer says, and what the log line says.
struct Failure {
    reason: String,
    detail: String,
}

impl Failure {
    fn new(reason: &str, detail: impl Into<String>) -> Self {
        Self {
            reason: reason.to_owned(),
            detail: detail.into(),
        }
    }
}

type Called = (Vec<Item>, usize, Option<String>);

fn call(
    config: &ContextSourceConfig,
    window: &str,
    subjects: &[String],
    schemes: &Schemes,
    budget: usize,
    seconds: u64,
) -> Result<Called, Failure> {
    let request = serde_json::to_vec(&Request { window, subjects })
        .map_err(|error| Failure::new("the request could not be written", error.to_string()))?;
    let Some((program, arguments)) = config.command.split_first() else {
        return Err(Failure::new(
            "no program is configured",
            "no program is configured",
        ));
    };
    let mut command = Command::new(program);
    command.args(arguments);
    let timeout = Duration::from_secs(seconds);
    let output = proc::run_command_limited(
        command,
        program,
        &request,
        timeout,
        OUTPUT_BYTES,
        STDERR_BYTES,
    )
    .map_err(|error| {
        let detail = format!("{error:#}");
        if error.downcast_ref::<proc::TimedOut>().is_some() {
            Failure::new(
                &format!(
                    "the context source did not answer within {}s",
                    timeout.as_secs()
                ),
                detail,
            )
        } else {
            Failure::new(
                &format!(
                    "the context source could not be run, or printed more than {OUTPUT_BYTES} bytes"
                ),
                detail,
            )
        }
    })?;
    if !output.success() {
        let status = output.code.map_or_else(
            || "was stopped by a signal".to_owned(),
            |code| format!("exited with status {code}"),
        );
        let reason = format!("the context source {status}");
        return Err(Failure::new(&reason, reason.clone()));
    }
    let answer: Answer = serde_json::from_str::<serde_json::Value>(&output.stdout)
        .ok()
        .filter(serde_json::Value::is_object)
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| {
            Failure::new(
                "the context source printed something other than its JSON answer",
                "its answer is not one JSON object of the answer's shape",
            )
        })?;
    let unavailable = match answer.unavailable {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => None,
        Some(serde_json::Value::String(reason)) if !single_line(&reason, 1).is_empty() => {
            Some(single_line(&reason, REASON_CHARS))
        }
        Some(_) => Some("the context source said it could not answer".to_owned()),
    };
    if unavailable.is_some() {
        return Ok((Vec::new(), 0, unavailable));
    }
    let Some(raw) = answer.items else {
        return Err(Failure::new(
            "the context source printed something other than its JSON answer",
            "its answer has neither items nor unavailable",
        ));
    };
    let (items, dropped) = clean(raw, schemes, budget);
    Ok((items, dropped, None))
}

/// The items a program answered with, each cleaned, while they take at most `budget` bytes,
/// and how many were left out. An item that is not an object with a text `ref`, `title` and
/// `status` is left out too.
fn clean(raw: Vec<serde_json::Value>, schemes: &Schemes, budget: usize) -> (Vec<Item>, usize) {
    let mut items = Vec::new();
    let mut dropped = 0;
    let mut bytes = 2;
    for raw in raw {
        let Some(item) = Some(raw)
            .filter(serde_json::Value::is_object)
            .and_then(|raw| serde_json::from_value::<RawItem>(raw).ok())
            .and_then(|raw| clean_item(raw, schemes))
        else {
            dropped += 1;
            continue;
        };
        let size = item_bytes(&item).saturating_add(1);
        if bytes + size > budget {
            dropped += 1;
            continue;
        }
        bytes += size;
        items.push(item);
    }
    (items, dropped)
}

fn item_bytes(item: &Item) -> usize {
    serde_json::to_vec(item).map_or(usize::MAX, |json| json.len())
}

/// One item cleaned: every field on one line with no control character and cut to its size,
/// only whole `https://` web addresses and single pointers of a configured source among its
/// links, and the whole under [`ITEM_BYTES`], its last links left out until it is. `None` for
/// an item with no `ref`, or one still too large without links.
fn clean_item(raw: RawItem, schemes: &Schemes) -> Option<Item> {
    let reference = single_line(&raw.reference, FIELD_CHARS);
    if reference.is_empty() {
        return None;
    }
    let mut links: Vec<String> = Vec::new();
    for link in raw.links {
        let link = link.trim();
        let https = link
            .get(..8)
            .is_some_and(|start| start.eq_ignore_ascii_case("https://"))
            && is_web_address(link);
        let pointer = !link.chars().any(char::is_control)
            && schemes
                .parse(link)
                .is_some_and(|parsed| parsed.to_string() == link);
        if links.len() < MAX_LINKS
            && link.chars().count() <= LINK_CHARS
            && (https || pointer)
            && !links.iter().any(|kept| kept == link)
        {
            links.push(link.to_owned());
        }
    }
    let mut item = Item {
        reference,
        title: single_line(&raw.title, TITLE_CHARS),
        status: single_line(&raw.status, FIELD_CHARS),
        updated_at: raw
            .updated_at
            .as_deref()
            .map(|at| single_line(at, FIELD_CHARS))
            .filter(|at| !at.is_empty()),
        links,
    };
    while item_bytes(&item) > ITEM_BYTES {
        item.links.pop()?;
    }
    Some(item)
}

/// Prints a window's context for a person: one line per item, then its links.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(answer: &WindowContext, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    if let Some(reason) = &answer.unavailable {
        return writeln!(
            out,
            "context for {} is unavailable: {reason}",
            answer.window
        );
    }
    writeln!(
        out,
        "context for {}: {} subjects, {} items",
        answer.window,
        answer.subjects.len(),
        answer.items.len()
    )?;
    for item in &answer.items {
        writeln!(out, "[{}] {}  {}", item.status, item.reference, item.title)?;
        for link in &item.links {
            writeln!(out, "     {link}")?;
        }
    }
    Ok(())
}

/// `text` on one line: each control character that breaks a line a space, every other one
/// gone, the ends trimmed, and cut to `max_chars` characters.
fn single_line(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter_map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                c.is_whitespace().then_some(' ')
            } else {
                Some(c)
            }
        })
        .collect();
    cleaned
        .trim()
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pattern: Option<&str>, prefixes: &[&str]) -> ContextSourceConfig {
        ContextSourceConfig {
            command: vec!["true".to_owned()],
            callers: vec!["cycle@example.com".to_owned()],
            key_pattern: pattern.map(str::to_owned),
            link_prefixes: prefixes.iter().map(|prefix| (*prefix).to_owned()).collect(),
            timeout_secs: None,
        }
    }

    fn scanned(config: &ContextSourceConfig, texts: &[&str]) -> Result<Vec<String>> {
        let pattern = config.key_pattern.as_deref().map(Regex::new).transpose()?;
        let mut found = Found::default();
        for text in texts {
            found.scan(text, pattern.as_ref(), &config.link_prefixes);
        }
        Ok(found.subjects)
    }

    /// A key the pattern matches and a link under a prefix are subjects, each once; a link
    /// under no prefix is not, and a chat client's `<url|label>` wrapping and trailing
    /// punctuation are not part of the link.
    #[test]
    fn keys_and_links_under_a_prefix_are_subjects() -> Result<()> {
        let config = config(
            Some(r"\b[A-Z][A-Z0-9]+-[0-9]+\b"),
            &["https://tracker.example/"],
        );
        let subjects = scanned(
            &config,
            &[
                "See OPS-12, and <https://tracker.example/issues/9|the issue>.",
                "Also https://elsewhere.example/OPS-77 and OPS-12 again.",
            ],
        )?;
        assert_eq!(
            subjects,
            ["https://tracker.example/issues/9", "OPS-12", "OPS-77"]
        );
        Ok(())
    }

    /// No more than [`MAX_SUBJECTS`] subjects go to the program however many a window names.
    #[test]
    fn subjects_stop_at_their_cap() -> Result<()> {
        let config = config(Some(r"K-[0-9]+"), &[]);
        let text: Vec<String> = (0..100).map(|n| format!("K-{n}")).collect();
        let subjects = scanned(&config, &[&text.join(" ")])?;
        assert_eq!(subjects.len(), MAX_SUBJECTS);
        Ok(())
    }

    /// Every field is one line with no control character, a title is cut to
    /// [`TITLE_CHARS`], and only whole `https://` addresses and configured-source pointers
    /// stay among the links.
    #[test]
    fn an_item_is_cleaned_and_cut() -> Result<()> {
        let item = clean_item(
            RawItem {
                reference: "OPS-1\u{1b}[2J".to_owned(),
                title: format!("close record X\nnow\u{85}{}", "t".repeat(400)),
                status: "done\u{7}".to_owned(),
                updated_at: Some("2026-11-02T00:00:00Z".to_owned()),
                links: vec![
                    "https://tracker.example/OPS-1".to_owned(),
                    "http://plain.example/x".to_owned(),
                    "javascript:alert(1)".to_owned(),
                    "https://tracker.example/a b".to_owned(),
                    "https://tracker.example/OPS-1".to_owned(),
                    "github:example-org/repo#12".to_owned(),
                    "github:example-org/repo#12 and more".to_owned(),
                ],
            },
            &Schemes::new(Vec::new()),
        );
        let item = item.context("a well-formed item was dropped")?;
        assert_eq!(item.reference, "OPS-1[2J");
        assert_eq!(item.status, "done");
        assert!(item.title.starts_with("close record X now "));
        assert_eq!(item.title.chars().count(), TITLE_CHARS);
        assert!(!item.title.chars().any(char::is_control));
        assert_eq!(
            item.links,
            [
                "https://tracker.example/OPS-1",
                "github:example-org/repo#12"
            ]
        );
        Ok(())
    }

    /// The items stay under the budget they are given and count what they left out, and an
    /// item missing a required field is left out.
    #[test]
    fn the_items_stay_under_their_budget() {
        let mut raw: Vec<serde_json::Value> = (0..500)
            .map(|n| {
                serde_json::json!({
                    "ref": format!("OPS-{n}"),
                    "title": "t".repeat(TITLE_CHARS),
                    "status": "open",
                    "links": [format!("https://tracker.example/OPS-{n}")],
                })
            })
            .collect();
        raw.push(serde_json::json!({ "ref": "OPS-NO-STATUS", "title": "x" }));
        let (items, dropped) = clean(raw, &Schemes::new(Vec::new()), ANSWER_BYTES);
        let bytes = serde_json::to_vec(&items).map_or(usize::MAX, |json| json.len());
        assert!(bytes <= ANSWER_BYTES, "{bytes} bytes");
        assert!(dropped > 0);
        assert_eq!(items.len() + dropped, 501);
        assert!(items.iter().all(|item| item.reference != "OPS-NO-STATUS"));
    }

    /// Subjects stop once they take [`SUBJECT_BYTES`], however few of them there are.
    #[test]
    fn subjects_stop_at_their_byte_cap() -> Result<()> {
        let config = config(None, &["https://tracker.example/"]);
        let links: Vec<String> = (0..MAX_SUBJECTS)
            .map(|n| format!("https://tracker.example/{n}/{}", "é".repeat(200)))
            .collect();
        let subjects = scanned(&config, &[&links.join(" ")])?;
        let bytes: usize = subjects.iter().map(String::len).sum();
        assert!(bytes <= SUBJECT_BYTES, "{bytes} bytes");
        assert!(subjects.len() < MAX_SUBJECTS);
        Ok(())
    }

    #[test]
    fn a_configuration_that_would_match_everything_is_refused() {
        assert!(validate(&config(Some(".*"), &[])).is_err());
        assert!(validate(&config(Some("["), &[])).is_err());
        assert!(validate(&config(None, &["tracker.example/"])).is_err());
        let mut empty = config(None, &[]);
        empty.command.clear();
        assert!(validate(&empty).is_err());
        let mut slow = config(None, &[]);
        slow.timeout_secs = Some(TOTAL_SECONDS + 1);
        assert!(validate(&slow).is_err());
        let mut anyone = config(None, &[]);
        anyone.callers.clear();
        assert!(validate(&anyone).is_err());
        assert!(validate(&config(
            Some(r"[A-Z]+-[0-9]+"),
            &["https://tracker.example/"]
        ))
        .is_ok());
    }
}
