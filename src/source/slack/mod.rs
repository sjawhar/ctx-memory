//! Built-in Slack source.

mod auth;
mod fetch;
mod records;
mod serve;

use anyhow::{bail, Context as _, Result};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use crate::api::slack::SlackClient;

use super::{Message, Request, Source};
use auth::{fetch_authenticator, load_credential};
use fetch::{emit_resolve_hello, resolve_with_api, run_fetch};

pub use records::record_from_message;
pub use serve::{slack_schemes, Mentions};

const DEFAULT_THREAD_POLL_DAYS: u32 = 30;

/// Built-in source for Slack conversations.
pub struct SlackSource {
    api: Option<String>,
}

impl SlackSource {
    /// Creates a Slack source using Slack's production API endpoint.
    #[must_use]
    pub const fn new() -> Self {
        Self { api: None }
    }

    /// Creates a Slack source using an alternate API endpoint.
    #[must_use]
    pub fn with_api(api: impl Into<String>) -> Self {
        Self {
            api: Some(api.into()),
        }
    }
}

impl Default for SlackSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for SlackSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                let credential = load_credential()?;
                let auth = fetch_authenticator(credential, Utc::now());
                run_fetch(request, &config, self.api.as_deref(), &auth, sink)
            }
            Request::Resolve(request) => {
                let config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)?;
                // A live resolve never rotates the token: only the fetch job renews the Slack
                // credential (contract C6), so another holder is not raced here.
                let credential = load_credential()?;
                let token = credential.access();
                if let Some(api) = &self.api {
                    let client = SlackClient::with_api(token, api);
                    resolve_with_api(request, &config, &client, sink)
                } else {
                    let client = SlackClient::new(token);
                    resolve_with_api(request, &config, &client, sink)
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackConfig {
    workspace: String,
    /// Private channels to fetch. Public channels the signed-in user is in are discovered from
    /// the API; a private channel is fetched only when it is listed here.
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default = "default_thread_poll_days")]
    thread_poll_days: u32,
}

const fn default_thread_poll_days() -> u32 {
    DEFAULT_THREAD_POLL_DAYS
}

fn parse_config(value: &Value) -> Result<SlackConfig> {
    let config: SlackConfig =
        serde_json::from_value(value.clone()).context("parsing Slack source configuration")?;
    if config.workspace.is_empty() {
        bail!("Slack source workspace must not be empty");
    }
    Ok(config)
}

#[cfg(test)]
mod tests;
