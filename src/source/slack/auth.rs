//! Slack token rotation and credential save.

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};

use crate::api::client;
use crate::api::slack::{refresh_credential, SlackCredential};
use crate::credentials;

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

/// The fetch job's authenticator: it refreshes the Slack credential when it is near expiry and
/// saves the rotated credential before any call uses it (Review Focus 2).
pub(super) struct Authenticator<R, S> {
    pub(super) credential: SlackCredential,
    pub(super) now: DateTime<Utc>,
    pub(super) refresh: R,
    pub(super) save: S,
}

impl<R, S> SlackAuth for Authenticator<R, S>
where
    R: Fn(&SlackCredential, DateTime<Utc>) -> Result<SlackCredential>,
    S: Fn(&SlackCredential) -> Result<()>,
{
    fn access_token(&self) -> Result<String> {
        if !self.credential.near_expiry(self.now) {
            return Ok(self.credential.access.clone());
        }
        let renewed = (self.refresh)(&self.credential, self.now)
            .context("refreshing the Slack access token")?;
        // The old refresh token is dead the moment the line above redeems it, so the renewed
        // credential is saved before it is returned. A save failure loses the new token, which
        // is why it fails loudly here rather than let a call use an unsaved token.
        (self.save)(&renewed).context("saving the rotated Slack credential")?;
        Ok(renewed.access)
    }
}

/// Builds the authenticator the fetch job uses: a real refresh over HTTP and a real save to
/// Secret Manager.
pub(super) fn fetch_authenticator(
    credential: SlackCredential,
    now: DateTime<Utc>,
) -> impl SlackAuth {
    Authenticator {
        credential,
        now,
        refresh: |credential: &SlackCredential, now| {
            refresh_credential(credential, now, |url, form| {
                let (status, _headers, body) = client::post_form(url, form)?;
                if !status.is_success() {
                    bail!("the Slack token endpoint returned {status}");
                }
                Ok(body)
            })
        },
        save: |credential: &SlackCredential| credentials::save("slack", &credential.to_secret()?),
    }
}
