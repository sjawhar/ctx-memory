//! Google Workspace APIs called directly with the signed-in user's OAuth credential.
//!
//! The credential is named `google` and comes from `credentials::load`: a Secret Manager
//! version in Google Cloud, and a file or an environment variable on a dev box. A source runs
//! behind the same boundary as an external one, which receives credentials only through the
//! environment, so this module reads no tree configuration. The value is either the broker's
//! `authorized_user` JSON (`client_id`, `client_secret`, `refresh_token`), which this module
//! exchanges for an access token, or a bare access token, which a dev-box session receives
//! from `secret-run`.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use crate::api::client;
use crate::credentials::{self, Secret};

/// Every Workspace API used here is served under this host.
const DEFAULT_API_BASE: &str = "https://www.googleapis.com";
/// Where an `authorized_user` credential is exchanged for an access token.
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The credential's name, which `credentials::load` resolves.
const CREDENTIAL: &str = "google";
/// Overrides the API host; a test or a staging run points this at its own server.
const API_BASE_VAR: &str = "CTX_GOOGLE_API_BASE";
/// Overrides the token endpoint. Defaults to `<API base>/token` when the base is overridden.
const TOKEN_URL_VAR: &str = "CTX_GOOGLE_TOKEN_URL";
/// A cached access token is reused until this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);
/// How many times one request is sent before its back-off reply is final.
///
/// Gmail counts its quota in minutes, so the pauses have to add up to more than one.
const RETRY_ATTEMPTS: u32 = 8;
/// The first pause after a back-off reply; each later pause doubles it.
const RETRY_FIRST_PAUSE: Duration = Duration::from_secs(1);
/// No pause is longer than this.
const RETRY_MAX_PAUSE: Duration = Duration::from_secs(60);
/// A page request asks for this many items.
const PAGE_SIZE: u32 = 100;
/// The refusals of a text export that are about the file itself, as Drive names them: the
/// owner turned off download (`cannotExportFile`), the export would pass Drive's 10 MB limit
/// (`exportSizeLimitExceeded`), the file is of a kind Drive does not export (`fileNotExportable`),
/// the user may not read this file (`insufficientFilePermissions`), or the file is gone
/// (`notFound`). Every other refusal — a spent daily limit (`dailyLimitExceeded`), a token
/// without the Drive scope (`insufficientPermissions`), an export format the request chose badly
/// (`badRequest`), a `403` from a proxy with no reason at all — is about the caller, and the
/// file's text is still there for a later run.
///
/// Drive documents `fileNotExportable`, `insufficientFilePermissions`, `notFound` and the
/// caller's reasons at <https://developers.google.com/workspace/drive/api/guides/handle-errors>.
/// `exportSizeLimitExceeded` is not on that page; Google's issue tracker shows Drive answering
/// it (<https://issuetracker.google.com/issues/36761333>). Drive has answered
/// `fileNotExportable` live for a plain-text file and a folder, and `notFound` for an unknown
/// id. No reply with `cannotExportFile` has been seen, because no Drive checked so far held a
/// document with download turned off.
const FILE_REFUSALS: [(u16, &str); 5] = [
    (403, "cannotExportFile"),
    (403, "exportSizeLimitExceeded"),
    (403, "fileNotExportable"),
    (403, "insufficientFilePermissions"),
    (404, "notFound"),
];

/// How far past the moment a run reads an item its own date may lie and still be the time it
/// changed or arrived: room for this machine's clock to run behind Google's.
pub const CLOCK_SKEW_MARGIN: TimeDelta = TimeDelta::minutes(10);

/// The latest date an item read now can carry and still be a change that happened: the later of
/// the window end and now, plus [`CLOCK_SKEW_MARGIN`].
///
/// Google takes a file's modified time and a message's arrival date from what a client or a
/// sender wrote, so either can lie in the future. Such an item has not changed after the window;
/// its date is wrong. A source that held its cursor for it would hold every later change behind
/// it until the clock passed the date, so it stores the item in the current window instead. An
/// item dated at or before this bound and after the window end did change after the window, and
/// waits for the next one. A source takes the bound once, just after it reads the listing, page
/// or history that names the item.
#[must_use]
pub fn latest_believable(window_end: DateTime<Utc>) -> DateTime<Utc> {
    window_end.max(Utc::now()) + CLOCK_SKEW_MARGIN
}

