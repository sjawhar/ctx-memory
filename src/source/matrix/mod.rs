//! Built-in Matrix source: the rooms one Matrix account has joined, read through the
//! client-server API with that account's access token.
//!
//! Matrix is how one person's chats reach ctx when a bridge hub carries them: text messages and
//! each chat network the hub bridges arrive as rooms on one homeserver. One room is one stream
//! (`fetch` module). Every message is the sender's own words,
//! so the source is verbatim, and a record is a message, its edit, or the fact of its removal
//! (`records` module). A fetch never writes to the homeserver: no receipt, no read marker, no
//! presence (`homeserver` module).

mod fetch;
mod homeserver;
mod records;
mod resolve;
mod rooms;
mod state;

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::api::matrix::{homeserver_base, MatrixClient};

use super::{Message, Op, Request, Source, PROTOCOL_VERSION};
use rooms::Selection;

/// The pointer scheme a Matrix source's records use. Its pointers hold the `!` and `$` of
/// Matrix ids, which only this scheme's parser reads.
const SCHEME: &str = "matrix";
/// How many requests one fetch may spend reading rooms before it leaves the rest for the next.
const DEFAULT_REQUEST_BUDGET: u32 = 1000;

/// Built-in source for the rooms of a Matrix account.
pub struct MatrixSource;

impl MatrixSource {
    /// Creates a Matrix source.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for MatrixSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for MatrixSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config, &request.source.scheme)?;
                let selection = Selection::new(&config.rooms, &config.exclude_rooms)?;
                let client = MatrixClient::load(&config.homeserver)?;
                fetch::run(request, &config, &selection, &client, sink)
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config, &request.source.scheme)?;
                let selection = Selection::new(&config.rooms, &config.exclude_rooms)?;
                sink(Message::Hello {
                    protocol: PROTOCOL_VERSION,
                    scheme: request.source.scheme.clone(),
                    ops: vec![Op::Fetch, Op::Resolve],
                    verbatim: true,
                    streams: Vec::new(),
                })?;
                if request.pointers.is_empty() {
                    return Ok(());
                }
                let client = MatrixClient::load(&config.homeserver)?;
                resolve::run(request, &selection, &client, sink)
            }
        }
    }
}

/// How a fetch learns which rooms changed before it reads any.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SyncMode {
    /// One `/sync` with no `since` token each run, which lists every room with its newest
    /// message event. It acknowledges nothing, so it is safe with an access token another
    /// client also uses.
    #[default]
    Full,
    /// One `/sync` since the token the last run stored. A homeserver deletes the to-device
    /// messages queued for the token's device up to `since`, so it needs a login of its own.
    Incremental,
    /// No `/sync`: every room is read each run.
    Off,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixConfig {
    /// The homeserver's base URL.
    homeserver: String,
    /// The rooms to read, by id, alias or name; empty reads every joined room.
    #[serde(default)]
    rooms: Vec<String>,
    /// The rooms never to read, by id, alias or name.
    #[serde(default)]
    exclude_rooms: Vec<String>,
    /// Requests one fetch may spend reading rooms.
    #[serde(default = "default_budget")]
    request_budget: u32,
    #[serde(default)]
    sync: SyncMode,
}

const fn default_budget() -> u32 {
    DEFAULT_REQUEST_BUDGET
}

fn parse_config(value: &Value, scheme: &str) -> Result<MatrixConfig> {
    if scheme != SCHEME {
        bail!(
            "a matrix source's pointers use the `{SCHEME}` scheme, whose parser reads Matrix ids; remove `scheme = \"{scheme}\"` from its configuration"
        );
    }
    let config: MatrixConfig =
        serde_json::from_value(value.clone()).context("parsing Matrix source configuration")?;
    homeserver_base(&config.homeserver)?;
    Ok(config)
}

#[cfg(test)]
mod tests;
