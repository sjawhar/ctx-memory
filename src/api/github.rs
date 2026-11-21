//! The deployment's GitHub App: the only way this program touches GitHub.
//!
//! This program acts with the app's installation tokens, which it gets in one of two ways. With
//! the app's key, the key signs a short-lived JWT, and the JWT buys an installation token limited
//! to the repositories and permissions the caller asks for. The server asks for one repository
//! with contents write, so a token that leaks can write only the graph repository. With a token
//! secret (`CTX_SECRET_GITHUB_TOKENS`), a minter outside this program holds the key and writes
//! each installation's read-only token to a Secret Manager secret, which this program only reads.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::api::client::{self, CutShort, JsonReply, NoAnswer};
use crate::credentials::{self, Secret};
use crate::redact::one_line;

/// The default GitHub API endpoint.
const API: &str = "https://api.github.com";

/// The name the app's credential is loaded by: `CTX_SECRET_GITHUB_APP`,
/// `CTX_CREDENTIAL_GITHUB_APP_FILE` or `CTX_CREDENTIAL_GITHUB_APP` names its source, or a
/// `[credentials]` entry does.
pub const CREDENTIAL: &str = "github_app";

/// The name the token secret is loaded by: `CTX_SECRET_GITHUB_TOKENS` names it.
const TOKENS_CREDENTIAL: &str = "github_tokens";

/// The variable that names the token secret, and so puts `ctx` in token-secret mode.
const TOKENS_ENV: &str = "CTX_SECRET_GITHUB_TOKENS";

/// The variables that name an app key, which token-secret mode refuses beside it: a process
/// that holds both would read with the minter's read-only tokens and leave the key unused. A
/// `github_app` entry under `[credentials]` in the tree's `ctx.toml` is not among them: it is
/// read only once a tree is loaded, and token-secret mode reads no `github_app` source.
const APP_ENVS: [&str; 3] = [
    "CTX_SECRET_GITHUB_APP",
    "CTX_CREDENTIAL_GITHUB_APP_FILE",
    "CTX_CREDENTIAL_GITHUB_APP",
];

/// Whether `ctx` reads GitHub in token-secret mode: with the installation tokens a minter writes
/// to the secret `CTX_SECRET_GITHUB_TOKENS` names, rather than with tokens it mints from the
/// `github_app` credential. A variable that is set counts even when it is empty, as in
/// [`credentials::configured`].
///
/// # Errors
///
/// Returns an error naming both variables when `CTX_SECRET_GITHUB_TOKENS` is set beside a
/// variable that names an app key (`CTX_SECRET_GITHUB_APP`, `CTX_CREDENTIAL_GITHUB_APP_FILE` or
/// `CTX_CREDENTIAL_GITHUB_APP`): such a deployment has not said which of the two it reads with.
/// Returns an error naming `CTX_SECRET_GITHUB_TOKENS` when it is set but empty, so names no
/// secret.
pub fn reads_a_token_secret() -> Result<bool> {
    let Some(tokens) = std::env::var_os(TOKENS_ENV) else {
        return Ok(false);
    };
    if let Some(app) = APP_ENVS
        .iter()
        .find(|name| std::env::var_os(name).is_some())
    {
        bail!(
            "{TOKENS_ENV} and {app} are both set: ctx reads GitHub either with the installation tokens of the secret {TOKENS_ENV} names or with tokens it mints from the app key {app} names, so unset one of them"
        );
    }
    if tokens.is_empty() {
        bail!(
            "{TOKENS_ENV} is set but empty: name the Secret Manager secret the GitHub tokens are in, projects/<project>/secrets/<secret>, or unset it"
        );
    }
    Ok(true)
}

/// One installation's entry in the token secret. Other fields are ignored.
#[derive(Deserialize)]
struct TokenEntry {
    /// The login of the account the installation is on.
    owner: String,
    /// The installation's token.
    token: String,
}

/// The shape of the token secret, as its refusal names it.
const TOKEN_SECRET_SHAPE: &str =
    "an object from each installation id, a string of digits, to {owner, token}";

