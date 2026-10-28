//! Built-in Gmail source.
//!
//! The cursor is the mailbox history ID: a fetch asks `users.history.list` for what changed
//! since the last run, and a first fetch reads the mailbox's current history ID, lists the
//! backfill window, and stores that ID. A history walk fetches at most `MAX_HISTORY_MESSAGES`
//! messages a run, and a listing reads at most `MAX_SLICE_DAYS` a run, carrying the history ID
//! it read first and the time it began in the cursor until a slice reaches the window end, so a
//! long gap takes several runs. A carried ID Gmail no longer holds starts the listing again.
//!
//! Mail that arrived after the window end waits for the next window. Mail dated later than any
//! clock has reached ([`google::latest_believable`]) is stored at the window end with a warning,
//! so a wrong date cannot hold the cursor.

mod fetch;
mod render;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

use super::{Message, Request, Source};
use fetch::{
    emit_fetch_hello, emit_resolve_hello, fetch_with_api, resolve_with_api, GoogleGmailApi,
};

pub use render::record_from_message;

/// The mailbox is one stream; Gmail has no per-label cursors.
const STREAM: &str = "me";
const DEFAULT_QUERY: &str = "-category:promotions -category:social";

/// Built-in source for Gmail messages.
pub struct GmailSource;

impl GmailSource {
    /// Creates a Gmail source that calls the Gmail API.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for GmailSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for GmailSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                emit_fetch_hello(request, sink)?;
                let api = GoogleGmailApi::load()?;
                fetch_with_api(request, &config, &api, sink)
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)?;
                let api = GoogleGmailApi::load()?;
                resolve_with_api(request, &config, &api, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GmailConfig {
    #[serde(default = "default_query")]
    query: String,
}

fn default_query() -> String {
    DEFAULT_QUERY.to_owned()
}

fn parse_config(value: &Value) -> Result<GmailConfig> {
    serde_json::from_value(value.clone()).context("parsing Gmail source configuration")
}

#[cfg(test)]
mod tests;
