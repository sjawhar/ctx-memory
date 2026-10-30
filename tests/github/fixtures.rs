//! The emulated GitHub, the fetch runners and the result readers that more than one GitHub
//! check uses.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use ctx_memory::source::{
    FetchRequest, GithubSource, Message, Request, Source, SourceRef, StreamStatus,
};
use serde_json::{json, Value};

use crate::support;
use crate::support::fake_http::{Recorded, Response};

/// Environment variables are process-wide, so these tests take turns.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

pub(super) fn guard() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn scratch(label: &str) -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("ctx-github-it-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The `StreamEnd` of each stream the run reported: its id, status, cursor, and error.
pub(super) fn stream_ends(
    messages: &[Message],
) -> Vec<(String, StreamStatus, Option<String>, Option<String>)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::StreamEnd {
                stream,
                status,
                cursor,
                error,
            } => Some((stream.clone(), *status, cursor.clone(), error.clone())),
            _ => None,
        })
        .collect()
}

/// Runs the source once, as a fetch would, with the cursors a previous run left.
pub(super) fn run_fetch(
    config: Value,
    state_dir: std::path::PathBuf,
    cursors: BTreeMap<String, String>,
) -> Result<Vec<Message>> {
    run_fetch_until(config, state_dir, cursors, "2026-10-27T00:00:00Z")
}

/// Runs the source once for the window that ends at `window_end`.
pub(super) fn run_fetch_until(
    config: Value,
    state_dir: std::path::PathBuf,
    cursors: BTreeMap<String, String>,
    window_end: &str,
) -> Result<Vec<Message>> {
    run_fetch_excluding(config, state_dir, cursors, window_end, &[])
}

/// Runs the source once for the window that ends at `window_end`, with the deployment's
/// exclusion patterns.
pub(super) fn run_fetch_excluding(
    config: Value,
    state_dir: std::path::PathBuf,
    cursors: BTreeMap<String, String>,
    window_end: &str,
    exclude: &[&str],
) -> Result<Vec<Message>> {
    let request = Request::Fetch(FetchRequest {
        protocol: 1,
        source: SourceRef {
            id: "github".to_owned(),
            kind: "github".to_owned(),
            scheme: "github".to_owned(),
        },
        config,
        window_end: DateTime::parse_from_rfc3339(window_end)?.with_timezone(&Utc),
        backfill_hours: 24,
        cursors,
        discovered: Vec::new(),
        state_dir,
        exclude: exclude
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
    });
    let mut messages = Vec::new();
    GithubSource::new().run(&request, &mut |message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(messages)
}

/// The pointers of the records a run sent.
pub(super) fn pointers(messages: &[Message]) -> BTreeSet<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Record(record) => Some(record.pointer.clone()),
            _ => None,
        })
        .collect()
}

/// Runs `body` against the GitHub at `address`, with the app's key in the environment.
pub(super) fn against<T>(address: SocketAddr, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", format!("http://{address}"));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);
    let outcome = body();
    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");
    outcome
}

/// The one repository the emulated GitHub holds.
pub(super) const BUSY: &str = "/repos/example-org/busy";
/// Its stream.
pub(super) const BUSY_STREAM: &str = "github:example-org/busy";

/// A change the emulated GitHub makes to its data once.
pub(super) type Change = Box<dyn FnOnce(&mut Busy) + Send>;

/// A GitHub that answers the issues feed, an issue's comments, a pull request's review comments,
/// the commits listing and a comparison the way the real one does — the feed paged by key, the
/// rest by page number — from data a test changes between two page requests.
#[derive(Default)]
pub(super) struct Busy {
    /// The issues and pull requests of `example-org/busy`.
    pub(super) issues: Vec<Value>,
    /// Each issue's comments, by number.
    pub(super) comments: BTreeMap<u64, Vec<Value>>,
    /// Each pull request's review comments, by number.
    pub(super) review_comments: BTreeMap<u64, Vec<Value>>,
    /// Every commit by sha: its parents and its date.
    pub(super) commits: BTreeMap<String, (Vec<String>, String)>,
    /// The commit `main` points at; without one the branch is empty, and GitHub answers 409.
    pub(super) main: Option<String>,
    /// The repository's answer to the GraphQL pre-pass; without one, `/graphql` answers 404.
    pub(super) activity: Option<Value>,
    /// The most commits a comparison lists across its pages; its `total_commits` still counts
    /// them all.
    pub(super) compare_cap: Option<usize>,
    /// A change made once, right after the first request to this path is answered; a path
    /// followed by `?page=<n>` waits for the first request to that page of it.
    pub(super) change: Option<(String, Change)>,
    /// Every request's target, oldest first.
    pub(super) requests: Vec<String>,
    /// How many installation tokens were minted.
    pub(super) minted: usize,
}

