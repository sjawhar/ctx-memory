//! Slack token rotation and credential save.

use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};

use crate::api::client;
use crate::api::slack::{refresh_app_credential, refresh_credential, SlackApp, SlackCredential};
use crate::credentials;
use crate::lease;
use crate::redact::one_line;

/// The pauses before the second, third and fourth attempt to save a rotated credential. Slack
/// retires the redeemed refresh token "after a short grace period", so a credential that is not
/// stored is a sign-in that is lost; a Secret Manager write that fails for a moment gets these
/// chances before the fetch gives up.
pub(super) const SAVE_RETRY_PAUSES: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

/// Reads the Slack credential from the environment, as an external source receives its own.
pub(super) fn load_credential() -> Result<SlackCredential> {
    SlackCredential::from_secret(&credentials::load("slack")?)
}

/// Supplies a Slack access token valid now, saving any rotated credential before returning it.
pub(super) trait SlackAuth {
    /// A token valid now, refreshing and saving the rotated credential first when the current
    /// one is near expiry. A save failure is an error, and the renewed token is never used.
    fn access_token(&self) -> Result<String>;
}

/// The fetch job's authenticator. A token that outlives the longest fetch is used as it is.
/// One that expires sooner is refreshed only by a process that holds the fetch lease, so two
/// processes never redeem one refresh token, and the rotated credential is saved before any call
/// uses it (Review Focus 2).
pub(super) struct Authenticator<R, S, P> {
    pub(super) credential: SlackCredential,
    pub(super) now: DateTime<Utc>,
    /// Whether this process holds the fetch lease, the one right to rotate the credential.
    pub(super) may_rotate: bool,
    pub(super) refresh: R,
    pub(super) save: S,
    /// Waits out one of [`SAVE_RETRY_PAUSES`]; a test passes one that returns at once.
    pub(super) pause: P,
}

impl<R, S, P> SlackAuth for Authenticator<R, S, P>
where
    R: Fn(&SlackCredential, DateTime<Utc>) -> Result<SlackCredential>,
    S: Fn(&SlackCredential) -> Result<()>,
    P: Fn(Duration),
{
    fn access_token(&self) -> Result<String> {
        if !self.credential.near_expiry(self.now) {
            return Ok(self.credential.access().to_owned());
        }
        if !self.may_rotate {
            bail!(
                "the Slack token expires at {}, before a fetch could finish, and only a `ctx fetch` that holds the fetch lease (CTX_LEASE_BUCKET) may refresh it, so this process leaves the stored credential alone; let the fetch job refresh it, or run `ctx slack login` to sign in again",
                self.credential
                    .expires()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            );
        }
        let renewed = (self.refresh)(&self.credential, self.now)
            .context("refreshing the Slack access token")?;
        // The old refresh token is retired once the line above redeems it, so the renewed
        // credential is saved before it is returned, with a few more tries for a write that
        // fails for a moment. If no try succeeds the renewed token is not used: the fetch fails
        // now, loudly, rather than run on a sign-in nothing stores.
        let attempts = SAVE_RETRY_PAUSES.len() + 1;
        let mut failure = None;
        for (attempt, pause) in std::iter::once(None)
            .chain(SAVE_RETRY_PAUSES.iter().copied().map(Some))
            .enumerate()
        {
            if let Some(pause) = pause {
                (self.pause)(pause);
            }
            match (self.save)(&renewed) {
                Ok(()) => return Ok(renewed.access().to_owned()),
                Err(error) => {
                    eprintln!(
                        "slack: saving the rotated Slack credential failed, attempt {} of {attempts}: {}",
                        attempt + 1,
                        one_line(&format!("{error:#}"))
                    );
                    failure = Some(error);
                }
            }
        }
        let error = failure.map_or_else(String::new, |error| format!("{error:#}"));
        // The fix comes before the save's own error, so a log line cut to its first few hundred
        // characters still says what to do.
        bail!(
            "the rotated Slack credential could not be saved after {attempts} tries, so the new token is not used; if the next fetch reports \"invalid_refresh_token\", run `ctx slack login` to sign in again, because Slack retires the stored refresh token after a short grace period. The last save failed: {error}"
        )
    }
}

/// Builds the authenticator the fetch job uses: a real refresh over HTTP, a real save to Secret
/// Manager, and the right to rotate only while this process holds the fetch lease. The
/// ctx-memory app's token is refreshed with the app's client id and secret (`slack_app`), read
/// only when a refresh is due.
pub(super) fn fetch_authenticator(
    credential: SlackCredential,
    now: DateTime<Utc>,
) -> impl SlackAuth {
    Authenticator {
        credential,
        now,
        may_rotate: lease::holds("fetch"),
        refresh: |credential: &SlackCredential, now| {
            // The app secret is read first, and the lease asked again just before the refresh
            // token is redeemed, so time spent reading never stretches past the lease.
            let app = match credential {
                SlackCredential::App(_) => Some(SlackApp::load()?),
                SlackCredential::ClaudeCode(_) => None,
            };
            if !lease::holds("fetch") {
                bail!("this process no longer holds the fetch lease, so it does not redeem the Slack refresh token");
            }
            match (credential, app) {
                (SlackCredential::App(credential), Some(app)) => {
                    refresh_app_credential(credential, &app, now).map(SlackCredential::App)
                }
                (SlackCredential::ClaudeCode(credential), _) => {
                    refresh_credential(credential, now, |url, form| {
                        let (status, _headers, body) = client::post_form(url, form)?;
                        if !status.is_success() {
                            bail!("the Slack token endpoint returned {status}");
                        }
                        Ok(body)
                    })
                    .map(SlackCredential::ClaudeCode)
                }
                (SlackCredential::App(_), None) => bail!("the Slack app credential was not read"),
            }
        },
        save: |credential: &SlackCredential| credentials::save("slack", &credential.to_secret()?),
        pause: std::thread::sleep,
    }
}
