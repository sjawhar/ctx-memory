//! Who called the server, and how they proved it.
//!
//! Two ports, two rules. Cloud Run ingress carries a Google identity token, which is checked
//! against Google's published keys for the audience this server was given; a header is never
//! trusted there. The loopback port behind the Tailscale sidecar carries the login header
//! Tailscale Serve sets for a signed-in user device and strips for a tagged one. A tagged node
//! is identified, when the deployment allows its tag, by asking tailscaled's local API who the
//! address in `X-Forwarded-For` is: Tailscale Serve overwrites that header with the tailnet
//! address the request came from, and only the sidecar can reach the loopback port. A request
//! with neither has no caller.

use std::net::IpAddr;
use std::path::Path;
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

/// The header Tailscale Serve overwrites with the tailnet address a request came from.
pub const FORWARDED_FOR_HEADER: &str = "X-Forwarded-For";

/// The host name tailscaled's local API requires on every request to it.
const LOCAL_API_HOST: &str = "local-tailscaled.sock";

/// How long one question to tailscaled's local API may take.
const WHOIS_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// A tagged tailnet node, by a tag the deployment allows; tailscaled named its tags.
    TailscaleTag,
}

/// The claims this server reads from a Google identity token.
#[derive(Debug, Deserialize)]
struct Claims {
    email: Option<String>,
    email_verified: Option<bool>,
    name: Option<String>,
}

/// Who may call the ingress listener, or read a window's context.
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
    /// Tailnet tags (`tag:<name>`); only [`Allowed::callers`] takes them.
    tags: Vec<String>,
}

impl Allowed {
    /// Reads the list the deployment configured for the ingress listener.
    ///
    /// An entry is an account email, or `@domain` for every account in one domain.
    ///
    /// # Errors
    ///
    /// Returns an error when the list is empty or an entry is neither of those, because a
    /// server that would answer anybody must not start by accident.
    pub fn parse(entries: &[String]) -> Result<Self> {
        Self::read(entries, false)
    }

    /// Reads a list of callers that may come through either door: an account email, `@domain`,
    /// or a tailnet tag (`tag:<name>`) for a tagged node `--tailnet-allow-tag` admits.
    ///
    /// # Errors
    ///
    /// Returns an error when the list is empty or an entry is none of those.
    pub fn callers(entries: &[String]) -> Result<Self> {
        Self::read(entries, true)
    }

    fn read(entries: &[String], take_tags: bool) -> Result<Self> {
        let mut accounts = Vec::new();
        let mut domains = Vec::new();
        let mut tags = Vec::new();
        for entry in entries {
            let trimmed = entry.trim();
            if take_tags && trimmed.starts_with("tag:") {
                tags.push(tag(trimmed)?);
                continue;
            }
            let entry = trimmed.to_lowercase();
            if let Some(domain) = entry.strip_prefix('@') {
                if domain.is_empty() || domain.contains('@') {
                    bail!("{entry} is not a domain");
                }
                domains.push(domain.to_owned());
            } else if entry.contains('@') {
                accounts.push(entry);
            } else if !entry.is_empty() {
                if take_tags {
                    bail!("{entry} is neither an account email, an @domain nor a tag:<name>");
                }
                bail!("{entry} is neither an account email nor an @domain");
            }
        }
        if accounts.is_empty() && domains.is_empty() && tags.is_empty() {
            bail!("no principal is allowed to call this server");
        }
        Ok(Self {
            accounts,
            domains,
            tags,
        })
    }

    /// Whether this server answers a caller, named by its email or, for a tagged tailnet
    /// node, by its tag.
    #[must_use]
    pub fn admits(&self, email: &str) -> bool {
        let trimmed = email.trim();
        if trimmed.starts_with("tag:") {
            return self.tags.iter().any(|allowed| allowed == trimmed);
        }
        let email = trimmed.to_lowercase();
        if self.accounts.iter().any(|allowed| allowed == &email) {
            return true;
        }
        let Some((_, domain)) = email.split_once('@') else {
            return false;
        };
        self.domains.iter().any(|allowed| allowed == domain)
    }
}