/// A short-lived Google access token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessToken {
    pub value: String,
    pub expires_in: Option<i64>,
}

/// OAuth access tokens for the Workspace credential.
pub struct Token;

impl Token {
    /// Exchanges `credential` for an access token at `token_url`.
    ///
    /// A credential that is not an `authorized_user` JSON object is already an access token and
    /// is returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential is JSON without a refresh token, the token endpoint
    /// cannot be reached, or it refuses the credential.
    pub fn refresh(credential: &Secret, token_url: &str) -> Result<AccessToken> {
        let Some(form) = refresh_form(credential)? else {
            return Ok(AccessToken {
                value: credential.expose().trim().to_owned(),
                expires_in: None,
            });
        };
        let (status, _headers, body) =
            client::post_form(token_url, &form).context("refreshing the Google access token")?;
        if !status.is_success() {
            bail!(
                "{token_url} refused the Google credential: {status} {}",
                error_message(&body)
            );
        }
        access_token(&body)
    }
}

/// An authenticated client for one Google API host.
pub struct Google {
    base: String,
    token: String,
}

/// The items of every page of a list call, with the last page's envelope.
pub struct Pages {
    pub items: Vec<Value>,
    pub last: Value,
}

/// What a text export returned.
#[derive(Debug, Eq, PartialEq)]
pub enum Text {
    /// The text, and whether the document was longer than the limit.
    Body { text: String, truncated: bool },
    /// Google will not export this file: a refusal whose reason is the file itself, such as a
    /// download its owner turned off, with its status and Google's own words.
    Refused { status: u16, message: String },
}

impl Google {
    /// Builds a client from the `google` credential and the API host in the environment.
    ///
    /// # Errors
    ///
    /// Returns an error when no credential is configured or it cannot be exchanged for an
    /// access token.
    pub fn load() -> Result<Self> {
        Ok(Self {
            base: api_base(),
            token: cached_token()?,
        })
    }

    /// `GET <base>/<path>`, returning the reply status and body.
    ///
    /// A reply Google asks a client to back off from is retried, because a window of mail or
    /// files costs more requests per minute than one user may spend.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be sent or its body is not JSON.
    pub fn reply(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, Value)> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        with_retries(
            || {
                let (status, _headers, body) = client::get_json(&url, &self.token, query)
                    .with_context(|| format!("calling Google {path}"))?;
                Ok((status.as_u16(), body))
            },
            std::thread::sleep,
        )
    }

    /// `GET <base>/<path>`, failing on any status outside 2xx.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or Google refuses it.
    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let (status, body) = self.reply(path, query)?;
        if !(200..300).contains(&status) {
            bail!("Google {path} returned {status}: {}", error_message(&body));
        }
        Ok(body)
    }

    /// Follows `nextPageToken` through a list call, collecting `items_key` from every page.
    ///
    /// # Errors
    ///
    /// Returns an error when a page fails or the API repeats a page token.
    pub fn paginate(&self, path: &str, query: &[(&str, String)], items_key: &str) -> Result<Pages> {
        paginate_with(query, items_key, |page_query| self.get(path, page_query))
    }

    /// Exports `path` as text, reading at most `limit` bytes.
    ///
    /// A reply Google asks a client to back off from is sent again, as [`Self::reply`] does. A
    /// refusal whose reason is the file itself comes back as [`Text::Refused`], so the caller can
    /// tell a file Google will not export from a failed call.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails, or Google answers with a server error, a
    /// back-off that never clears, or a refusal that is not about the file.
    pub fn get_text(&self, path: &str, query: &[(&str, String)], limit: usize) -> Result<Text> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut body = None;
        let (status, error) = with_retries(
            || {
                let (status, _headers, bytes, truncated) =
                    client::get_bytes(&url, &self.token, query, limit)
                        .with_context(|| format!("exporting Google {path}"))?;
                if status.is_success() {
                    body = Some((bytes, truncated));
                    return Ok((status.as_u16(), Value::Null));
                }
                Ok((status.as_u16(), error_body(&bytes)))
            },
            std::thread::sleep,
        )?;
        text_reply(path, status, &error, body)
    }
}

