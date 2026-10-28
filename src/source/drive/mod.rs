//! Built-in Google Drive source.
//!
//! The `drive` stream follows the Drive changes feed, so its cursor is a changes page token with
//! the time Drive is stored through beside it, `<token>@<rfc3339>`, read at most
//! `MAX_CHANGED_FILES` files a run. A token Drive refuses is dropped: the stream lists again from
//! that time under a new token. A first fetch takes a start page token, then lists the backfill
//! window by time, a slice of at most `MAX_SLICE_DAYS` a run; until a slice reaches the window
//! end the cursor names the time listed through and carries that token, and the run whose slice
//! reaches the window end names the token. That run lists with no upper bound; when it sees a
//! file changed after the window end it names the window end, still carrying the token, so the
//! next run lists that file in its own window. A file dated later than any clock has reached
//! ([`google::latest_believable`]) is stored at the window end with a warning, so a wrong date
//! cannot hold the cursor. The `meet` stream follows the Meet artifacts folder, whose cursor is
//! a timestamp.
//!
//! A Doc, Sheet or Slides file is stored as its exported text, capped at `max_export_bytes`; any
//! other file is stored as metadata with a warning, so one large recording cannot stall a fetch.
//! So is a document Drive will not export for a reason that is the file itself — download
//! restricted, too large to export, of a kind Drive does not export, not readable by the user,
//! or gone — so one such file cannot stop the stream on every run. Any other refusal, such as a
//! spent daily limit, ends the stream with an error, so its cursor stays before the file.

mod api;
mod fetch;
mod records;

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

use super::{emit_stream, Message, Request, Source};
use api::GoogleDriveApi;
use fetch::{emit_fetch_hello, emit_resolve_hello, fetch_with_api};

/// Where Meet files a folder per meeting, holding that meeting's notes and transcript.
const DEFAULT_MEET_FOLDER: &str = "Google Meet";
/// An export stops at this many bytes of text.
const DEFAULT_EXPORT_BYTES: usize = 5 * 1024 * 1024;

/// Built-in source for Google Drive files.
pub struct DriveSource;

impl DriveSource {
    /// Creates a Drive source that calls the Drive API.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DriveSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for DriveSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                emit_fetch_hello(request, &config, sink)?;
                let api = GoogleDriveApi::load()?;
                fetch_with_api(request, &config, &api, sink)
            }
            Request::Resolve(request) => {
                let _config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriveConfig {
    /// Follow every change the credential can see.
    #[serde(default = "enabled")]
    changes: bool,
    /// Follow Meet transcripts and notes.
    #[serde(default = "enabled")]
    meet: bool,
    #[serde(default = "default_meet_folder")]
    meet_folder: String,
    #[serde(default = "default_export_bytes")]
    max_export_bytes: usize,
}

const fn enabled() -> bool {
    true
}

fn default_meet_folder() -> String {
    DEFAULT_MEET_FOLDER.to_owned()
}

const fn default_export_bytes() -> usize {
    DEFAULT_EXPORT_BYTES
}

fn parse_config(value: &Value) -> Result<DriveConfig> {
    let config: DriveConfig =
        serde_json::from_value(value.clone()).context("parsing Drive source configuration")?;
    if config.meet_folder.trim().is_empty() {
        bail!("Drive source meet_folder must not be empty");
    }
    if config.max_export_bytes == 0 {
        bail!("Drive source max_export_bytes must be greater than zero");
    }
    Ok(config)
}

#[cfg(test)]
mod tests;
