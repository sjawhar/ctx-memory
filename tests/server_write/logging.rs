//! The server's log lines: `write failed:`, the follow heartbeat, a failed follow pass, and the
//! request log.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{
    broken_zeppelin_record, good_record, install_hook, put_body, refuse_every_push, within,
};
use crate::support;
use crate::support::{fetch_window, fixture_clone, remote_of, remove_fixture};

/// A `ctx serve` whose standard error goes to a file, as Cloud Logging collects it.
struct Logged {
    child: std::process::Child,
    tailnet: SocketAddr,
    client: reqwest::blocking::Client,
    /// What the server prints on standard output after its listener line.
    stdout: Receiver<String>,
}

impl Logged {
    fn start(tree: &Path, extra: &[&str], log: &Path) -> Result<Self> {
        let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
        let mut command = support::binary();
        command
            .args([
                "--memory",
                tree_text,
                "serve",
                "--tailnet-listen",
                "127.0.0.1:0",
            ])
            .args(extra)
            .envs(support::tree_env(tree)?)
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(log)?);
        let mut child = command.spawn().context("starting ctx serve")?;
        let stdout = support::read_lines(child.stdout.take().context("ctx serve has no stdout")?);
        let tailnet = loop {
            let Ok(line) = stdout.recv() else {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("ctx serve stopped before it listened");
            };
            if let Some(address) = line.strip_prefix("tailnet-listen ") {
                break address.parse()?;
            }
        };
        Ok(Self {
            child,
            tailnet,
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
            stdout,
        })
    }

    /// Stops the server, then returns every line of its request log: all of them, so a line too
    /// many fails a test as a line too few does.
    fn stop_and_read_request_log(&mut self) -> Result<Vec<Value>> {
        self.stop();
        // The pipe closes when the server exits, which ends the thread that reads it.
        self.stdout
            .iter()
            .map(|line| serde_json::from_str(&line).with_context(|| format!("{line:?}")))
            .collect()
    }

    /// Stops the server as Cloud Run does, with SIGTERM, so it finishes the follow pass it is in
    /// before it exits. ctx runs each git command in a process group of its own
    /// (`src/proc.rs`), so a SIGKILL would leave a pass's git running in the fixture while the
    /// test removes it, and the removal would fail. SIGKILL only if it has not stopped within a
    /// minute.
    fn stop(&mut self) {
        // A child already waited for is not signalled again: its pid may be another process's.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: the pid is this process's own child, which has not been waited for.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Posts `body` to `path`, signed in as `user` when there is one, and returns the status.
    fn post(&self, user: Option<&str>, path: &str, body: &Value) -> Result<u16> {
        Ok(self.send(user, path, body)?.status().as_u16())
    }

    /// Posts as [`Logged::post`] does, and returns the status and the answer.
    fn post_reply(&self, user: Option<&str>, path: &str, body: &Value) -> Result<(u16, Value)> {
        let response = self.send(user, path, body)?;
        Ok((response.status().as_u16(), response.json()?))
    }

    fn send(
        &self,
        user: Option<&str>,
        path: &str,
        body: &Value,
    ) -> Result<reqwest::blocking::Response> {
        let mut request = self
            .client
            .post(format!("http://{}{path}", self.tailnet))
            .json(body);
        if let Some(user) = user {
            request = request.header("Tailscale-User-Login", user);
        }
        Ok(request.send()?)
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A write that did not happen says so on standard error in one fixed form an alert matches,
/// `write failed: <route> <status> <reason>`, whatever route it came by and whatever stopped
/// it. Cloud Run's own request log never sees a write that arrives over the tailnet, and the
/// server's request log is not a form an alert matches, so this line is the alert's. The line
/// never carries what the write carried: no record text, no credential.
#[test]
fn every_write_that_fails_says_so_in_one_line_an_alert_matches() -> Result<()> {
    let tree = fixture_clone("log-write-failed")?;
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let user = Some("ada@example.com");
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let statuses = [
        server.post(
            None,
            "/v1/records/put",
            &put_body("decisions/anonymous.md", &good_record(), "s"),
        )?,
        server.post(
            user,
            "/v1/records/put",
            &put_body(
                &format!("decisions/{token}.md"),
                &format!("{}\nThe key is {token}.\n", good_record()),
                &token,
            ),
        )?,
        server.post(
            user,
            "/v1/records/delete",
            &json!({ "path": "decisions/nothing-here.md", "session": "s" }),
        )?,
        server.post(
            user,
            "/v1/marker/advance",
            &json!({ "window": "no-such-window", "session": "s" }),
        )?,
    ];
    refuse_every_push(&bare)?;
    let unpushed = server.post(
        user,
        "/v1/records/put",
        &put_body("decisions/unpushed.md", &good_record(), "s"),
    )?;
    std::fs::remove_file(bare.join("hooks/pre-receive"))?;
    let stored = server.post(
        user,
        "/v1/records/put",
        &put_body("decisions/stored.md", &good_record(), "s"),
    )?;
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    remove_fixture(&tree)?;

    assert_eq!(statuses, [401, 422, 400, 400], "unexpected answers");
    assert_eq!(unpushed, 500, "the refused push was not a server failure");
    assert_eq!(stored, 200, "the last write did not land");
    let failures: Vec<&str> = logged
        .lines()
        .filter(|line| line.starts_with("write failed: "))
        .collect();
    let expected = [
        "write failed: /v1/records/put 401 ",
        "write failed: /v1/records/put 422 ",
        "write failed: /v1/records/delete 400 ",
        "write failed: /v1/marker/advance 400 ",
        "write failed: /v1/records/put 500 ",
    ];
    assert_eq!(
        failures.len(),
        expected.len(),
        "one line per failed write, none for the write that landed: {logged}"
    );
    for (line, prefix) in failures.iter().zip(expected) {
        assert!(
            line.starts_with(prefix),
            "{line:?} does not start {prefix:?}"
        );
        assert!(
            line.len() > prefix.len(),
            "{line:?} does not say why the write failed"
        );
    }
    assert!(
        !logged.contains(&token),
        "the log repeats a credential: {logged}"
    );
    assert!(
        !logged.contains("Keep the echo cadence"),
        "the log repeats a record's text: {logged}"
    );
    Ok(())
}

/// A refused write is one line of the request log too, with the codes of the findings that
/// refused it, the path it named and the caller who sent it, and never the record it carried:
/// no record text, no credential, wherever the caller put it. A path is one line of plain text
/// there, as in a `write failed:` line, whatever control characters the caller put in it.
#[test]
fn a_refused_put_logs_its_refusal_codes_and_never_its_record() -> Result<()> {
    let tree = fixture_clone("log-request-refused")?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let user = Some("ada@example.com");
    let mut server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let (gap, gap_reply) = server.post_reply(
        user,
        "/v1/records/put",
        &put_body("decisions/zeppelin.md", &broken_zeppelin_record(), "s"),
    )?;
    let (credential, credential_reply) = server.post_reply(
        user,
        "/v1/records/put",
        &put_body(
            &format!("decisions/{token}.md"),
            &format!("{}\nThe key is {token}.\n", good_record()),
            "s",
        ),
    )?;
    let forged = server.post(
        user,
        "/v1/records/put",
        &put_body("decisions/x\nforged\n\u{1b}[2J.md", &good_record(), "s"),
    )?;
    let logged = server.stop_and_read_request_log()?;
    drop(server);
    remove_fixture(&tree)?;

    assert_eq!(
        [gap, credential, forged],
        [422, 422, 400],
        "unexpected answers"
    );
    let text = serde_json::to_string(&logged)?;
    for secret in [
        token.as_str(),
        "Zeppelin budget",
        "a-record-that-does-not-exist",
        "The stream confirmed the cadence",
        "Keep the echo cadence",
    ] {
        assert!(
            !text.contains(secret),
            "the request log holds {secret:?}: {text}"
        );
    }
    let [gap_line, credential_line, forged_line] = logged.as_slice() else {
        anyhow::bail!("not one line per request: {text}");
    };
    for (line, reply) in [(gap_line, &gap_reply), (credential_line, &credential_reply)] {
        let codes: Vec<Value> = reply
            .get("findings")
            .and_then(Value::as_array)
            .context("a 422 carries its findings")?
            .iter()
            .filter_map(|finding| finding.get("kind").cloned())
            .collect();
        assert!(!codes.is_empty(), "{reply}");
        assert_eq!(line.get("refusals"), Some(&json!(codes)), "{line}");
        assert_eq!(line.get("result"), Some(&json!("refused")), "{line}");
        assert_eq!(line.get("status"), Some(&json!(422)), "{line}");
        assert_eq!(line.get("command"), Some(&json!("record put")), "{line}");
        assert_eq!(
            line.get("caller"),
            Some(&json!("ada@example.com")),
            "{line}"
        );
        assert_eq!(line.get("door"), Some(&json!("tailnet")), "{line}");
    }
    assert_eq!(
        gap_line.get("path"),
        Some(&json!("decisions/zeppelin.md")),
        "{gap_line}"
    );
    assert_eq!(
        credential_line.get("path"),
        Some(&json!("decisions/[redacted].md")),
        "{credential_line}"
    );
    assert_eq!(
        forged_line.get("result"),
        Some(&json!("bad-request")),
        "{forged_line}"
    );
    assert!(forged_line.get("refusals").is_none(), "{forged_line}");
    assert_eq!(
        forged_line.get("path"),
        Some(&json!("decisions/x forged [2J.md")),
        "{forged_line}"
    );
    Ok(())
}

/// A caller that hangs up before its answer is ready still has its line, at the moment it hung
/// up: `unanswered`, with no status and no body, and everything the server had learned of it.
/// The write runs on and lands; the line does not say how it ended, because nobody was told.
#[test]
fn a_caller_that_hangs_up_has_an_unanswered_line() -> Result<()> {
    let tree = fixture_clone("log-request-hang-up")?;
    // The record cites the echo fixture's message, which a fetched window stores.
    fetch_window(&tree)?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let bare = remote_of(&tree)?;
    install_hook(&bare, "pre-receive", "#!/bin/sh\nsleep 4\nexit 0\n")?;
    let mut server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let impatient = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()?;
    let hung_up = impatient
        .post(format!("http://{}/v1/records/put", server.tailnet))
        .header("Tailscale-User-Login", "ada@example.com")
        .json(&put_body("decisions/patience.md", &good_record(), "s"))
        .send();
    // The write lands once the hook lets the push through.
    let landed = within(120, || {
        let subject = support::git(&bare, &["log", "--format=%s", "-1", "main"])?;
        Ok(subject.contains("patience").then_some(()))
    });
    let logged = server.stop_and_read_request_log()?;
    drop(server);
    remove_fixture(&tree)?;

    assert!(hung_up.is_err(), "the caller was answered: {hung_up:?}");
    landed
        .with_context(|| format!("the write did not land after its caller hung up: {logged:?}"))?;
    let [line] = logged.as_slice() else {
        anyhow::bail!("not one line for the request: {logged:?}");
    };
    for (field, value) in [
        ("result", json!("unanswered")),
        ("status", Value::Null),
        ("response_bytes", Value::Null),
        ("severity", json!("WARNING")),
        ("command", json!("record put")),
        ("caller", json!("ada@example.com")),
        ("path", json!("decisions/patience.md")),
    ] {
        assert_eq!(line.get(field), Some(&value), "{line}");
    }
    let waited = line
        .get("duration_ms")
        .and_then(Value::as_u64)
        .context("the line has no duration")?;
    assert!(
        (900..4000).contains(&waited),
        "the line was not written when the caller hung up: {line}"
    );
    Ok(())
}

/// A caller's text cannot forge another line, or act on whoever reads the log. A refused path
/// is quoted in its `write failed:` line, so a path holding a newline and the heartbeat's
/// words, or an escape sequence, stays inside that one line, with its control characters
/// spaced out; and a caller that was not identified gets a fixed reason, never what it sent,
/// such as an identity token's key id (round-4 review of d5655fc, N4-4).
#[test]
fn a_callers_text_stays_inside_its_write_failed_line() -> Result<()> {
    let tree = fixture_clone("log-caller-text")?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let user = Some("ada@example.com");
    let server = Logged::start(&tree, &["--pull-interval", "3600"], &log)?;
    let statuses = [
        server.post(
            None,
            "/v1/records/put",
            &put_body("decisions/anonymous.md", &good_record(), "s"),
        )?,
        server.post(
            user,
            "/v1/records/put",
            &put_body("follow heartbeat: ok/AGENTS.md", &good_record(), "s"),
        )?,
        server.post(
            user,
            "/v1/records/put",
            &put_body(
                "decisions/x\nfollow heartbeat: ok forged\n\u{1b}[2J.md",
                &good_record(),
                "s",
            ),
        )?,
    ];
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    remove_fixture(&tree)?;

    assert_eq!(statuses, [401, 400, 400], "unexpected answers: {logged}");
    let lines: Vec<&str> = logged.lines().collect();
    assert!(
        lines.contains(&"write failed: /v1/records/put 401 the caller was not identified"),
        "the 401 line does not give the fixed reason: {logged}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("follow heartbeat: ok forged")),
        "a caller's path forged a heartbeat line: {logged:?}"
    );
    for line in lines.iter().filter(|line| line.contains("AGENTS.md")) {
        assert!(line.starts_with("write failed: "), "{line:?}");
    }
    assert!(
        !logged
            .chars()
            .any(|character| character.is_control() && character != '\n'),
        "the log carries a control character: {logged:?}"
    );
    Ok(())
}

/// While its passes succeed, the follow loop says so on standard error at most once every
/// five minutes, so an alert on the line's absence catches a pass that never ends and a copy
/// that is down. It starts with words no failure line starts with.
#[test]
fn a_following_copy_says_so_once_and_not_on_every_pass() -> Result<()> {
    let tree = fixture_clone("log-heartbeat")?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let server = Logged::start(&tree, &["--pull-interval", "1"], &log)?;
    let heard = within(60, || {
        let logged = std::fs::read_to_string(&log)?;
        Ok(logged
            .lines()
            .any(|line| line == "follow heartbeat: ok")
            .then_some(()))
    });
    // Several more passes: a heartbeat is not a line per pass.
    std::thread::sleep(std::time::Duration::from_secs(4));
    drop(server);
    let logged = std::fs::read_to_string(&log)?;
    remove_fixture(&tree)?;

    heard.context("the follow loop never said it was following")?;
    assert_eq!(
        logged
            .lines()
            .filter(|line| *line == "follow heartbeat: ok")
            .count(),
        1,
        "{logged}"
    );
    assert!(
        !logged
            .lines()
            .any(|line| line.starts_with("following main: ")),
        "a pass failed, so this test checks nothing: {logged}"
    );
    Ok(())
}

/// A failed follow pass is one line too. README promises three fixed-form lines an alert can
/// match; `write failed:` folds git's error onto its line, but `following main: <why>` prints
/// git's standard error as it came, so a pull that fails writes the prefix line and then git's
/// further lines (and blank ones), each its own log entry in Cloud Logging, matching no form.
/// Observed on the real binary: with the remote moved away, each failed pass wrote
/// `following main: git pull … failed: fatal: '…/remote.git' does not appear to be a git
/// repository`, then `fatal: Could not read from remote repository.`, an empty line, and two
/// more lines of git's advice.
#[test]
fn a_failed_follow_pass_says_so_in_one_line() -> Result<()> {
    let tree = fixture_clone("log-follow-one-line")?;
    let root = tree.parent().context("the fixture has a root")?;
    let log = root.join("serve.log");
    let remote = root.join("remote.git");
    let away = root.join("remote.git.away");
    let server = Logged::start(&tree, &["--pull-interval", "1"], &log)?;
    let outcome = (|| -> Result<String> {
        within(60, || {
            Ok(std::fs::read_to_string(&log)?
                .lines()
                .any(|line| line == "follow heartbeat: ok")
                .then_some(()))
        })
        .context("the follow loop never said it was following")?;
        std::fs::rename(&remote, &away)?;
        within(60, || {
            Ok(std::fs::read_to_string(&log)?
                .lines()
                .any(|line| line.starts_with("following main: "))
                .then_some(()))
        })
        .context("the follow loop never said it failed")?;
        std::fs::rename(&away, &remote)?;
        Ok(std::fs::read_to_string(&log)?)
    })();
    drop(server);
    remove_fixture(&tree)?;

    let logged = outcome?;
    let stray: Vec<&str> = logged
        .lines()
        .filter(|line| {
            !["following main: ", "follow heartbeat: ok", "write failed: "]
                .iter()
                .any(|form| line.starts_with(form))
        })
        .collect();
    assert!(
        stray.is_empty(),
        "the server's log holds lines no alert form matches: {stray:?}\n{logged}"
    );
    Ok(())
}
