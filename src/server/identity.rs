//! Who called the server, and how they proved it.
//!
//! Two ports, two rules. Cloud Run ingress carries a Google identity token, which is checked
//! against Google's published keys for the audience this server was given; a header is never
//! trusted there. The loopback port behind the Tailscale sidecar carries the login header
//! Tailscale Serve sets for a signed-in user device and strips for a tagged one, so a request
//! without it has no caller.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Where Google publishes the keys that sign identity tokens.
pub const GOOGLE_KEYS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";

/// The issuer values Google puts in an identity token.
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// The header Tailscale Serve sets for a signed-in user device.
pub const TAILSCALE_LOGIN_HEADER: &str = "Tailscale-User-Login";

/// How long a fetched key set is used before it is fetched again.
const KEYS_LIFETIME: Duration = Duration::from_secs(3600);

/// Who asked for a command, and how they proved it.
#[derive(Clone, Debug)]
pub struct Caller {
    pub name: String,
    pub email: String,
    pub via: Via,
}

/// How a caller proved who they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    GoogleToken,
    Tailscale,
}

/// The claims this server reads from a Google identity token.
#[derive(Debug, Deserialize)]
struct Claims {
    email: Option<String>,
    email_verified: Option<bool>,
    name: Option<String>,
}

/// Who may call the ingress listener.
///
/// A Google identity token proves which account minted it, nothing more: every Google account
/// and every service account in every project can mint one for any audience it is told. The
/// audience says which server the caller meant to reach. This list says who the server will
/// answer, and it is the only such check this program has of its own — the Cloud Run invoker
/// check lives in the deployment, which this program cannot read.
#[derive(Clone, Debug)]
pub struct Allowed {
    /// Full account emails, lowercased.
    accounts: Vec<String>,
    /// Domains, lowercased and without the `@`.
    domains: Vec<String>,
}

impl Allowed {
    /// Reads the list the deployment configured.
    ///
    /// An entry is an account email, or `@domain` for every account in one domain.
    ///
    /// # Errors
    ///
    /// Returns an error when the list is empty or an entry is neither of those, because a
    /// server that would answer anybody must not start by accident.
    pub fn parse(entries: &[String]) -> Result<Self> {
        let mut accounts = Vec::new();
        let mut domains = Vec::new();
        for entry in entries {
            let entry = entry.trim().to_lowercase();
            if let Some(domain) = entry.strip_prefix('@') {
                if domain.is_empty() || domain.contains('@') {
                    bail!("{entry} is not a domain");
                }
                domains.push(domain.to_owned());
            } else if entry.contains('@') {
                accounts.push(entry);
            } else if !entry.is_empty() {
                bail!("{entry} is neither an account email nor an @domain");
            }
        }
        if accounts.is_empty() && domains.is_empty() {
            bail!("no principal is allowed to call this server");
        }
        Ok(Self { accounts, domains })
    }

    /// Whether this server answers the account that minted a token.
    #[must_use]
    pub fn admits(&self, email: &str) -> bool {
        let email = email.trim().to_lowercase();
        if self.accounts.iter().any(|allowed| allowed == &email) {
            return true;
        }
        let Some((_, domain)) = email.split_once('@') else {
            return false;
        };
        self.domains.iter().any(|allowed| allowed == domain)
    }
}

/// One key set, with the moment it was fetched.
struct Cached {
    fetched: Instant,
    keys: JwkSet,
}

/// Checks Google identity tokens for one audience, from the principals one server answers.
pub struct Google {
    keys_url: String,
    audience: String,
    allowed: Allowed,
    client: reqwest::Client,
    cached: Mutex<Option<Cached>>,
}

