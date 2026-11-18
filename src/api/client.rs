//! HTTP calls to external JSON APIs.
//!
//! Tokens travel in the `Authorization` header, never in a URL or an argument. Every call
//! returns the response status and headers beside its body, so a caller can act on a rate
//! limit or a `Link` header without a second request.

use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde_json::Value;

/// One HTTP round trip may take this long before the client gives up.
const TIMEOUT: Duration = Duration::from_secs(90);

/// A status, response headers, and a parsed JSON body.
pub type JsonReply = (StatusCode, HeaderMap, Value);

/// A request that got no whole answer: it could not connect, timed out, or the connection
/// closed or reset before the reply ended. It carries no HTTP status, so a caller whose request
/// is safe to repeat may send it again. Its message names the cause first and the request
/// after it, without the query string, so a log line cut to a few hundred characters still
/// says why however long the URL is.
#[derive(Debug)]
pub struct NoAnswer(pub(crate) String);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NoAnswer {}

/// A refusal whose body ended early: the status and headers came, the body did not. The status
/// is the answer, so a caller that acts on statuses (GitHub's throttle) reads them from here;
/// any other caller gets the cause first and the request, as for [`NoAnswer`]. It is never a
/// [`NoAnswer`], since the server did answer.
#[derive(Debug)]
pub struct CutShort {
    pub status: StatusCode,
    pub headers: HeaderMap,
    message: String,
}

impl std::fmt::Display for CutShort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CutShort {}

/// `GET url` with `Authorization: Bearer <bearer>`, parsed as JSON.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON.
pub fn get_json(url: &str, bearer: &str, query: &[(&str, String)]) -> Result<JsonReply> {
    get_json_with_headers(url, bearer, query, &[])
}

/// `GET url` with `Authorization: Bearer <bearer>` and extra request headers.
///
/// # Errors
///
/// Returns an error when a header is invalid, the request cannot be sent, or the body is not
/// JSON.
pub fn get_json_with_headers(
    url: &str,
    bearer: &str,
    query: &[(&str, String)],
    headers: &[(&str, String)],
) -> Result<JsonReply> {
    let mut request = authorized(client()?.get(url), bearer).query(query);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    json_reply(send(request, "GET", url)?, "GET", Answer::Data)
}

/// `GET url` returning at most `limit` bytes, and whether the body was longer than that.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body cannot be read.
pub fn get_bytes(
    url: &str,
    bearer: &str,
    query: &[(&str, String)],
    limit: usize,
) -> Result<(StatusCode, HeaderMap, Vec<u8>, bool)> {
    let request = authorized(client()?.get(url), bearer).query(query);
    let response = send(request, "GET", url)?;
    let status = response.status();
    let headers = response.headers().clone();
    let ceiling = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    response
        .take(ceiling)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            no_answer(
                &error,
                &format!("reading the body of GET {}", without_query(url)),
            )
        })?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((status, headers, bytes, truncated))
}

/// `POST url` with a form-encoded body and no credential, parsed as JSON.
///
/// OAuth token endpoints take their parameters this way.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON. A body that is
/// not JSON is refused without any of its characters, since a token endpoint can answer a
/// token in a form-encoded body.
pub fn post_form(url: &str, form: &[(&str, String)]) -> Result<JsonReply> {
    let request = client()?.post(url).form(form);
    json_reply(send(request, "POST", url)?, "POST", Answer::Credential)
}

/// `POST url` with a form-encoded body, authenticated as an OAuth client with HTTP Basic
/// (RFC 6749 section 2.3.1), parsed as JSON. The client secret travels only in the
/// `Authorization` header, never in the body or the URL.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON. A body that is
/// not JSON is refused without any of its characters, since a token endpoint can answer a
/// token in a form-encoded body.
pub fn post_form_basic(
    url: &str,
    client_id: &str,
    client_secret: &str,
    form: &[(&str, String)],
) -> Result<JsonReply> {
    let request = client()?
        .post(url)
        .basic_auth(client_id, Some(client_secret))
        .form(form);
    json_reply(send(request, "POST", url)?, "POST", Answer::Credential)
}

/// `POST url` with a JSON body and `Authorization: Bearer <bearer>`, parsed as JSON.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON.
pub fn post_json(url: &str, bearer: &str, body: &Value) -> Result<JsonReply> {
    let request = authorized(client()?.post(url), bearer).json(body);
    json_reply(send(request, "POST", url)?, "POST", Answer::Data)
}

