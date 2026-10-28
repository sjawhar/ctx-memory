//! The sjawhar-agent GitHub App: the only way this program touches GitHub.
//!
//! The app's key signs a short-lived JWT, and the JWT buys an installation token limited to
//! the repositories and permissions the caller asks for. The server asks for one repository
//! with contents write, so a token that leaks can write only the graph repository.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client;
use crate::credentials::{self, Secret};

/// The default GitHub API endpoint.
const API: &str = "https://api.github.com";

/// How long the app JWT is valid. GitHub refuses more than ten minutes.
const JWT_LIFETIME: u64 = 540;

/// The shape of the `github_app` credential.
#[derive(Debug, Deserialize)]
struct AppCredential {
    app_id: String,
    private_key: String,
}

/// How long before its stated end an installation token is treated as spent. A call that
/// starts inside the margin can still be running when GitHub stops accepting the token.
const TOKEN_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// One installation token, with the moment it stops working.
#[derive(Clone, Debug, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: String,
}

impl InstallationToken {
    /// Whether this token is worth starting a call with.
    ///
    /// GitHub expires an installation token an hour after it is minted, so a token held for
    /// the life of a process stops working part-way through a long run. A token whose stated
    /// end cannot be read is treated as spent, which costs one mint and never a failed call.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.expires_at
            .parse::<DateTime<Utc>>()
            .is_ok_and(|end| end - Utc::now() > TOKEN_MARGIN)
    }
}

/// What an installation token may do.
#[derive(Debug, Serialize)]
pub struct TokenRequest {
    /// Repository names, without the owner.
    pub repositories: Vec<String>,
    /// Permission name to level, as GitHub names them.
    pub permissions: BTreeMap<String, String>,
}

impl TokenRequest {
    /// A token that may read and write the contents of one repository, and nothing else.
    #[must_use]
    pub fn contents_write(repository: &str) -> Self {
        Self::contents(repository, "write")
    }

    /// A token that may read the contents of one repository, and nothing else. Cloning,
    /// pulling and fetching need no more than this.
    #[must_use]
    pub fn contents_read(repository: &str) -> Self {
        Self::contents(repository, "read")
    }

    fn contents(repository: &str, level: &str) -> Self {
        let permissions = BTreeMap::from([
            ("contents".to_owned(), level.to_owned()),
            ("metadata".to_owned(), "read".to_owned()),
        ]);
        Self {
            repositories: vec![repository.to_owned()],
            permissions,
        }
    }

    /// A token that may only read, which the fetch job uses.
    #[must_use]
    pub fn read_only() -> Self {
        let mut permissions = BTreeMap::new();
        for name in ["metadata", "contents", "issues", "pull_requests"] {
            permissions.insert(name.to_owned(), "read".to_owned());
        }
        Self {
            repositories: Vec::new(),
            permissions,
        }
    }
}

/// GitHub refused a request to protect a rate limit, rather than failing it.
///
/// It travels as context on the error, so a caller that can come back later tells a refusal of
/// its pace from a refusal of the request itself.
#[derive(Debug)]
pub struct Throttled;

impl std::fmt::Display for Throttled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitHub refused the request to protect a rate limit")
    }
}

impl std::error::Error for Throttled {}

/// The GitHub App this program acts as.
pub struct App {
    identifier: String,
    key: EncodingKey,
    client: reqwest::blocking::Client,
}

impl App {
    /// Loads the app from the `github_app` credential.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential is missing, is not `{app_id, private_key}`, or
    /// carries a key that is not a PEM private key.
    pub fn load(config: &credentials::Config) -> Result<Self> {
        Self::from_secret(&credentials::load_with(config, "github_app")?)
    }

