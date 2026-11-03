//! The Matrix client-server API, called with one access token.
//!
//! The credential is named `matrix` and comes from `credentials::load`: a Secret Manager version
//! in Google Cloud, and a file or an environment variable elsewhere. Its value is the access
//! token, or a JSON object whose `access_token` field carries it, so the answer of a
//! `POST /_matrix/client/v3/login` works as it is. The token travels in the `Authorization`
//! header and never in a URL, a message or an error.
//!
//! Nothing here writes to the homeserver: every call is a `GET`, so a fetch never sends a read
//! receipt, a read marker, presence or a message.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::api::client;
use crate::credentials::{self, Secret};

/// The credential's name, which `credentials::load` resolves.
const CREDENTIAL: &str = "matrix";

/// The Matrix error code of a rate-limited request.
const LIMIT_EXCEEDED: &str = "M_LIMIT_EXCEEDED";

/// The Matrix error codes that refuse the access token or the account, not the thing asked
/// for: an unknown or missing token, and a locked, suspended or deactivated account.
const AUTH_FAILURES: [&str; 5] = [
    "M_UNKNOWN_TOKEN",
    "M_MISSING_TOKEN",
    "M_USER_LOCKED",
    "M_USER_SUSPENDED",
    "M_USER_DEACTIVATED",
];

/// A homeserver read with one access token.
pub struct MatrixClient {
    base: String,
    token: Secret,
}

/// One answer: its status, its JSON body, and how long the homeserver asked the caller to wait
/// before it tries again, when it named a time.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub retry_after: Option<Duration>,
}

impl Reply {
    /// Whether the status is a success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The Matrix error code the body names (`M_NOT_FOUND`, `M_FORBIDDEN`, …).
    #[must_use]
    pub fn errcode(&self) -> Option<&str> {
        self.body.get("errcode").and_then(Value::as_str)
    }

    /// Whether the homeserver refused the request for its rate limit.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429 || self.errcode() == Some(LIMIT_EXCEEDED)
    }

    /// Whether the homeserver answered that what was asked for does not exist: `404` with
    /// `M_NOT_FOUND`. A bare `404`, as a proxy in front of a homeserver may give for a route it
    /// does not know, says nothing about the thing asked for.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.status == 404 && self.errcode() == Some("M_NOT_FOUND")
    }

    /// Whether the homeserver does not serve the endpoint at all, as a server answers for an
    /// optional endpoint it does not implement.
    #[must_use]
    pub fn is_unrecognized(&self) -> bool {
        self.errcode() == Some("M_UNRECOGNIZED")
    }

    /// Whether the homeserver refused the access token or the account (`401`, or an error code
    /// that says so), which no later request of the run gets past. A `403 M_FORBIDDEN` refuses
    /// only the thing asked for.
    #[must_use]
    pub fn is_auth_failure(&self) -> bool {
        self.status == 401
            || self
                .errcode()
                .is_some_and(|code| AUTH_FAILURES.contains(&code))
    }

    /// An error naming the status, the Matrix error code and message, and the request.
    #[must_use]
    pub fn refusal(&self, request: &str) -> anyhow::Error {
        let message = self
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.errcode() {
            Some(code) if message.is_empty() => {
                anyhow!(
                    "the homeserver answered {} {code} to {request}",
                    self.status
                )
            }
            Some(code) => anyhow!(
                "the homeserver answered {} {code} ({message}) to {request}",
                self.status
            ),
            None => anyhow!("the homeserver answered {} to {request}", self.status),
        }
    }
}

impl MatrixClient {
    /// A client of the homeserver at `homeserver`, with the access token the `matrix`
    /// credential holds.
    ///
    /// # Errors
    ///
    /// Returns an error when the address is refused ([`homeserver_base`]) or the credential is
    /// missing or of neither shape.
    pub fn load(homeserver: &str) -> Result<Self> {
        let base = homeserver_base(homeserver)?;
        let token = access_token(&credentials::load(CREDENTIAL)?)?;
        Ok(Self { base, token })
    }

    /// `GET <homeserver><path>` with the access token, sent once.
    ///
    /// # Errors
    ///
    /// Returns an error when the request gets no answer or its body is not JSON.
    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Reply> {
        let url = format!("{}{path}", self.base);
        let (status, headers, body) = client::get_json(&url, self.token.expose(), query)?;
        let retry_after = retry_after(&body, &headers);
        Ok(Reply {
            status: status.as_u16(),
            body,
            retry_after,
        })
    }
}

/// The base URL of the homeserver at `url`, without a trailing `/`.
///
/// HTTPS is the rule, since every request carries the access token. Plain HTTP is taken only
/// for a host whose traffic does not cross the internet: a loopback address or `localhost`, a
/// private address (RFC 1918 or an IPv6 unique local address), the shared address range
/// 100.64.0.0/10 that a tailnet hands out, or a tailnet name (`*.ts.net`). A bridge hub that
/// one person runs often listens on such an address.
///
/// # Errors
///
/// Returns an error when `url` is not a URL with a host, or is plain HTTP to any other host.
pub fn homeserver_base(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url.trim())
        .with_context(|| format!("matrix homeserver {url:?} is not a URL"))?;
    let host = parsed
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty() || !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("matrix homeserver {url:?} must be an https:// URL with a host and no user name");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("matrix homeserver {url:?} must not carry a query or a fragment");
    }
    match parsed.scheme() {
        "https" => {}
        "http" if is_local_host(&host) => {}
        _ => bail!(
            "matrix homeserver {url:?} is refused: every request carries the access token, so it \
             takes https://, or http:// only to a loopback, private or tailnet address \
             (localhost, 127.0.0.1, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10, \
             fc00::/7, *.ts.net)"
        ),
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// Whether plain HTTP to `host` stays off the internet (see [`homeserver_base`]).
fn is_local_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".ts.net") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            let [first, second, ..] = address.octets();
            address.is_loopback() || address.is_private() || (first == 100 && (second & 0xc0) == 64)
        }
        Ok(IpAddr::V6(address)) => {
            address.is_loopback()
                || address
                    .segments()
                    .first()
                    .is_some_and(|first| first & 0xfe00 == 0xfc00)
        }
        Err(_) => false,
    }
}

