//! A fake Cloud Storage JSON API: object generations and the preconditions a lease needs.
//!
//! A lease is only as safe as the precondition each write carries, so this server implements
//! generations the way Cloud Storage documents them: `ifGenerationMatch=0` creates an object
//! that does not exist, `ifGenerationMatch=<n>` replaces exactly generation `n`, and a write
//! whose precondition does not hold is `412 Precondition Failed`
//! (cloud.google.com/storage/docs/request-preconditions). Every request is recorded, so a
//! test asserts the request Cloud Storage would have received rather than assuming it. An
//! error's body is JSON broken over lines, as Cloud Storage answers by default (`prettyPrint`
//! defaults to `true`, <https://cloud.google.com/storage/docs/json_api/v1/parameters>).

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};

use super::fake_http::{self, Response};

/// One request the fake storage API received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub authorization: Option<String>,
    pub body: String,
}

impl Recorded {
    /// The value of one query parameter.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    fn from_http(request: &fake_http::Recorded) -> Self {
        Self {
            method: request.method.clone(),
            path: request.path.clone(),
            query: query(&request.target),
            authorization: request.headers.get("authorization").cloned(),
            body: request.body.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Object {
    generation: i64,
    content: String,
}

#[derive(Default)]
struct Shared {
    objects: HashMap<String, Object>,
    requests: Vec<Recorded>,
    generations: i64,
    /// The answer every later request of a method gets, whatever it asks.
    refusals: HashMap<String, (u16, String)>,
}

/// A loopback Cloud Storage JSON API holding objects in memory.
pub struct FakeGcs {
    address: SocketAddr,
    shared: Arc<Mutex<Shared>>,
}

impl FakeGcs {
    /// Binds a loopback port and serves until the test process exits.
    ///
    /// # Errors
    ///
    /// Returns an error when the port cannot be bound.
    pub fn start() -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let serving = Arc::clone(&shared);
        let address = fake_http::serve("the fake storage API", move |request, _| {
            let recorded = Recorded::from_http(request);
            let mut state = serving
                .lock()
                .map_err(|_| anyhow!("the fake storage API's state was poisoned"))?;
            state.requests.push(recorded.clone());
            Ok(route(&mut state, &recorded))
        })?;
        Ok(Self { address, shared })
    }

    /// The base URL `CTX_STORAGE_URL` carries for this server.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The generation and decoded content of one object, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned or the content is not JSON.
    pub fn object(&self, name: &str) -> Result<Option<(i64, Value)>> {
        let shared = self.lock()?;
        shared
            .objects
            .get(name)
            .map(|object| {
                let content = serde_json::from_str(&object.content)
                    .with_context(|| format!("decoding the object {name}"))?;
                Ok((object.generation, content))
            })
            .transpose()
    }

    /// Every request the server received, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn requests(&self) -> Result<Vec<Recorded>> {
        Ok(self.lock()?.requests.clone())
    }

    /// The requests that wrote an object, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn writes(&self) -> Result<Vec<Recorded>> {
        Ok(self
            .requests()?
            .into_iter()
            .filter(|request| request.method == "POST")
            .collect())
    }

    /// The requests that deleted an object, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn deletes(&self) -> Result<Vec<Recorded>> {
        Ok(self
            .requests()?
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect())
    }

    /// Answers every later request of `method` with `status` and `message`, as Cloud Storage
    /// does when it is down or throttling.
    ///
    /// # Errors
    ///
    /// Returns an error when the state is poisoned.
    pub fn refuse(&self, method: &str, status: u16, message: &str) -> Result<()> {
        self.lock()?
            .refusals
            .insert(method.to_owned(), (status, message.to_owned()));
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Shared>> {
        self.shared
            .lock()
            .map_err(|_| anyhow!("the fake storage API's state was poisoned"))
    }
}

/// Answers one request the way the Cloud Storage JSON API answers it.
fn route(state: &mut Shared, request: &Recorded) -> Response {
    if request.authorization.is_none() {
        return error(401, "the storage API needs a bearer token");
    }
    if let Some((status, message)) = state.refusals.get(&request.method) {
        return error(*status, message);
    }
    let segments: Vec<&str> = request
        .path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["upload", "storage", "v1", "b", _bucket, "o"]) => upload(state, request),
        ("GET", ["storage", "v1", "b", _bucket, "o", name]) => read(state, request, name),
        ("DELETE", ["storage", "v1", "b", _bucket, "o", name]) => remove(state, request, name),
        _ => error(
            404,
            &format!("no route for {} {}", request.method, request.path),
        ),
    }
}

/// `POST /upload/storage/v1/b/<bucket>/o?uploadType=media&name=<name>&ifGenerationMatch=<n>`
fn upload(state: &mut Shared, request: &Recorded) -> Response {
    if request.parameter("uploadType") != Some("media") {
        return error(400, "a lease writes the object with uploadType=media");
    }
    let Some(name) = request.parameter("name").map(str::to_owned) else {
        return error(400, "the upload names no object");
    };
    let Some(expected) = request
        .parameter("ifGenerationMatch")
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return error(400, "every lease write carries ifGenerationMatch");
    };
    let current = state
        .objects
        .get(&name)
        .map_or(0, |object| object.generation);
    if current != expected {
        return error(
            412,
            &format!("object {name} is at generation {current}, not {expected}"),
        );
    }
    state.generations += 1;
    let generation = state.generations;
    state.objects.insert(
        name.clone(),
        Object {
            generation,
            content: request.body.clone(),
        },
    );
    metadata(&name, generation)
}

/// `GET /storage/v1/b/<bucket>/o/<name>`, with `alt=media` for the content.
fn read(state: &Shared, request: &Recorded, name: &str) -> Response {
    let name = decode(name);
    let Some(object) = state.objects.get(&name) else {
        return error(404, &format!("object {name} does not exist"));
    };
    if request.parameter("alt") == Some("media") {
        return Response::json(200, object.content.clone());
    }
    metadata(&name, object.generation)
}

/// `DELETE /storage/v1/b/<bucket>/o/<name>?ifGenerationMatch=<n>`
fn remove(state: &mut Shared, request: &Recorded, name: &str) -> Response {
    let name = decode(name);
    let Some(expected) = request
        .parameter("ifGenerationMatch")
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return error(400, "every lease delete carries ifGenerationMatch");
    };
    let current = state
        .objects
        .get(&name)
        .map_or(0, |object| object.generation);
    if current != expected {
        return error(
            412,
            &format!("object {name} is at generation {current}, not {expected}"),
        );
    }
    state.objects.remove(&name);
    Response::json(204, "")
}

fn metadata(name: &str, generation: i64) -> Response {
    Response::json(
        200,
        json!({
            "kind": "storage#object",
            "name": name,
            "generation": generation.to_string(),
        })
        .to_string(),
    )
}

fn error(status: u16, message: &str) -> Response {
    let body = json!({ "error": { "code": status, "message": message } });
    Response::json(
        status,
        serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
    )
}

/// The decoded query parameters of a request target.
fn query(target: &str) -> HashMap<String, String> {
    let (_, query) = target.split_once('?').unwrap_or((target, ""));
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

/// Decodes the percent escapes a client puts in a path segment or a query value.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'%') => {
                let pair = value.get(index + 1..index + 3).unwrap_or_default();
                if let Ok(byte) = u8::from_str_radix(pair, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            Some(byte) => {
                out.push(*byte);
                index += 1;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