/// Reads the final reply of a text export: the body of a success, or the refusal of a client
/// error whose reason is the file itself. Any other reply is an error, so the stream that asked
/// ends with it and its cursor stays where it was.
fn text_reply(
    path: &str,
    status: u16,
    error: &Value,
    body: Option<(Vec<u8>, bool)>,
) -> Result<Text> {
    if (200..300).contains(&status) {
        let (bytes, truncated) =
            body.ok_or_else(|| anyhow!("Google {path} returned {status} without a body"))?;
        return Ok(Text::Body {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        });
    }
    let names_the_file = reason(error).is_some_and(|reason| {
        FILE_REFUSALS
            .iter()
            .any(|&(refused, named)| refused == status && named == reason)
    });
    if names_the_file {
        return Ok(Text::Refused {
            status,
            message: error_message(error),
        });
    }
    bail!("Google {path} returned {status}: {}", error_message(error))
}

fn paginate_with<F>(query: &[(&str, String)], items_key: &str, mut page: F) -> Result<Pages>
where
    F: FnMut(&[(&str, String)]) -> Result<Value>,
{
    let mut items = Vec::new();
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut page_query = query.to_vec();
        if let Some(token) = &token {
            page_query.push(("pageToken", token.clone()));
        }
        let response = page(&page_query)?;
        if let Some(page_items) = response.get(items_key).and_then(Value::as_array) {
            items.extend(page_items.iter().cloned());
        }
        token = response
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|next| !next.is_empty())
            .map(str::to_owned);
        match &token {
            Some(next) if seen.contains(next) => {
                bail!("Google list repeated page token {next:?}");
            }
            Some(next) => seen.push(next.clone()),
            None => {
                return Ok(Pages {
                    items,
                    last: response,
                })
            }
        }
    }
}

/// Whether Google answered that what was asked for does not exist: `404` with the reason
/// `notFound`. A `404` without that reason — an empty body, a proxy's page, a wrong API base —
/// says nothing about the message or file asked for, and treating it as gone would step over
/// content that is still there.
#[must_use]
pub fn not_found(status: u16, body: &Value) -> bool {
    status == 404 && reason(body) == Some("notFound")
}

/// Whether Google refused the value of one query parameter: `400` with the reason `invalid` at
/// `parameter`. Drive answers so for a changes page token it does not accept; any other `400`
/// is about the request, not the value the caller stored.
#[must_use]
pub fn invalid_parameter(status: u16, body: &Value, parameter: &str) -> bool {
    status == 400
        && reason(body) == Some("invalid")
        && body
            .pointer("/error/errors/0/location")
            .and_then(Value::as_str)
            == Some(parameter)
}

/// The reason of a Google error reply, which names its cause more narrowly than its status.
fn reason(body: &Value) -> Option<&str> {
    body.pointer("/error/errors/0/reason")
        .and_then(Value::as_str)
}

/// An error reply's body: its JSON, or the start of its text when something other than Google
/// answered, so the error names what came back.
fn error_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| {
        Value::String(String::from_utf8_lossy(bytes).chars().take(200).collect())
    })
}

/// Whether Google is asking the caller to try the same request again later.
///
/// Gmail prices a window of mail above one user's per-minute quota, so a `403` that names a
/// rate limit is a pause, not a refusal.
fn retryable(status: u16, body: &Value) -> bool {
    match status {
        429 | 500..=599 => true,
        403 => {
            let reason = reason(body).unwrap_or_default().to_ascii_lowercase();
            let message = error_message(body).to_ascii_lowercase();
            reason.contains("ratelimit")
                || reason.contains("quota")
                || message.contains("rate limit")
                || message.contains("quota exceeded")
        }
        _ => false,
    }
}

/// Calls `request`, pausing and trying again while Google asks for a back-off.
fn with_retries<F, S>(mut request: F, mut pause: S) -> Result<(u16, Value)>
where
    F: FnMut() -> Result<(u16, Value)>,
    S: FnMut(Duration),
{
    let mut wait = RETRY_FIRST_PAUSE;
    for _ in 1..RETRY_ATTEMPTS {
        let (status, body) = request()?;
        if !retryable(status, &body) {
            return Ok((status, body));
        }
        pause(wait);
        wait = wait.saturating_mul(2).min(RETRY_MAX_PAUSE);
    }
    request()
}