    /// Builds the app from a credential value.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not `{app_id, private_key}` with a PEM key.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        let credential: AppCredential = secret.parse()?;
        let key = EncodingKey::from_rsa_pem(credential.private_key.as_bytes())
            .context("reading the GitHub App private key")?;
        Ok(Self {
            identifier: credential.app_id,
            key,
            client: credentials::client()?,
        })
    }

    /// Signs the JWT that proves this program is the app.
    ///
    /// # Errors
    ///
    /// Returns an error when the clock cannot be read or the key cannot sign.
    pub fn jwt(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Claims<'a> {
            iat: u64,
            exp: u64,
            iss: &'a str,
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("reading the clock")?
            .as_secs();
        encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                // A minute of leeway, as GitHub's own documentation advises.
                iat: now.saturating_sub(60),
                exp: now + JWT_LIFETIME,
                iss: &self.identifier,
            },
            &self.key,
        )
        .context("signing the GitHub App token")
    }

    /// The installation that covers one repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the app is not installed on the repository or GitHub refuses.
    pub fn installation_for(&self, owner: &str, repository: &str) -> Result<u64> {
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        let url = format!("{}/repos/{owner}/{repository}/installation", base());
        let response = self
            .get(&url)
            .send()
            .with_context(|| format!("asking for the installation on {owner}/{repository}"))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().unwrap_or_default();
            bail!("{owner}/{repository} has no installation: {status} {detail}");
        }
        let installation: Installation = response
            .json()
            .context("decoding the installation answer")?;
        Ok(installation.id)
    }

    /// Mints an installation token with exactly the repositories and permissions asked for.
    ///
    /// # Errors
    ///
    /// Returns an error when GitHub refuses to mint the token.
    pub fn installation_token(
        &self,
        installation: u64,
        request: &TokenRequest,
    ) -> Result<InstallationToken> {
        let url = format!("{}/app/installations/{installation}/access_tokens", base());
        let mut body = serde_json::Map::new();
        if !request.repositories.is_empty() {
            body.insert(
                "repositories".to_owned(),
                serde_json::to_value(&request.repositories)?,
            );
        }
        body.insert(
            "permissions".to_owned(),
            serde_json::to_value(&request.permissions)?,
        );
        let response = self
            .post(&url, &serde_json::Value::Object(body))
            .send()
            .with_context(|| format!("minting a token for installation {installation}"))?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let detail = response.text().unwrap_or_default();
            let refusal = anyhow!(
                "minting a token for installation {installation} failed: {status} {detail}"
            );
            return Err(if throttled(status, &headers) {
                refusal.context(Throttled)
            } else {
                refusal
            });
        }
        response
            .json()
            .context("decoding the installation token answer")
    }

    fn get(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        self.headers(self.client.get(url))
    }

    fn post(&self, url: &str, body: &serde_json::Value) -> reqwest::blocking::RequestBuilder {
        self.headers(self.client.post(url)).json(body)
    }

    fn headers(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        let jwt = self.jwt().unwrap_or_default();
        request
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "ctx-memory")
            .timeout(Duration::from_secs(30))
    }
}

/// One repository the app may read, with the branch its commits follow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
}

/// What one request to a GitHub listing came back as.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PageKind {
    /// GitHub answered with this page of the listing.
    #[default]
    Items,
    /// GitHub refused the request to protect a rate limit, so the listing is unfinished and
    /// has to be read again later.
    Throttled,
    /// `404 Not Found`. Most callers treat it as an error; a comparison of two commits answers
    /// it when they share no history, which its caller handles.
    Missing,
}

/// One page of a GitHub list endpoint, with what the next request needs.
#[derive(Clone, Debug, Default)]
pub struct Page {
    /// The items the page carried; empty unless GitHub answered with items.
    pub items: Vec<Value>,
    /// `X-RateLimit-Remaining`, so a caller stops before the shared quota runs out.
    pub rate_remaining: Option<u32>,
    /// `X-RateLimit-Resource`: the limit `rate_remaining` counts, such as `core` for REST.
    pub rate_resource: Option<String>,
    /// The `rel="next"` URL from the `Link` header, when the listing has more pages.
    pub next: Option<String>,
    /// The `rel="last"` URL from the `Link` header, which a listing paged by page number names.
    pub last: Option<String>,
    /// A comparison's `total_commits`: how many commits it holds, which its pages may not all
    /// list.
    pub total: Option<u64>,
    /// What GitHub answered: the page, or a refusal.
    pub kind: PageKind,
}