/// One tailnet tag, checked: `tag:` and a name of ASCII letters, digits and dashes that starts
/// with a letter, as Tailscale's policy file takes them.
///
/// # Errors
///
/// Returns an error when the entry is not of that shape.
pub fn tag(entry: &str) -> Result<String> {
    let entry = entry.trim();
    let name = entry
        .strip_prefix("tag:")
        .ok_or_else(|| anyhow!("{entry:?} is not a tailnet tag: a tag is tag:<name>"))?;
    let starts_with_letter = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if !starts_with_letter || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!(
            "{entry:?} is not a tailnet tag: the name after tag: is ASCII letters, digits and \
             dashes, starting with a letter"
        );
    }
    Ok(entry.to_owned())
}

/// Reads the tailnet tags a deployment allows, skipping repeats. Every entry must be a tag: a
/// blank one (`--tailnet-allow-tag ''`, a stray comma) is refused rather than dropped, since
/// dropping it would start a server that meant to admit a tag without the checks that go with
/// one.
///
/// # Errors
///
/// Returns an error when an entry is not a tailnet tag ([`tag`]).
pub fn tags(entries: &[String]) -> Result<Vec<String>> {
    let mut tags: Vec<String> = Vec::new();
    for entry in entries {
        let tag = tag(entry)?;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    Ok(tags)
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

/// The tagged tailnet nodes the tailnet listener answers, and how it asks who a node is.
///
/// Tailscale Serve sends no login header for a tagged node, so the node is named by the address
/// Serve writes into `X-Forwarded-For`, which tailscaled's local API maps to the node and its
/// tags. Only a node carrying one of these tags is answered, and it is named by that tag.
#[derive(Debug)]
pub struct TailnetTags {
    tags: Vec<String>,
    whois: reqwest::Client,
}

/// What this server reads from the local API's answer to `whois`.
#[derive(Debug, Deserialize)]
struct WhoIs {
    #[serde(rename = "Node")]
    node: Option<WhoIsNode>,
}

#[derive(Debug, Deserialize)]
struct WhoIsNode {
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

impl TailnetTags {
    /// Answers the nodes carrying one of `tags`, asking the tailscaled whose local API listens
    /// on `socket`. The socket is not opened until a request needs it.
    ///
    /// # Errors
    ///
    /// Returns an error when `tags` is empty or the HTTP client cannot be built.
    pub fn new(tags: Vec<String>, socket: &Path) -> Result<Self> {
        if tags.is_empty() {
            bail!("no tailnet tag is allowed");
        }
        let whois = reqwest::Client::builder()
            .unix_socket(socket)
            .timeout(WHOIS_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the tailscaled local API client")?;
        Ok(Self { tags, whois })
    }

    /// Identifies a request without a login header by the node it came from.
    async fn caller(&self, headers: &HeaderMap) -> Result<Caller> {
        let address = forwarded_for(headers)?;
        // A failure to reach or read the local API is logged here and answered with a fixed
        // message: its error chain names the socket path and the local API's URL, which the
        // caller has no need of.
        let unanswered = |error: &dyn std::fmt::Display| {
            eprintln!(
                "asking tailscaled who {address} is: {}",
                crate::redact::one_line(&error.to_string())
            );
            anyhow!("tailscaled's local API did not say who {address} is")
        };
        let response = self
            .whois
            .get(format!("http://{LOCAL_API_HOST}/localapi/v0/whois"))
            .query(&[("addr", address.to_string())])
            .send()
            .await
            .map_err(|error| unanswered(&format!("{:#}", anyhow::Error::from(error))))?;
        let status = response.status();
        if !status.is_success() {
            bail!("tailscaled does not name a node at {address} (status {status})");
        }
        let whois: WhoIs = response
            .json()
            .await
            .map_err(|error| unanswered(&format!("{:#}", anyhow::Error::from(error))))?;
        let node_tags = whois.node.and_then(|node| node.tags).unwrap_or_default();
        if node_tags.is_empty() {
            bail!("no {TAILSCALE_LOGIN_HEADER} header, and the node at {address} carries no tag");
        }
        let tag = self
            .tags
            .iter()
            .find(|allowed| node_tags.contains(allowed))
            .ok_or_else(|| {
                anyhow!(
                    "the node at {address} carries {}, and --tailnet-allow-tag allows none of them",
                    node_tags.join(", ")
                )
            })?;
        Ok(Caller {
            name: tag.clone(),
            email: tag.clone(),
            via: Via::TailscaleTag,
        })
    }
}

/// The one address Tailscale Serve wrote into `X-Forwarded-For`.
fn forwarded_for(headers: &HeaderMap) -> Result<IpAddr> {
    let mut values = headers.get_all(FORWARDED_FOR_HEADER).iter();
    let value = values.next().ok_or_else(|| {
        anyhow!("no {TAILSCALE_LOGIN_HEADER} header, and no {FORWARDED_FOR_HEADER} header")
    })?;
    if values.next().is_some() {
        bail!("more than one {FORWARDED_FOR_HEADER} header");
    }
    value
        .to_str()
        .ok()
        .and_then(|text| text.trim().parse::<IpAddr>().ok())
        .ok_or_else(|| anyhow!("the {FORWARDED_FOR_HEADER} header is not exactly one IP address"))
}

/// Identifies the caller of a request that came through the Tailscale sidecar.
///
/// The login header names a signed-in user device whenever it is there. Without it, a tagged
/// node is identified by its tag when `tags` allows one ([`TailnetTags`]).
///
/// # Errors
///
/// Returns an error when the login header is empty, or missing and the request's node is not
/// one `tags` admits (or `tags` is `None`).
pub async fn tailscale(headers: &HeaderMap, tags: Option<&TailnetTags>) -> Result<Caller> {
    match tags {
        Some(tags) if !headers.contains_key(TAILSCALE_LOGIN_HEADER) => tags.caller(headers).await,
        _ => login(headers),
    }
}

/// Identifies a signed-in user device by the login header Tailscale Serve sets.
fn login(headers: &HeaderMap) -> Result<Caller> {
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
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use super::{
        local_part, tag, tags, tailscale, Allowed, TailnetTags, Via, FORWARDED_FOR_HEADER,
        TAILSCALE_LOGIN_HEADER,
    };
    use axum::http::HeaderMap;

    #[tokio::test]
    async fn tailscale_login_names_the_caller() -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(TAILSCALE_LOGIN_HEADER, "ada@example.com".parse()?);
        let caller = tailscale(&headers, None).await?;
        assert_eq!(caller.email, "ada@example.com");
        assert_eq!(caller.name, "ada");
        assert_eq!(caller.via, Via::Tailscale);
        Ok(())
    }

    #[tokio::test]
    async fn a_tagged_device_has_no_caller_without_an_allowed_tag() {
        let error = tailscale(&HeaderMap::new(), None)
            .await
            .err()
            .map(|e| e.to_string());
        assert_eq!(error, Some(format!("no {TAILSCALE_LOGIN_HEADER} header")));
    }

    #[test]
    fn a_login_without_a_domain_is_its_own_name() {
        assert_eq!(local_part("ada"), "ada");
    }

    /// A fake tailscaled local API on a unix socket: `whois?addr=` answers each known address
    /// with its node's tags, and 404 for any other, as tailscaled does. Every request line is
    /// recorded with its Host header.
    struct FakeLocalApi {
        dir: PathBuf,
        socket: PathBuf,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl FakeLocalApi {
        fn start(label: &str, nodes: &[(&str, Option<&[&str]>)]) -> anyhow::Result<Self> {
            let dir =
                std::env::temp_dir().join(format!("ctx-localapi-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            let socket = dir.join("tailscaled.sock");
            let listener = UnixListener::bind(&socket)?;
            let asked = Arc::new(Mutex::new(Vec::new()));
            let record = Arc::clone(&asked);
            let nodes: Vec<(String, Option<Vec<String>>)> = nodes
                .iter()
                .map(|(address, tags)| {
                    (
                        (*address).to_owned(),
                        tags.map(|tags| tags.iter().map(|t| (*t).to_owned()).collect()),
                    )
                })
                .collect();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let Ok(reader) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(reader);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let mut host = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("host") {
                                value.trim().clone_into(&mut host);
                            }
                        }
                    }
                    let target = request_line.split(' ').nth(1).unwrap_or("").to_owned();
                    if let Ok(mut asked) = record.lock() {
                        asked.push(format!("{host} {target}"));
                    }
                    let address = target
                        .split_once("addr=")
                        .map(|(_, address)| address.replace("%3A", ":"))
                        .unwrap_or_default();
                    let (status, body) = match nodes.iter().find(|(known, _)| *known == address) {
                        Some((_, tags)) => (
                            "200 OK",
                            serde_json::json!({
                                "Node": { "Name": "node.example.ts.net.", "Tags": tags },
                                "UserProfile": { "LoginName": "tagged-devices" },
                            })
                            .to_string(),
                        ),
                        None => ("404 Not Found", "no match for IP:port\n".to_owned()),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Ok(Self { dir, socket, asked })
        }

        fn asked(&self) -> Vec<String> {
            self.asked
                .lock()
                .map(|asked| asked.clone())
                .unwrap_or_default()
        }
    }

    impl Drop for FakeLocalApi {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn allow(socket: &Path) -> anyhow::Result<TailnetTags> {
        TailnetTags::new(
            vec!["tag:ctx-ingest".to_owned(), "tag:ctx-legion".to_owned()],
            socket,
        )
    }

    fn forwarded(values: &[&str]) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(FORWARDED_FOR_HEADER, value.parse()?);
        }
        Ok(headers)
    }

    async fn refusal(headers: &HeaderMap, tags: &TailnetTags) -> String {
        match tailscale(headers, Some(tags)).await {
            Ok(caller) => format!("admitted {caller:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// A tagged node carrying an allowed tag is named by that tag, as name and email both, and
    /// the local API is asked with the host name tailscaled requires.
    #[tokio::test]
    async fn an_allowed_tag_names_the_caller() -> anyhow::Result<()> {
        let api = FakeLocalApi::start(
            "allowed",
            &[
                ("100.64.0.7", Some(&["tag:other", "tag:ctx-ingest"])),
                ("fd7a:115c:a1e0::7", Some(&["tag:ctx-legion"])),
            ],
        )?;
        let tags = allow(&api.socket)?;
        let caller = tailscale(&forwarded(&["100.64.0.7"])?, Some(&tags)).await?;
        assert_eq!(
            (caller.name.as_str(), caller.email.as_str(), caller.via),
            ("tag:ctx-ingest", "tag:ctx-ingest", Via::TailscaleTag)
        );
        let caller = tailscale(&forwarded(&["fd7a:115c:a1e0::7"])?, Some(&tags)).await?;
        assert_eq!(caller.email, "tag:ctx-legion");
        assert_eq!(
            api.asked().first().map(String::as_str),
            Some("local-tailscaled.sock /localapi/v0/whois?addr=100.64.0.7")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_node_whose_tags_are_not_allowed_is_refused() -> anyhow::Result<()> {
        let api = FakeLocalApi::start("not-allowed", &[("100.64.0.8", Some(&["tag:other"]))])?;
        let tags = allow(&api.socket)?;
        assert_eq!(
            refusal(&forwarded(&["100.64.0.8"])?, &tags).await,
            "the node at 100.64.0.8 carries tag:other, and --tailnet-allow-tag allows none of them"
        );
        Ok(())
    }

    /// A user device without the login header is no tagged node: Serve always sends the header
    /// for one, so its absence there means something is wrong, and nobody is admitted.
    #[tokio::test]
    async fn an_untagged_node_without_a_login_header_is_refused() -> anyhow::Result<()> {
        let api = FakeLocalApi::start(
            "untagged",
            &[("100.64.0.9", None), ("100.64.0.10", Some(&[]))],
        )?;
        let tags = allow(&api.socket)?;
        for address in ["100.64.0.9", "100.64.0.10"] {
            assert_eq!(
                refusal(&forwarded(&[address])?, &tags).await,
                format!(
                    "no {TAILSCALE_LOGIN_HEADER} header, and the node at {address} carries no tag"
                )
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn an_unknown_address_is_refused() -> anyhow::Result<()> {
        let api = FakeLocalApi::start("unknown", &[])?;
        let tags = allow(&api.socket)?;
        assert_eq!(
            refusal(&forwarded(&["100.64.0.11"])?, &tags).await,
            "tailscaled does not name a node at 100.64.0.11 (status 404 Not Found)"
        );
        Ok(())
    }

    /// Serve writes exactly one address; anything else did not come from Serve.
    #[tokio::test]
    async fn a_missing_or_repeated_forwarded_for_is_refused() -> anyhow::Result<()> {
        let api = FakeLocalApi::start("forwarded", &[("100.64.0.7", Some(&["tag:ctx-ingest"]))])?;
        let tags = allow(&api.socket)?;
        assert_eq!(
            refusal(&HeaderMap::new(), &tags).await,
            format!("no {TAILSCALE_LOGIN_HEADER} header, and no {FORWARDED_FOR_HEADER} header")
        );
        assert_eq!(
            refusal(&forwarded(&["100.64.0.7", "100.64.0.7"])?, &tags).await,
            format!("more than one {FORWARDED_FOR_HEADER} header")
        );
        for value in ["100.64.0.7, 100.64.0.8", "", "ctx-ingest", "100.64.0.7:443"] {
            assert_eq!(
                refusal(&forwarded(&[value])?, &tags).await,
                format!("the {FORWARDED_FOR_HEADER} header is not exactly one IP address"),
                "{value:?}"
            );
        }
        assert!(api.asked().is_empty(), "{:?}", api.asked());
        Ok(())
    }

    #[tokio::test]
    async fn an_absent_socket_is_refused() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("ctx-localapi-absent-{}", std::process::id()));
        let tags = allow(&dir.join("tailscaled.sock"))?;
        let refused = refusal(&forwarded(&["100.64.0.7"])?, &tags).await;
        assert!(
            refused == "tailscaled's local API did not say who 100.64.0.7 is",
            "{refused}"
        );
        Ok(())
    }

    /// The login header names a signed-in user wherever it is present, and the local API is
    /// never asked about that request.
    #[tokio::test]
    async fn a_login_header_wins_and_asks_nobody() -> anyhow::Result<()> {
        let api = FakeLocalApi::start("login", &[("100.64.0.7", Some(&["tag:ctx-ingest"]))])?;
        let tags = allow(&api.socket)?;
        let mut headers = forwarded(&["100.64.0.7"])?;
        headers.insert(TAILSCALE_LOGIN_HEADER, "ada@example.com".parse()?);
        let caller = tailscale(&headers, Some(&tags)).await?;
        assert_eq!(
            (caller.email.as_str(), caller.via),
            ("ada@example.com", Via::Tailscale)
        );
        headers.insert(TAILSCALE_LOGIN_HEADER, "".parse()?);
        assert_eq!(
            refusal(&headers, &tags).await,
            format!("the {TAILSCALE_LOGIN_HEADER} header is empty")
        );
        assert!(api.asked().is_empty(), "{:?}", api.asked());
        Ok(())
    }

    #[test]
    fn a_tag_is_tag_and_a_name() {
        for good in ["tag:ctx-ingest", " tag:ctx-legion ", "tag:a1"] {
            assert_eq!(tag(good).ok(), Some(good.trim().to_owned()), "{good}");
        }
        for bad in [
            "ctx-ingest",
            "tag:",
            "tag:-x",
            "tag:1x",
            "tag:a b",
            "tag:a,b",
            "TAG:x",
        ] {
            assert!(tag(bad).is_err(), "{bad}");
        }
        assert_eq!(
            tags(&["tag:a".to_owned(), "tag:a".to_owned(), "tag:b".to_owned()]).ok(),
            Some(vec!["tag:a".to_owned(), "tag:b".to_owned()])
        );
        for blank in ["", "  "] {
            assert!(
                tags(&["tag:a".to_owned(), blank.to_owned()]).is_err(),
                "{blank:?}"
            );
        }
    }

    /// A context source's callers may name a tagged node by its tag; the ingress list may not,
    /// since no Google token is ever minted for a tag.
    #[test]
    fn callers_take_tags_and_the_ingress_list_does_not() -> anyhow::Result<()> {
        let callers = Allowed::callers(&["tag:ctx-ingest".to_owned(), "@example.com".to_owned()])?;
        assert!(callers.admits("tag:ctx-ingest"));
        assert!(callers.admits("ada@example.com"));
        assert!(!callers.admits("tag:ctx-legion"));
        assert!(Allowed::callers(&["tag:".to_owned()]).is_err());
        assert!(Allowed::parse(&["tag:ctx-ingest".to_owned()]).is_err());
        let ingress = Allowed::parse(&["@example.com".to_owned()])?;
        assert!(!ingress.admits("tag:ctx-ingest"));
        Ok(())
    }
}
