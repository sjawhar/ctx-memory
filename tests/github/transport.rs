//! A GitHub read that gets no answer at all: `ctx fetch` asks again, logging each retry, and a
//! stream that still gets none says why on its line of the fetch's output, however long the URL.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{ensure, Context as _, Result};
use serde_json::json;

use crate::fixtures::scratch;
use crate::support::fake_http::FakeHttp;

/// An owner as long as GitHub allows, less one, so the repository name stays under its limit.
const OWNER: &str = "example-org-with-a-long-name-for-tests";
const WINDOW_END: &str = "2026-10-27T00:00:00Z";
/// The issues feed of a stream with no cursor asks from 24 hours before the window's end.
const SINCE: &str = "2026-10-26T00%3A00%3A00Z";
/// How many characters of outside text a log line carries (`LINE_LIMIT` in `src/redact.rs`).
const LINE_LIMIT: usize = 300;

/// A GitHub with one installation that covers one repository, whose issues feed URL is
/// `length` characters long. Its commits listing answers `409`, as an empty repository's does,
/// so the issues feed is the only read that can fail. Returns the repository's name.
fn github_with_one_repository(api: &FakeHttp, length: usize) -> Result<String> {
    let feed = |name: &str| {
        format!(
            "{}/repos/{OWNER}/{name}/issues?state=all&sort=updated&direction=asc&since={SINCE}&per_page=100",
            api.url("")
        )
    };
    let name = format!(
        "repo-{}",
        "r".repeat(length.saturating_sub(feed("repo-").len()))
    );
    ensure!(
        feed(&name).len() == length,
        "no name gives a {length}-character URL"
    );
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_example", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [{ "full_name": format!("{OWNER}/{name}"), "default_branch": "main" }],
        })
        .to_string(),
    )?;
    api.json(
        &format!("/repos/{OWNER}/{name}/commits"),
        409,
        &json!({ "message": "Git Repository is empty." }).to_string(),
    )?;
    Ok(name)
}

fn feed_path(name: &str) -> String {
    format!("/repos/{OWNER}/{name}/issues")
}

/// Runs `ctx fetch` once, as the fetch job does, against the GitHub at `api`.
fn fetch(api: &FakeHttp, root: &Path) -> Result<Output> {
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree).with_context(|| format!("creating {}", tree.display()))?;
    std::fs::write(
        tree.join("ctx.toml"),
        "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"github\"\nkind = \"github\"\nscheme = \"github\"\ninstallations = [1000002]\n",
    )?;
    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ctx"));
    command
        .arg("--memory")
        .arg(&tree)
        .args(["fetch", "--window-end", WINDOW_END])
        .env("CTX_STATE_DIR", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CTX_GITHUB_API", api.url(""))
        .env("CTX_CREDENTIAL_GITHUB_APP", credential);
    for name in [
        "CTX_SERVER",
        "CTX_MEMORY",
        "CTX_MEMORY_REMOTE",
        "CTX_DATA_DIR",
        "CTX_LEASE_BUCKET",
        "CLOUD_RUN_JOB",
        "CTX_EMBED_MODEL",
        "CTX_GITHUB_INSTALLATIONS",
        "CTX_SECRET_GITHUB_APP",
        "CTX_CREDENTIAL_GITHUB_APP_FILE",
    ] {
        command.env_remove(name);
    }
    let output = command.output().context("running ctx fetch")?;
    std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))?;
    Ok(output)
}

/// The lines of standard error that say a GitHub read is being tried again.
fn retry_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with("github: ") && line.contains("trying again"))
        .map(str::to_owned)
        .collect()
}

/// What the HTTP client says, deepest in its error, about a connection closed with no answer.
fn cause_of_no_answer(api: &FakeHttp) -> Result<String> {
    api.hang_up("/no-answer")?;
    let error = reqwest::blocking::get(api.url("/no-answer"))
        .err()
        .context("a request the fake server hung up on succeeded")?;
    let mut cause: &(dyn std::error::Error + 'static) = &error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    Ok(cause.to_string())
}

#[test]
fn a_read_that_gets_no_answer_once_is_tried_again_and_the_stream_succeeds() -> Result<()> {
    let api = FakeHttp::start()?;
    let name = github_with_one_repository(&api, 250)?;
    api.hang_up(&feed_path(&name))?;
    api.then_json(&feed_path(&name), 200, "[]")?;

    let output = fetch(&api, &scratch("transport-once")?)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "the fetch failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.lines().any(|line| line == "github: ok"), "{stdout}");
    assert!(!stdout.contains(": error"), "{stdout}");
    assert_eq!(
        api.hits(&feed_path(&name))?,
        2,
        "the read was not tried again"
    );
    let retries = retry_lines(&output);
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert!(
        retries.iter().all(|line| line.contains("try 1 of 3")),
        "{retries:?}"
    );
    Ok(())
}

#[test]
fn a_read_that_never_gets_an_answer_errors_its_stream_with_the_cause_in_the_line() -> Result<()> {
    let api = FakeHttp::start()?;
    let name = github_with_one_repository(&api, 250)?;
    api.hang_up(&feed_path(&name))?;

    let output = fetch(&api, &scratch("transport-never")?)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Every try is the same 250-character request.
    let sent: Vec<String> = api
        .requests()?
        .into_iter()
        .filter(|request| request.path == feed_path(&name))
        .map(|request| api.url(&request.target))
        .collect();
    assert_eq!(sent.len(), 3, "one request and two more tries: {sent:?}");
    assert!(sent.iter().all(|url| url.len() == 250), "{sent:?}");

    // The fetch still fails, and its stream's line names the cause within the line's limit.
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let prefix = format!("  stream github:{OWNER}/{name}: error: ");
    let error = stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .with_context(|| format!("no error line for the stream: {stdout}"))?;
    assert!(error.chars().count() <= LINE_LIMIT + 1, "{error}");
    let cause = cause_of_no_answer(&api)?;
    assert!(error.contains(&cause), "{cause:?} is not in {error:?}");
    assert!(error.matches("/repos/").count() <= 1, "{error}");

    let retries = retry_lines(&output);
    assert_eq!(retries.len(), 2, "{retries:?}");
    assert!(
        retries
            .first()
            .is_some_and(|line| line.contains("try 1 of 3")),
        "{retries:?}"
    );
    assert!(
        retries
            .last()
            .is_some_and(|line| line.contains("try 2 of 3")),
        "{retries:?}"
    );
    Ok(())
}