impl Busy {
    pub(super) fn serve(self) -> Result<(SocketAddr, Arc<Mutex<Self>>)> {
        let shared = Arc::new(Mutex::new(self));
        let serving = Arc::clone(&shared);
        let address = support::fake_http::serve("an emulated GitHub", move |request, _| {
            let mut busy = serving
                .lock()
                .map_err(|_| anyhow!("the emulated GitHub was poisoned"))?;
            busy.requests.push(request.target.clone());
            let response = busy.answer(request)?;
            if busy
                .change
                .as_ref()
                .is_some_and(|(key, _)| triggers(key, request))
            {
                if let Some((_, change)) = busy.change.take() {
                    change(&mut busy);
                }
            }
            Ok(response)
        })?;
        Ok((address, shared))
    }

    fn answer(&mut self, request: &Recorded) -> Result<Response> {
        let path = request.path.as_str();
        if path == "/app/installations/1000002/access_tokens" {
            self.minted += 1;
            let token = format!("ghs_run{}", self.minted);
            return Ok(Response::json(
                201,
                json!({ "token": token, "expires_at": "2099-01-01T00:00:00Z" }).to_string(),
            ));
        }
        if path == "/installation/repositories" {
            return Ok(Response::json(
                200,
                json!({ "total_count": 1, "repositories": [
                    { "full_name": "example-org/busy", "default_branch": "main" } ] })
                .to_string(),
            ));
        }
        if path == "/graphql" {
            return Ok(self.activity.as_ref().map_or_else(
                || Response::json(404, "{}"),
                |node| Response::json(200, json!({ "data": { "r0": node } }).to_string()),
            ));
        }
        let Some(rest) = path.strip_prefix(BUSY) else {
            return Ok(Response::json(404, "{}"));
        };
        let host = request.headers.get("host").cloned().unwrap_or_default();
        let url = format!("http://{host}{path}");
        let query = query_of(&request.target);
        let segments: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        Ok(match segments.as_slice() {
            ["issues"] => feed_page(&self.issues, &url, &query),
            ["issues", number, "comments"] => {
                let listed = self.comments.get(&number.parse()?).cloned();
                offset_page(&listed.unwrap_or_default(), &url, &query, false)
            }
            ["pulls", number, "comments"] => {
                let listed = self.review_comments.get(&number.parse()?).cloned();
                offset_page(&listed.unwrap_or_default(), &url, &query, true)
            }
            ["commits"] => self.commits_page(&url, &query),
            ["compare", range] => self.compare_page(range, &url, &query),
            ["pulls", _, "reviews"] => Response::json(200, "[]"),
            _ => Response::json(404, "{}"),
        })
    }

    /// Every commit reachable from `start`, `start` included.
    fn history(&self, start: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![start.to_owned()];
        while let Some(sha) = todo.pop() {
            if let Some((parents, _)) = self.commits.get(&sha) {
                if seen.insert(sha) {
                    todo.extend(parents.iter().cloned());
                }
            }
        }
        seen
    }

    fn commit_json(&self, sha: &str) -> Value {
        let (parents, date) = self.commits.get(sha).cloned().unwrap_or_default();
        json!({
            "sha": sha,
            "commit": {
                "message": format!("commit {sha}"),
                "author": { "name": "Ada", "date": date },
                "committer": { "date": date }
            },
            "author": { "login": "ada", "id": 1 },
            "parents": parents.iter().map(|parent| json!({ "sha": parent })).collect::<Vec<_>>()
        })
    }