impl Google {
    /// Prepares the checker for one audience, one key set location and one allow list.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(keys_url: String, audience: String, allowed: Allowed) -> Result<Self> {
        Ok(Self {
            keys_url,
            audience,
            allowed,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("building the identity HTTP client")?,
            cached: Mutex::new(None),
        })
    }

    /// Identifies the caller of a request that carries a Google identity token.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is missing, unreadable, signed by an unknown key,
    /// meant for another audience, expired, carries no verified email, or names an account
    /// this server does not answer.
    pub async fn caller(&self, headers: &HeaderMap) -> Result<Caller> {
        let authorization = headers
            .get(AUTHORIZATION)
            .ok_or_else(|| anyhow!("no Authorization header"))?
            .to_str()
            .context("the Authorization header is not text")?;
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or_else(|| anyhow!("the Authorization header is not a bearer token"))?
            .trim();
        let header = decode_header(token).context("reading the token header")?;
        if header.alg != Algorithm::RS256 {
            bail!("identity tokens must be signed with RS256");
        }
        let key_id = header
            .kid
            .ok_or_else(|| anyhow!("the token names no signing key"))?;
        let key = self.key(&key_id).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_issuer(&GOOGLE_ISSUERS);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let decoded =
            decode::<Claims>(token, &key, &validation).context("checking the identity token")?;
        let email = decoded
            .claims
            .email
            .ok_or_else(|| anyhow!("the identity token carries no email"))?;
        // Google states for itself whether it checked the address. An unchecked one names
        // nobody, and the allow list below is read against the name.
        if decoded.claims.email_verified != Some(true) {
            bail!("the identity token's email is not verified");
        }
        if !self.allowed.admits(&email) {
            bail!("{email} is not allowed to call this server");
        }
        let name = decoded
            .claims
            .name
            .unwrap_or_else(|| local_part(&email).to_owned());
        Ok(Caller {
            name,
            email,
            via: Via::GoogleToken,
        })
    }

    /// The signing key with this id, fetching the key set when it is missing or stale.
    async fn key(&self, key_id: &str) -> Result<DecodingKey> {
        let mut cached = self.cached.lock().await;
        let fresh = cached.as_ref().is_some_and(|cached| {
            cached.fetched.elapsed() < KEYS_LIFETIME && cached.keys.find(key_id).is_some()
        });
        if !fresh {
            let keys: JwkSet = self
                .client
                .get(&self.keys_url)
                .send()
                .await
                .with_context(|| format!("fetching {}", self.keys_url))?
                .error_for_status()
                .with_context(|| format!("fetching {}", self.keys_url))?
                .json()
                .await
                .with_context(|| format!("decoding {}", self.keys_url))?;
            *cached = Some(Cached {
                fetched: Instant::now(),
                keys,
            });
        }
        let keys = cached
            .as_ref()
            .ok_or_else(|| anyhow!("no signing keys were loaded"))?;
        let key = keys
            .keys
            .find(key_id)
            .ok_or_else(|| anyhow!("no signing key {key_id} in {}", self.keys_url))?;
        DecodingKey::from_jwk(key).context("reading a Google signing key")
    }
}

/// Identifies the caller of a request that came through the Tailscale sidecar.
///
/// # Errors
///
/// Returns an error when the login header is missing or empty, which is what Tailscale Serve
/// leaves for a tagged device.
pub fn tailscale(headers: &HeaderMap) -> Result<Caller> {
    let login = headers
        .get(TAILSCALE_LOGIN_HEADER)
        .ok_or_else(|| anyhow!("no {TAILSCALE_LOGIN_HEADER} header"))?
        .to_str()
        .context("the Tailscale login header is not text")?
        .trim();
    if login.is_empty() {
        bail!("the {TAILSCALE_LOGIN_HEADER} header is empty");
    }
    Ok(Caller {
        name: local_part(login).to_owned(),
        email: login.to_owned(),
        via: Via::Tailscale,
    })
}

fn local_part(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

#[cfg(test)]
mod tests {
    use super::{local_part, tailscale, Via, TAILSCALE_LOGIN_HEADER};
    use axum::http::HeaderMap;

    #[test]
    fn tailscale_login_names_the_caller() -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(TAILSCALE_LOGIN_HEADER, "ada@example.com".parse()?);
        let caller = tailscale(&headers)?;
        assert_eq!(caller.email, "ada@example.com");
        assert_eq!(caller.name, "ada");
        assert_eq!(caller.via, Via::Tailscale);
        Ok(())
    }

    #[test]
    fn a_tagged_device_has_no_caller() {
        let error = tailscale(&HeaderMap::new()).err().map(|e| e.to_string());
        assert_eq!(error, Some(format!("no {TAILSCALE_LOGIN_HEADER} header")));
    }

    #[test]
    fn a_login_without_a_domain_is_its_own_name() {
        assert_eq!(local_part("sami"), "sami");
    }
}
