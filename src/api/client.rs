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
    json_reply(send(request, "GET", url)?)
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
        .with_context(|| format!("reading the body of {url}"))?;
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
/// Returns an error when the request cannot be sent or the body is not JSON.
pub fn post_form(url: &str, form: &[(&str, String)]) -> Result<JsonReply> {
    let request = client()?.post(url).form(form);
    json_reply(send(request, "POST", url)?)
}

/// `POST url` with a form-encoded body, authenticated as an OAuth client with HTTP Basic
/// (RFC 6749 section 2.3.1), parsed as JSON. The client secret travels only in the
/// `Authorization` header, never in the body or the URL.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON.
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
    json_reply(send(request, "POST", url)?)
}

/// `POST url` with a JSON body and `Authorization: Bearer <bearer>`, parsed as JSON.
///
/// # Errors
///
/// Returns an error when the request cannot be sent or the body is not JSON.
pub fn post_json(url: &str, bearer: &str, body: &Value) -> Result<JsonReply> {
    let request = authorized(client()?.post(url), bearer).json(body);
    json_reply(send(request, "POST", url)?)
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

fn send(request: RequestBuilder, method: &str, url: &str) -> Result<Response> {
    request.send().with_context(|| format!("{method} {url}"))
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

fn json_reply(response: Response) -> Result<JsonReply> {
    let status = response.status();
    let headers = response.headers().clone();
    let url = response.url().clone();
    let body = response
        .text()
        .with_context(|| format!("reading the body of {url}"))?;
    let value = parse_json(&body, status, url.as_str())?;
    Ok((status, headers, value))
}

fn parse_json(body: &str, status: StatusCode, url: &str) -> Result<Value> {
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(body).with_context(|| {
        let mut excerpt: String = body.chars().take(200).collect();
        if body.chars().count() > 200 {
            excerpt.push('…');
        }
        format!("{url} returned {status} with a body that is not JSON: {excerpt}")
    })
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
            let listener = TcpListener::bind("127.0.0.1:0").context("binding a fake server")?;
            let port = listener
                .local_addr()
                .context("reading the fake port")?
                .port();
            let handle = std::thread::spawn(move || serve_once(&listener, response, &body));
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
}
