//! Where a credential comes from, and how a rotated one is stored.
//!
//! In Google Cloud a credential is a Secret Manager version, read with the token the
//! metadata server hands the running service identity. On a dev box it is a file or an
//! environment variable the tree's configuration names. Nothing writes a credential into the
//! tree, and no copy of a secret is kept anywhere else.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::error::Category;
use serde_json::json;

/// The default host of Google's metadata server.
const METADATA_HOST: &str = "http://metadata.google.internal";

/// The default Secret Manager endpoint.
const SECRET_MANAGER: &str = "https://secretmanager.googleapis.com";

/// A secret value. Its `Debug` output never carries the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value itself. Every caller of this is a place a secret may leak.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Reads the secret as JSON, which is the shape every credential this program holds has.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret is not JSON of the `expected` shape. The error names
    /// where the JSON went wrong and the shape expected, and never carries the secret.
    pub fn parse<T: DeserializeOwned>(&self, expected: &str) -> Result<T> {
        serde_json::from_str(&self.0).map_err(|error| not_the_expected_json(&error, expected))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(redacted)")
    }
}

/// Why JSON that holds a secret was refused, from the error's category and position and the
/// shape the caller `expected`, and never from serde's message, which can quote any value it
/// was handed: a credential that is only its key, or whose fields are swapped, hands it the key.
#[must_use]
pub fn json_refusal(error: &serde_json::Error, expected: &str) -> String {
    let why = match error.classify() {
        Category::Syntax => "it is not valid JSON",
        Category::Eof => "it ends before its JSON does",
        Category::Data => "its fields or values are not the expected ones",
        Category::Io => "it could not be read",
    };
    // An error from a value already parsed, as `serde_json::from_value` makes, has no position.
    if error.line() > 0 {
        format!(
            "{why} at line {} column {}; expected {expected}",
            error.line(),
            error.column()
        )
    } else {
        format!("{why}; expected {expected}")
    }
}

/// The refusal of a credential as the JSON a caller `expected`, which never carries the
/// credential: see [`json_refusal`].
#[must_use]
pub fn not_the_expected_json(error: &serde_json::Error, expected: &str) -> anyhow::Error {
    anyhow!(
        "the credential is not the expected JSON ({})",
        json_refusal(error, expected)
    )
}

/// Where one credential lives when it is not in Secret Manager.
///
/// An entry of `[credentials]` in `ctx.toml` is written `name = { file = "<path>" }`, where a
/// path that starts with `~/` is read under `HOME`, or `name = { env = "<VARIABLE>" }`. A table
/// that names both keys, or any other key, is refused.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(
    untagged,
    deny_unknown_fields,
    expecting = "a table { file = \"<path>\" } or { env = \"<VARIABLE>\" }"
)]
pub enum Source {
    /// A file holding the credential.
    File { file: PathBuf },
    /// An environment variable holding the credential.
    Env { env: String },
}

/// The `[credentials]` table of `ctx.toml`.
pub type Config = HashMap<String, Source>;

/// Loads a credential by name, from the environment alone.
///
/// `CTX_SECRET_<NAME>` names a Secret Manager secret and wins; otherwise
/// `CTX_CREDENTIAL_<NAME>_FILE` names a file and `CTX_CREDENTIAL_<NAME>` carries the value.
/// A caller that holds the tree's configuration calls [`load_with`] instead, which also reads
/// the sources named there.
///
/// # Errors
///
/// Returns an error when no variable names the credential or the source cannot be read.
pub fn load(name: &str) -> Result<Secret> {
    match from_environment(name)? {
        Some(secret) => Ok(secret),
        None => bail!(
            "no credential named {name}: set {} or {}",
            secret_env(name),
            credential_env(name)
        ),
    }
}

/// Loads a credential by name, including the sources the tree's configuration names.
///
/// A `[credentials]` file entry that starts with `~/` is read under `HOME`, so
/// `github_app = { file = "~/.config/ctx/github-app.json" }` names a file in the home
/// directory.
///
/// # Errors
///
/// Returns an error when no source is configured for the name or the source cannot be read.
pub fn load_with(config: &Config, name: &str) -> Result<Secret> {
    if let Some(secret) = from_environment(name)? {
        return Ok(secret);
    }
    match config.get(name) {
        Some(Source::File { file }) => match file.to_str() {
            Some(text) => read_file(&crate::config::expand_home(text)),
            None => read_file(file),
        },
        Some(Source::Env { env }) => std::env::var(env)
            .map(Secret)
            .with_context(|| format!("reading {env}")),
        None => bail!(
            "no credential named {name}: set {} or {}, or name one under [credentials] in ctx.toml",
            secret_env(name),
            credential_env(name)
        ),
    }
}

