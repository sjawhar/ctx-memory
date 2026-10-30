//! Remote mode: `ctx` talks to the one server that holds the graph.
//!
//! An agent anywhere reads through `/v1/command` and writes through the record endpoints, so
//! no session needs a clone, a store, or a source credential. Who the caller is depends on
//! where it runs: a Kubernetes pod impersonates a service account, anything else inside Google
//! Cloud takes an identity token from the metadata server, and a dev box reaches the server
//! over the tailnet, where Tailscale itself says who is calling.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cli::{CiteCommand, Cli, Command, Remote};
use crate::config::UserConfig;
use crate::credentials;
use crate::marker;
use crate::output::{self, Outcome, Sink};
use crate::protocol::{
    Captured, CommandRequest, Committed, DeleteRequest, Draft, MarkerRequest, MarkerResponse,
    PutRequest, Refused, COMMAND_ROUTE, DELETE_ROUTE, MARKER_ROUTE, PUT_ROUTE,
};

/// The default endpoint of Google's IAM credentials API.
const IAM_CREDENTIALS: &str = "https://iamcredentials.googleapis.com";

/// A flag `ctx` takes whatever command it is running, and may be written before the command
/// name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Global {
    /// `--memory <path>` or `--memory=<path>`: which tree to act on.
    Memory,
    /// `--json`: emit the output envelope instead of prose.
    Json,
}

/// The global flag this one argument is.
///
/// Two places act on these flags and have to agree about them: the client, which moves the
/// flags that travel after the command name, and the server, which reads the command name out
/// of the argv it is handed and refuses a tree named there. Written out twice, the two drifted
/// — the server's own guard caught `--memory` but not `--memory=<path>` — so they read it here.
#[must_use]
pub fn global_flag(argument: &str) -> Option<Global> {
    match argument.split_once('=').map_or(argument, |(name, _)| name) {
        "--memory" => Some(Global::Memory),
        "--json" if argument == "--json" => Some(Global::Json),
        _ => None,
    }
}

/// The ctx server, as a client sees it.
pub struct Client {
    base: String,
    http: reqwest::blocking::Client,
    token: Option<String>,
}

impl Client {
    /// Builds a client for one server.
    ///
    /// The server is sent every record a caller writes and, inside Google Cloud, the caller's
    /// identity token, so it must be reached over HTTPS, over loopback, or over the tailnet
    /// ([`origin`]); the token must be minted for that server and no other
    /// ([`identity_token`]); and an answer that redirects to another origin is not followed.
    ///
    /// # Errors
    ///
    /// Returns an error when the base is not such an address, when `CTX_SERVER_AUDIENCE` names
    /// another server, when the HTTP client cannot be built, or when an identity token is
    /// asked for and cannot be minted.
    pub fn new(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/');
        let origin = origin(base)?;
        let token = identity_token(&origin)?;
        let mut builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(same_origin_redirects(origin.clone()));
        if origin.starts_with("http://") {
            // Plain HTTP is taken only to loopback or over the tailnet, whose tunnel carries
            // it. A proxy from `HTTP_PROXY` or `ALL_PROXY` would carry the record and the
            // token outside that tunnel, unencrypted, so plain HTTP goes to the server direct.
            builder = builder.no_proxy();
        }
        Ok(Self {
            base: base.to_owned(),
            http: builder.build().context("building the remote HTTP client")?,
            token,
        })
    }

    /// Runs one command on the server and returns what it printed. `drafts` are the files on
    /// this machine a `cite verify` names, which the server checks as text.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be reached or refuses the command.
    pub fn command(&self, argv: &[String], drafts: &[Draft]) -> Result<Captured> {
        let value = self.post(
            COMMAND_ROUTE,
            &CommandRequest {
                argv: argv.to_vec(),
                drafts: drafts.to_vec(),
            },
        )?;
        serde_json::from_value(value).context("decoding the server's answer")
    }

