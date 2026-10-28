//! A loopback GitHub that demands a credential, which is what the graph repository is.
//!
//! It serves the two API endpoints `ctx` mints an installation token from, and the git remote
//! it then clones, pulls and pushes. Every git request without the right `Authorization`
//! header is answered `401`, as a private repository answers an anonymous one, so a test here
//! proves the credential reaches git rather than assuming it.
//!
//! git itself does the git half: the requests are handed to `git http-backend`, the CGI program
//! that serves the smart HTTP protocol.

#![allow(dead_code)]

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use super::fake_http::{self, Recorded, Response};

/// The installation this GitHub reports for the repository.
const INSTALLATION: u64 = 1_000_002;

/// The token this GitHub mints, and the only one its git remote accepts.
const TOKEN: &str = "ghs_fixture_installation_token";

/// One git request this GitHub received.
#[derive(Clone, Debug)]
pub struct GitRequest {
    pub path: String,
    /// Whether the request carried the `Authorization` header git was given.
    pub authorized: bool,
}

#[derive(Default)]
struct Seen {
    git: Vec<GitRequest>,
    minted: usize,
}

/// A loopback GitHub: an App API and a private git remote on one host.
pub struct FakeGithub {
    address: SocketAddr,
    /// Where the bare repositories live, one per `<owner>/<name>.git`.
    root: PathBuf,
    seen: Arc<Mutex<Seen>>,
}

impl FakeGithub {
    /// Serves `<owner>/<name>.git` from a bare repository under `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the port cannot be bound.
    pub fn start(root: &Path) -> Result<Self> {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let serving = Arc::clone(&seen);
        let served_root = root.to_path_buf();
        let address = fake_http::serve("the fake GitHub", move |request, body| {
            route(&served_root, &serving, request, body)
        })?;
        Ok(Self {
            address,
            root: root.to_path_buf(),
            seen,
        })
    }

    /// The base URL of this GitHub's API, for `CTX_GITHUB_API`.
    #[must_use]
    pub fn api(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The remote URL of one repository, for `CTX_MEMORY_REMOTE`.
    #[must_use]
    pub fn remote(&self, owner: &str, name: &str) -> String {
        format!("http://{}/{owner}/{name}.git", self.address)
    }

    /// Where the bare repository of one name lives on disk.
    #[must_use]
    pub fn bare(&self, owner: &str, name: &str) -> PathBuf {
        self.root.join(owner).join(format!("{name}.git"))
    }

    /// The app credential a caller needs to mint a token from this GitHub.
    #[must_use]
    pub fn app_credential() -> String {
        serde_json::json!({
            "app_id": "1234567",
            "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
        })
        .to_string()
    }

    /// Every git request this GitHub received, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn git_requests(&self) -> Result<Vec<GitRequest>> {
        Ok(self.lock()?.git.clone())
    }

    /// How many installation tokens were minted.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared state is poisoned.
    pub fn minted(&self) -> Result<usize> {
        Ok(self.lock()?.minted)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Seen>> {
        self.seen
            .lock()
            .map_err(|_| anyhow!("the fake GitHub's state was poisoned"))
    }
}

/// Creates `<owner>/<name>.git` under `root` with the content of `source`, and allows pushes.
///
/// # Errors
///
/// Returns an error when git refuses to create or fill the repository.
pub fn publish(root: &Path, owner: &str, name: &str, source: &Path) -> Result<PathBuf> {
    let bare = root.join(owner).join(format!("{name}.git"));
    std::fs::create_dir_all(bare.parent().unwrap_or(root))?;
    run_git(root, &["init", "--bare", "--initial-branch=main"], &[&bare])?;
    // `http-backend` serves a push only when the repository says it may.
    run_git(&bare, &["config", "http.receivepack", "true"], &[])?;
    let bare_text = bare.to_str().context("the bare path is not valid UTF-8")?;
    run_git(source, &["remote", "add", "origin", bare_text], &[])?;
    run_git(source, &["push", "--quiet", "origin", "main"], &[])?;
    Ok(bare)
}

fn run_git(directory: &Path, arguments: &[&str], paths: &[&Path]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .args(paths)
        .output()
        .with_context(|| format!("running git {}", arguments.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Answers the App API itself and hands an authorized git request to `git http-backend`.
fn route(root: &Path, seen: &Mutex<Seen>, request: &Recorded, body: &[u8]) -> Result<Response> {
    let path = request.path.as_str();
    if path.ends_with("/installation") {
        return Ok(Response::json(200, format!("{{\"id\":{INSTALLATION}}}")));
    }
    if path.ends_with("/access_tokens") {
        let expires = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        seen.lock()
            .map_err(|_| anyhow!("the fake GitHub's state was poisoned"))?
            .minted += 1;
        return Ok(Response::json(
            201,
            serde_json::json!({ "token": TOKEN, "expires_at": expires.to_rfc3339() }).to_string(),
        ));
    }

    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("x-access-token:{TOKEN}"))
    );
    let authorized = request.headers.get("authorization") == Some(&expected);
    seen.lock()
        .map_err(|_| anyhow!("the fake GitHub's state was poisoned"))?
        .git
        .push(GitRequest {
            path: path.to_owned(),
            authorized,
        });
    if !authorized {
        return Ok(
            Response::json(401, "{\"message\":\"Requires authentication\"}")
                .header("WWW-Authenticate", "Basic realm=\"GitHub\""),
        );
    }
    serve_git(root, request, body)
}

/// Hands one request to `git http-backend` and relays what it answers.
fn serve_git(root: &Path, request: &Recorded, body: &[u8]) -> Result<Response> {
    let query = request
        .target
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    let mut backend = Command::new("git");
    backend
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", &request.path)
        .env("QUERY_STRING", query)
        .env("REMOTE_USER", "x-access-token")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(content_type) = request.headers.get("content-type") {
        backend.env("CONTENT_TYPE", content_type);
    }
    if let Some(encoding) = request.headers.get("content-encoding") {
        backend.env("HTTP_CONTENT_ENCODING", encoding);
    }
    if let Some(protocol) = request.headers.get("git-protocol") {
        backend.env("HTTP_GIT_PROTOCOL", protocol);
    }
    let mut child = backend.spawn().context("starting git http-backend")?;
    {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git http-backend has no stdin"))?;
        input.write_all(body)?;
    }
    let output = child
        .wait_with_output()
        .context("waiting for git http-backend")?;

    // CGI answers with its own headers, then a blank line, then the body.
    let split = output
        .stdout
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
        .ok_or_else(|| anyhow!("git http-backend printed no headers"))?;
    let (head, body) = output.stdout.split_at(split);
    let head = String::from_utf8_lossy(head);
    let mut response = Response {
        status: 200,
        headers: Vec::new(),
        body: body.to_vec(),
    };
    for line in head.lines().filter(|line| !line.trim().is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name == "Status" {
            response.status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
            continue;
        }
        response = response.header(name, value.trim());
    }
    Ok(response)
}
