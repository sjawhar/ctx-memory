//! Shared helpers for the `ctx serve` tests: scratch trees and a server child process.

#![allow(dead_code)]

pub mod fake_embed;
pub mod fake_gcs;
pub mod fake_github;
pub mod fake_google;
pub mod fake_http;

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

/// Google's signing keys as a test key set, and the private key that matches them.
pub const JWKS: &str = include_str!("../fixtures/jwks.json");
const SIGNING_KEY: &str = include_str!("../fixtures/jwt-signing-key.pem");
/// The key id the fixture key set publishes.
pub const KEY_ID: &str = "ctx-test-key";
/// The issuer Google puts in an identity token.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";

/// Mints an identity token shaped like Google's, signed with the fixture key.
pub fn google_token(audience: &str, email: &str) -> Result<String> {
    sign_token(audience, email, GOOGLE_ISSUER, KEY_ID)
}

/// Mints an identity token with every field a test may want to spoil.
pub fn sign_token(audience: &str, email: &str, issuer: &str, key_id: &str) -> Result<String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(SIGNING_KEY.as_bytes())
        .context("reading the fixture signing key")?;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(key_id.to_owned());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("reading the clock")?
        .as_secs();
    let claims = json!({
        "iss": issuer,
        "aud": audience,
        "sub": "100000000000000000001",
        "email": email,
        "email_verified": true,
        "iat": now,
        "exp": now + 3600,
    });
    jsonwebtoken::encode(&header, &claims, &key).context("signing a test identity token")
}

/// A fresh empty directory under the system temporary directory.
pub fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-{label}-{}-{suffix}",
        std::process::id()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