    /// Writes one record through the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be reached or the write fails for a reason
    /// other than a check finding.
    pub fn put(&self, path: &str, content: &str, session: &str) -> Result<Written> {
        self.write(
            PUT_ROUTE,
            &PutRequest {
                path: path.to_owned(),
                content: content.to_owned(),
                session: Some(session.to_owned()),
            },
        )
    }

    /// Removes one record through the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be reached or the write fails for a reason
    /// other than a check finding.
    pub fn delete(&self, path: &str, session: &str) -> Result<Written> {
        self.write(
            DELETE_ROUTE,
            &DeleteRequest {
                path: path.to_owned(),
                session: Some(session.to_owned()),
            },
        )
    }

    /// Moves the agent marker through the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be reached or refuses the window.
    pub fn marker_advance(&self, window: &str, session: &str) -> Result<marker::Marker> {
        let value = self.post(
            MARKER_ROUTE,
            &MarkerRequest {
                window: window.to_owned(),
                session: Some(session.to_owned()),
            },
        )?;
        let answer: MarkerResponse = serde_json::from_value(value)
            .context("decoding the marker the server answered with")?;
        Ok(answer.marker)
    }

    fn write(&self, path: &str, body: &impl Serialize) -> Result<Written> {
        let (status, value) = self.answer(path, body)?;
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            // A refusal carries the findings that refused the write. Without them the caller
            // has nothing to act on, and the answer is not a refusal this client knows.
            let refused = Refused::deserialize(&value).with_context(|| {
                format!(
                    "the server answered {status} to {path} without the findings that refused it: {value}"
                )
            })?;
            return Ok(Written::Refused(refused));
        }
        if !status.is_success() {
            bail!("{}", error_text(status, &value));
        }
        let committed: Committed = serde_json::from_value(value)
            .map_err(|_| anyhow!("the server answered without a commit"))?;
        Ok(Written::Committed(committed.commit))
    }

    fn post(&self, path: &str, body: &impl Serialize) -> Result<Value> {
        let (status, value) = self.answer(path, body)?;
        if !status.is_success() {
            bail!("{}", error_text(status, &value));
        }
        Ok(value)
    }

    /// Sends one request and reads the answer: its status, and its body as JSON.
    ///
    /// The status is read first. Cloud Run, a load balancer or a proxy can answer in place of
    /// the server, in text or HTML, and the status is the part of such an answer a caller can
    /// act on: it is reported with the start of the body.
    fn answer(&self, path: &str, body: &impl Serialize) -> Result<(reqwest::StatusCode, Value)> {
        let response = self.send(path, body)?;
        let status = response.status();
        let text = response
            .text()
            .with_context(|| format!("reading the answer to {path} ({status})"))?;
        if let Ok(value) = serde_json::from_str(&text) {
            return Ok((status, value));
        }
        let start: String = text.trim().chars().take(200).collect();
        bail!("the server answered {status} to {path}, not in JSON: {start}")
    }

    fn send(&self, path: &str, body: &impl Serialize) -> Result<reqwest::blocking::Response> {
        let mut request = self.http.post(format!("{}{path}", self.base)).json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
            .send()
            .with_context(|| format!("asking {}{path}", self.base))
    }
}

/// What a write did, as the client reports it.
#[derive(Debug)]
pub enum Written {
    Committed(String),
    Refused(Refused),
}

fn error_text(status: reqwest::StatusCode, value: &Value) -> String {
    value.get("error").and_then(Value::as_str).map_or_else(
        || format!("the server answered {status}"),
        |message| format!("the server answered {status}: {message}"),
    )
}

