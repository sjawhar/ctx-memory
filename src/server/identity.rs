//! Who called the server, and how they proved it.
//!
//! Three ports, three rules. Cloud Run ingress carries a Google identity token, which is checked
//! against Google's published keys for the audience this server was given; a header is never
//! trusted there. The loopback port behind the Tailscale sidecar carries the login header
//! Tailscale Serve sets for a signed-in user device and strips for a tagged one, so a request
//! without it has no caller. The local port, on a machine with one person on it, asks for no
//! proof: every request there is the one caller the server was started with, unless a web page
//! in the machine's browser sent it.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
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
    /// The local listener of a machine with one person on it, who was named when the server
    /// started.
    Local,
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

/// Reads the one caller of the local listener from `Name <email>`, the form git writes an
/// author in.
///
/// # Errors
///
/// Returns an error when the text is not one name and one address in that form, which git
/// would refuse as an author or read as someone else: a part that begins or ends with a
/// character git strips there ([`git_strips`]) is written without it, and a name of nothing
/// else is refused.
pub fn local(text: &str) -> Result<Caller> {
    let refused = || anyhow!("{text:?} is not `Name <email>`, the form git writes an author in");
    let (name, email) = text
        .trim()
        .strip_suffix('>')
        .and_then(|inner| inner.split_once('<'))
        .ok_or_else(refused)?;
    let name = name.trim();
    let well_formed = !text.contains(char::is_control)
        && !name.is_empty()
        && !name.contains(['<', '>'])
        && !email.contains(['<', '>'])
        && !email.contains(char::is_whitespace)
        && email.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
        && ![name, email]
            .iter()
            .any(|part| part.starts_with(git_strips) || part.ends_with(git_strips));
    if !well_formed {
        return Err(refused());
    }
    Ok(Caller {
        name: name.to_owned(),
        email: email.to_owned(),
        via: Via::Local,
    })
}

/// The characters git strips from each end of an author's name and address (`crud` in git's
/// `ident.c`).
fn git_strips(c: char) -> bool {
    c <= ' ' || matches!(c, ',' | ':' | ';' | '<' | '>' | '"' | '\\' | '\'')
}

/// Refuses a request at the local listener that a web page made.
///
/// That port asks for no proof, which holds for the processes of the one person on the
/// machine, but a page from anywhere on the web runs in that person's browser too. A browser
/// names the page's origin in `Origin` on every request a page posts to another origin, so a
/// request that carries `Origin` is refused. A page whose own host name was pointed at this
/// machine (DNS rebinding) is of the port's origin, and could read its answers, but it still
/// names its own host in `Host`, so a `Host` other than `localhost` or a loopback address is
/// refused too. The `ctx` client sends no `Origin`, and names the loopback address it was
/// given.
///
/// # Errors
///
/// Returns an error naming the header that shows a web page sent the request.
pub fn local_request(headers: &HeaderMap) -> Result<()> {
    if headers.contains_key(ORIGIN) {
        bail!("the local listener answers no web page, and this request carries an Origin header");
    }
    let Some(host) = headers.get(HOST) else {
        return Ok(());
    };
    if !host.to_str().is_ok_and(names_this_machine) {
        bail!(
            "the local listener answers only requests to localhost or a loopback address, and \
             this one names {host:?} as its Host"
        );
    }
    Ok(())
}

/// Whether `host`, a `Host` header's value, is `localhost` or a loopback address, with or
/// without a port.
fn names_this_machine(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(address, _)| address),
        None => host.split_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn local_part(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

#[cfg(test)]
mod tests {
    use super::{local, local_part, local_request, tailscale, Via, TAILSCALE_LOGIN_HEADER};
    use axum::http::header::{HOST, ORIGIN};
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
        assert_eq!(local_part("ada"), "ada");
    }

    #[test]
    fn the_local_caller_is_a_name_and_an_email() -> anyhow::Result<()> {
        let caller = local("  Ada Lovelace  <ada@example.org> ")?;
        assert_eq!(caller.name, "Ada Lovelace");
        assert_eq!(caller.email, "ada@example.org");
        assert_eq!(caller.via, Via::Local);
        Ok(())
    }

    /// Each part git needs for an author is there, once, and holds nothing that would change
    /// what git reads: no angle bracket in the name, no second address, no line break.
    #[test]
    fn a_local_caller_that_is_not_a_name_and_an_email_is_refused() {
        for text in [
            "",
            "ada@example.org",
            "Ada Lovelace",
            "<ada@example.org>",
            "Ada Lovelace <>",
            "Ada Lovelace <ada>",
            "Ada Lovelace <@example.org>",
            "Ada Lovelace <ada@>",
            "Ada Lovelace <ada@example.org> again",
            "Ada <Lovelace> <ada@example.org>",
            "Ada Lovelace <ada@example.org",
            "Ada Lovelace <ada <ada@example.org>>",
            "Ada Lovelace <ada example@example.org>",
            "Ada\nLovelace <ada@example.org>",
            "Ada Lovelace <ada@example.org\n>",
            "' <ada@example.org>",
            "Ada, <ada@example.org>",
            "Ada Lovelace <\"ada\"@example.org>",
            "Ada Lovelace <ada@example.org;>",
            "Ada <ada<x@example.org>",
            "Ada <a@b@example.org>",
            "Ada > Lovelace <ada@example.org>",
        ] {
            let refused = local(text).err().map(|error| error.to_string());
            assert!(
                refused
                    .as_deref()
                    .is_some_and(|why| why.contains("Name <email>")),
                "{text:?}: {refused:?}"
            );
        }
    }

    /// The `ctx` client names the loopback address it was given and sends no `Origin`; a web
    /// page in the browser sends `Origin`, and one reached by DNS rebinding names its own host.
    #[test]
    fn the_local_listener_answers_no_web_page() -> anyhow::Result<()> {
        for host in [
            None,
            Some("127.0.0.1:4100"),
            Some("127.0.0.1"),
            Some("LOCALHOST:4100"),
            Some("[::1]:4100"),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(host) = host {
                headers.insert(HOST, host.parse()?);
            }
            assert!(local_request(&headers).is_ok(), "{host:?}");
        }
        for host in [
            "example.org:4100",
            "127.0.0.1.example.org:4100",
            "localhost.example.org",
            "[2001:db8::1]:4100",
            "0.0.0.0:4100",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(HOST, host.parse()?);
            assert!(local_request(&headers).is_err(), "{host:?}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(HOST, "127.0.0.1:4100".parse()?);
        headers.insert(ORIGIN, "https://example.org".parse()?);
        assert!(local_request(&headers).is_err());
        Ok(())
    }
}