/// The sjawhar-agent app as a reader: it mints a read-only token for each installation and
/// reads the repositories that installation covers. One token is minted per installation and
/// reused until it is nearly expired, so a fetch that runs longer than a token's hour mints a
/// fresh one rather than failing on a stale one.
pub struct GithubClient {
    app: App,
    tokens: RefCell<BTreeMap<u64, InstallationToken>>,
}

impl GithubClient {
    /// Loads the client from the `github_app` credential.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential is missing or is not an app key.
    pub fn load(config: &credentials::Config) -> Result<Self> {
        Ok(Self::from_app(App::load(config)?))
    }

    /// Builds the client from an already loaded app.
    #[must_use]
    pub fn from_app(app: App) -> Self {
        Self {
            app,
            tokens: RefCell::new(BTreeMap::new()),
        }
    }

    /// The read-only token for one installation, minted when there is no usable one.
    ///
    /// # Errors
    ///
    /// Returns an error when GitHub refuses to mint the token.
    fn token(&self, installation: u64) -> Result<String> {
        if let Some(held) = self
            .tokens
            .borrow()
            .get(&installation)
            .filter(|held| held.usable())
        {
            return Ok(held.token.clone());
        }
        let minted = self
            .app
            .installation_token(installation, &TokenRequest::read_only())?;
        self.tokens
            .borrow_mut()
            .insert(installation, minted.clone());
        Ok(minted.token)
    }

    /// The repositories one installation covers, after minting its read-only token.
    ///
    /// `None` means GitHub refused the discovery to protect a rate limit, in the mint or in the
    /// listing: the installation is read by a later fetch, and nothing about it is known yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be minted or GitHub refuses the listing for any
    /// other reason.
    pub fn repositories(&self, installation: u64) -> Result<Option<Vec<Repo>>> {
        let token = match self.token(installation) {
            Ok(token) => token,
            Err(error) if error.downcast_ref::<Throttled>().is_some() => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut repositories = Vec::new();
        let mut url = Some(format!("{}/installation/repositories?per_page=100", base()));
        while let Some(current) = url {
            let (status, headers, body) =
                client::get_json_with_headers(&current, &token, &[], &api_headers()).with_context(
                    || format!("listing repositories for installation {installation}"),
                )?;
            if throttled(status, &headers) {
                return Ok(None);
            }
            if !status.is_success() {
                bail!(
                    "listing repositories for installation {installation} failed: {status} {}",
                    excerpt(&body)
                );
            }
            if let Some(items) = body.get("repositories").and_then(Value::as_array) {
                for item in items {
                    if let Some(repository) = parse_repo(item) {
                        repositories.push(repository);
                    }
                }
            }
            url = link(&headers, "next");
        }
        Ok(Some(repositories))
    }

    /// `GET url` with the installation's token.
    ///
    /// Three answers are not failures. `409 Conflict` — which an empty repository answers its
    /// commit listing with — returns an empty page, because a repository with no commits has
    /// nothing to read. A rate limit GitHub is protecting returns a throttled page, which the
    /// caller comes back for later. `404 Not Found` returns a missing page, which the caller
    /// decides about. The items are the answer's array, or the `commits` array of a comparison
    /// (`/compare/<base>...<head>`), which wraps them in an object.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be minted, the request fails, or GitHub refuses
    /// it with a status that is none of those.
    pub fn get_page(&self, installation: u64, url: &str) -> Result<Page> {
        let token = self.token(installation)?;
        let (status, headers, body) =
            client::get_json_with_headers(url, &token, &[], &api_headers())
                .with_context(|| format!("reading {url}"))?;
        let rated = Page {
            rate_remaining: rate_remaining(&headers),
            rate_resource: header_value(&headers, "x-ratelimit-resource"),
            ..Page::default()
        };
        if status == StatusCode::CONFLICT {
            // "Git Repository is empty." — the repository holds no commits yet, so the listing
            // is empty and the stream stays healthy.
            return Ok(rated);
        }
        if throttled(status, &headers) {
            // GitHub is protecting a rate limit, so the listing is unfinished rather than
            // broken.
            return Ok(Page {
                kind: PageKind::Throttled,
                ..rated
            });
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(Page {
                kind: PageKind::Missing,
                ..rated
            });
        }
        if !status.is_success() {
            bail!("reading {url} failed: {status} {}", excerpt(&body));
        }
        let items = body
            .as_array()
            .or_else(|| body.get("commits").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        Ok(Page {
            items,
            next: link(&headers, "next"),
            last: link(&headers, "last"),
            total: body.get("total_commits").and_then(Value::as_u64),
            kind: PageKind::Items,
            ..rated
        })
    }

    /// Posts one GraphQL request (`{"query": …, "variables": …}`) with the installation's
    /// token and returns GitHub's answer, which may carry `errors` beside `data`.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be minted, the request fails, or GitHub answers
    /// with a status other than `200`.
    pub fn graphql(&self, installation: u64, request: &Value) -> Result<Value> {
        let token = self.token(installation)?;
        let url = format!("{}/graphql", base());
        let (status, _, body) =
            client::post_json(&url, &token, request).with_context(|| format!("posting {url}"))?;
        if !status.is_success() {
            bail!("posting {url} failed: {status} {}", excerpt(&body));
        }
        Ok(body)
    }
}

/// The request headers every GitHub REST call carries.
fn api_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Accept", "application/vnd.github+json".to_owned()),
        ("X-GitHub-Api-Version", "2022-11-28".to_owned()),
    ]
}