/// The configured server, if there is one.
#[must_use]
pub fn server_url(user: &UserConfig) -> Option<String> {
    std::env::var("CTX_SERVER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| user.server.clone())
}

/// Runs one command on the configured server, or returns `None` when it belongs here.
///
/// A session with `CTX_SERVER` set reads and writes through the server and never loads a
/// tree, so an agent anywhere needs no clone, no store and no source credential. Commands
/// that are the server's own work — fetching, running a source, serving — stay local.
///
/// # Errors
///
/// Returns an error when `ctx record put` or `ctx record rm` runs with no server configured,
/// or when the server cannot be reached or fails the request.
pub fn run(cli: &Cli, sink: &mut Sink<'_>) -> Result<Option<Outcome>> {
    let route = cli.command.remote();
    let user = UserConfig::load()?;
    let Some(base) = server_url(&user) else {
        // Only a record write has no local meaning at all: a tree on one machine still moves
        // its own marker and shows its own records, and routes both to the server when one is
        // configured.
        if matches!(route, Remote::Put { .. } | Remote::Rm { .. }) {
            bail!("no ctx server is configured; set CTX_SERVER or server in the user config");
        }
        return Ok(None);
    };
    if route == Remote::Local {
        return Ok(None);
    }
    // The server answers from its own tree. A tree named here as well cannot both be meant,
    // and answering from the server's would run the command on a tree the caller did not name.
    if let Some(tree) = &cli.memory {
        bail!(
            "--memory (or CTX_MEMORY) names the tree {}, and a ctx server is configured \
             (CTX_SERVER or server in the user config), which answers from its own tree; \
             unset one of them",
            tree.display()
        );
    }
    let client = || Client::new(&base);
    let session = marker::caller();
    match route {
        Remote::Local => Ok(None),
        Remote::Put { path, source } => {
            let content = record_content(source)?;
            let written = client()?.put(path, &content, &session)?;
            report_write(&written, cli.json, "record put", path, sink).map(Some)
        }
        Remote::Rm { path } => {
            let written = client()?.delete(path, &session)?;
            report_write(&written, cli.json, "record rm", path, sink).map(Some)
        }
        Remote::MarkerAdvance { window } => {
            // The shape a local advance answers with: `marker show`'s, and its text.
            let marker = client()?.marker_advance(window, &session)?;
            let payload = json!({ "marker": marker });
            output::emit(sink, cli.json, "marker advance", &payload, |_, out| {
                marker::print(Some(&marker), out)
            })?;
            Ok(Some(Outcome::Pass))
        }
        Remote::Forward => {
            let argv = forwarded_argv()?;
            let drafts = local_drafts(&cli.command)?;
            let captured = client()?.command(&argv, &drafts)?;
            // The server prints source text without control characters and escapes them in
            // JSON; text from a server that does not is cleaned here too.
            if cli.json {
                write!(sink.out, "{}", captured.stdout)?;
            } else {
                write!(sink.out, "{}", output::printable(&captured.stdout))?;
            }
            write!(sink.err, "{}", output::printable(&captured.stderr))?;
            Ok(Some(Outcome::Code(captured.exit_code)))
        }
    }
}

/// This process's own arguments, ready for the server.
///
/// A global flag the caller wrote first — `ctx --json pending` — would arrive at the server
/// as the command name, so the flags that travel are re-applied after it. clap takes them in
/// either place.
///
/// The server takes its arguments as text. A path is the one argument that may be any bytes,
/// and one that is not text is refused here, before anything is sent.
fn forwarded_argv() -> Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut globals = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let argument = argument.into_string().map_err(|argument| {
            anyhow!(
                "the argument {} is not text, and a ctx server takes its arguments as text",
                argument.display()
            )
        })?;
        match global_flag(&argument) {
            Some(Global::Json) => globals.push(argument),
            Some(Global::Memory) | None => argv.push(argument),
        }
    }
    argv.extend(globals);
    Ok(argv)
}

/// The files on this machine that a `cite verify` names, as drafts the server checks as text.
///
/// The server answers from its own tree, where a draft written here is not, so forwarding its
/// path alone checked nothing. A named path that is no file here is left to the server, which
/// checks the record of its tree that the path names, or refuses it.
fn local_drafts(command: &Command) -> Result<Vec<Draft>> {
    let Command::Cite {
        command: CiteCommand::Verify { files, .. },
    } = command
    else {
        return Ok(Vec::new());
    };
    let mut drafts = Vec::new();
    for file in files.iter().filter(|file| file.is_file()) {
        let path = file.to_str().ok_or_else(|| {
            anyhow!(
                "the path {} is not text, and a ctx server takes its arguments as text",
                file.display()
            )
        })?;
        let content =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        drafts.push(Draft {
            path: path.to_owned(),
            content,
        });
    }
    Ok(drafts)
}