/// The access token a `matrix` credential holds: the value itself, or the `access_token` field
/// of a JSON object.
///
/// # Errors
///
/// Returns an error when the value is empty, or is JSON without a non-empty `access_token`
/// string. The error never carries the value.
pub fn access_token(credential: &Secret) -> Result<Secret> {
    let trimmed = credential.expose().trim();
    if trimmed.is_empty() {
        bail!("the matrix credential is empty");
    }
    if !trimmed.starts_with('{') {
        if trimmed.contains(char::is_whitespace) {
            bail!("the matrix credential is neither an access token nor a JSON object");
        }
        return Ok(Secret::new(trimmed.to_owned()));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| anyhow!("the matrix credential is not valid JSON ({error})"))?;
    value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| Secret::new(token.to_owned()))
        .ok_or_else(|| anyhow!("the matrix credential has no access_token"))
}

/// How long a rate-limited answer asks the caller to wait: `retry_after_ms` in its body, else
/// a `Retry-After` header in seconds.
fn retry_after(body: &Value, headers: &HeaderMap) -> Option<Duration> {
    body.get("retry_after_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .or_else(|| {
            headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
        })
}

/// `id` encoded for one segment of a request path: room and event ids carry `!`, `$`, `:` and,
/// in room version 3, `/` and `+`.
#[must_use]
pub fn encode(id: &str) -> String {
    client::percent_encode(id)
}

#[cfg(test)]
mod tests {
    use super::{access_token, homeserver_base, retry_after, Reply};
    use crate::credentials::Secret;
    use anyhow::Result;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn https_is_taken_for_any_host_and_http_only_off_the_internet() -> Result<()> {
        for (url, base) in [
            ("https://matrix.example.org", "https://matrix.example.org"),
            ("https://matrix.example.org/", "https://matrix.example.org"),
            ("https://example.org/matrix/", "https://example.org/matrix"),
            ("http://127.0.0.1:8008", "http://127.0.0.1:8008"),
            ("http://localhost:8008/", "http://localhost:8008"),
            ("http://10.1.2.3:6167", "http://10.1.2.3:6167"),
            ("http://172.20.0.5", "http://172.20.0.5"),
            ("http://192.168.1.20:8008", "http://192.168.1.20:8008"),
            ("http://100.64.0.7:6167", "http://100.64.0.7:6167"),
            ("http://100.127.255.1", "http://100.127.255.1"),
            ("http://[::1]:8008", "http://[::1]:8008"),
            ("http://[fd00::5]:8008", "http://[fd00::5]:8008"),
            ("http://hub.example.ts.net", "http://hub.example.ts.net"),
        ] {
            assert_eq!(homeserver_base(url)?, base, "{url}");
        }
        for url in [
            "http://matrix.example.org",
            "http://8.8.8.8",
            "http://100.128.0.1",
            "http://172.32.0.1",
            "http://[2001:db8::1]",
            "ftp://matrix.example.org",
            "https://user:pass@matrix.example.org",
            "https://matrix.example.org/?a=b",
            "matrix.example.org",
            "",
        ] {
            assert!(homeserver_base(url).is_err(), "{url}");
        }
        Ok(())
    }

    #[test]
    fn a_credential_is_a_bare_token_or_a_login_answer() -> Result<()> {
        assert_eq!(
            access_token(&Secret::new("syt_example_token\n".to_owned()))?.expose(),
            "syt_example_token"
        );
        assert_eq!(
            access_token(&Secret::new(
                r#"{"user_id":"@alice:example.org","access_token":"syt_example","device_id":"CTXFETCH","home_server":"example.org"}"#
                    .to_owned()
            ))?
            .expose(),
            "syt_example"
        );
        for value in [
            "",
            "  ",
            "two words",
            "{}",
            r#"{"access_token":""}"#,
            "{not json",
        ] {
            let error = access_token(&Secret::new(value.to_owned()))
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(!error.is_empty(), "{value:?} must be refused");
            assert!(!error.contains("syt_"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn a_rate_limit_names_its_wait_in_the_body_or_a_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            retry_after(&json!({"retry_after_ms": 1500}), &headers),
            Some(Duration::from_millis(1500))
        );
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(
            retry_after(&json!({}), &headers),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after(&json!({"retry_after_ms": 20}), &headers),
            Some(Duration::from_millis(20))
        );
        let reply = Reply {
            status: 429,
            body: json!({"errcode": "M_LIMIT_EXCEEDED"}),
            retry_after: None,
        };
        assert!(reply.is_rate_limited());
    }

    #[test]
    fn a_refused_token_or_account_is_an_auth_failure_and_a_forbidden_event_is_not() {
        let reply = |status: u16, code: &str| Reply {
            status,
            body: json!({"errcode": code}),
            retry_after: None,
        };
        assert!(reply(401, "M_UNKNOWN_TOKEN").is_auth_failure());
        assert!(reply(401, "M_MISSING_TOKEN").is_auth_failure());
        assert!(reply(403, "M_USER_DEACTIVATED").is_auth_failure());
        assert!(!reply(403, "M_FORBIDDEN").is_auth_failure());
        assert!(!reply(404, "M_NOT_FOUND").is_auth_failure());
    }
}
