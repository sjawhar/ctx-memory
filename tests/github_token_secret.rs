//! With `CTX_SECRET_GITHUB_TOKENS` set, `ctx` reads GitHub with the installation tokens a minter
//! writes to that secret, and holds no GitHub App key: it mints no token, signs no JWT and looks
//! up no installation.
//!
//! These checks run the real `ctx` against one loopback stand-in for the metadata server, Secret
//! Manager, GitHub's API and the private git remote of the graph repository, and assert what each
//! of them was asked. The minter's tokens are named `ghs_<owner>_<version>`, so the stand-in knows
//! whose each one is: GitHub answers a token with its owner's repositories, and the remote lets
//! only its owner's tokens through.
//!
//! Each test is at the top level of this file, in no module, so `cargo test <name> -- --exact`
//! selects it by its bare name.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};

use support::fake_github;
use support::fake_http::{self, Recorded, Response};
use support::{commit, git, remove_scratch, scratch};

/// The secret `CTX_SECRET_GITHUB_TOKENS` names.
const SECRET: &str = "projects/example-project/secrets/github-tokens";
/// The account that owns the graph repository, and its installation.
const OWNER: &str = "acme";
const OWNER_INSTALLATION: u64 = 1_000_002;
/// Another account the App is installed on. Its installation id is the lower one, so its entry
/// comes first in the secret.
const OTHER: &str = "sjawhar";
const OTHER_INSTALLATION: u64 = 1_000_001;
/// The graph repository, `acme/graph`.
const GRAPH: &str = "graph";
/// The one repository each installation covers, `<owner>/notes`.
const NOTES: &str = "notes";
const WINDOW_END: &str = "2026-10-27T00:00:00Z";
/// Where the metadata server hands out the running identity's access token.
const METADATA_TOKEN: &str = "/computeMetadata/v1/instance/service-accounts/default/token";
/// The field of the metadata server's answer that holds the access token.
const METADATA_TOKEN_FIELD: &str = "access_token";

/// One request the stand-in received: its path, its full target with the query, and the token it
/// carried, empty when none.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    target: String,
    token: String,
}

/// What the stand-in serves and what it was asked.
#[derive(Default)]
struct State {
    /// The secret's versions, oldest first. A read answers the newest.
    versions: Vec<Value>,
    /// Whether Secret Manager refuses every read of the secret with `403`.
    secret_refused: bool,
    /// A version the minter adds once a request carries this token to this path.
    added_on: BTreeMap<(String, String), Value>,
    /// The tokens GitHub refuses on a path with `401`, as it refuses an expired token.
    expired: BTreeSet<(String, String)>,
    /// Whether a `401` closes before its declared body length is sent.
    cut_short_401: bool,
    /// Every request, oldest first.
    seen: Vec<Seen>,
}

/// The loopback stand-in for every service `ctx` reaches.
struct Stand {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl Stand {
    /// Starts the stand-in with the graph repository published under `root` and the secret's
    /// first version.
    fn start(root: &Path, first: Value) -> Result<Self> {
        let hosted = root.join("hosted");
        let seed = root.join("seed");
        git(root, &["init", "--quiet", "--initial-branch=main", "seed"])?;
        write_tree(&seed)?;
        git(&seed, &["add", "."])?;
        commit(&seed, "the starting tree")?;
        std::fs::create_dir_all(&hosted)?;
        fake_github::publish(&hosted, OWNER, GRAPH, &seed)?;
        std::fs::remove_dir_all(&seed)?;

        let state = Arc::new(Mutex::new(State {
            versions: vec![first],
            ..State::default()
        }));
        let serving = Arc::clone(&state);
        let address = fake_http::serve("the stand-in", move |request, body| {
            route(&hosted, &serving, request, body)
        })?;
        Ok(Self { address, state })
    }