/// Reads a record from the named file, or from standard input for `-`.
fn record_content(source: &str) -> Result<String> {
    if source != "-" {
        return std::fs::read_to_string(source).with_context(|| format!("reading {source}"));
    }
    let mut content = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut content)
        .context("reading the record from standard input")?;
    Ok(content)
}

/// Prints what a write did, and fails the command when the server refused it.
fn report_write(
    written: &Written,
    json: bool,
    command: &str,
    path: &str,
    sink: &mut Sink<'_>,
) -> Result<Outcome> {
    match written {
        Written::Committed(commit) => {
            let payload = json!({ "path": path, "commit": commit });
            output::emit(sink, json, command, &payload, |_, out| {
                writeln!(
                    out,
                    "{} {}",
                    output::printable(commit),
                    output::printable(path)
                )
            })?;
            Ok(Outcome::Pass)
        }
        Written::Refused(refused) => {
            let payload = json!({ "path": path, "findings": refused.findings });
            // A finding can quote the record, and the record quotes its sources.
            output::emit(sink, json, command, &payload, |_, out| {
                writeln!(out, "refused {}", output::printable(path))?;
                for finding in &refused.findings {
                    writeln!(
                        out,
                        "  {} — {}",
                        output::printable(&finding.kind),
                        output::printable(&finding.detail)
                    )?;
                }
                Ok(())
            })?;
            Ok(Outcome::StructuralDefects)
        }
    }
}

/// The origin of the server at `base` (`scheme://host[:port]`), when it is an address the
/// client may send records and a token to.
///
/// HTTPS is the rule. Plain HTTP is taken only where something else keeps the traffic from
/// being read or redirected on the way: a loopback address, which never leaves the machine,
/// and a tailnet name (`*.ts.net`), which resolves only inside the tailnet, whose tunnels
/// encrypt and authenticate every packet. The deployment serves the tailnet as plain HTTP.
fn origin(base: &str) -> Result<String> {
    let url = reqwest::Url::parse(base).with_context(|| {
        format!("CTX_SERVER (or server in the user config) {base} is not a URL")
    })?;
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    let tailnet = host.ends_with(".ts.net");
    match url.scheme() {
        "https" if !host.is_empty() => {}
        "http" if loopback || tailnet => {}
        _ => bail!(
            "CTX_SERVER (or server in the user config) {base} is refused: ctx sends it every \
             record it writes and the caller's identity token, so it takes https://, or http:// \
             only to this machine (localhost, 127.0.0.1, ::1) or to a tailnet name (*.ts.net), \
             whose WireGuard encrypts the traffic"
        ),
    }
    Ok(url.origin().ascii_serialization())
}

/// Follows a redirect only within the server's own origin.
///
/// The token and the record a request carries are meant for that server alone. An answer that
/// redirects elsewhere — another host, another port, or HTTP for HTTPS — is refused, so the
/// request is not sent there at all, with its token or without it.
fn same_origin_redirects(origin: String) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let next = attempt.url().origin().ascii_serialization();
        if next != origin {
            let why = format!(
                "the server redirected to {next}, another origin; ctx sends its token and \
                 records only to {origin}"
            );
            return attempt.error(why);
        }
        if attempt.previous().len() >= 10 {
            return attempt.error("the server redirected more than 10 times");
        }
        attempt.follow()
    })
}