/// The credential the environment names, or `None` when no variable names it.
fn from_environment(name: &str) -> Result<Option<Secret>> {
    if let Some(resource) = std::env::var_os(secret_env(name)) {
        let resource = resource
            .to_str()
            .ok_or_else(|| anyhow!("{} is not valid UTF-8", secret_env(name)))?
            .to_owned();
        return access(&resource).map(Some);
    }
    if let Some(path) = std::env::var_os(credential_file_env(name)) {
        return read_file(&PathBuf::from(path)).map(Some);
    }
    if let Some(value) = std::env::var_os(credential_env(name)) {
        return Ok(Some(Secret(
            value
                .to_str()
                .ok_or_else(|| anyhow!("{} is not valid UTF-8", credential_env(name)))?
                .to_owned(),
        )));
    }
    Ok(None)
}

/// Whether any source names the credential: `CTX_SECRET_<NAME>`, `CTX_CREDENTIAL_<NAME>_FILE`
/// or `CTX_CREDENTIAL_<NAME>` is set, or the tree's `[credentials]` table has an entry for it.
///
/// Nothing is read, so a credential named here may still fail to load. A caller that can go
/// on without the credential uses this to tell "no source names it", the one case it may
/// treat as "no credential", from a credential that is named but cannot be read.
#[must_use]
pub fn configured(config: &Config, name: &str) -> bool {
    // A variable that is set counts even when it is empty, as in `from_environment`.
    std::env::var_os(secret_env(name)).is_some()
        || std::env::var_os(credential_file_env(name)).is_some()
        || std::env::var_os(credential_env(name)).is_some()
        || config.contains_key(name)
}

/// Stores a new version of a credential, which only a rotated credential needs.
///
/// # Errors
///
/// Returns an error when the name has no Secret Manager secret or the version cannot be
/// added. A caller must treat a failure as "the new credential is lost" and not use it.
pub fn save(name: &str, value: &Secret) -> Result<()> {
    let resource = std::env::var(secret_env(name)).with_context(|| {
        format!(
            "{} must name a Secret Manager secret to save {name}",
            secret_env(name)
        )
    })?;
    let token = metadata_token()?;
    let url = format!("{}/v1/{resource}:addVersion", secret_manager_base());
    let response = client()?
        .post(&url)
        .bearer_auth(token.expose())
        .json(&json!({ "payload": { "data": STANDARD.encode(value.expose()) } }))
        .send()
        .with_context(|| format!("adding a version to {resource}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        bail!("adding a version to {resource} failed: {status} {detail}");
    }
    Ok(())
}

/// Reads the newest version of a Secret Manager secret.
fn access(resource: &str) -> Result<Secret> {
    #[derive(Deserialize)]
    struct Access {
        payload: Payload,
    }
    #[derive(Deserialize)]
    struct Payload {
        data: String,
    }
    let token = metadata_token()?;
    let url = format!(
        "{}/v1/{}:access",
        secret_manager_base(),
        version_path(resource)
    );
    let response = client()?
        .get(&url)
        .bearer_auth(token.expose())
        .send()
        .with_context(|| format!("reading {resource}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        bail!("reading {resource} failed: {status} {detail}");
    }
    // The answer holds the credential, so its refusal is built without serde's message.
    let text = response
        .text()
        .with_context(|| format!("reading {resource}"))?;
    let access: Access = serde_json::from_str(&text).map_err(|error| {
        anyhow!(
            "decoding {resource}: the answer is not the expected JSON ({})",
            json_refusal(&error, "{payload: {data}}")
        )
    })?;
    let decoded = STANDARD
        .decode(access.payload.data.as_bytes())
        .with_context(|| format!("decoding the payload of {resource}"))?;
    Ok(Secret(
        String::from_utf8(decoded).with_context(|| format!("{resource} is not UTF-8"))?,
    ))
}

/// The access token of the identity this process runs as.
///
/// # Errors
///
/// Returns an error when the metadata server cannot be reached or answers without a token.
pub fn metadata_token() -> Result<Secret> {
    #[derive(Deserialize)]
    struct Token {
        access_token: String,
    }
    let url = format!(
        "{}/computeMetadata/v1/instance/service-accounts/default/token",
        metadata_base()
    );
    let response = client()?
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .send()
        .with_context(|| format!("asking {url} for an access token"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("the metadata server answered {status}");
    }
    let text = response.text().context("reading the metadata token")?;
    let token: Token = serde_json::from_str(&text).map_err(|error| {
        anyhow!(
            "decoding the metadata token: the answer is not the expected JSON ({})",
            json_refusal(&error, "{access_token}")
        )
    })?;
    Ok(Secret(token.access_token))
}

/// Mints an identity token for an audience, for a caller inside Google Cloud.
///
/// # Errors
///
/// Returns an error when the metadata server cannot be reached or refuses the audience.
pub fn metadata_identity_token(audience: &str) -> Result<Secret> {
    let url = format!(
        "{}/computeMetadata/v1/instance/service-accounts/default/identity",
        metadata_base()
    );
    let response = client()?
        .get(&url)
        .query(&[("audience", audience), ("format", "full")])
        .header("Metadata-Flavor", "Google")
        .send()
        .with_context(|| format!("asking {url} for an identity token"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("the metadata server answered {status}");
    }
    Ok(Secret(
        response
            .text()
            .context("reading the identity token")?
            .trim()
            .to_owned(),
    ))
}

fn read_file(path: &std::path::Path) -> Result<Secret> {
    let value =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Secret(value.trim().to_owned()))
}

/// The HTTP client every credential call shares.
static CLIENT: LazyLock<Result<reqwest::blocking::Client, String>> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| error.to_string())
});