pub fn remove_scratch(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// Writes `script` to `path` as an executable, from a child process.
///
/// Tests run on many threads, and a child another thread forks holds a copy of every file this
/// process has open until it execs. A program this process wrote itself could be held open for
/// writing by such a child when git or ctx runs it, which then fails with "Text file busy".
/// Only the short-lived `sh` below holds the file open, and it starts nothing else.
pub fn write_executable(path: &Path, script: &str) -> Result<()> {
    let mut writer = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the writer of {}", path.display()))?;
    std::io::Write::write_all(
        &mut writer
            .stdin
            .take()
            .ok_or_else(|| anyhow!("the writer has no standard input"))?,
        script.as_bytes(),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    let status = writer.wait()?;
    if !status.success() {
        bail!("writing {} failed: {status}", path.display());
    }
    Ok(())
}

/// Waits until `path` exists, or fails after two minutes with `what`.
pub fn wait_for_path(path: &Path, what: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            bail!("{what} never happened");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// A small tree with one decision record, with its own state and data directories.
///
/// Each fixture keeps its own directories, so one test cannot see another's windows.
pub fn fixture_tree(label: &str) -> Result<PathBuf> {
    let root = scratch(label)?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    write_tree(&tree)?;
    Ok(tree)
}

pub fn write_tree(tree: &Path) -> Result<()> {
    std::fs::write(tree.join("ctx.toml"), "version = 1\nid = \"server-test\"\n")?;
    std::fs::write(tree.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(
        tree.join("decisions/renewal.md"),
        "---\ntype: decision\nstatus: open\ntitle: Renewal terms\n---\nThe vendor renewal keeps the quarterly cadence.\n",
    )?;
    Ok(())
}

/// Removes the whole fixture, tree and directories alike.
pub fn remove_fixture(tree: &Path) -> Result<()> {
    let root = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?;
    remove_scratch(root)
}

/// The state and data directories one fixture tree uses.
pub fn tree_env(tree: &Path) -> Result<Vec<(String, String)>> {
    let root = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?;
    let text = |path: PathBuf| -> Result<String> {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("path is not valid UTF-8"))
    };
    Ok(vec![
        ("CTX_STATE_DIR".to_owned(), text(root.join("state"))?),
        ("CTX_DATA_DIR".to_owned(), text(root.join("data"))?),
    ])
}

/// A tree that is a clone of a bare remote, as the server's own tree is.
///
/// Returns the clone. The bare remote is `remote.git` beside it, and its source is the echo
/// fixture, so a test can fetch a real window and cite the records it stored.
pub fn fixture_clone(label: &str) -> Result<PathBuf> {
    let root = scratch(label)?;
    let bare = root.join("remote.git");
    git(
        &root,
        &["init", "--bare", "--initial-branch=main", "remote.git"],
    )?;
    let seed = root.join("seed");
    git(&root, &["init", "--initial-branch=main", "seed"])?;
    write_source_tree(&seed)?;
    git(&seed, &["add", "."])?;
    commit(&seed, "the starting tree")?;
    let bare_text = bare.to_str().context("remote path is not valid UTF-8")?;
    git(&seed, &["remote", "add", "origin", bare_text])?;
    git(&seed, &["push", "--quiet", "origin", "main"])?;
    std::fs::remove_dir_all(&seed)?;
    git(&root, &["clone", "--quiet", bare_text, "tree"])?;
    Ok(root.join("tree"))
}

/// The bare remote beside a cloned fixture tree.
pub fn remote_of(tree: &Path) -> Result<PathBuf> {
    let root = tree
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", tree.display()))?;
    Ok(root.join("remote.git"))
}

/// A tree whose only source is the echo fixture, so a fetch stores citable records.
pub fn write_source_tree(tree: &Path) -> Result<()> {
    std::fs::create_dir_all(tree)?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"server-write\"\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
    )?;
    std::fs::write(tree.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
    std::fs::write(
        tree.join("index.md"),
        "---\ntype: index\n---\n# Index\n\nRecords live under decisions/.\n",
    )?;
    std::fs::create_dir_all(tree.join("decisions"))?;
    Ok(())
}

/// Runs git in a directory and returns its trimmed standard output.
pub fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
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

/// Commits everything staged in a repository under a fixed test identity.
///
/// Signing is off. A fixture commit means nothing about who made it, and a developer whose
/// `commit.gpgsign` is on would otherwise have every fixture commit here signed with their own
/// key — which is both a false assertion and, through the signing program, the slowest step of
/// these tests.
pub fn commit(directory: &Path, message: &str) -> Result<String> {
    git(
        directory,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--message",
            message,
        ],
    )?;
    git(directory, &["rev-parse", "HEAD"])
}

/// Runs one real fetch of the echo source, so the store holds citable records.
pub fn fetch_window(tree: &Path) -> Result<String> {
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = format!(
        "{}:{}",
        fixtures.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = binary()
        .args([
            "--json",
            "--memory",
            tree_text,
            "fetch",
            "--window-end",
            "2026-01-01T00:00:00Z",
        ])
        .envs(tree_env(tree)?)
        .env("PATH", path)
        .output()
        .context("running ctx fetch")?;
    if !output.status.success() {
        bail!(
            "ctx fetch failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding fetch output")?;
    value
        .get("data")
        .and_then(|data| data.get("window"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("fetch printed no window id"))
}

pub fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ctx"))
}

/// What one local `ctx` run produced.
pub struct Local {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Runs `ctx` locally against a tree, with the environment a test server also gets.
pub fn ctx_local(tree: &Path, arguments: &[&str]) -> Result<Local> {
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree_text])
        .args(arguments)
        .envs(tree_env(tree)?)
        .env_remove("CTX_SERVER")
        .output()
        .context("running ctx")?;
    Ok(Local {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Runs `ctx` as a remote client: no tree, a working directory that holds none, and the
/// environment a dev box or a Kubernetes pod would carry.
pub fn ctx_remote(
    cwd: &Path,
    environment: &[(&str, &str)],
    arguments: &[&str],
    stdin: Option<&str>,
) -> Result<Local> {
    let mut command = binary();
    command
        .args(arguments)
        .current_dir(cwd)
        .env_remove("CTX_MEMORY")
        .env_remove("CTX_IMPERSONATE_SA")
        .env_remove("CTX_SERVER_AUDIENCE")
        .env_remove("GCE_METADATA_HOST")
        .env_remove("CTX_IAM_CREDENTIALS_URL")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().context("running ctx as a remote client")?;
    if let Some(text) = stdin {
        let mut handle = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("the client has no stdin"))?;
        std::io::Write::write_all(&mut handle, text.as_bytes())?;
    }
    let output = child.wait_with_output().context("waiting for ctx")?;
    Ok(Local {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// A record that cites the echo fixture's stored message, so every check passes.
#[must_use]
pub fn cited_record() -> String {
    "---\n\
     type: decision\n\
     status: open\n\
     title: Keep the echo cadence\n\
     quote: \"echo C0EXAMPLE01\"\n\
     source: \"echo:C0EXAMPLE01/record\"\n\
     tier: 1\n\
     ---\n\
     The stream confirmed the cadence.\n"
        .to_owned()
}
/// One HTTP response from the server.
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    pub fn field(&self, name: &str) -> Result<&Value> {
        self.body
            .get(name)
            .ok_or_else(|| anyhow!("response has no {name}: {}", self.body))
    }

    pub fn text(&self, name: &str) -> Result<&str> {
        self.field(name)?
            .as_str()
            .ok_or_else(|| anyhow!("{name} is not a string: {}", self.body))
    }
}

/// A `ctx serve` child process bound to loopback ports the kernel chose.
pub struct Server {
    child: Child,
    pub tailnet: SocketAddr,
    pub ingress: Option<SocketAddr>,
    client: reqwest::blocking::Client,
}

impl Server {
    /// Starts `ctx serve` on a tree with a tailnet listener, plus any extra arguments.
    pub fn start(tree: &Path, extra: &[&str]) -> Result<Self> {
        Self::start_with_env(tree, extra, &[])
    }

    /// Starts `ctx serve` with extra environment, as a container would carry.
    pub fn start_with_env(
        tree: &Path,
        extra: &[&str],
        environment: &[(&str, &str)],
    ) -> Result<Self> {
        let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
        let mut command = binary();
        command
            .args(["--memory", tree_text, "serve"])
            .envs(tree_env(tree)?)
            .args(["--tailnet-listen", "127.0.0.1:0"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command.spawn().context("starting ctx serve")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ctx serve has no stdout"))?;
        let (tailnet, ingress) = match read_listeners(stdout, extra.contains(&"--listen")) {
            Ok(addresses) => addresses,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            tailnet,
            ingress,
            client: reqwest::blocking::Client::builder()
                // Long enough to mean "the server is not answering" rather than "the box is
                // busy". One write forks a dozen git processes, and on a shared machine under
                // heavy load each fork costs a fraction of a second, so 30 seconds was a
                // report of the load average, not of the server.
                .timeout(Duration::from_secs(300))
                .build()?,
        })
    }

    /// Posts to the tailnet listener as a signed-in tailnet user.
    pub fn post_as(&self, user: &str, path: &str, body: &Value) -> Result<Reply> {
        self.post(self.tailnet, path, body, &[("Tailscale-User-Login", user)])
    }

    pub fn post(
        &self,
        address: SocketAddr,
        path: &str,
        body: &Value,
        headers: &[(&str, &str)],
    ) -> Result<Reply> {
        let mut request = self
            .client
            .post(format!("http://{address}{path}"))
            .json(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().context("posting to ctx serve")?;
        reply(response)
    }

    pub fn get(&self, address: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Result<Reply> {
        let mut request = self.client.get(format!("http://{address}{path}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().context("getting from ctx serve")?;
        reply(response)
    }

    /// Runs one allowlisted command over `/v1/command` as a tailnet user.
    pub fn command(&self, argv: &[&str]) -> Result<Reply> {
        self.post_as("ada@example.com", "/v1/command", &json!({ "argv": argv }))
    }

    pub fn ingress_address(&self) -> Result<SocketAddr> {
        self.ingress
            .ok_or_else(|| anyhow!("this server has no ingress listener"))
    }

    /// Stops the server the way Cloud Run does, and waits for it to finish.
    pub fn terminate(&mut self) -> Result<()> {
        let pid = i32::try_from(self.child.id()).context("the server has an unusable pid")?;
        // SAFETY: the pid is this process's own child, which has not been waited for.
        let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
        if sent != 0 {
            bail!("sending SIGTERM to {pid} failed");
        }
        let status = self.child.wait().context("waiting for ctx serve")?;
        if !status.success() {
            bail!("ctx serve stopped with {status}");
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reply(response: reqwest::blocking::Response) -> Result<Reply> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let text = response.text()?;
    if !content_type.starts_with("application/json") {
        bail!("response {status} is not JSON ({content_type}): {text}");
    }
    Ok(Reply {
        status,
        body: serde_json::from_str(&text).with_context(|| format!("decoding {text}"))?,
    })
}

/// Reads the listener lines `ctx serve` prints once it is bound.
fn read_listeners(
    stdout: ChildStdout,
    expect_ingress: bool,
) -> Result<(SocketAddr, Option<SocketAddr>)> {
    let mut tailnet = None;
    let mut ingress = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading ctx serve output")?;
        if let Some(address) = line.strip_prefix("tailnet-listen ") {
            tailnet = Some(address.trim().parse()?);
        } else if let Some(address) = line.strip_prefix("listen ") {
            ingress = Some(address.trim().parse()?);
        }
        if tailnet.is_some() && (!expect_ingress || ingress.is_some()) {
            break;
        }
    }
    let tailnet = tailnet.ok_or_else(|| anyhow!("ctx serve printed no tailnet listener"))?;
    Ok((tailnet, ingress))
}