    /// `commits?sha=<branch or sha>&since=…`: the history newest first, dated at or after
    /// `since`, by page number.
    fn commits_page(&self, url: &str, query: &[(String, String)]) -> Response {
        let start = match param(query, "sha") {
            Some("main") => self.main.clone(),
            Some(sha) => Some(sha.to_owned()),
            None => None,
        };
        let Some(start) = start.filter(|sha| self.commits.contains_key(sha)) else {
            return Response::json(
                409,
                json!({ "message": "Git Repository is empty." }).to_string(),
            );
        };
        let since = param(query, "since").unwrap_or_default();
        let mut listed: Vec<(String, String)> = self
            .history(&start)
            .into_iter()
            .filter_map(|sha| {
                let date = self.commits.get(&sha)?.1.clone();
                (date.as_str() >= since).then_some((date, sha))
            })
            .collect();
        listed.sort();
        listed.reverse();
        let items: Vec<Value> = listed
            .iter()
            .map(|(_, sha)| self.commit_json(sha))
            .collect();
        let (page, link) = numbered(&items, url, query);
        with_link(Response::json(200, Value::Array(page).to_string()), link)
    }

    /// `compare/<base>...<head>`: the commits `head` has that `base`'s history lacks, oldest
    /// first, by page number, no more than `compare_cap` of them, with `total_commits`
    /// counting them all; `404` when either is unknown.
    fn compare_page(&self, range: &str, url: &str, query: &[(String, String)]) -> Response {
        let Some((base, head)) = range.split_once("...") else {
            return Response::json(404, "{}");
        };
        if !self.commits.contains_key(base) || !self.commits.contains_key(head) {
            return Response::json(404, json!({ "message": "Not Found" }).to_string());
        }
        let behind = self.history(base);
        let mut listed: Vec<(String, String)> = self
            .history(head)
            .into_iter()
            .filter(|sha| !behind.contains(sha))
            .filter_map(|sha| Some((self.commits.get(&sha)?.1.clone(), sha)))
            .collect();
        listed.sort();
        let status = if self.history(head).contains(base) {
            "ahead"
        } else {
            "diverged"
        };
        let items: Vec<Value> = listed
            .iter()
            .take(self.compare_cap.unwrap_or(usize::MAX))
            .map(|(_, sha)| self.commit_json(sha))
            .collect();
        let (page, link) = numbered(&items, url, query);
        with_link(
            Response::json(
                200,
                json!({ "status": status, "total_commits": listed.len(), "commits": page })
                    .to_string(),
            ),
            link,
        )
    }
}

/// Whether `request` is the one a change keyed `key` waits for: a request to the path `key`
/// names, and to the page it names after `?page=`, when it names one.
fn triggers(key: &str, request: &Recorded) -> bool {
    match key.split_once("?page=") {
        Some((path, page)) => {
            path == request.path && param(&query_of(&request.target), "page") == Some(page)
        }
        None => key == request.path,
    }
}

/// A request's query parameters, decoded.
fn query_of(target: &str) -> Vec<(String, String)> {
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), decode(value)))
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        if let (b'%', Some(decoded)) = (byte, hex) {
            out.push(decoded);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(value: &str) -> String {
    value.replace(':', "%3A").replace('|', "%7C")
}

fn param<'q>(query: &'q [(String, String)], key: &str) -> Option<&'q str> {
    query
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn per_page(query: &[(String, String)]) -> usize {
    param(query, "per_page")
        .and_then(|value| value.parse().ok())
        .unwrap_or(30)
}

