//! The graph repository is private, so every git call carries a credential.
//!
//! A private GitHub repository answers an anonymous clone with `401`, and a container holds no
//! git credential helper and no ssh key. These checks run a real `ctx serve` against a git
//! remote that demands an `Authorization` header: the server has to mint an installation
//! token and carry it on the clone, the pull and the push, or it cannot start at all.

mod support;

use anyhow::{Context as _, Result};
use serde_json::json;

use support::fake_github::{self, FakeGithub};
use support::{git, remove_scratch, scratch, Server};

const OWNER: &str = "other-org";
const NAME: &str = "memory";

/// A private remote with the fixture tree on `main`, and the GitHub that guards it.
fn private_remote(label: &str) -> Result<(std::path::PathBuf, FakeGithub)> {
    let root = scratch(label)?;
    let seed = root.join("seed");
    support::write_source_tree(&seed)?;
    git(&root, &["init", "--initial-branch=main", "seed"])?;
    support::write_source_tree(&seed)?;
    git(&seed, &["add", "."])?;
    support::commit(&seed, "the starting tree")?;
    let hosted = root.join("hosted");
    std::fs::create_dir_all(&hosted)?;
    fake_github::publish(&hosted, OWNER, NAME, &seed)?;
    std::fs::remove_dir_all(&seed)?;
    let github = FakeGithub::start(&hosted)?;
    Ok((root, github))
}

/// The environment a container carries: the remote, the GitHub to mint from, and the app key
/// `credential` holds, when it holds one.
fn container_env(github: &FakeGithub, credential: Option<String>) -> Vec<(String, String)> {
    let mut environment = vec![
        ("CTX_MEMORY_REMOTE".to_owned(), github.remote(OWNER, NAME)),
        ("CTX_GITHUB_API".to_owned(), github.api()),
    ];
    if let Some(credential) = credential {
        environment.push(("CTX_CREDENTIAL_GITHUB_APP".to_owned(), credential));
    }
    environment
}

fn borrowed(environment: &[(String, String)]) -> Vec<(&str, &str)> {
    environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// The whole life of a container against a private remote: it clones on start, follows the
/// remote, and pushes a record back, with no credential helper and nothing in the URL.
#[test]
fn the_server_clones_pulls_and_pushes_a_private_remote() -> Result<()> {
    let (root, github) = private_remote("private-remote")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    let environment = container_env(&github, Some(FakeGithub::app_credential()));
    let bare = github.bare(OWNER, NAME);
    let head = git(&bare, &["rev-parse", "main"])?;

    let server = Server::start_with_env(&tree, &["--pull-interval", "1"], &borrowed(&environment))?;
    let health = server.get(server.tailnet_address()?, "/healthz", &[])?;
    support::fetch_window(&tree)?;
    let put = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &json!({
            "path": "decisions/private.md",
            "content": support::cited_record(),
            "session": "session-private",
        }),
    )?;
    drop(server);

    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "main"])?;
    let remote_url = git(&tree, &["remote", "get-url", "origin"])?;
    let config = std::fs::read_to_string(tree.join(".git/config"))?;
    let requests = github.git_requests()?;
    let minted = github.minted()?;
    remove_scratch(&root)?;

    assert_eq!(health.status, 200, "unexpected body: {}", health.body);
    assert_eq!(
        health.text("head")?,
        head,
        "the server did not clone the private remote"
    );
    assert_eq!(put.status, 200, "unexpected body: {}", put.body);
    assert!(
        listing.lines().any(|name| name == "decisions/private.md"),
        "the push did not reach the private remote: {listing}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path.ends_with("git-upload-pack")),
        "the clone never reached the remote: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path.ends_with("git-receive-pack")),
        "the push never reached the remote: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.authorized),
        "a git request went out without the credential: {requests:?}"
    );
    // A token in the URL would outlive its hour in `.git/config`, and every later `git remote
    // -v` and error message would carry it.
    assert!(
        !remote_url.contains("x-access-token") && !config.contains("ghs_"),
        "the token was written into the clone: {remote_url} {config}"
    );
    assert!(minted >= 1, "no installation token was minted");
    Ok(())
}

