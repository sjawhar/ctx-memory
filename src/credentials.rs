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
    /// Returns an error when the secret is not JSON of the expected shape. The error never
    /// carries the secret.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.0)
            .map_err(|error| anyhow!("the credential is not the expected JSON ({error})"))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(redacted)")
    }
}

/// Where one credential lives when it is not in Secret Manager.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
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
/// # Errors
///
/// Returns an error when no source is configured for the name or the source cannot be read.
pub fn load_with(config: &Config, name: &str) -> Result<Secret> {
    if let Some(secret) = from_environment(name)? {
        return Ok(secret);
    }
    match config.get(name) {
        Some(Source::File { file }) => read_file(file),
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
    if let Some(path) = std::env::var_os(format!("{}_FILE", credential_env(name))) {
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
    let access: Access = response
        .json()
        .with_context(|| format!("decoding {resource}"))?;
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
    let token: Token = response.json().context("decoding the metadata token")?;
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
    use super::{load, load_with, Config, Secret, Source};

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("sk-live-value".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
    }

    #[test]
    fn a_missing_credential_names_both_variables() {
        let error = load_with(&Config::new(), "slack")
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
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
            .map(|error| error.to_string())
            .unwrap_or_default();
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
            .map(|error| error.to_string())
            .unwrap_or_default();
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
}