/// What one read of the token secret holds: each installation's token, beside the login of the
/// account it is installed on, as the minter writes them. It has no `Debug`, which would print
/// the tokens.
pub struct InstallationTokens(BTreeMap<u64, TokenEntry>);

impl InstallationTokens {
    /// Reads the newest version of the secret `CTX_SECRET_GITHUB_TOKENS` names.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret cannot be read or is not the minter's form.
    pub fn read() -> Result<Self> {
        credentials::load(TOKENS_CREDENTIAL)
            .and_then(|secret| Self::from_secret(&secret))
            .context("reading the GitHub token secret")
    }

    /// Reads a token secret's value: one JSON object from each installation id to an object of
    /// two strings, `owner` and `token`. An entry whose token is empty holds no token, so no
    /// request carries an empty one and [`for_installation`](Self::for_installation) and
    /// [`for_owner`](Self::for_owner) refuse it by its installation or owner, as they refuse an
    /// entry that is not there.
    ///
    /// # Errors
    ///
    /// Returns an error, which carries none of the value, when the value is not of that form.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        let mut entries: BTreeMap<u64, TokenEntry> = secret.parse(TOKEN_SECRET_SHAPE)?;
        // An empty token would also reach `git::Credential::redact` as an empty pattern, which
        // `str::replace` matches between every character of git's error.
        entries.retain(|_, entry| !entry.token.is_empty());
        Ok(Self(entries))
    }

    /// The token of one installation.
    ///
    /// # Errors
    ///
    /// Returns an error naming the installation when the secret holds no token for it.
    pub fn for_installation(&self, installation: u64) -> Result<&str> {
        self.0
            .get(&installation)
            .map(|entry| entry.token.as_str())
            .ok_or_else(|| {
                anyhow!("the GitHub token secret holds no token for installation {installation}")
            })
    }

    /// The token of the installation on `owner`'s account. GitHub logins are case-insensitive,
    /// so the owner a remote URL names matches the minter's login in either case.
    ///
    /// # Errors
    ///
    /// Returns an error naming the owner when no entry on that account holds a token.
    pub fn for_owner(&self, owner: &str) -> Result<&str> {
        self.0
            .values()
            .find(|entry| entry.owner.eq_ignore_ascii_case(owner))
            .map(|entry| entry.token.as_str())
            .ok_or_else(|| {
                anyhow!("the GitHub token secret holds no token of an installation on {owner}")
            })
    }
}

/// How long the app JWT is valid. GitHub refuses more than ten minutes.
const JWT_LIFETIME: u64 = 540;

/// How many times a read that gets no answer is sent in all: once, and twice more.
const READ_ATTEMPTS: u32 = 3;

/// The pause before a read that got no answer is sent again. The network that dropped it, or a
/// connection GitHub closed, is replaced by a fresh connection at the next try; the pause only
/// keeps the tries out of the same moment of trouble, and two of them cost a run of a few
/// minutes nothing.
const READ_RETRY_PAUSE: Duration = Duration::from_secs(2);

/// The shape of the `github_app` credential. `app_id` is a number or a string of digits, and
/// other fields, such as the `client_id` GitHub shows beside the id, are ignored.
#[derive(Debug, Deserialize)]
struct AppCredential {
    #[serde(deserialize_with = "app_id_text")]
    app_id: String,
    private_key: String,
}

/// The shape of the `github_app` credential, as its refusal names it.
const APP_CREDENTIAL_SHAPE: &str =
    "{app_id, private_key}, where app_id is a number or a string of digits";

/// Why an `app_id` was refused. The loader's refusal never shows serde's message
/// ([`credentials::json_refusal`]), and this text names no part of the value either: a
/// credential whose fields were swapped carries the private key in `app_id`.
const APP_ID_REFUSED: &str =
    "app_id must be a GitHub App id: a non-negative whole number, or a string of the digits 0 to 9";

