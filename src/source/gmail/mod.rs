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
//! clock has reached ([`crate::source::latest_believable`]) is stored at the window end with a warning,
//! so a wrong date cannot hold the cursor.
//!
//! Mail from a mailing list the deployment excludes by its `List-Id` ([`lists`]), mail whose
//! text names a key of the content net ([`super::content_net`]), and mail from, to, cc or
//! reply-to an address of the net, is never stored, on any path that makes a record of a
//! message: a history walk, a listing, and a resolve. Each drop is logged to standard error by
//! its pointer ([`super::content_net::withheld`]).

mod fetch;
mod lists;
mod render;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

use super::content_net::ContentNet;
use super::{Message, Request, Source};
use fetch::{
    emit_fetch_hello, emit_resolve_hello, fetch_with_api, resolve_with_api, GoogleGmailApi,
    Withheld,
};
use lists::ExcludedLists;
pub(super) use lists::EXCLUDE_LIST_IDS_ENV;

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
                let withheld = withheld(&config)?;
                emit_fetch_hello(request, sink)?;
                let api = GoogleGmailApi::load()?;
                fetch_with_api(request, &config, &withheld, &api, sink)
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config)?;
                let withheld = withheld(&config)?;
                emit_resolve_hello(request, sink)?;
                let api = GoogleGmailApi::load()?;
                resolve_with_api(request, &withheld, &api, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GmailConfig {
    #[serde(default = "default_query")]
    query: String,
    /// List-IDs whose mail is never stored ([`lists`]).
    #[serde(default)]
    exclude_list_ids: Vec<String>,
    /// Keys of the content net: mail whose text names one is never stored.
    #[serde(default)]
    exclude_keys: Vec<String>,
    /// Addresses of the content net: mail from, to, cc or reply-to one is never stored.
    #[serde(default)]
    exclude_emails: Vec<String>,
}

fn default_query() -> String {
    DEFAULT_QUERY.to_owned()
}

fn parse_config(value: &Value) -> Result<GmailConfig> {
    serde_json::from_value(value.clone()).context("parsing Gmail source configuration")
}

/// The mail this source never stores, by its list, its text and its addresses: the
/// configuration's `exclude_list_ids`, `exclude_keys` and `exclude_emails`, and what
/// [`EXCLUDE_LIST_IDS_ENV`], `CTX_EXCLUDE_KEYS` and `CTX_EXCLUDE_EMAILS` add, read in the
/// environment of the process that runs the source, the fetch or the server, and never a
/// caller's. Refused while `--content-net` did not turn the settings on.
fn withheld(config: &GmailConfig) -> Result<Withheld> {
    let lists = ExcludedLists::from_parts(
        &config.exclude_list_ids,
        super::environment(EXCLUDE_LIST_IDS_ENV)?.as_deref(),
    )
    .context("reading the Gmail source's excluded List-IDs")?;
    if !lists.is_empty() {
        super::content_net::require("exclude_list_ids or CTX_GMAIL_EXCLUDE_LIST_IDS")?;
    }
    let net = ContentNet::configured(&config.exclude_keys, &config.exclude_emails)?;
    Ok(Withheld { lists, net })
}

#[cfg(test)]
mod tests;