/// The shared HTTP client every credential call uses, built once so connections are reused.
///
/// # Errors
///
/// Returns an error when the client cannot be built.
pub fn client() -> Result<reqwest::blocking::Client> {
    CLIENT
        .as_ref()
        .map(Clone::clone)
        .map_err(|error| anyhow!("building the credential HTTP client: {error}"))
}

/// `projects/x/secrets/y` names the secret; the newest version is what a reader wants.
fn version_path(resource: &str) -> String {
    if resource.contains("/versions/") {
        return resource.to_owned();
    }
    format!("{resource}/versions/latest")
}

fn secret_env(name: &str) -> String {
    format!("CTX_SECRET_{}", name.to_uppercase())
}

fn credential_env(name: &str) -> String {
    format!("CTX_CREDENTIAL_{}", name.to_uppercase())
}

fn credential_file_env(name: &str) -> String {
    format!("{}_FILE", credential_env(name))
}

/// Where the metadata server is, which a test points at its own.
#[must_use]
pub fn metadata_base() -> String {
    with_scheme(std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| METADATA_HOST.to_owned()))
}

fn secret_manager_base() -> String {
    with_scheme(
        std::env::var("CTX_SECRET_MANAGER_URL").unwrap_or_else(|_| SECRET_MANAGER.to_owned()),
    )
}

/// Google's own variable carries a bare host, so a scheme is added when one is missing.
fn with_scheme(host: String) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        return host;
    }
    format!("http://{host}")
}

#[cfg(test)]
mod tests {
    use super::{load, load_with, Config, Deserialize, Secret, Source};

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("sk-live-value".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
    }

    #[test]
    fn a_missing_credential_names_both_variables() {
        let error = load_with(&Config::new(), "slack")
            .err()
            .map_or_default(|error| error.to_string());
        assert!(
            error.contains("CTX_SECRET_SLACK") && error.contains("CTX_CREDENTIAL_SLACK"),
            "unexpected error: {error}"
        );
    }

    /// A caller that passes no configuration never reads the tree's table, so its error must
    /// not send a reader there: naming `google` under `[credentials]` would change nothing.
    #[test]
    fn a_caller_without_tree_configuration_is_not_sent_to_it() {
        let error = load("google")
            .err()
            .map_or_default(|error| error.to_string());
        assert!(
            error.contains("CTX_SECRET_GOOGLE") && error.contains("CTX_CREDENTIAL_GOOGLE"),
            "unexpected error: {error}"
        );
        assert!(!error.contains("ctx.toml"), "unexpected error: {error}");
    }