/// Reads an `app_id` as the text the JWT names the app by.
///
/// GitHub shows the id as a number, and a credential written by hand may quote it. A whole
/// number that is not negative becomes its decimal text, and a string of the ASCII digits 0 to
/// 9 is kept as written, leading zeros included. Every other value is refused.
fn app_id_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct AppId;

    impl de::Visitor<'_> for AppId {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a GitHub App id")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<String, E> {
            let value = u64::try_from(value).map_err(|_| E::custom(APP_ID_REFUSED))?;
            Ok(value.to_string())
        }

        fn visit_f64<E: de::Error>(self, _value: f64) -> Result<String, E> {
            Err(E::custom(APP_ID_REFUSED))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            if !value.is_empty() && value.as_bytes().iter().all(u8::is_ascii_digit) {
                Ok(value.to_owned())
            } else {
                Err(E::custom(APP_ID_REFUSED))
            }
        }
    }

    deserializer.deserialize_any(AppId)
}

/// How long before its stated end an installation token is treated as spent. A call that
/// starts inside the margin can still be running when GitHub stops accepting the token.
const TOKEN_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// One installation token, with the moment it stops working. Its `Debug` shows only that
/// moment, never the token.
#[derive(Clone, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: String,
}

impl std::fmt::Debug for InstallationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstallationToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
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

/// GitHub refused a token from the token secret with `401`: it expired or was revoked before the
/// minter's newer version was read.
///
/// It travels as context on the error of a page read ([`GithubClient::get_page`]), so the
/// caller, which charges each read to its budget, decides whether to read the secret again
/// ([`GithubClient::read_tokens_again`]) and send the read once more.
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitHub refused the installation's token from the token secret")
    }
}

