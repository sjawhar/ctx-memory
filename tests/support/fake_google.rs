//! A fake Google API for tests that exercise the Gmail, Drive and Calendar sources.

#![allow(dead_code)]

use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::fake_http::{self, Recorded, Response};

static ENVIRONMENT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Serializes the tests that set environment variables, even after one of them failed.
pub fn environment_lock() -> MutexGuard<'static, ()> {
    ENVIRONMENT_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct Seen {
    /// The request lines the server answered, as `GET /path?query`.
    requests: Vec<String>,
    /// The bearer token of every request that carried one, in the order they arrived.
    bearers: Vec<String>,
}

/// A fake Google API: a route is a path fragment and the reply it answers with. A request no
/// route matches is answered as Google answers for something that does not exist.
pub struct FakeGoogle {
    base: String,
    seen: Arc<Mutex<Seen>>,
}

/// One route: the path fragment it matches, and the status and body it answers with.
pub type Route = (&'static str, u16, String);

impl FakeGoogle {
    /// Starts a server on loopback that answers the first route its request line contains with
    /// `200` and that route's JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the loopback port cannot be bound.
    pub fn start(routes: Vec<(&'static str, Value)>) -> Result<Self> {
        Self::start_answering(
            routes
                .into_iter()
                .map(|(fragment, body)| (fragment, 200, body.to_string()))
                .collect(),
        )
    }

    /// Starts a server whose routes answer with their own status and body, which need not be
    /// JSON: a proxy in front of Google answers with its own page.
    ///
    /// # Errors
    ///
    /// Returns an error when the loopback port cannot be bound.
    pub fn start_answering(routes: Vec<Route>) -> Result<Self> {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let serving = Arc::clone(&seen);
        let address = fake_http::serve("the fake Google API", move |request, _| {
            route(&routes, &serving, request)
        })?;
        Ok(Self {
            base: format!("http://127.0.0.1:{}", address.port()),
            seen,
        })
    }

    /// The base URL `CTX_GOOGLE_API_BASE` is pointed at.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Every request the server has answered so far.
    ///
    /// # Errors
    ///
    /// Returns an error when the recording lock cannot be taken.
    pub fn requests(&self) -> Result<Vec<String>> {
        self.seen
            .lock()
            .map(|seen| seen.requests.clone())
            .map_err(|error| anyhow!("reading fake Google requests: {error}"))
    }

    /// The bearer token every request so far presented, which is what authenticates a call.
    ///
    /// # Errors
    ///
    /// Returns an error when the recording lock cannot be taken.
    pub fn bearers(&self) -> Result<Vec<String>> {
        self.seen
            .lock()
            .map(|seen| seen.bearers.clone())
            .map_err(|error| anyhow!("reading fake Google bearer tokens: {error}"))
    }

    /// Points ctx at this server with a bare access token.
    pub fn use_in_environment(&self, token: &str) {
        std::env::set_var("CTX_GOOGLE_API_BASE", &self.base);
        std::env::set_var("CTX_CREDENTIAL_GOOGLE", token);
    }
}

/// Removes the variables [`FakeGoogle::use_in_environment`] set.
pub fn clear_google_environment() {
    std::env::remove_var("CTX_GOOGLE_API_BASE");
    std::env::remove_var("CTX_CREDENTIAL_GOOGLE");
}

/// Records the request and its bearer token, and answers with the first route whose fragment
/// the request line contains.
fn route(routes: &[Route], seen: &Mutex<Seen>, request: &Recorded) -> Result<Response> {
    let line = format!("{} {}", request.method, request.target);
    {
        let mut seen = seen
            .lock()
            .map_err(|error| anyhow!("recording a fake Google request: {error}"))?;
        seen.requests.push(line.clone());
        if let Some(bearer) = request
            .headers
            .get("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
        {
            seen.bearers.push(bearer.trim().to_owned());
        }
    }
    let (status, body) = routes
        .iter()
        .find(|(fragment, _, _)| line.contains(fragment))
        .map_or_else(
            || (404, not_found(&format!("no route for {line}"))),
            |(_, status, body)| (*status, body.clone()),
        );
    Ok(Response::json(status, body))
}

/// Google's answer for something that does not exist: `404` with the reason `notFound`, as Drive
/// documents it (developers.google.com/workspace/drive/api/guides/handle-errors#notfound) and
/// Gmail answers for a message the mailbox no longer holds.
#[must_use]
pub fn not_found(message: &str) -> String {
    json!({"error": {
        "code": 404,
        "errors": [{"domain": "global", "reason": "notFound", "message": message}],
        "message": message
    }})
    .to_string()
}