/// The form fields of a refresh request, or `None` when the credential is an access token.
fn refresh_form(credential: &Secret) -> Result<Option<Vec<(&'static str, String)>>> {
    let trimmed = credential.expose().trim();
    if !trimmed.starts_with('{') {
        if trimmed.is_empty() {
            bail!("the Google credential is empty");
        }
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(trimmed).context("parsing the Google credential as JSON")?;
    let field = |name: &str| -> Result<String> {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|field| !field.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("the Google credential is missing {name}"))
    };
    Ok(Some(vec![
        ("grant_type", "refresh_token".to_owned()),
        ("client_id", field("client_id")?),
        ("client_secret", field("client_secret")?),
        ("refresh_token", field("refresh_token")?),
    ]))
}

fn access_token(body: &Value) -> Result<AccessToken> {
    let value = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("the Google token response has no access_token"))?;
    Ok(AccessToken {
        value: value.to_owned(),
        expires_in: body.get("expires_in").and_then(Value::as_i64),
    })
}

fn error_message(body: &Value) -> String {
    body.pointer("/error/message")
        .or_else(|| body.pointer("/error_description"))
        .or_else(|| body.pointer("/error"))
        .and_then(Value::as_str)
        .or_else(|| body.as_str())
        .map_or_else(|| body.to_string(), str::to_owned)
}

fn api_base() -> String {
    std::env::var(API_BASE_VAR)
        .ok()
        .filter(|base| !base.trim().is_empty())
        .map_or_else(
            || DEFAULT_API_BASE.to_owned(),
            |base| base.trim_end_matches('/').to_owned(),
        )
}

fn token_url() -> String {
    if let Some(url) = std::env::var(TOKEN_URL_VAR)
        .ok()
        .filter(|url| !url.trim().is_empty())
    {
        return url;
    }
    match std::env::var(API_BASE_VAR)
        .ok()
        .filter(|base| !base.trim().is_empty())
    {
        Some(base) => format!("{}/token", base.trim_end_matches('/')),
        None => DEFAULT_TOKEN_URL.to_owned(),
    }
}

/// One access token per credential, shared by every Google source in a run.
static TOKEN_CACHE: LazyLock<Mutex<BTreeMap<String, (String, Instant)>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn cached_token() -> Result<String> {
    let credential = credentials::load(CREDENTIAL)?;
    let token_url = token_url();
    let key = format!("{token_url}\n{}", credential.expose());
    let mut cache = TOKEN_CACHE
        .lock()
        .map_err(|error| anyhow!("reading the Google token cache: {error}"))?;
    if let Some((token, expiry)) = cache.get(&key) {
        if Instant::now() < *expiry {
            return Ok(token.clone());
        }
    }
    let token = Token::refresh(&credential, &token_url)?;
    let lifetime = token
        .expires_in
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map_or(EXPIRY_MARGIN, |seconds| {
            Duration::from_secs(seconds).saturating_sub(EXPIRY_MARGIN)
        });
    if let Some(expiry) = Instant::now().checked_add(lifetime) {
        cache.insert(key, (token.value.clone(), expiry));
    }
    Ok(token.value)
}

