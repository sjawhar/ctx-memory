//! Normalized source records and their human-readable rendering.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One source item stored locally for resolution, verification, and delta computation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub pointer: String,
    pub stream: String,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub author_id: Option<String>,
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub text: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub context: bool,
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// Renders one record for `ctx resolve` output.
#[must_use]
pub fn render(record: &Record) -> String {
    let author = record.author.as_deref().unwrap_or("unknown");
    let mut rendered = match record.thread.as_deref() {
        Some(thread) => format!(
            "[{}] {author} [reply in thread {thread}]\n{}",
            record.ts.to_rfc3339(),
            record.pointer
        ),
        None => format!(
            "[{}] {author} [top-level]\n{}",
            record.ts.to_rfc3339(),
            record.pointer
        ),
    };
    if let Some(title) = record.title.as_deref() {
        rendered.push('\n');
        rendered.push_str(title);
    }
    rendered.push('\n');
    rendered.push_str(&record.text);
    rendered
}