    #[test]
    fn a_caller_that_reads_the_tree_is_sent_to_it() {
        let error = load_with(&Config::new(), "slack")
            .err()
            .map_or_default(|error| error.to_string());
        assert!(
            error.contains("[credentials] in ctx.toml"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_configured_file_is_read() -> anyhow::Result<()> {
        let path = std::env::temp_dir().join(format!("ctx-credential-{}", std::process::id()));
        std::fs::write(&path, "{\"app_id\":\"1\"}\n")?;
        let mut config = Config::new();
        config.insert("github_app".to_owned(), Source::File { file: path.clone() });
        let secret = load_with(&config, "github_app")?;
        std::fs::remove_file(&path)?;
        assert_eq!(secret.expose(), "{\"app_id\":\"1\"}");
        Ok(())
    }

    /// The two forms the README writes, as `ctx.toml` parses them, and a table naming both.
    #[test]
    fn a_credentials_entry_is_a_file_or_an_env_table() -> anyhow::Result<()> {
        let parse = |entry: &str| toml::from_str::<Config>(&format!("github_app = {entry}\n"));
        let file = std::path::PathBuf::from("~/github-app.json");
        let env = "CTX_FIXTURE_APP".to_owned();
        assert_eq!(
            parse(r#"{ file = "~/github-app.json" }"#)?.get("github_app"),
            Some(&Source::File { file })
        );
        assert_eq!(
            parse(r#"{ env = "CTX_FIXTURE_APP" }"#)?.get("github_app"),
            Some(&Source::Env { env })
        );
        let refused = parse(r#"{ file = "a", env = "B" }"#)
            .err()
            .map_or_default(|error| error.to_string());
        assert!(
            refused.contains(r#"a table { file = "<path>" } or { env = "<VARIABLE>" }"#),
            "a table naming both keys was not refused with the two forms: {refused}"
        );
        Ok(())
    }

    /// serde names a value it was handed, so a token where the credential's JSON wants a
    /// number, a struct or an enum tag, or a token that is the whole credential with text after
    /// it, would reach serde's message. The refusal is built without that message: it says
    /// where the JSON went wrong and what was expected, and never carries the token.
    #[test]
    fn a_refusal_never_carries_a_string_the_credential_holds() -> anyhow::Result<()> {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            App,
        }
        #[derive(Debug, Deserialize)]
        struct Fixture {
            count: u64,
            kind: Kind,
        }
        const TOKEN: &str = "fixture-secret-not-a-real-token";
        const EXPECTED: &str = "{count, kind}";
        let refusal = |json: &str| {
            Secret::new(json.to_owned())
                .parse::<Fixture>(EXPECTED)
                .err()
                .map_or_default(|error| format!("{error:#}"))
        };
        let well_formed = Secret::new(r#"{"count": 1, "kind": "app"}"#.to_owned());
        let fixture: Fixture = well_formed.parse(EXPECTED)?;
        assert_eq!((fixture.count, fixture.kind), (1, Kind::App));
        for json in [
            format!("{TOKEN:?}"),
            format!(r#"{{"count": "{TOKEN}", "kind": "app"}}"#),
            format!(r#"{{"count": 1, "kind": "{TOKEN}"}}"#),
            format!("{TOKEN:?},"),
            format!("{TOKEN:?}\n}}"),
        ] {
            let error = refusal(&json);
            assert!(
                error.starts_with(
                    "the credential is not the expected JSON (its fields or values are not the \
                     expected ones at line 1 column "
                ),
                "{json}: unexpected error {error:?}"
            );
            assert!(
                !error.contains(TOKEN),
                "{json}: the error carries the token: {error}"
            );
            assert!(
                error.ends_with("; expected {count, kind})"),
                "{json}: the error no longer says what was expected: {error}"
            );
        }
        for (json, why) in [
            (r#"{"count": "#, "it ends before its JSON does"),
            ("{count: 1}", "it is not valid JSON"),
        ] {
            let error = refusal(json);
            assert!(
                error.starts_with(&format!(
                    "the credential is not the expected JSON ({why} at line 1 column "
                )),
                "{json}: unexpected error {error:?}"
            );
            assert!(
                error.ends_with("; expected {count, kind})") && !error.contains("EOF"),
                "{json}: unexpected error {error:?}"
            );
        }
        Ok(())
    }
}