/// The page size every list call asks for.
#[must_use]
pub const fn page_size() -> u32 {
    PAGE_SIZE
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn secret(value: &str) -> Secret {
        Secret::new(value.to_owned())
    }

    #[test]
    fn an_authorized_user_credential_becomes_a_refresh_request() -> Result<()> {
        let form = refresh_form(&secret(
            r#"{"type":"authorized_user","client_id":"id","client_secret":"secret","refresh_token":"refresh"}"#,
        ))?
        .ok_or_else(|| anyhow!("an authorized_user credential needs a refresh request"))?;

        assert_eq!(
            form,
            vec![
                ("grant_type", "refresh_token".to_owned()),
                ("client_id", "id".to_owned()),
                ("client_secret", "secret".to_owned()),
                ("refresh_token", "refresh".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn a_bare_access_token_is_used_as_it_is() -> Result<()> {
        assert_eq!(refresh_form(&secret("ya29.example\n"))?, None);
        let token = Token::refresh(&secret("ya29.example\n"), "http://127.0.0.1:1/unused")?;
        assert_eq!(token.value, "ya29.example");
        Ok(())
    }

    #[test]
    fn a_credential_without_a_refresh_token_is_named() {
        let error = refresh_form(&secret(r#"{"type":"authorized_user","client_id":"id"}"#)).err();
        assert_eq!(
            error.map(|error| error.to_string()),
            Some("the Google credential is missing client_secret".to_owned())
        );
    }

    #[test]
    fn a_token_response_carries_its_lifetime() -> Result<()> {
        let token = access_token(&json!({"access_token": "ya29.example", "expires_in": 3599}))?;
        assert_eq!(token.value, "ya29.example");
        assert_eq!(token.expires_in, Some(3599));
        Ok(())
    }

    #[test]
    fn pagination_follows_page_tokens_and_keeps_the_last_envelope() -> Result<()> {
        let mut pages = vec![
            json!({"files": [{"id": "a"}], "nextPageToken": "second"}),
            json!({"files": [{"id": "b"}], "newStartPageToken": "end"}),
        ]
        .into_iter();
        let mut seen_tokens = Vec::new();
        let collected = paginate_with(&[("q", "trashed = false".to_owned())], "files", |query| {
            seen_tokens.push(
                query
                    .iter()
                    .find(|(name, _)| *name == "pageToken")
                    .map(|(_, value)| value.clone()),
            );
            pages
                .next()
                .ok_or_else(|| anyhow!("the fake pager ran out of pages"))
        })?;

        assert_eq!(collected.items.len(), 2);
        assert_eq!(seen_tokens, vec![None, Some("second".to_owned())]);
        assert_eq!(
            collected
                .last
                .get("newStartPageToken")
                .and_then(Value::as_str),
            Some("end")
        );
        Ok(())
    }

    #[test]
    fn a_repeated_page_token_is_an_error() {
        let error = paginate_with(&[], "files", |_| {
            Ok(json!({"files": [], "nextPageToken": "same"}))
        })
        .err();
        assert_eq!(
            error.map(|error| error.to_string()),
            Some("Google list repeated page token \"same\"".to_owned())
        );
    }

    #[test]
    fn a_rate_limited_reply_is_retried_after_a_pause() -> Result<()> {
        let quota = json!({"error": {"code": 403, "errors": [{"reason": "rateLimitExceeded"}],
            "message": "Quota exceeded for quota metric 'Total Query Cost'"}});
        let mut replies = vec![
            (403, quota.clone()),
            (403, quota),
            (200, json!({"id": "message"})),
        ]
        .into_iter();
        let mut pauses = Vec::new();
        let (status, body) = with_retries(
            || {
                replies
                    .next()
                    .ok_or_else(|| anyhow!("the fake caller ran out of replies"))
            },
            |wait| pauses.push(wait),
        )?;

        assert_eq!(status, 200);
        assert_eq!(body.get("id").and_then(Value::as_str), Some("message"));
        assert_eq!(pauses, vec![Duration::from_secs(1), Duration::from_secs(2)]);
        Ok(())
    }

    #[test]
    fn a_refusal_that_is_not_a_rate_limit_is_final() -> Result<()> {
        let mut calls = 0_u32;
        let mut pauses = Vec::new();
        let (status, _body) = with_retries(
            || {
                calls += 1;
                Ok((404, json!({"error": {"message": "Not Found"}})))
            },
            |wait| pauses.push(wait),
        )?;

        assert_eq!(status, 404);
        assert_eq!(calls, 1);
        assert_eq!(pauses, [] as [Duration; 0]);
        Ok(())
    }

    #[test]
    fn a_back_off_pause_never_passes_its_cap() -> Result<()> {
        let mut pauses = Vec::new();
        let (_status, _body) = with_retries(
            || Ok((429, json!({"error": {"message": "Too Many Requests"}}))),
            |wait| pauses.push(wait),
        )?;

        assert_eq!(pauses.first().copied(), Some(Duration::from_secs(1)));
        assert_eq!(pauses.last().copied(), Some(RETRY_MAX_PAUSE));
        assert!(
            pauses.iter().sum::<Duration>() > Duration::from_secs(60),
            "{pauses:?}"
        );
        Ok(())
    }

    #[test]
    fn a_back_off_that_never_clears_returns_its_last_reply() -> Result<()> {
        let mut calls = 0_u32;
        let mut pauses = Vec::new();
        let (status, _body) = with_retries(
            || {
                calls += 1;
                Ok((429, json!({"error": {"message": "Too Many Requests"}})))
            },
            |wait| pauses.push(wait),
        )?;

        assert_eq!(status, 429);
        assert_eq!(calls, RETRY_ATTEMPTS);
        assert_eq!(pauses.len(), usize::try_from(RETRY_ATTEMPTS)? - 1);
        Ok(())
    }

    /// A Drive error reply as Google writes it, with its reason beside the message.
    fn drive_error(status: u16, reason: &str, message: &str) -> Value {
        json!({"error": {
            "code": status,
            "errors": [{"domain": "global", "reason": reason, "message": message}],
            "message": message
        }})
    }

    #[test]
    fn an_export_google_will_not_make_for_this_file_is_a_refusal() -> Result<()> {
        for (status, reason, message) in [
            (
                403,
                "cannotExportFile",
                "This file cannot be exported by the user.",
            ),
            (
                403,
                "exportSizeLimitExceeded",
                "This file is too large to be exported.",
            ),
            (
                403,
                "insufficientFilePermissions",
                "The user does not have sufficient permissions for file a.",
            ),
            (404, "notFound", "File not found: a."),
        ] {
            assert_eq!(
                text_reply(
                    "drive/v3/files/a/export",
                    status,
                    &drive_error(status, reason, message),
                    None
                )?,
                Text::Refused {
                    status,
                    message: message.to_owned()
                },
                "{reason}"
            );
        }
        Ok(())
    }

    /// Drive's answer, observed live on 2026-10-28, to an export of a file that is not a Docs
    /// Editors file (a plain-text file and a folder in the signed-in user's Drive); Google
    /// documents the same reason for a Google Vids file. It names the file's kind, so a later
    /// run gets the same answer.
    #[test]
    fn an_export_of_a_file_drive_cannot_export_is_a_refusal() -> Result<()> {
        let message = "Export only supports Docs Editors files.";
        assert_eq!(
            text_reply(
                "drive/v3/files/a/export",
                403,
                &drive_error(403, "fileNotExportable", message),
                None
            )?,
            Text::Refused {
                status: 403,
                message: message.to_owned()
            }
        );
        Ok(())
    }

    #[test]
    fn an_export_refused_for_a_reason_that_is_not_the_file_is_an_error() {
        // Each of these says nothing about the file: storing it as its name would move the
        // cursor past text that a run with quota, scope, or a clear path to Google can read.
        for (status, body) in [
            (
                403,
                drive_error(403, "dailyLimitExceeded", "Daily Limit Exceeded"),
            ),
            (
                403,
                drive_error(
                    403,
                    "insufficientPermissions",
                    "Request had insufficient authentication scopes.",
                ),
            ),
            (
                403,
                drive_error(403, "userRateLimitExceeded", "User Rate Limit Exceeded"),
            ),
            (
                403,
                drive_error(
                    403,
                    "domainPolicy",
                    "The domain administrators have disabled Drive apps.",
                ),
            ),
            // Drive's live answer to an export format the file cannot take: the request chose
            // the format, so the fault is the caller's.
            (
                400,
                drive_error(
                    400,
                    "badRequest",
                    "The requested conversion is not supported.",
                ),
            ),
            // A proxy's own page, which is not JSON, and the same page before it was read as text.
            (403, json!("<html><body>403 Forbidden</body></html>")),
            (403, Value::Null),
            // A 404 with no reason did not come from Drive's file lookup.
            (404, json!({"error": {"code": 404, "message": "Not Found"}})),
            (401, drive_error(401, "authError", "Invalid Credentials")),
            (429, json!({"error": {"message": "Too Many Requests"}})),
            (500, json!({"error": {"message": "Backend Error"}})),
        ] {
            assert!(
                text_reply("drive/v3/files/a/export", status, &body, None).is_err(),
                "{status} {body}"
            );
        }
    }

    #[test]
    fn an_exported_body_is_its_text() -> Result<()> {
        assert_eq!(
            text_reply(
                "drive/v3/files/a/export",
                200,
                &Value::Null,
                Some((b"plan text".to_vec(), true))
            )?,
            Text::Body {
                text: "plan text".to_owned(),
                truncated: true
            }
        );
        Ok(())
    }
}