impl std::error::Error for Unauthorized {}

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
    /// Returns an error when the credential is missing, is not `{app_id, private_key}`, where
    /// `app_id` is a number or a string of digits and other fields are ignored, or carries a
    /// key that is not a PEM private key.
    pub fn load(config: &credentials::Config) -> Result<Self> {
        Self::from_secret(&credentials::load_with(config, CREDENTIAL)?)
    }

    /// Builds the app from a credential value.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not `{app_id, private_key}` with a PEM key, where
    /// `app_id` is a number or a string of digits and other fields are ignored.
    pub fn from_secret(secret: &Secret) -> Result<Self> {
        let credential: AppCredential = secret.parse(APP_CREDENTIAL_SHAPE)?;
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
        // The answer holds the token, so its refusal is built without serde's message.
        let text = response
            .text()
            .context("reading the installation token answer")?;
        serde_json::from_str(&text).map_err(|error| {
            anyhow!(
                "decoding the installation token answer: the answer is not the expected JSON ({})",
                credentials::json_refusal(&error, "{token, expires_at}")
            )
        })
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
    /// GitHub's numeric id of the repository, which a rename or a transfer does not change.
    pub id: u64,
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
    /// `422 Unprocessable Entity` saying the diff is not available: GitHub could not generate
    /// the diff of a comparison (`/compare/<base>...<head>`) in time, so it lists none of the
    /// comparison's commits. It answered so on every page of one comparison, and on every
    /// request of it for an hour, after a merge brought about 930 commits of another branch
    /// into a default branch (2026-10-31): the caller reads those commits another way.
    DiffUnavailable,
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

/// The GitHub App as a reader: it gets a read-only token for each installation and reads the
/// repositories that installation covers.
pub struct GithubClient {
    tokens: Tokens,
}

/// Where a [`GithubClient`] gets each installation's token.
enum Tokens {
    /// Minted from the app's key, one per installation, each reused until it is nearly expired,
    /// so a fetch that runs longer than a token's hour mints a fresh one rather than failing on a
    /// stale one.
    Minted {
        app: App,
        held: RefCell<BTreeMap<u64, InstallationToken>>,
    },
    /// Read from the token secret, whose newest read is kept until the secret is read again:
    /// before each installation's fetch, and when GitHub refuses a token with `401`.
    Secret(RefCell<Option<InstallationTokens>>),
}

impl GithubClient {
    /// Loads the client: from the token secret when `CTX_SECRET_GITHUB_TOKENS` names one, which
    /// is read only once the client needs a token, and otherwise from the `github_app`
    /// credential.
    ///
    /// # Errors
    ///
    /// Returns an error when an app key variable is set beside `CTX_SECRET_GITHUB_TOKENS`, or,
    /// with no token secret, when the credential is missing or is not an app key.
    pub fn load(config: &credentials::Config) -> Result<Self> {
        if reads_a_token_secret()? {
            return Ok(Self {
                tokens: Tokens::Secret(RefCell::new(None)),
            });
        }
        Ok(Self::from_app(App::load(config)?))
    }

    /// Builds the client from an already loaded app.
    #[must_use]
    pub fn from_app(app: App) -> Self {
        Self {
            tokens: Tokens::Minted {
                app,
                held: RefCell::new(BTreeMap::new()),
            },
        }
    }

    /// The read-only token for one installation: minted when there is no usable one, or taken
    /// from the token secret's newest read, which is made now when none was.
    ///
    /// # Errors
    ///
    /// Returns an error when GitHub refuses to mint the token, or when the token secret cannot be
    /// read or holds no token for the installation.
    fn token(&self, installation: u64) -> Result<String> {
        match &self.tokens {
            Tokens::Minted { app, held } => {
                if let Some(token) = held
                    .borrow()
                    .get(&installation)
                    .filter(|token| token.usable())
                {
                    return Ok(token.token.clone());
                }
                let minted = app.installation_token(installation, &TokenRequest::read_only())?;
                held.borrow_mut().insert(installation, minted.clone());
                Ok(minted.token)
            }
            Tokens::Secret(newest) => {
                let mut newest = newest.borrow_mut();
                let tokens = match &*newest {
                    Some(tokens) => tokens,
                    None => newest.insert(InstallationTokens::read()?),
                };
                tokens.for_installation(installation).map(str::to_owned)
            }
        }
    }

    /// Reads the token secret again and keeps what it holds. With an app key there is no secret,
    /// and this does nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret cannot be read or is not the minter's form.
    pub fn read_tokens_again(&self) -> Result<()> {
        if let Tokens::Secret(newest) = &self.tokens {
            *newest.borrow_mut() = Some(InstallationTokens::read()?);
        }
        Ok(())
    }

    /// Whether `reply` is GitHub refusing a token from the token secret with `401`, its body
    /// whole or cut short. A minted token's `401` is not: minting again gives no newer token.
    fn refused_a_secret_token(&self, reply: &Result<JsonReply>) -> bool {
        let status = match reply {
            Ok((status, _, _)) => *status,
            Err(error) => match error.downcast_ref::<CutShort>() {
                Some(cut) => cut.status,
                None => return false,
            },
        };
        status == StatusCode::UNAUTHORIZED && matches!(self.tokens, Tokens::Secret(_))
    }

    /// Sends one request with `token`, the installation's, by `send`. When GitHub refuses a token
    /// from the token secret with `401` ([`Unauthorized`]), it reads the secret again, puts the
    /// installation's newest token in `token`, and sends the request once more with it, whose
    /// answer stands. The repository listing and the GraphQL pre-pass, which the request budget
    /// does not cover, send this way; a page read leaves the second try to its caller
    /// ([`GithubClient::get_page`]).
    ///
    /// # Errors
    ///
    /// Returns an error when `send` fails, or the secret cannot be read again or holds no token
    /// for the installation.
    fn send_with_token(
        &self,
        installation: u64,
        token: &mut String,
        send: impl Fn(&str) -> Result<JsonReply>,
    ) -> Result<JsonReply> {
        let reply = send(token);
        if !self.refused_a_secret_token(&reply) {
            return reply;
        }
        *token = self
            .read_tokens_again()
            .and_then(|()| self.token(installation))
            .with_context(|| {
                format!("GitHub refused the token of installation {installation} with 401")
            })?;
        send(token)
    }

    /// The repositories one installation covers, after getting its read-only token. With a token
    /// secret, each installation's fetch starts here, so the secret is read again first: a token
    /// read for an earlier installation of a long run is not the one this installation reads
    /// with.
    ///
    /// `None` means GitHub refused the discovery to protect a rate limit, in the mint or in the
    /// listing: the installation is read by a later fetch, and nothing about it is known yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be minted or read, or GitHub refuses the listing
    /// for any other reason.
    pub fn repositories(&self, installation: u64) -> Result<Option<Vec<Repo>>> {
        self.read_tokens_again()?;
        let mut token = match self.token(installation) {
            Ok(token) => token,
            Err(error) if error.downcast_ref::<Throttled>().is_some() => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut repositories = Vec::new();
        let mut url = Some(format!("{}/installation/repositories?per_page=100", base()));
        while let Some(current) = url {
            // The caller names the installation, so the error says it once.
            let mut attempt = 1;
            let (status, headers, body) = loop {
                let listed = self.send_with_token(installation, &mut token, |token| {
                    client::get_json_with_headers(&current, token, &[], &api_headers())
                });
                match listed {
                    Ok(reply) => break reply,
                    Err(error) if cut_short_throttle(&error).is_some() => return Ok(None),
                    Err(error) => retry_after(error, attempt)?,
                }
                attempt += 1;
            };
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
    /// Four answers are not failures. `409 Conflict` — which an empty repository answers its
    /// commit listing with — returns an empty page, because a repository with no commits has
    /// nothing to read. A rate limit GitHub is protecting returns a throttled page, which the
    /// caller comes back for later. `404 Not Found` returns a missing page, and a comparison
    /// whose diff GitHub cannot generate returns a page saying so; the caller decides about
    /// both. The items are the answer's array, or the `commits` array of a comparison
    /// (`/compare/<base>...<head>`), which wraps them in an object.
    ///
    /// It sends the request once: a caller that may send it again after it got no answer
    /// ([`retry_after`]) counts each request it sends. With a token secret, a `401` is an error
    /// carrying [`Unauthorized`], after which the caller may read the secret again
    /// ([`GithubClient::read_tokens_again`]) and send the request once more.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be minted or read, the request fails, or GitHub
    /// refuses it with a status that is none of those.
    pub fn get_page(&self, installation: u64, url: &str) -> Result<Page> {
        let token = self.token(installation)?;
        let reply = client::get_json_with_headers(url, &token, &[], &api_headers());
        if self.refused_a_secret_token(&reply) {
            let refusal = match reply {
                Ok((status, _, body)) => {
                    anyhow!("GitHub answered {status} {}, to GET {url}", excerpt(&body))
                }
                Err(error) => error,
            };
            return Err(refusal.context(Unauthorized));
        }
        let (status, headers, body) = match reply {
            Ok(reply) => reply,
            Err(error) => {
                // A throttle whose body GitHub cut short is still a throttle.
                let Some(headers) = cut_short_throttle(&error) else {
                    return Err(error);
                };
                return Ok(Page {
                    rate_remaining: rate_remaining(&headers),
                    rate_resource: header_value(&headers, "x-ratelimit-resource"),
                    kind: PageKind::Throttled,
                    ..Page::default()
                });
            }
        };
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
        if status == StatusCode::UNPROCESSABLE_ENTITY && diff_unavailable(&body) {
            return Ok(Page {
                kind: PageKind::DiffUnavailable,
                ..rated
            });
        }
        if !status.is_success() {
            bail!("GitHub answered {status} {}, to GET {url}", excerpt(&body));
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
    /// Returns an error when the token cannot be minted or read, the request fails, or GitHub
    /// answers with a status other than `200`.
    pub fn graphql(&self, installation: u64, request: &Value) -> Result<Value> {
        let mut token = self.token(installation)?;
        let url = format!("{}/graphql", base());
        let (status, _, body) = self.send_with_token(installation, &mut token, |token| {
            client::post_json(&url, token, request)
        })?;
        if !status.is_success() {
            bail!("GitHub answered {status} {}, to POST {url}", excerpt(&body));
        }
        Ok(body)
    }
}

/// What to do after try `attempt` of a GitHub `GET` failed with `error`. A read that got no
/// answer at all — it could not connect, it timed out, the connection closed or reset before a
/// status — and has tries left is sent again: this logs the retry on one line of standard
/// error, pauses [`READ_RETRY_PAUSE`], and returns `Ok`. A `GET` is safe to repeat. Any other
/// error, an HTTP status among them, and a read out of its [`READ_ATTEMPTS`] tries, comes back
/// as the error to fail with.
///
/// # Errors
///
/// Returns the error to fail with when the read is not to be sent again.
pub fn retry_after(error: anyhow::Error, attempt: u32) -> Result<()> {
    if error.downcast_ref::<NoAnswer>().is_none() {
        return Err(error);
    }
    if attempt >= READ_ATTEMPTS {
        return Err(error.context(format!("GitHub gave no answer in {attempt} tries")));
    }
    eprintln!(
        "{}",
        one_line(&format!(
            "github: try {attempt} of {READ_ATTEMPTS} got no answer, trying again in {} s: {error:#}",
            READ_RETRY_PAUSE.as_secs()
        ))
    );
    std::thread::sleep(READ_RETRY_PAUSE);
    Ok(())
}

/// The headers of a refusal whose body GitHub cut short, when it refused to protect a rate
/// limit.
fn cut_short_throttle(error: &anyhow::Error) -> Option<HeaderMap> {
    error
        .downcast_ref::<CutShort>()
        .filter(|cut| throttled(cut.status, &cut.headers))
        .map(|cut| cut.headers.clone())
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

/// Whether a `422` answer says a comparison's diff is not available. GitHub answers a
/// comparison whose diff it cannot generate in time with the message "Server Error: Sorry, this
/// diff is taking too long to generate." and one error naming the field `diff` and the code
/// `not_available`, on resource `Comparison` (read 2026-10-31).
fn diff_unavailable(body: &Value) -> bool {
    body.get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| {
            errors.iter().any(|error| {
                error.get("field").and_then(Value::as_str) == Some("diff")
                    && error.get("code").and_then(Value::as_str) == Some("not_available")
            })
        })
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

/// One repository of an installation listing, when it names an id, an owner and a repo.
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
        id: item.get("id").and_then(Value::as_u64)?,
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

    use super::{
        diff_unavailable, parse_repo, repository_of, throttled, InstallationTokens, Repo,
        TokenRequest,
    };
    use crate::credentials::Secret;

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
    fn a_comparison_whose_diff_github_cannot_generate_is_told_apart() {
        // GitHub's answer to such a comparison, every page of it (read 2026-10-31).
        let refused = serde_json::json!({
            "message": "Server Error: Sorry, this diff is taking too long to generate.",
            "errors": [{"resource": "Comparison", "field": "diff", "code": "not_available"}],
            "status": "422"
        });
        assert!(diff_unavailable(&refused));
        // Any other `422` is a refusal of the request itself.
        let invalid = serde_json::json!({
            "message": "Validation Failed",
            "errors": [{"resource": "Issue", "field": "title", "code": "missing_field"}]
        });
        assert!(!diff_unavailable(&invalid));
        assert!(!diff_unavailable(
            &serde_json::json!({"message": "Unprocessable"})
        ));
    }

    /// A listed repository carries GitHub's id of it, by which a deployment excludes it whatever
    /// it is called now. An item without one is not read: it could be a repository the
    /// deployment excludes. Control: the same item with its id is read.
    #[test]
    fn a_listed_repository_is_read_with_its_id_and_never_without_one() {
        let listed = serde_json::json!({
            "id": 9_000_000_001_u64,
            "full_name": "example-org/renamed",
            "default_branch": "trunk"
        });
        assert_eq!(
            parse_repo(&listed),
            Some(Repo {
                id: 9_000_000_001,
                owner: "example-org".to_owned(),
                name: "renamed".to_owned(),
                default_branch: "trunk".to_owned(),
            })
        );
        let mut without_id = listed;
        if let Some(fields) = without_id.as_object_mut() {
            fields.remove("id");
        }
        assert_eq!(parse_repo(&without_id), None);
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

    fn token_secret(value: &serde_json::Value) -> anyhow::Result<InstallationTokens> {
        InstallationTokens::from_secret(&Secret::new(value.to_string()))
    }

    #[test]
    fn a_token_secret_gives_each_installation_and_each_owner_its_token() -> anyhow::Result<()> {
        // The form the minter writes: each installation id to its account's login and token.
        let tokens = token_secret(&serde_json::json!({
            "1000001": { "owner": "sjawhar", "token": "ghs_first" },
            "1000002": { "owner": "acme", "token": "ghs_second", "expires_at": "ignored" },
        }))?;
        assert_eq!(tokens.for_installation(1_000_002)?, "ghs_second");
        assert_eq!(tokens.for_installation(1_000_001)?, "ghs_first");
        assert_eq!(tokens.for_owner("acme")?, "ghs_second");
        // GitHub logins are case-insensitive, and a remote URL may spell one either way.
        assert_eq!(tokens.for_owner("Acme")?, "ghs_second");
        let stored_mixed = token_secret(&serde_json::json!({
            "1000003": { "owner": "WidgetCo", "token": "ghs_third" },
        }))?;
        assert_eq!(stored_mixed.for_owner("widgetco")?, "ghs_third");
        assert_eq!(stored_mixed.for_owner("WIDGETCO")?, "ghs_third");
        Ok(())
    }

    #[test]
    fn a_token_secret_with_no_entry_names_what_it_lacks() -> anyhow::Result<()> {
        let tokens = token_secret(&serde_json::json!({
            "1000001": { "owner": "sjawhar", "token": "ghs_first" },
        }))?;
        let installation = tokens
            .for_installation(1_000_009)
            .err()
            .map_or_default(|error| format!("{error:#}"));
        assert!(installation.contains("1000009"), "{installation}");
        let owner = tokens
            .for_owner("acme")
            .err()
            .map_or_default(|error| format!("{error:#}"));
        assert!(owner.contains("acme"), "{owner}");
        assert!(!owner.contains("ghs_first"), "{owner}");
        Ok(())
    }

    #[test]
    fn an_empty_token_in_the_secret_is_no_token() -> anyhow::Result<()> {
        // An empty token would be sent as no credential, and would reach git's error redaction
        // as an empty pattern, which matches between every character.
        let tokens = token_secret(&serde_json::json!({
            "1000001": { "owner": "sjawhar", "token": "" },
            "1000002": { "owner": "acme", "token": "ghs_second" },
        }))?;
        assert!(tokens.for_installation(1_000_001).is_err());
        assert!(tokens.for_owner("sjawhar").is_err());
        assert_eq!(tokens.for_installation(1_000_002)?, "ghs_second");
        assert_eq!(tokens.for_owner("acme")?, "ghs_second");
        Ok(())
    }

    #[test]
    fn a_token_secret_that_does_not_parse_is_refused_without_its_tokens() {
        // A token where its entry belongs, and an id that is not a number.
        for value in [
            serde_json::json!({ "1000001": "ghs_leaked_value" }),
            serde_json::json!({ "ghs_leaked_value": { "owner": "acme", "token": "ghs_x" } }),
            serde_json::json!({ "1000001": { "owner": "acme" } }),
        ] {
            let error = token_secret(&value)
                .err()
                .map_or_default(|error| format!("{error:#}"));
            assert!(
                error.contains("the credential is not the expected JSON"),
                "{value} gave {error}"
            );
            assert!(!error.contains("ghs_leaked_value"), "{error}");
        }
    }
}