/// The token this caller presents to the server at `origin`, in the order the deployment needs.
///
/// A Kubernetes pod runs as a Kubernetes service account Cloud Run does not accept, so it
/// impersonates a Google service account; anything else inside Google Cloud asks the metadata
/// server directly; a dev box sends no token and is identified by its Tailscale login.
///
/// `CTX_SERVER_AUDIENCE` asks for a token, and must name the origin the token is sent to: a
/// token says which server it is for, and one minted for another server would let the server
/// that receives it call that one as the caller.
fn identity_token(origin: &str) -> Result<Option<String>> {
    let Some(audience) = std::env::var("CTX_SERVER_AUDIENCE")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    if audience.trim().trim_end_matches('/') != origin {
        bail!(
            "CTX_SERVER_AUDIENCE is {audience}, which is not the origin of CTX_SERVER \
             ({origin}): a token is minted only for the server it is sent to, so set \
             CTX_SERVER_AUDIENCE to {origin}"
        );
    }
    if let Some(account) = std::env::var("CTX_IMPERSONATE_SA")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(impersonated_token(&account, origin)?));
    }
    Ok(Some(
        credentials::metadata_identity_token(origin)?
            .expose()
            .to_owned(),
    ))
}

/// Mints an identity token for an audience as another service account.
fn impersonated_token(account: &str, audience: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Minted {
        token: String,
    }
    let access = credentials::metadata_token()?;
    let url = format!(
        "{}/v1/projects/-/serviceAccounts/{account}:generateIdToken",
        iam_credentials_base()
    );
    let response = credentials::client()?
        .post(&url)
        .bearer_auth(access.expose())
        .json(&json!({ "audience": audience, "includeEmail": true }))
        .send()
        .with_context(|| format!("asking {url} for an identity token"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        bail!("minting an identity token as {account} failed: {status} {detail}");
    }
    let minted: Minted = response.json().context("decoding the identity token")?;
    Ok(minted.token)
}

fn iam_credentials_base() -> String {
    std::env::var("CTX_IAM_CREDENTIALS_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| IAM_CREDENTIALS.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{error_text, origin, server_url};
    use crate::config::UserConfig;

    /// The server is sent records and the caller's token: HTTPS anywhere, plain HTTP only to
    /// this machine or over the tailnet, and nothing else.
    #[test]
    fn a_server_is_reached_over_https_loopback_or_the_tailnet_only() {
        for (base, expected) in [
            ("https://ctx.example.run.app", "https://ctx.example.run.app"),
            (
                "https://ctx.example.run.app:8443/",
                "https://ctx.example.run.app:8443",
            ),
            ("http://127.0.0.1:8081", "http://127.0.0.1:8081"),
            ("http://127.7.0.1", "http://127.7.0.1"),
            ("http://localhost:9000", "http://localhost:9000"),
            ("http://[::1]:9000", "http://[::1]:9000"),
            (
                "http://ctx-memory.example.ts.net",
                "http://ctx-memory.example.ts.net",
            ),
        ] {
            assert_eq!(origin(base).ok().as_deref(), Some(expected), "{base}");
        }
        for base in [
            "http://ctx.example.run.app",
            "http://10.0.0.5:8081",
            "http://100.101.102.103",
            "http://ts.net.example.com",
            "http://localhost.example.com",
            "ftp://ctx.example.com",
            "ctx.example.run.app",
        ] {
            let refused = origin(base).err().map(|error| format!("{error:#}"));
            assert!(
                refused.as_deref().is_some_and(|why| why.contains(base)),
                "{base} was not refused by name: {refused:?}"
            );
        }
    }

    #[test]
    fn an_error_answer_carries_the_server_s_message() {
        let text = error_text(
            reqwest::StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "fetch is not a remote command" }),
        );
        assert_eq!(
            text,
            "the server answered 400 Bad Request: fetch is not a remote command"
        );
    }

    #[test]
    fn user_configuration_names_a_server_when_the_environment_does_not() {
        let user = UserConfig {
            server: Some("http://ctx-memory.example.ts.net".to_owned()),
            ..UserConfig::default()
        };
        // The test process may carry CTX_SERVER; only the fallback is asserted here.
        if std::env::var_os("CTX_SERVER").is_none() {
            assert_eq!(
                server_url(&user).as_deref(),
                Some("http://ctx-memory.example.ts.net")
            );
        }
        assert_eq!(
            server_url(&UserConfig::default()),
            std::env::var("CTX_SERVER").ok()
        );
    }
}