/// Reads one response header as a string, when it is present and valid.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `X-RateLimit-Remaining`, parsed as a count.
fn rate_remaining(headers: &HeaderMap) -> Option<u32> {
    header_value(headers, "x-ratelimit-remaining").and_then(|value| value.parse().ok())
}

/// Whether GitHub refused this request to protect a rate limit, rather than failing it.
///
/// A spent primary rate limit answers `403` or `429` with `x-ratelimit-remaining: 0`, and a
/// secondary rate limit answers `403` or `429`, with `retry-after` when it says how long to
/// wait. `429` alone also stands for "try again", so every `429` counts. A `403` that still
/// has requests left and offers no retry is a refusal of the request itself, not of its pace.
fn throttled(status: StatusCode, headers: &HeaderMap) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if status != StatusCode::FORBIDDEN {
        return false;
    }
    rate_remaining(headers) == Some(0) || header_value(headers, "retry-after").is_some()
}

/// The URL a `Link` header names for `rel` (`next`, `last`), when it names one.
fn link(headers: &HeaderMap, rel: &str) -> Option<String> {
    let link = header_value(headers, "link")?;
    let wanted = format!("rel=\"{rel}\"");
    link.split(',').find_map(|part| {
        let mut fields = part.split(';');
        let url = fields.next()?.trim();
        if fields.any(|field| field.trim() == wanted) {
            Some(url.trim_start_matches('<').trim_end_matches('>').to_owned())
        } else {
            None
        }
    })
}

/// One repository of an installation listing, when it names an owner, repo, and branch.
fn parse_repo(item: &Value) -> Option<Repo> {
    let full_name = item.get("full_name").and_then(Value::as_str)?;
    let (owner, name) = full_name.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let default_branch = item
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or("main");
    Some(Repo {
        owner: owner.to_owned(),
        name: name.to_owned(),
        default_branch: default_branch.to_owned(),
    })
}