/// Percent-encodes `value` for a URL path segment or query value, so every byte outside
/// RFC 3986's unreserved set arrives as data. A raw `+`, `&`, `#`, `/`, `@`, or space would
/// otherwise change the shape of the URL — GitHub answers `404` for a branch such as
/// `v3.3.1+qvar` unless the `+` is encoded.
#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

fn authorized(request: RequestBuilder, bearer: &str) -> RequestBuilder {
    if bearer.is_empty() {
        request
    } else {
        request.bearer_auth(bearer)
    }
}

/// Sends `request`. A failure to get an answer is a [`NoAnswer`]; a request that could not be
/// built, or a redirect loop, is a plain error with the same cause-first message.
fn send(request: RequestBuilder, method: &str, url: &str) -> Result<Response> {
    request.send().map_err(|error| {
        let refused = error.is_builder() || error.is_redirect();
        let error = error.without_url();
        let request = format!("on {method} {}", without_query(url));
        if refused {
            anyhow!("{}, {request}", root_cause(&error))
        } else {
            no_answer(&error, &request)
        }
    })
}

/// A [`NoAnswer`] for `error`, which happened while doing `what`.
fn no_answer(error: &(dyn std::error::Error + 'static), what: &str) -> anyhow::Error {
    anyhow::Error::new(NoAnswer(format!("{}, {what}", root_cause(error))))
}

/// The innermost error under `error`, which says what happened on the wire (`connection
/// closed before message completed`, `operation timed out`, `Connection refused (os error
/// 111)`). The errors that wrap it say only that a request failed, and the request is named
/// beside it.
fn root_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut cause = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

/// `url` without its query string, which can be longer than the rest and says nothing a log
/// needs to tell one request from another of a run.
fn without_query(url: &str) -> &str {
    url.split_once('?').map_or(url, |(path, _)| path)
}

/// One client, so connections and the TLS session cache are shared by every call.
static CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("ctx-memory/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())
});

fn client() -> Result<&'static Client> {
    CLIENT
        .as_ref()
        .map_err(|error| anyhow!("building the HTTP client: {error}"))
}

/// Whether an answer may carry a credential, which decides what a refusal of its body may
/// show.
#[derive(Clone, Copy)]
enum Answer {
    /// Data an API serves, which a refusal may quote to say what came instead.
    Data,
    /// A credential fetched or refreshed, which may arrive in a body that is not JSON (GitHub
    /// answers `access_token=…` form-encoded unless asked for JSON), so a refusal quotes none
    /// of it.
    Credential,
}

/// The status, headers and JSON body of `response`, an answer of the kind `answer`, which
/// decides what a refusal of the body may show. A body that ends early is a [`NoAnswer`] when
/// the status is a success, since the answer is missing, and a [`CutShort`] carrying the status
/// and headers otherwise.
fn json_reply(response: Response, method: &str, answer: Answer) -> Result<JsonReply> {
    let status = response.status();
    let headers = response.headers().clone();
    let url = response.url().clone();
    let request = format!("{method} {}", without_query(url.as_str()));
    let body = match response.text() {
        Ok(body) => body,
        Err(error) if status.is_success() => {
            return Err(no_answer(
                &error.without_url(),
                &format!("reading the body of {request}"),
            ))
        }
        Err(error) => {
            let message = format!(
                "{}, reading the body of the {status} answer to {request}",
                root_cause(&error.without_url())
            );
            return Err(anyhow::Error::new(CutShort {
                status,
                headers,
                message,
            }));
        }
    };
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("no content type");
    let value = parse_json(&body, status, &request, answer, content_type)?;
    Ok((status, headers, value))
}

