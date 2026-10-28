//! A tiny HTTP server for tests: canned answers per path, and a record of what was asked.
//!
//! The identity, credential and GitHub paths all talk to HTTP services. The tests drive the
//! real client code against this, so the request a service would receive is asserted rather
//! than assumed.
//!
//! [`serve`] is the transport every fake in this tree shares: it reads one request into a
//! [`Recorded`], hands it to the fake's routing, and writes the [`Response`] back. A fake is
//! only its routing.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _, Result};

/// One request the fake server received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// The full request target, query string and all, exactly as it arrived.
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// One answer: a status, its headers and the body. The transport adds `Content-Length` and
/// `Connection: close`.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A JSON answer.
    #[must_use]
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.into().into_bytes(),
        }
    }

    /// This answer with one more header.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// Binds a loopback port and answers each connection on its own thread until the test process
/// exits. `route` gets the request and the exact bytes of its body; when it fails, the
/// connection closes without an answer.
///
/// # Errors
///
/// Returns an error when the port cannot be bound.
pub fn serve<F>(name: &str, route: F) -> Result<SocketAddr>
where
    F: Fn(&Recorded, &[u8]) -> Result<Response> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").with_context(|| format!("binding {name}"))?;
    let address = listener.local_addr()?;
    let route = Arc::new(route);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let route = Arc::clone(&route);
            std::thread::spawn(move || {
                let _ = exchange(stream, route.as_ref());
            });
        }
    });
    Ok(address)
}

fn exchange<F>(mut stream: TcpStream, route: &F) -> Result<()>
where
    F: Fn(&Recorded, &[u8]) -> Result<Response>,
{
    let Some((request, body)) = read_request(&stream)? else {
        return Ok(());
    };
    let response = route(&request, &body)?;
    write_response(&mut stream, &response)
}

/// Reads one request, or `None` when the client closed the connection without sending one.
fn read_request(stream: &TcpStream) -> Result<Option<(Recorded, Vec<u8>)>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    let body = read_body(&mut reader, &headers)?;
    let path = target
        .split('?')
        .next()
        .unwrap_or(target.as_str())
        .to_owned();
    let request = Recorded {
        method,
        path,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    Ok(Some((request, body)))
}

/// Reads the request body, which a client sends either with a length or, as git does, in
/// chunks.
fn read_body(
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
) -> Result<Vec<u8>> {
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        let mut body = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let size = usize::from_str_radix(line.trim(), 16).unwrap_or(0);
            if size == 0 {
                // The trailing CRLF of the last chunk.
                let mut end = String::new();
                let _ = reader.read_line(&mut end);
                break;
            }
            let mut chunk = vec![0_u8; size];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0_u8; 2];
            reader.read_exact(&mut crlf)?;
        }
        return Ok(body);
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(body)
}

fn write_response(stream: &mut TcpStream, response: &Response) -> Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    for (name, value) in &response.headers {
        write!(head, "{name}: {value}\r\n")?;
    }
    write!(
        head,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    )?;
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        412 => "Precondition Failed",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

#[derive(Default)]
struct Shared {
    /// The answers queued per path; the last one repeats.
    routes: HashMap<String, Vec<Response>>,
    requests: Vec<Recorded>,
}

/// A loopback HTTP server that answers the paths a test registers.
pub struct FakeHttp {
    address: SocketAddr,
    shared: Arc<Mutex<Shared>>,
}

impl FakeHttp {
    /// Binds a loopback port and serves until the test process exits.
    ///
    /// # Errors
    ///
    /// Returns an error when the port cannot be bound.
    pub fn start() -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let serving = Arc::clone(&shared);
        let address = serve("the fake server", move |request, _| {
            answer(&serving, request)
        })?;
        Ok(Self { address, shared })
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// Answers `path` with `body` as JSON, replacing any earlier answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn json(&self, path: &str, status: u16, body: &str) -> Result<()> {
        self.json_with_headers(path, status, &[], body)
    }

    /// Adds one more answer for `path`, used after the answers already queued.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn then_json(&self, path: &str, status: u16, body: &str) -> Result<()> {
        self.then_json_with_headers(path, status, &[], body)
    }

    /// Answers `path` with `body` and these response headers, replacing any earlier answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn json_with_headers(
        &self,
        path: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<()> {
        let mut shared = self.lock()?;
        shared
            .routes
            .insert(path.to_owned(), vec![canned(status, headers, body)]);
        Ok(())
    }

    /// Adds one more answer with headers for `path`, used after the answers already queued.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn then_json_with_headers(
        &self,
        path: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<()> {
        let mut shared = self.lock()?;
        shared
            .routes
            .entry(path.to_owned())
            .or_default()
            .push(canned(status, headers, body));
        Ok(())
    }

    /// Every request the server received, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn requests(&self) -> Result<Vec<Recorded>> {
        Ok(self.lock()?.requests.clone())
    }

    /// How many requests arrived for one path.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn hits(&self, path: &str) -> Result<usize> {
        Ok(self
            .requests()?
            .iter()
            .filter(|request| request.path == path)
            .count())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Shared>> {
        self.shared
            .lock()
            .map_err(|_| anyhow!("the fake server's state was poisoned"))
    }
}

/// Records the request and answers with the next answer queued for its path.
fn answer(shared: &Mutex<Shared>, request: &Recorded) -> Result<Response> {
    let mut state = shared
        .lock()
        .map_err(|_| anyhow!("the fake server's state was poisoned"))?;
    state.requests.push(request.clone());
    let queued = match state.routes.get_mut(&request.path) {
        Some(answers) if answers.len() > 1 => Some(answers.remove(0)),
        Some(answers) => answers.first().cloned(),
        None => None,
    };
    Ok(queued.unwrap_or_else(|| {
        Response::json(
            404,
            format!("{{\"error\":\"no answer for {}\"}}", request.path),
        )
    }))
}

fn canned(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
    headers
        .iter()
        .fold(Response::json(status, body), |response, (name, value)| {
            response.header(name, value)
        })
}