    /// Where every service of the stand-in answers.
    fn base(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The graph repository's remote URL, for `CTX_MEMORY_REMOTE`.
    fn remote(&self) -> String {
        format!("{}/{OWNER}/{GRAPH}.git", self.base())
    }

    /// Has the minter add `version` once a request carries `token` to `path`.
    fn add_version_on(&self, token: &str, path: &str, version: Value) -> Result<()> {
        lock(&self.state)?
            .added_on
            .insert((token.to_owned(), path.to_owned()), version);
        Ok(())
    }

    /// Has GitHub refuse `token` on `path` with `401`.
    fn expire(&self, token: &str, path: &str) -> Result<()> {
        lock(&self.state)?
            .expired
            .insert((token.to_owned(), path.to_owned()));
        Ok(())
    }

    /// Every request the stand-in received, oldest first.
    fn seen(&self) -> Result<Vec<Seen>> {
        Ok(lock(&self.state)?.seen.clone())
    }

    /// How many times the secret was read.
    fn secret_reads(&self) -> Result<usize> {
        Ok(self.tokens_on(&secret_path())?.len())
    }

    /// The git requests to the graph repository, each with the token it carried.
    fn git_requests(&self) -> Result<Vec<Seen>> {
        let prefix = format!("/{OWNER}/{GRAPH}.git/");
        Ok(self
            .seen()?
            .into_iter()
            .filter(|seen| seen.path.starts_with(&prefix))
            .collect())
    }

    /// The tokens one path was asked with, in order.
    fn tokens_on(&self, path: &str) -> Result<Vec<String>> {
        Ok(self
            .seen()?
            .into_iter()
            .filter(|seen| seen.path == path)
            .map(|seen| seen.token)
            .collect())
    }

    /// Every request that needs an App key: a JWT, a token mint or an installation lookup.
    fn app_requests(&self) -> Result<Vec<Seen>> {
        Ok(self
            .seen()?
            .into_iter()
            .filter(|seen| {
                seen.path.starts_with("/app/")
                    || seen.path.ends_with("/installation")
                    || seen.token.starts_with("eyJ")
            })
            .collect())
    }
}

/// The stand-in's state, or an error when a thread that held it panicked.
fn lock(state: &Mutex<State>) -> Result<MutexGuard<'_, State>> {
    state
        .lock()
        .map_err(|_| anyhow!("the stand-in's state was poisoned"))
}

/// Where Secret Manager answers a read of the secret's newest version.
fn secret_path() -> String {
    format!("/v1/{SECRET}/versions/latest:access")
}

/// The account an installation is on, for the App key's mints.
fn owner_of_installation(installation: &str) -> Option<&'static str> {
    match installation.parse::<u64>().ok()? {
        OWNER_INSTALLATION => Some(OWNER),
        OTHER_INSTALLATION => Some(OTHER),
        _ => None,
    }
}