/// `body` as JSON; an empty body is `null`. A body that is not JSON says so first, then which
/// request it answered. For [`Answer::Data`] it then shows how the body starts, which says what
/// came instead. For [`Answer::Credential`] it names only the body's `content_type` and length,
/// never a character of it, and carries no source, so not even the parser's message is in it.
fn parse_json(
    body: &str,
    status: StatusCode,
    request: &str,
    answer: Answer,
    content_type: &str,
) -> Result<Value> {
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    match answer {
        Answer::Data => serde_json::from_str(body).with_context(|| {
            let mut excerpt: String = body.chars().take(200).collect();
            if body.chars().count() > 200 {
                excerpt.push('…');
            }
            format!("{status} with a body that is not JSON, from {request}: {excerpt}")
        }),
        Answer::Credential => serde_json::from_str(body).map_err(|_| {
            anyhow!(
                "{status} with a body that is not JSON ({content_type}, {} bytes), from {request}",
                body.len()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    use anyhow::anyhow;

    use super::*;

    #[test]
    fn percent_encode_escapes_bytes_outside_the_unreserved_set() {
        assert_eq!(percent_encode("team@example.com"), "team%40example.com");
        assert_eq!(percent_encode("v3.3.1+qvar"), "v3.3.1%2Bqvar");
    }

    /// One request a fake server received.
    struct Received {
        head: String,
        body: Vec<u8>,
    }

    /// A single-request HTTP server on loopback.
    struct FakeServer {
        url: String,
        handle: JoinHandle<Result<Received>>,
    }

    impl FakeServer {
        fn start(response: &'static str, body: Vec<u8>) -> Result<Self> {
            Self::serve(move |listener| serve_once(listener, response, &body))
        }

        /// A server that reads the request and closes the connection without answering it.
        fn hang_up() -> Result<Self> {
            Self::serve(|listener| {
                let (mut stream, _) = listener.accept().context("accepting a fake request")?;
                read_request(&mut stream)
            })
        }

        /// A server that answers with `response` and a `Content-Length` longer than the body
        /// it sends, then closes: the reply ends before its body does.
        fn cut_short(response: &'static str) -> Result<Self> {
            Self::serve(move |listener| {
                let (mut stream, _) = listener.accept().context("accepting a fake request")?;
                let received = read_request(&mut stream)?;
                let reply = format!("HTTP/1.1 {response}\r\nContent-Length: 100\r\n\r\n{{\"a\"");
                stream
                    .write_all(reply.as_bytes())
                    .context("writing a fake reply")?;
                stream.flush().context("flushing a fake reply")?;
                Ok(received)
            })
        }

        fn serve(
            answer: impl FnOnce(&TcpListener) -> Result<Received> + Send + 'static,
        ) -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").context("binding a fake server")?;
            let port = listener
                .local_addr()
                .context("reading the fake port")?
                .port();
            let handle = std::thread::spawn(move || answer(&listener));
            Ok(Self {
                url: format!("http://127.0.0.1:{port}"),
                handle,
            })
        }

        fn received(self) -> Result<Received> {
            self.handle
                .join()
                .map_err(|_| anyhow!("the fake server thread failed"))?
        }
    }

    fn serve_once(listener: &TcpListener, response: &str, body: &[u8]) -> Result<Received> {
        let (mut stream, _) = listener.accept().context("accepting a fake request")?;
        let received = read_request(&mut stream)?;
        let mut reply = format!(
            "HTTP/1.1 {response}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        reply.extend_from_slice(body);
        stream.write_all(&reply).context("writing a fake reply")?;
        stream.flush().context("flushing a fake reply")?;
        Ok(received)
    }

    fn read_request(stream: &mut TcpStream) -> Result<Received> {
        let mut buffer = Vec::new();
        let mut byte = [0_u8; 1];
        while !buffer.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut byte).context("reading a fake request")?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&byte);
        }
        let head = String::from_utf8_lossy(&buffer).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        let mut body = vec![0_u8; length];
        if length > 0 {
            stream
                .read_exact(&mut body)
                .context("reading a fake request body")?;
        }
        Ok(Received { head, body })
    }

    /// HTTP header names are case-insensitive, and hyper sends them lowercased.
    fn has_header(head: &str, line: &str) -> bool {
        head.to_ascii_lowercase()
            .contains(&line.to_ascii_lowercase())
    }

    #[test]
    fn get_json_sends_the_bearer_and_query_and_returns_the_body() -> Result<()> {
        let server = FakeServer::start("200 OK", br#"{"ok":true}"#.to_vec())?;
        let (status, _headers, body) = get_json(
            &format!("{}/drive/v3/files", server.url),
            "token-example",
            &[("q", "name = 'a b'".to_owned())],
        )?;
        let received = server.received()?;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true));
        assert!(has_header(
            &received.head,
            "Authorization: Bearer token-example"
        ));
        assert!(
            received
                .head
                .contains("GET /drive/v3/files?q=name+%3D+%27a+b%27 HTTP/1.1"),
            "{}",
            received.head
        );
        Ok(())
    }

    #[test]
    fn get_json_returns_the_status_of_an_error_reply() -> Result<()> {
        let server = FakeServer::start(
            "404 Not Found",
            br#"{"error":{"message":"not found"}}"#.to_vec(),
        )?;
        let (status, _headers, body) = get_json(&server.url, "token-example", &[])?;
        server.received()?;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body.pointer("/error/message").and_then(Value::as_str),
            Some("not found")
        );
        Ok(())
    }

    #[test]
    fn get_json_with_headers_sends_a_conditional_request_and_reads_an_empty_body() -> Result<()> {
        let server = FakeServer::start("304 Not Modified", Vec::new())?;
        let (status, _headers, body) = get_json_with_headers(
            &server.url,
            "token-example",
            &[],
            &[("If-None-Match", "\"etag-example\"".to_owned())],
        )?;
        let received = server.received()?;

        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(body, Value::Null);
        assert!(has_header(
            &received.head,
            "If-None-Match: \"etag-example\""
        ));
        Ok(())
    }

    #[test]
    fn get_bytes_stops_at_the_limit_and_reports_truncation() -> Result<()> {
        let server = FakeServer::start("200 OK", vec![b'x'; 4096])?;
        let (status, _headers, bytes, truncated) =
            get_bytes(&server.url, "token-example", &[], 128)?;
        server.received()?;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes.len(), 128);
        assert!(truncated);
        Ok(())
    }

    #[test]
    fn get_bytes_reports_a_complete_body() -> Result<()> {
        let server = FakeServer::start("200 OK", b"short".to_vec())?;
        let (_status, _headers, bytes, truncated) =
            get_bytes(&server.url, "token-example", &[], 128)?;
        server.received()?;

        assert_eq!(bytes, b"short");
        assert!(!truncated);
        Ok(())
    }

    #[test]
    fn post_form_sends_url_encoded_fields_without_a_credential() -> Result<()> {
        let server = FakeServer::start("200 OK", br#"{"access_token":"a"}"#.to_vec())?;
        let (_status, _headers, body) = post_form(
            &server.url,
            &[
                ("grant_type", "refresh_token".to_owned()),
                ("refresh_token", "1//secret value".to_owned()),
            ],
        )?;
        let received = server.received()?;

        assert_eq!(body.get("access_token").and_then(Value::as_str), Some("a"));
        assert_eq!(
            String::from_utf8_lossy(&received.body),
            "grant_type=refresh_token&refresh_token=1%2F%2Fsecret+value"
        );
        assert!(!has_header(&received.head, "Authorization"));
        assert!(has_header(
            &received.head,
            "content-type: application/x-www-form-urlencoded"
        ));
        Ok(())
    }

    /// The head of a token endpoint's answer in a form-encoded body.
    const FORM_TOKEN_HEAD: &str = "200 OK\r\nContent-Type: application/x-www-form-urlencoded";

    /// A token in a form-encoded body, the way GitHub answers a token request that does not
    /// ask for JSON.
    const FORM_TOKEN: &str =
        "access_token=fixture-secret-not-a-real-token&token_type=bearer&scope=repo";

    /// `error`, the refusal of a [`FORM_TOKEN`] answer, names its status, content type and
    /// length, carries no source, and shows no character of the body.
    fn assert_refused_without_the_body(error: &anyhow::Error) {
        let message = format!("{error:#}");
        assert!(
            message.contains("200 OK with a body that is not JSON"),
            "{message}"
        );
        assert!(
            message.contains("application/x-www-form-urlencoded"),
            "{message}"
        );
        assert!(
            message.contains(&format!("{} bytes", FORM_TOKEN.len())),
            "{message}"
        );
        assert!(
            !message.contains("fixture-secret-not-a-real-token"),
            "{message}"
        );
        assert!(!message.contains("access_token"), "{message}");
        assert_eq!(error.chain().count(), 1, "{message}");
    }

    /// A token endpoint can answer a token in a body that is not JSON, so its refusal names the
    /// status, content type and length of the body and none of its characters.
    #[test]
    fn a_token_answer_that_is_not_json_is_refused_without_its_body() -> Result<()> {
        let server = FakeServer::start(FORM_TOKEN_HEAD, FORM_TOKEN.as_bytes().to_vec())?;
        let error = post_form(
            &format!("{}/oauth/token", server.url),
            &[("grant_type", "refresh_token".to_owned())],
        )
        .err()
        .context("a form-encoded token answer was read as JSON")?;
        server.received()?;

        assert_refused_without_the_body(&error);
        Ok(())
    }

    /// The same refusal for a token request authenticated as an OAuth client.
    #[test]
    fn post_form_basic_refuses_a_token_answer_without_its_body() -> Result<()> {
        let server = FakeServer::start(FORM_TOKEN_HEAD, FORM_TOKEN.as_bytes().to_vec())?;
        let error = post_form_basic(
            &format!("{}/oauth/token", server.url),
            "client-example",
            "fixture-client-secret",
            &[("grant_type", "refresh_token".to_owned())],
        )
        .err()
        .context("a form-encoded token answer was read as JSON")?;
        server.received()?;

        assert_refused_without_the_body(&error);
        Ok(())
    }

    /// An answer that carries no credential and is not JSON shows how it starts, which says
    /// what came instead.
    #[test]
    fn a_data_answer_that_is_not_json_shows_how_it_starts() -> Result<()> {
        let server = FakeServer::start(
            "503 Service Unavailable\r\nContent-Type: text/html",
            b"<html>maintenance</html>".to_vec(),
        )?;
        let error = get_json(&server.url, "token-example", &[])
            .err()
            .context("an HTML page was read as JSON")?;
        server.received()?;

        let message = format!("{error:#}");
        assert!(
            message.contains("503 Service Unavailable with a body that is not JSON"),
            "{message}"
        );
        assert!(message.contains("<html>maintenance</html>"), "{message}");
        Ok(())
    }

    #[test]
    fn post_json_sends_the_body_with_the_bearer() -> Result<()> {
        let server = FakeServer::start("200 OK", br#"{"ok":true}"#.to_vec())?;
        let (_status, _headers, _body) = post_json(
            &server.url,
            "token-example",
            &serde_json::json!({"instances": [{"content": "hello"}]}),
        )?;
        let received = server.received()?;

        assert!(has_header(
            &received.head,
            "Authorization: Bearer token-example"
        ));
        assert_eq!(
            String::from_utf8_lossy(&received.body),
            r#"{"instances":[{"content":"hello"}]}"#
        );
        Ok(())
    }

    /// A request that gets no answer fails with the cause first, so a log line cut to a few
    /// hundred characters still says it, and with the URL once, without its query.
    #[test]
    fn a_request_that_gets_no_answer_names_the_cause_before_the_url_once() -> Result<()> {
        let probe = FakeServer::hang_up()?;
        let expected = root_cause(
            &reqwest::blocking::get(&probe.url)
                .err()
                .context("a request the fake server hung up on succeeded")?,
        );
        probe.received()?;

        let server = FakeServer::hang_up()?;
        let path = format!("{}/repos/example-org/example-repo/issues", server.url);
        let error = get_json(
            &format!("{path}?state=all&since=2026-10-26T00%3A00%3A00Z&per_page=100"),
            "token-example",
            &[],
        )
        .err()
        .context("a request the fake server hung up on succeeded")?;
        server.received()?;

        let message = format!("{error:#}");
        assert!(message.starts_with(&expected), "{expected:?}: {message}");
        assert_eq!(message.matches(&path).count(), 1, "{message}");
        assert!(!message.contains("since="), "{message}");
        assert!(message.contains(&format!("GET {path}")), "{message}");
        Ok(())
    }

    /// A success whose body ends early has no answer to give: it is a [`NoAnswer`], so a
    /// caller that may repeat the request does.
    #[test]
    fn a_success_whose_body_is_cut_short_is_no_answer() -> Result<()> {
        let server = FakeServer::cut_short("200 OK")?;
        let error = get_json(&server.url, "token-example", &[])
            .err()
            .context("a body cut short was read whole")?;
        server.received()?;

        assert!(error.downcast_ref::<NoAnswer>().is_some(), "{error:#}");
        assert!(
            format!("{error:#}").contains("reading the body of GET"),
            "{error:#}"
        );
        Ok(())
    }

    /// A refusal whose body ends early is still the refusal: the error carries its status and
    /// headers, so a throttle is seen as one, and names the cause and the request for any other
    /// caller. It is never a [`NoAnswer`], so nothing sends it again.
    #[test]
    fn a_refusal_whose_body_is_cut_short_keeps_its_status() -> Result<()> {
        let server = FakeServer::cut_short("429 Too Many Requests")?;
        let url = server.url.clone();
        let error = get_json(&server.url, "token-example", &[])
            .err()
            .context("a body cut short was read whole")?;
        server.received()?;

        let cut = error
            .downcast_ref::<CutShort>()
            .with_context(|| format!("not a CutShort: {error:#}"))?;
        assert_eq!(cut.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(error.downcast_ref::<NoAnswer>().is_none(), "{error:#}");
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!(
                "reading the body of the 429 Too Many Requests answer to GET {url}"
            )),
            "{message}"
        );
        Ok(())
    }
}