/// The negative control: with no app key there is no credential, the remote refuses the
/// clone, and `ctx serve` stops rather than serving an empty tree.
#[test]
fn without_a_credential_the_private_remote_refuses_the_clone() -> Result<()> {
    let (root, github) = private_remote("private-remote-anon")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    let environment = container_env(&github, None);

    let started = Server::start_with_env(&tree, &[], &borrowed(&environment));
    let requests = github.git_requests()?;
    let cloned = tree.join("ctx.toml").is_file();
    remove_scratch(&root)?;

    assert!(
        started.is_err(),
        "ctx serve started against a remote it cannot read"
    );
    assert!(!cloned, "an anonymous clone succeeded");
    assert!(
        requests.iter().any(|request| !request.authorized),
        "the remote was never asked without a credential: {requests:?}"
    );
    Ok(())
}

/// A `ctx` run outside the server clones the same way, which is what the fetch job does.
#[test]
fn the_fetch_job_clones_a_private_remote() -> Result<()> {
    let (root, github) = private_remote("private-remote-fetch")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    let tree_text = tree.to_str().context("the tree path is not valid UTF-8")?;
    let environment = container_env(&github, Some(FakeGithub::app_credential()));

    let mut command = support::binary();
    command.args(["--memory", tree_text, "rules"]);
    for (name, value) in &environment {
        command.env(name, value);
    }
    for (name, value) in support::tree_env(&tree)? {
        command.env(name, value);
    }
    let output = command.output().context("running ctx rules")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "ctx rules failed: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("# Rules"),
        "the private tree was not cloned: {stdout}"
    );
    Ok(())
}

/// The start-up clone that v0.3.6 could not make with a key whose `app_id` is a number, the
/// shape GitHub itself shows: that version refused the key and ran git with no credential.
#[test]
fn the_server_clones_a_private_remote_with_a_numeric_app_id() -> Result<()> {
    let (root, github) = private_remote("private-remote-numeric")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    let environment = container_env(&github, Some(FakeGithub::numeric_app_credential()));
    let head = git(&github.bare(OWNER, NAME), &["rev-parse", "main"])?;

    let server = Server::start_with_env(&tree, &[], &borrowed(&environment))?;
    let health = server.get(server.tailnet_address()?, "/healthz", &[])?;
    drop(server);

    let requests = github.git_requests()?;
    let minted = github.minted()?;
    remove_scratch(&root)?;

    assert_eq!(health.status, 200, "unexpected body: {}", health.body);
    assert_eq!(
        health.text("head")?,
        head,
        "the server did not clone the private remote"
    );
    assert!(!requests.is_empty(), "the clone never reached the remote");
    assert!(
        requests.iter().all(|request| request.authorized),
        "a git request went out without the credential: {requests:?}"
    );
    assert!(minted >= 1, "no installation token was minted");
    Ok(())
}

/// A credential that is named but does not parse stops `ctx` with its own error, and git
/// never runs. Before this change, ctx ran git with no credential and said only that git's
/// authentication failed.
#[test]
fn a_credential_that_does_not_parse_stops_ctx_and_names_the_cause() -> Result<()> {
    let (root, github) = private_remote("private-remote-unparsed")?;
    let tree = root.join("tree");
    std::fs::create_dir_all(&tree)?;
    let tree_text = tree.to_str().context("the tree path is not valid UTF-8")?;
    // The client id where the app id belongs: GitHub shows the two side by side.
    let credential = json!({
        "app_id": "Iv23liFixture",
        "private_key": include_str!("fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    let environment = container_env(&github, Some(credential));

    let mut command = support::binary();
    command.args(["--memory", tree_text, "rules"]);
    for (name, value) in &environment {
        command.env(name, value);
    }
    for (name, value) in support::tree_env(&tree)? {
        command.env(name, value);
    }
    let output = command.output().context("running ctx rules")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let requests = github.git_requests()?;
    remove_scratch(&root)?;

    assert!(
        !output.status.success(),
        "ctx rules ran with a credential it cannot parse: {stdout}{stderr}"
    );
    assert!(
        stderr.contains("the credential is not the expected JSON"),
        "the error does not name the cause: {stderr}"
    );
    assert!(
        !stderr.contains("PRIVATE KEY"),
        "the error carries the key: {stderr}"
    );
    assert!(
        requests.is_empty(),
        "git asked the remote without the credential: {requests:?}"
    );
    Ok(())
}