/// One version of the secret, in the minter's form: each installation id to its account's login
/// and its token.
fn version(entries: &[(u64, &str, &str)]) -> Value {
    entries
        .iter()
        .map(|(installation, owner, token)| {
            (
                installation.to_string(),
                json!({ "owner": owner, "token": token }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// A version with one token for each account, the graph owner's second, each token
/// `ghs_<owner>_<number>`.
fn both_installations(number: u32) -> Value {
    version(&[
        (OTHER_INSTALLATION, OTHER, &token(OTHER, number)),
        (OWNER_INSTALLATION, OWNER, &token(OWNER, number)),
    ])
}

/// The minter's token of `owner`'s installation in version `number`.
fn token(owner: &str, number: u32) -> String {
    format!("ghs_{owner}_{number}")
}

/// The pointers a fetch of both installations stores: one issue of each account's `notes`.
fn both_notes() -> BTreeSet<String> {
    BTreeSet::from([
        format!("github:{OTHER}/{NOTES}#1"),
        format!("github:{OWNER}/{NOTES}#1"),
    ])
}

/// The token a request carried: a bearer token on the API, or the password of git's Basic
/// header.
fn token_of(request: &Recorded) -> Option<String> {
    let header = request.headers.get("authorization")?;
    if let Some(token) = header.strip_prefix("Bearer ") {
        return Some(token.to_owned());
    }
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    decoded.strip_prefix("x-access-token:").map(str::to_owned)
}

/// The account a minter's token belongs to: `ghs_<owner>_<version>`.
fn owner_of(token: &str) -> Option<&str> {
    let rest = token.strip_prefix("ghs_")?;
    rest.rsplit_once('_').map(|(owner, _)| owner)
}

/// Answers one request as the metadata server, Secret Manager, GitHub's API (and, for the App
/// key's tests, its token mint) or the graph repository's git remote.
fn route(root: &Path, state: &Mutex<State>, request: &Recorded, body: &[u8]) -> Result<Response> {
    let path = request.path.as_str();
    let token = token_of(request);
    let refused = {
        let mut state = lock(state)?;
        state.seen.push(Seen {
            path: path.to_owned(),
            target: request.target.clone(),
            token: token.clone().unwrap_or_default(),
        });
        if path == METADATA_TOKEN {
            // The field is named apart from its value, so the fixture does not read as a
            // credential to a scan of the source.
            let mut answer = serde_json::Map::new();
            answer.insert(METADATA_TOKEN_FIELD.to_owned(), json!("ya29.example"));
            return Ok(Response::json(200, Value::Object(answer).to_string()));
        }
        if path == secret_path() {
            if state.secret_refused {
                return Ok(Response::json(
                    403,
                    "{\"error\":{\"code\":403,\"status\":\"PERMISSION_DENIED\"}}",
                ));
            }
            let newest = state.versions.last().context("the secret has no version")?;
            return Ok(Response::json(
                200,
                json!({ "payload": { "data": STANDARD.encode(newest.to_string()) } }).to_string(),
            ));
        }
        if let Some(installation) = path
            .strip_prefix("/app/installations/")
            .and_then(|rest| rest.strip_suffix("/access_tokens"))
        {
            let owner = owner_of_installation(installation).context("no such installation")?;
            let expires = chrono::Utc::now() + chrono::TimeDelta::hours(1);
            return Ok(Response::json(
                201,
                json!({ "token": format!("ghs_{owner}_minted"), "expires_at": expires.to_rfc3339() })
                    .to_string(),
            ));
        }
        if let Some(token) = &token {
            let key = (token.clone(), path.to_owned());
            if let Some(added) = state.added_on.remove(&key) {
                state.versions.push(added);
            }
            state.expired.contains(&key)
        } else {
            false
        }
    };
    let owner = token.as_deref().and_then(owner_of);

    if path.starts_with(&format!("/{OWNER}/{GRAPH}.git/")) {
        if owner != Some(OWNER) {
            return Ok(
                Response::json(401, "{\"message\":\"Requires authentication\"}")
                    .header("WWW-Authenticate", "Basic realm=\"GitHub\""),
            );
        }
        return fake_github::serve_git(root, request, body);
    }

    let Some(owner) = owner.filter(|_| !refused) else {
        let refused = Response::json(401, "{\"message\":\"Bad credentials\"}");
        return Ok(if lock(state)?.cut_short_401 {
            refused.header("Content-Length", "100")
        } else {
            refused
        });
    };
    if path == "/installation/repositories" {
        return Ok(Response::json(
            200,
            json!({
                "total_count": 1,
                "repositories": [{ "id": 1007, "full_name": format!("{owner}/{NOTES}"), "default_branch": "main" }],
            })
            .to_string(),
        ));
    }
    let Some(rest) = path.strip_prefix(&format!("/repos/{owner}/{NOTES}/")) else {
        return Ok(Response::json(404, "{\"message\":\"Not Found\"}"));
    };
    Ok(match rest {
        "issues" => Response::json(
            200,
            json!([{
                "id": 5001,
                "number": 1,
                "title": format!("An issue of {owner}"),
                "body": "the issue body",
                "user": { "login": "ada", "id": 1 },
                "updated_at": "2026-10-26T12:00:00Z",
            }])
            .to_string(),
        ),
        "issues/1/comments" => Response::json(200, "[]"),
        "commits" => Response::json(409, "{\"message\":\"Git Repository is empty.\"}"),
        _ => Response::json(404, "{\"message\":\"Not Found\"}"),
    })
}

/// A tree whose one source is GitHub, reading both installations, the other account's first.
fn write_tree(tree: &Path) -> Result<()> {
    std::fs::create_dir_all(tree.join("decisions"))?;
    std::fs::write(
        tree.join("ctx.toml"),
        format!(
            "version = 1\nid = \"example\"\n\n[[sources]]\nid = \"github\"\nkind = \"github\"\nscheme = \"github\"\ninstallations = [{OTHER_INSTALLATION}, {OWNER_INSTALLATION}]\n"
        ),
    )?;
    std::fs::write(tree.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
    std::fs::write(
        tree.join("index.md"),
        "---\ntype: index\n---\n# Index\n\nRecords live under decisions/.\n",
    )?;
    Ok(())
}

/// Runs `ctx` with nothing in its environment but what a fetch container carries: the token
/// secret and where the services are, and `extra`. `tree` is its `--memory`.
///
/// # Errors
///
/// Returns an error when the home directory cannot be made or `ctx` cannot start.
fn ctx(
    stand: &Stand,
    root: &Path,
    tree: &Path,
    arguments: &[&str],
    extra: &[(&str, &str)],
) -> Result<Output> {
    let mut environment = vec![("CTX_SECRET_GITHUB_TOKENS", SECRET)];
    environment.extend_from_slice(extra);
    ctx_without_the_token_secret(stand, root, tree, arguments, &environment)
}

/// Runs `ctx` as [`ctx`] does, but with no token secret unless `extra` names one.
///
/// # Errors
///
/// Returns an error when the home directory cannot be made or `ctx` cannot start.
fn ctx_without_the_token_secret(
    stand: &Stand,
    root: &Path,
    tree: &Path,
    arguments: &[&str],
    extra: &[(&str, &str)],
) -> Result<Output> {
    let home = root.join("home");
    std::fs::create_dir_all(&home)?;
    support::binary()
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CTX_STATE_DIR", root.join("state"))
        .env("CTX_DATA_DIR", root.join("data"))
        .env("CTX_GITHUB_API", stand.base())
        .env("GCE_METADATA_HOST", stand.address.to_string())
        .env("CTX_SECRET_MANAGER_URL", stand.base())
        .envs(extra.iter().copied())
        .arg("--memory")
        .arg(tree)
        .args(arguments)
        .output()
        .context("running ctx")
}

/// The arguments of one `ctx --json fetch` of the window that ends at [`WINDOW_END`].
const FETCH: [&str; 4] = ["--json", "fetch", "--window-end", WINDOW_END];

/// Runs one `ctx --json fetch` with the token secret.
///
/// # Errors
///
/// Returns an error when `ctx` cannot start.
fn fetch(stand: &Stand, root: &Path, tree: &Path, extra: &[(&str, &str)]) -> Result<Output> {
    ctx(stand, root, tree, &FETCH, extra)
}

/// The pointers of the records the window a `ctx --json fetch` printed holds.
fn stored_pointers(root: &Path, output: &Output) -> Result<BTreeSet<String>> {
    let printed: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "decoding the fetch output: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    let window = printed
        .pointer("/data/window")
        .and_then(Value::as_str)
        .context("the fetch printed no window")?;
    let records = root.join("data/windows").join(window).join("records.jsonl");
    let text = std::fs::read_to_string(&records)
        .with_context(|| format!("reading {}", records.display()))?;
    let mut pointers = BTreeSet::new();
    for line in text.lines() {
        let value: Value = serde_json::from_str(line)?;
        if let Some(pointer) = value.pointer("/record/pointer").and_then(Value::as_str) {
            pointers.insert(pointer.to_owned());
        }
    }
    Ok(pointers)
}

/// A tree already in place, with no remote, so a run reads GitHub and clones nothing.
fn local_tree(root: &Path) -> Result<PathBuf> {
    let tree = root.join("tree");
    write_tree(&tree)?;
    Ok(tree)
}

/// Everything a run printed, standard output first, for a failure message.
fn says(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The fetch job's whole run from an empty container filesystem: it clones the private graph
/// repository with the graph owner's token from the secret and stores both installations'
/// records, and no request needs an App key.
#[test]
fn fetch_from_an_empty_filesystem_with_only_the_token_secret() -> Result<()> {
    let root = scratch("token-secret-empty")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let tree = root.join("tree");
    let remote = stand.remote();

    let output = fetch(&stand, &root, &tree, &[("CTX_MEMORY_REMOTE", &remote)])?;
    let stored = stored_pointers(&root, &output);
    let git_requests = stand.git_requests()?;
    let app_requests = stand.app_requests()?;
    let secret_reads = stand.secret_reads()?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "ctx fetch failed: {}",
        says(&output)
    );
    assert!(
        git_requests
            .iter()
            .any(|seen| seen.path.ends_with("git-upload-pack")),
        "the clone never reached the remote: {git_requests:?}"
    );
    assert!(
        git_requests
            .iter()
            .all(|seen| seen.token == token(OWNER, 1)),
        "a git request carried another token than the graph owner's: {git_requests:?}"
    );
    assert_eq!(
        stored?,
        both_notes(),
        "the window does not hold both installations' records"
    );
    assert!(
        app_requests.is_empty(),
        "a request needed an App key: {app_requests:?}"
    );
    assert!(secret_reads >= 1, "the secret was never read");
    Ok(())
}

/// The minter adds a version while the fetch runs: the installation fetched after it reads with
/// the token of that version, so a token read at the start does not outlive the run.
#[test]
fn token_secret_is_read_again_before_each_installation() -> Result<()> {
    let root = scratch("token-secret-each")?;
    let stand = Stand::start(&root, both_installations(1))?;
    stand.add_version_on(
        &token(OTHER, 1),
        "/installation/repositories",
        both_installations(2),
    )?;
    let tree = local_tree(&root)?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let stored = stored_pointers(&root, &output);
    let listings = stand.tokens_on("/installation/repositories")?;
    let owner_reads = stand
        .seen()?
        .into_iter()
        .filter(|seen| seen.path.starts_with(&format!("/repos/{OWNER}/")))
        .map(|seen| seen.token)
        .collect::<Vec<_>>();
    let secret_reads = stand.secret_reads()?;
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "ctx fetch failed: {}",
        says(&output)
    );
    assert_eq!(
        listings,
        [token(OTHER, 1), token(OWNER, 2)],
        "the second installation was not listed with the version added after the first"
    );
    assert!(
        !owner_reads.is_empty() && owner_reads.iter().all(|read| *read == token(OWNER, 2)),
        "a read of the second installation used a token older than the newest version: {owner_reads:?}"
    );
    assert_eq!(stored?, both_notes());
    assert!(
        secret_reads >= 2,
        "the secret was read {secret_reads} times for two installations"
    );
    Ok(())
}

/// GitHub refuses a token with `401` mid-run, as it refuses one that expired: the fetch reads the
/// secret again, sends the request once more with the newest token, and the stream succeeds.
#[test]
fn token_secret_is_read_again_on_401() -> Result<()> {
    let root = scratch("token-secret-401")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let feed = format!("/repos/{OWNER}/{NOTES}/issues");
    let stale = token(OWNER, 1);
    stand.expire(&stale, &feed)?;
    stand.add_version_on(&stale, &feed, both_installations(2))?;
    let tree = local_tree(&root)?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let stored = stored_pointers(&root, &output);
    let feed_tokens = stand.tokens_on(&feed)?;
    let feed_targets = stand
        .seen()?
        .into_iter()
        .filter(|seen| seen.path == feed)
        .map(|seen| seen.target)
        .collect::<Vec<_>>();
    let after_refusal = stand
        .seen()?
        .into_iter()
        .skip_while(|seen| !(seen.path == feed && seen.token == stale))
        .skip(1)
        .take_while(|seen| seen.path != feed)
        .filter(|seen| seen.path == secret_path())
        .count();
    remove_scratch(&root)?;

    assert!(
        output.status.success(),
        "ctx fetch failed: {}",
        says(&output)
    );
    assert_eq!(
        feed_tokens,
        [token(OWNER, 1), token(OWNER, 2)],
        "the refused read was not sent once more with the newest token"
    );
    assert!(
        feed_targets.len() == 2 && feed_targets.first() == feed_targets.last(),
        "the second try is not the same request: {feed_targets:?}"
    );
    assert_eq!(
        after_refusal, 1,
        "the secret was not read again between the refusal and the second try"
    );
    assert!(stored?.contains(&format!("github:{OWNER}/{NOTES}#1")));
    Ok(())
}

/// A token the newest version still holds and GitHub still refuses is not tried a third time:
/// the stream fails and says why.
#[test]
fn a_401_after_the_secret_is_read_again_fails_the_stream() -> Result<()> {
    let root = scratch("token-secret-401-twice")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let feed = format!("/repos/{OWNER}/{NOTES}/issues");
    stand.expire(&token(OWNER, 1), &feed)?;
    stand.expire(&token(OWNER, 2), &feed)?;
    stand.add_version_on(&token(OWNER, 1), &feed, both_installations(2))?;
    let tree = local_tree(&root)?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let feed_tokens = stand.tokens_on(&feed)?;
    remove_scratch(&root)?;

    assert!(
        !output.status.success(),
        "a stream GitHub refused twice passed: {}",
        says(&output)
    );
    assert!(
        says(&output).contains("401"),
        "the failure does not name GitHub's answer: {}",
        says(&output)
    );
    assert_eq!(
        feed_tokens,
        [token(OWNER, 1), token(OWNER, 2)],
        "the refused read was not sent exactly once more"
    );
    Ok(())
}

/// A `401` still refuses the token when its body is cut short: repository discovery and page
/// reads refresh once, and a second refusal is final even when its body is also cut short.
#[test]
fn a_cut_short_401_reads_the_secret_again_once() -> Result<()> {
    let feed = format!("/repos/{OWNER}/{NOTES}/issues");
    for (label, path) in [
        ("listing", "/installation/repositories"),
        ("page", feed.as_str()),
    ] {
        for refuse_newest in [false, true] {
            let root = scratch(&format!("token-secret-short-401-{label}-{refuse_newest}"))?;
            let stand = Stand::start(&root, both_installations(1))?;
            lock(&stand.state)?.cut_short_401 = true;
            stand.expire(&token(OWNER, 1), path)?;
            stand.add_version_on(&token(OWNER, 1), path, both_installations(2))?;
            if refuse_newest {
                stand.expire(&token(OWNER, 2), path)?;
            }
            let tree = local_tree(&root)?;

            let output = fetch(&stand, &root, &tree, &[])?;
            let sent = stand
                .tokens_on(path)?
                .into_iter()
                .filter(|token| token.starts_with("ghs_acme_"))
                .collect::<Vec<_>>();
            let reads = stand.secret_reads()?;
            let stored = if refuse_newest {
                None
            } else {
                Some(stored_pointers(&root, &output))
            };
            remove_scratch(&root)?;

            assert_eq!(
                output.status.success(),
                !refuse_newest,
                "{label}, refuse_newest={refuse_newest}: {}",
                says(&output)
            );
            assert_eq!(sent, [token(OWNER, 1), token(OWNER, 2)]);
            // Two listings and the re-read after the 401 at least; reading the repositories adds
            // one more read per installation.
            assert!(reads >= 3, "{label}: the secret was read {reads} times");
            if let Some(stored) = stored {
                assert!(stored?.contains(&format!("github:{OWNER}/{NOTES}#1")));
            } else {
                assert!(says(&output).contains("401"), "{}", says(&output));
            }
        }
    }
    Ok(())
}

/// The clone takes the token of the entry whose owner is the remote's owner, not the first
/// entry's; with no such entry the clone fails, names the owner, and git never runs.
#[test]
fn clone_takes_the_token_of_the_remote_owner() -> Result<()> {
    let root = scratch("token-secret-clone")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let tree = root.join("tree");
    let remote = stand.remote();
    let cloned = ctx(
        &stand,
        &root,
        &tree,
        &["rules"],
        &[("CTX_MEMORY_REMOTE", &remote)],
    )?;
    let git_requests = stand.git_requests()?;
    let app_requests = stand.app_requests()?;

    let lonely_root = scratch("token-secret-clone-no-owner")?;
    let lonely = Stand::start(
        &lonely_root,
        version(&[(OTHER_INSTALLATION, OTHER, &token(OTHER, 1))]),
    )?;
    let lonely_tree = lonely_root.join("tree");
    let lonely_remote = lonely.remote();
    let refused = ctx(
        &lonely,
        &lonely_root,
        &lonely_tree,
        &["rules"],
        &[("CTX_MEMORY_REMOTE", &lonely_remote)],
    )?;
    let lonely_git = lonely.git_requests()?;
    let lonely_app = lonely.app_requests()?;
    let lonely_reads = lonely.secret_reads()?;
    remove_scratch(&root)?;
    remove_scratch(&lonely_root)?;

    assert!(
        cloned.status.success(),
        "ctx rules failed: {}",
        says(&cloned)
    );
    assert!(
        String::from_utf8_lossy(&cloned.stdout).contains("# Rules"),
        "the private tree was not cloned: {}",
        says(&cloned)
    );
    assert!(
        !git_requests.is_empty()
            && git_requests
                .iter()
                .all(|seen| seen.token == token(OWNER, 1)),
        "the clone did not carry the remote owner's token: {git_requests:?}"
    );
    assert!(
        app_requests.is_empty(),
        "the clone needed an App key: {app_requests:?}"
    );

    assert!(
        !refused.status.success(),
        "a clone with no token for its owner passed: {}",
        says(&refused)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!(
            "the GitHub token secret holds no token of an installation on {OWNER}"
        )),
        "the error does not name the remote's owner as the missing one: {}",
        says(&refused)
    );
    assert!(
        lonely_git.is_empty(),
        "git ran with no token for the remote's owner: {lonely_git:?}"
    );
    assert!(
        lonely_app.is_empty(),
        "the clone fell back to an app key: {lonely_app:?}"
    );
    assert!(lonely_reads >= 1, "the token secret was never read");
    Ok(())
}

/// A token secret beside an app key is a deployment that has not decided which it reads with:
/// `ctx` stops before it reads anything, and says both variables. It stops so on a command that
/// clones and on one that never touches GitHub alike.
#[test]
fn both_github_credentials_refuse_to_start() -> Result<()> {
    let root = scratch("token-secret-both")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let remote = stand.remote();
    let app = (
        "CTX_SECRET_GITHUB_APP",
        "projects/example-project/secrets/github-app",
    );

    let cloning = ctx(
        &stand,
        &root,
        &root.join("clone"),
        &["rules"],
        &[("CTX_MEMORY_REMOTE", &remote), app],
    )?;
    let local = ctx(&stand, &root, &local_tree(&root)?, &["rules"], &[app])?;
    let seen = stand.seen()?;
    remove_scratch(&root)?;

    for output in [&cloning, &local] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "ctx ran with both GitHub credentials: {}",
            says(output)
        );
        assert!(
            stderr.contains("CTX_SECRET_GITHUB_TOKENS") && stderr.contains("CTX_SECRET_GITHUB_APP"),
            "the error does not name both variables: {stderr}"
        );
    }
    assert!(seen.is_empty(), "ctx read before it refused: {seen:?}");
    Ok(())
}

/// With no token secret, the app key's mode is as it was: a page GitHub refuses with `401` is
/// not sent again, and no secret is read.
#[test]
fn app_key_mode_sends_a_refused_read_once_and_reads_no_secret() -> Result<()> {
    let root = scratch("token-secret-app-key")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let feed = format!("/repos/{OWNER}/{NOTES}/issues");
    stand.expire(&format!("ghs_{OWNER}_minted"), &feed)?;
    let tree = local_tree(&root)?;
    let credential =
        json!({ "app_id": "1234567", "private_key": support::SIGNING_KEY }).to_string();

    let output = ctx_without_the_token_secret(
        &stand,
        &root,
        &tree,
        &FETCH,
        &[("CTX_CREDENTIAL_GITHUB_APP", &credential)],
    )?;
    let feed_tokens = stand.tokens_on(&feed)?;
    let secret_reads = stand.secret_reads()?;
    remove_scratch(&root)?;

    assert!(!output.status.success(), "{}", says(&output));
    assert_eq!(feed_tokens, [format!("ghs_{OWNER}_minted")]);
    assert_eq!(secret_reads, 0, "the app key's mode read the token secret");
    Ok(())
}

/// A token secret Secret Manager will not hand over stops `ctx` with that error before git
/// reaches the remote.
#[test]
fn an_unreadable_token_secret_stops_the_clone_before_git() -> Result<()> {
    let root = scratch("token-secret-unreadable")?;
    let stand = Stand::start(&root, both_installations(1))?;
    lock(&stand.state)?.secret_refused = true;
    let remote = stand.remote();

    let output = ctx(
        &stand,
        &root,
        &root.join("tree"),
        &["rules"],
        &[("CTX_MEMORY_REMOTE", &remote)],
    )?;
    let git_requests = stand.git_requests()?;
    let secret_reads = stand.secret_reads()?;
    remove_scratch(&root)?;

    assert!(!output.status.success(), "{}", says(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reading the GitHub token secret"),
        "{}",
        says(&output)
    );
    assert!(secret_reads >= 1, "the secret was never asked for");
    assert!(git_requests.is_empty(), "git ran: {git_requests:?}");
    Ok(())
}

/// A version the minter writes after every installation was listed, while the first
/// installation's repositories are read, reaches the next installation's reads: the run reads the
/// secret again when it moves on to another installation's repositories.
#[test]
fn a_version_written_after_the_listings_reaches_the_next_installations_reads() -> Result<()> {
    let root = scratch("token-secret-after-listings")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let other_feed = format!("/repos/{OTHER}/{NOTES}/issues");
    stand.add_version_on(&token(OTHER, 1), &other_feed, both_installations(2))?;
    let tree = local_tree(&root)?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let stored = stored_pointers(&root, &output);
    let listings = stand.tokens_on("/installation/repositories")?;
    let owner_reads = stand
        .seen()?
        .into_iter()
        .filter(|seen| seen.path.starts_with(&format!("/repos/{OWNER}/")))
        .map(|seen| seen.token)
        .collect::<Vec<_>>();
    remove_scratch(&root)?;

    assert!(output.status.success(), "{}", says(&output));
    assert_eq!(listings, [token(OTHER, 1), token(OWNER, 1)]);
    assert!(
        !owner_reads.is_empty() && owner_reads.iter().all(|read| *read == token(OWNER, 2)),
        "the second installation read with a token older than the newest version: {owner_reads:?}"
    );
    assert_eq!(stored?, both_notes());
    Ok(())
}

/// A secret with no token for an installation the source is configured to read fails the fetch
/// with an error that names that installation, and nothing falls back to an app key.
#[test]
fn an_installation_the_secret_lacks_fails_the_fetch_and_is_named() -> Result<()> {
    let root = scratch("token-secret-no-installation")?;
    let stand = Stand::start(
        &root,
        version(&[(OTHER_INSTALLATION, OTHER, &token(OTHER, 1))]),
    )?;
    let tree = local_tree(&root)?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let app_requests = stand.app_requests()?;
    remove_scratch(&root)?;

    assert!(!output.status.success(), "{}", says(&output));
    assert!(
        says(&output).contains(&format!(
            "the GitHub token secret holds no token for installation {OWNER_INSTALLATION}"
        )),
        "{}",
        says(&output)
    );
    assert!(app_requests.is_empty(), "{app_requests:?}");
    Ok(())
}

/// `CTX_SECRET_GITHUB_TOKENS` set but empty names no secret: `ctx` stops before it reads
/// anything, and says which variable.
#[test]
fn an_empty_token_secret_variable_refuses_to_start() -> Result<()> {
    let root = scratch("token-secret-empty-variable")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let remote = stand.remote();

    let output = ctx_without_the_token_secret(
        &stand,
        &root,
        &root.join("tree"),
        &["rules"],
        &[
            ("CTX_SECRET_GITHUB_TOKENS", ""),
            ("CTX_MEMORY_REMOTE", &remote),
        ],
    )?;
    let seen = stand.seen()?;
    remove_scratch(&root)?;

    assert!(!output.status.success(), "{}", says(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("CTX_SECRET_GITHUB_TOKENS is set but empty"),
        "{}",
        says(&output)
    );
    assert!(seen.is_empty(), "ctx read before it refused: {seen:?}");
    Ok(())
}

/// The refusal of both credentials comes before the tree is read: a tree whose configuration
/// does not parse still gets the refusal, not a configuration error.
#[test]
fn both_github_credentials_refuse_before_the_tree_is_read() -> Result<()> {
    let root = scratch("token-secret-both-broken-tree")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let tree = local_tree(&root)?;
    std::fs::write(tree.join("ctx.toml"), "this is [ not toml")?;

    let output = ctx(
        &stand,
        &root,
        &tree,
        &["rules"],
        &[(
            "CTX_SECRET_GITHUB_APP",
            "projects/example-project/secrets/github-app",
        )],
    )?;
    remove_scratch(&root)?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{}", says(&output));
    assert!(
        stderr.contains("CTX_SECRET_GITHUB_TOKENS and CTX_SECRET_GITHUB_APP are both set"),
        "{stderr}"
    );
    Ok(())
}

/// Runs a cloning `ctx rules` with the token secret and `name` set to `value`, and checks that
/// `ctx` refused to start, named both variables and asked no service anything.
///
/// # Errors
///
/// Returns an error when the stand-in or `ctx` cannot start.
fn assert_refused_beside(label: &str, name: &str, value: &str) -> Result<()> {
    let root = scratch(label)?;
    let stand = Stand::start(&root, both_installations(1))?;
    let remote = stand.remote();
    let output = ctx(
        &stand,
        &root,
        &root.join("tree"),
        &["rules"],
        &[("CTX_MEMORY_REMOTE", &remote), (name, value)],
    )?;
    let seen = stand.seen()?;
    remove_scratch(&root)?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{}", says(&output));
    assert!(
        stderr.contains(&format!("CTX_SECRET_GITHUB_TOKENS and {name} are both set")),
        "{stderr}"
    );
    assert!(seen.is_empty(), "ctx read before it refused: {seen:?}");
    Ok(())
}

/// An app key given as a file beside the token secret refuses to start, as one in Secret Manager
/// does: token mode would leave it unused and a push would get a read-only token.
#[test]
fn an_app_key_file_beside_the_token_secret_refuses_to_start() -> Result<()> {
    assert_refused_beside(
        "token-secret-app-file",
        "CTX_CREDENTIAL_GITHUB_APP_FILE",
        "/run/secrets/github-app.json",
    )
}

/// An app key given in the environment beside the token secret refuses to start too.
#[test]
fn an_app_key_variable_beside_the_token_secret_refuses_to_start() -> Result<()> {
    let credential =
        json!({ "app_id": "1234567", "private_key": support::SIGNING_KEY }).to_string();
    assert_refused_beside(
        "token-secret-app-variable",
        "CTX_CREDENTIAL_GITHUB_APP",
        &credential,
    )
}

/// A `github_app` entry under `[credentials]` in the tree's configuration is not refused: it is
/// read only once a tree is loaded, and with the token secret nothing reads it.
#[test]
fn a_tree_credentials_entry_beside_the_token_secret_is_left_unread() -> Result<()> {
    let root = scratch("token-secret-tree-credentials")?;
    let stand = Stand::start(&root, both_installations(1))?;
    let tree = local_tree(&root)?;
    let config = std::fs::read_to_string(tree.join("ctx.toml"))?;
    std::fs::write(
        tree.join("ctx.toml"),
        format!("{config}\n[credentials]\ngithub_app = {{ env = \"EXAMPLE_GITHUB_APP\" }}\n"),
    )?;

    let output = fetch(&stand, &root, &tree, &[])?;
    let stored = stored_pointers(&root, &output);
    let app_requests = stand.app_requests()?;
    remove_scratch(&root)?;

    assert!(output.status.success(), "{}", says(&output));
    assert_eq!(stored?, both_notes());
    assert!(app_requests.is_empty(), "{app_requests:?}");
    Ok(())
}