fn excerpt(body: &Value) -> String {
    let text = body
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| body.to_string(), str::to_owned);
    text.chars().take(200).collect()
}

/// The configured API host, or GitHub's own.
#[must_use]
pub fn api_base() -> String {
    base()
}

/// The API host a test may redirect with `CTX_GITHUB_API`.
fn base() -> String {
    std::env::var("CTX_GITHUB_API").unwrap_or_else(|_| API.to_owned())
}

/// The owner and repository of a GitHub remote URL, in either form git writes.
#[must_use]
pub fn repository_of(url: &str) -> Option<(String, String)> {
    let redirected = redirected_host();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| {
            redirected
                .as_deref()
                .and_then(|host| url.strip_prefix(host))
        })?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repository) = rest.split_once('/')?;
    if owner.is_empty() || repository.is_empty() {
        return None;
    }
    Some((owner.to_owned(), repository.to_owned()))
}

/// Where `CTX_GITHUB_API` points, as a remote-URL prefix.
///
/// A test runs GitHub's API and its git remotes on one loopback server, so a remote on that
/// host is the same GitHub this program mints tokens from. Nothing in production sets the
/// variable, and GitHub's own host needs no redirection.
fn redirected_host() -> Option<String> {
    let base = std::env::var("CTX_GITHUB_API").ok()?;
    Some(format!("{}/", base.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use reqwest::StatusCode;

    use super::{repository_of, throttled, TokenRequest};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        map
    }

    #[test]
    fn a_spent_rate_limit_is_a_refusal_not_a_failure() {
        // A spent primary rate limit answers `403` or `429` with `x-ratelimit-remaining: 0`, and
        // a secondary rate limit answers `403` or `429` with `retry-after`
        // (docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api, read
        // 2026-10-27). GitHub also answers `429` under load on the commits listing.
        assert!(throttled(StatusCode::TOO_MANY_REQUESTS, &headers(&[])));
        assert!(throttled(
            StatusCode::FORBIDDEN,
            &headers(&[("x-ratelimit-remaining", "0")])
        ));
        assert!(throttled(
            StatusCode::FORBIDDEN,
            &headers(&[("retry-after", "60")])
        ));
        // A permission refusal keeps its remaining requests and offers no retry: a real error.
        assert!(!throttled(
            StatusCode::FORBIDDEN,
            &headers(&[("x-ratelimit-remaining", "4999")])
        ));
        assert!(!throttled(
            StatusCode::NOT_FOUND,
            &headers(&[("x-ratelimit-remaining", "0")])
        ));
    }

    #[test]
    fn a_write_token_asks_for_one_repository_and_two_permissions() {
        let request = TokenRequest::contents_write("memory");
        assert_eq!(request.repositories, ["memory"]);
        assert_eq!(
            request.permissions.get("contents").map(String::as_str),
            Some("write")
        );
        assert_eq!(
            request.permissions.get("metadata").map(String::as_str),
            Some("read")
        );
        assert_eq!(request.permissions.len(), 2);
    }

    #[test]
    fn a_read_token_asks_for_four_read_permissions() {
        let request = TokenRequest::read_only();
        assert_eq!(request.repositories, Vec::<String>::new());
        assert_eq!(request.permissions.len(), 4);
        assert!(request
            .permissions
            .values()
            .all(|level| level.as_str() == "read"));
    }

    #[test]
    fn every_github_remote_form_names_the_repository() {
        for url in [
            "https://github.com/other-org/memory.git",
            "https://github.com/other-org/memory",
            "git@github.com:other-org/memory.git",
            "ssh://git@github.com/other-org/memory.git",
        ] {
            assert_eq!(
                repository_of(url),
                Some(("other-org".to_owned(), "memory".to_owned())),
                "{url}"
            );
        }
        assert_eq!(repository_of("/tmp/remote.git"), None);
    }
}
