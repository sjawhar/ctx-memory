//! The server's log lines: `write failed:`, the follow heartbeat, and a failed follow pass.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{good_record, put_body, refuse_every_push, within};
use crate::support;
use crate::support::{fetch_window, fixture_clone, remote_of, remove_fixture};

/// A `ctx serve` whose standard error goes to a file, as Cloud Logging collects it.
struct Logged {
    child: std::process::Child,
    tailnet: SocketAddr,
    client: reqwest::blocking::Client,
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
        let stdout = child.stdout.take().context("ctx serve has no stdout")?;
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
        let tailnet = loop {
            let Some(line) = lines.next() else {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("ctx serve stopped before it listened");
            };
            if let Some(address) = line?.strip_prefix("tailnet-listen ") {
                break address.parse()?;
            }
        };
        std::thread::spawn(move || lines.for_each(drop));
        Ok(Self {
            child,
            tailnet,
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
        })
    }

    /// Posts `body` to `path`, signed in as `user` when there is one, and returns the status.
    fn post(&self, user: Option<&str>, path: &str, body: &Value) -> Result<u16> {
        let mut request = self
            .client
            .post(format!("http://{}{path}", self.tailnet))
            .json(body);
        if let Some(user) = user {
            request = request.header("Tailscale-User-Login", user);
        }
        Ok(request.send()?.status().as_u16())
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A write that did not happen says so on standard error in one fixed form an alert matches,
/// `write failed: <route> <status> <reason>`, whatever route it came by and whatever stopped
/// it. A write that arrives over the tailnet passes no request log but this one. The line
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
/// Observed on the real binary (round-4 acceptance at d5655fc): with
/// the remote moved away, each failed pass wrote `following main: git pull … failed: fatal:
/// '…/remote.git' does not appear to be a git repository`, then `fatal: Could not read from
/// remote repository.`, an empty line, and two more lines of git's advice.
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