fn updated(item: &Value) -> String {
    item.get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn id_of(item: &Value) -> u64 {
    item.get("id").and_then(Value::as_u64).unwrap_or_default()
}

/// `query` without the parameters `dropped`, encoded again.
fn kept(query: &[(String, String)], dropped: &[&str]) -> String {
    query
        .iter()
        .filter(|(key, _)| !dropped.contains(&key.as_str()))
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// One page of the issues feed as GitHub answers it: the items changed since `since`, by
/// `updated_at` and then by descending id (the order the real API showed on 2026-10-28), after
/// the item the `after` cursor names, with a next link whose cursor names this page's last item.
fn feed_page(issues: &[Value], url: &str, query: &[(String, String)]) -> Response {
    let since = param(query, "since").unwrap_or_default();
    let size = per_page(query);
    let mut listed: Vec<&Value> = issues
        .iter()
        .filter(|item| updated(item).as_str() >= since)
        .collect();
    listed.sort_by_key(|item| (updated(item), std::cmp::Reverse(id_of(item))));
    if let Some((time, id)) = param(query, "after").and_then(|after| after.split_once('|')) {
        let id: u64 = id.parse().unwrap_or_default();
        listed.retain(|item| {
            let at = updated(item);
            at.as_str() > time || (at.as_str() == time && id_of(item) < id)
        });
    }
    let page: Vec<Value> = listed
        .iter()
        .take(size)
        .map(|item| (*item).clone())
        .collect();
    let mut response = Response::json(200, Value::Array(page.clone()).to_string());
    if let (true, Some(last)) = (listed.len() > size, page.last()) {
        let cursor = encode(&format!("{}|{}", updated(last), id_of(last)));
        let next = format!(
            "{url}?{}&after={cursor}&page=2",
            kept(query, &["after", "page"])
        );
        response = response.header("Link", &format!("<{next}>; rel=\"next\""));
    }
    response
}

/// The page of `items` that `query` asks for by number, and the `Link` header naming the next
/// and the last page, when there are more.
fn numbered(
    items: &[Value],
    url: &str,
    query: &[(String, String)],
) -> (Vec<Value>, Option<String>) {
    let size = per_page(query);
    let number: usize = param(query, "page")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
        .max(1);
    let page: Vec<Value> = items
        .iter()
        .skip((number - 1) * size)
        .take(size)
        .cloned()
        .collect();
    let count = items.len().div_ceil(size);
    let link = (number < count).then(|| {
        let to = |page: usize| format!("{url}?{}&page={page}", kept(query, &["page"]));
        format!(
            "<{}>; rel=\"next\", <{}>; rel=\"last\"",
            to(number + 1),
            to(count)
        )
    });
    (page, link)
}

fn with_link(response: Response, link: Option<String>) -> Response {
    match link {
        Some(link) => response.header("Link", &link),
        None => response,
    }
}

/// One page of a comment listing as GitHub answers it: the items changed since `since`, in
/// id order (or by `updated_at` when `sort=updated` is honoured), paged by page number.
fn offset_page(
    items: &[Value],
    url: &str,
    query: &[(String, String)],
    honours_sort: bool,
) -> Response {
    let since = param(query, "since").unwrap_or_default();
    let mut listed: Vec<&Value> = items
        .iter()
        .filter(|item| updated(item).as_str() >= since)
        .collect();
    listed.sort_by_key(|item| id_of(item));
    if honours_sort && param(query, "sort") == Some("updated") {
        listed.sort_by_key(|item| (updated(item), id_of(item)));
        if param(query, "direction") == Some("desc") {
            listed.reverse();
        }
    }
    let listed: Vec<Value> = listed.into_iter().cloned().collect();
    let (page, link) = numbered(&listed, url, query);
    with_link(Response::json(200, Value::Array(page).to_string()), link)
}

/// A distinct time on 2026-10-25 for item `index`, `index` minutes after 08:00.
pub(super) fn minute(index: u64) -> String {
    format!("2026-10-25T{:02}:{:02}:00Z", 8 + index / 60, index % 60)
}

pub(super) fn busy_issue(number: u64, updated: &str) -> Value {
    json!({
        "id": 5000 + number,
        "number": number,
        "title": format!("Issue {number}"),
        "body": "the issue body",
        "user": { "login": "ada", "id": 1 },
        "updated_at": updated
    })
}

pub(super) fn busy_comment(id: u64, updated: &str) -> Value {
    json!({
        "id": id,
        "body": format!("comment {id}"),
        "user": { "login": "bob", "id": 2 },
        "updated_at": updated
    })
}

/// Runs one fetch of `example-org/busy` from 2026-10-01, and returns its records' pointers.
pub(super) fn fetch_busy(address: SocketAddr, label: &str) -> Result<BTreeSet<String>> {
    let messages = against(address, || {
        run_fetch(
            json!({ "installations": [1_000_002] }),
            scratch(label)?,
            BTreeMap::from([(BUSY_STREAM.to_owned(), "2026-10-01T00:00:00Z".to_owned())]),
        )
    })?;
    let ends = stream_ends(&messages);
    assert!(
        ends.iter()
            .all(|(_, status, _, error)| *status == StreamStatus::Ok && error.is_none()),
        "{ends:?}"
    );
    Ok(pointers(&messages))
}
