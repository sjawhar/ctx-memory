//! Unit tests for the GitHub source.

pub(super) use std::cell::RefCell;
pub(super) use std::collections::{BTreeMap, BTreeSet};
pub(super) use std::path::PathBuf;
pub(super) use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) use anyhow::{anyhow, Context as _, Result};
pub(super) use chrono::{DateTime, Utc};
pub(super) use serde_json::{json, Value};

pub(super) use crate::api::github::{api_base, Page, PageKind, Repo};
pub(super) use crate::pointer::Schemes;

pub(super) use super::super::super::{
    FetchRequest, Session, SourceOutcome, SourceRef, StreamStatus,
};
pub(super) use super::super::cursor_state::{Position, OLD_ETAGS_FILE, STATE_FILE};
pub(super) use super::super::records::{
    commits_url, compare_url, head_url, issue_comments_url, issues_url, pull_review_comments_url,
    pull_reviews_url, stream_id,
};
pub(super) use super::super::{
    fetch_with_api, installations, GithubApi, GithubConfig, Request, PROTOCOL_VERSION,
};

pub(super) const WINDOW_END: &str = "2026-10-27T00:00:00Z";
pub(super) const CURSOR: &str = "2026-10-01T00:00:00Z";
pub(super) const NO_HEAD: &str = "0000000000000000000000000000000000000000";
pub(super) const SINCE: &str = "2026-10-01T00:00:00Z";
pub(super) const LATER_END: &str = "2026-10-27T00:15:00Z";

pub(super) static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

pub(super) fn scratch() -> Result<PathBuf> {
    let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ctx-github-{}-{suffix}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

#[derive(Default)]
pub(super) struct FakeGithub {
    pub(super) repos: BTreeMap<u64, Vec<Repo>>,
    pub(super) pages: RefCell<BTreeMap<String, Page>>,
    pub(super) installations_called: RefCell<Vec<u64>>,
    pub(super) get_log: RefCell<Vec<String>>,
    /// What the GraphQL API knows of each repository (`owner/name`), as one alias's answer.
    /// A repository with no entry is left out of the answer.
    pub(super) activity: RefCell<BTreeMap<String, Value>>,
    /// Errors the GraphQL API answers beside its data, when set.
    pub(super) graphql_errors: RefCell<Option<Value>>,
    /// Every GraphQL request the source sent.
    pub(super) graphql_log: RefCell<Vec<Value>>,
}

impl FakeGithub {
    pub(super) fn page(&self, url: &str, page: Page) {
        self.pages.borrow_mut().insert(url.to_owned(), page);
    }

    /// The GraphQL API says the newest change in `repo` is at `newest`, and its default
    /// branch head is commit `NO_HEAD` unless `head_is` says otherwise.
    pub(super) fn changed(&self, repo: &Repo, newest: &str) {
        self.activity.borrow_mut().insert(
            format!("{}/{}", repo.owner, repo.name),
            json!({
                "pushedAt": newest,
                "defaultBranchRef": {"target": {"oid": NO_HEAD, "committedDate": newest}},
                "issues": {"nodes": [{"updatedAt": newest}]},
                "pullRequests": {"nodes": []},
            }),
        );
    }

    /// The GraphQL API says `repo`'s default branch head is `oid`.
    pub(super) fn head_is(&self, repo: &Repo, oid: &str) {
        if let Some(target) = self
            .activity
            .borrow_mut()
            .get_mut(&format!("{}/{}", repo.owner, repo.name))
            .and_then(|node| node.pointer_mut("/defaultBranchRef/target"))
            .and_then(Value::as_object_mut)
        {
            target.insert("oid".to_owned(), Value::String(oid.to_owned()));
        }
    }

    /// How many repositories each GraphQL request asked about.
    pub(super) fn asked(&self) -> Vec<usize> {
        self.graphql_log
            .borrow()
            .iter()
            .map(|request| {
                request
                    .get("variables")
                    .and_then(Value::as_object)
                    .map_or(0, |variables| variables.len() / 2)
            })
            .collect()
    }
}

impl GithubApi for FakeGithub {
    fn repositories(&self, installation: u64) -> Result<Option<Vec<Repo>>> {
        self.installations_called.borrow_mut().push(installation);
        Ok(Some(
            self.repos.get(&installation).cloned().unwrap_or_default(),
        ))
    }

    fn get(&self, _installation: u64, url: &str) -> Result<Page> {
        self.get_log.borrow_mut().push(url.to_owned());
        self.pages
            .borrow()
            .get(url)
            .cloned()
            .ok_or_else(|| anyhow!("no fake page for {url}"))
    }

    fn graphql(&self, _installation: u64, request: &Value) -> Result<Value> {
        self.graphql_log.borrow_mut().push(request.clone());
        let variables = request
            .get("variables")
            .and_then(Value::as_object)
            .context("the request names variables")?;
        let mut data = serde_json::Map::new();
        for index in 0..variables.len() / 2 {
            let name = |prefix: &str| {
                variables
                    .get(&format!("{prefix}{index}"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let full = format!("{}/{}", name("o"), name("n"));
            if let Some(node) = self.activity.borrow().get(&full) {
                data.insert(format!("r{index}"), node.clone());
            }
        }
        let mut answer = json!({ "data": data });
        if let (Some(errors), Some(fields)) =
            (self.graphql_errors.borrow().clone(), answer.as_object_mut())
        {
            fields.insert("errors".to_owned(), errors);
        }
        Ok(answer)
    }
}

pub(super) fn repo(owner: &str, name: &str) -> Repo {
    Repo {
        owner: owner.to_owned(),
        name: name.to_owned(),
        default_branch: "main".to_owned(),
    }
}

pub(super) fn page(items: Vec<Value>) -> Page {
    Page {
        items,
        ..Page::default()
    }
}

pub(super) fn at(time: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(time)?.with_timezone(&Utc))
}

pub(super) fn request(cursors: BTreeMap<String, String>) -> Result<FetchRequest> {
    Ok(FetchRequest {
        protocol: PROTOCOL_VERSION,
        source: SourceRef {
            id: "github".to_owned(),
            kind: "github".to_owned(),
            scheme: "github".to_owned(),
        },
        config: Value::Null,
        window_end: at(WINDOW_END)?,
        backfill_hours: 24,
        cursors,
        discovered: Vec::new(),
        state_dir: scratch()?,
    })
}

/// The request of the run after `outcome`'s, fifteen minutes later, with the cursors it
/// proposed and the same state directory.
pub(super) fn next_request(
    earlier: &FetchRequest,
    outcome: &SourceOutcome,
) -> Result<FetchRequest> {
    let mut next = earlier.clone();
    next.window_end = at(LATER_END)?;
    for (stream, result) in &outcome.streams {
        if let Some(cursor) = &result.cursor {
            next.cursors.insert(stream.clone(), cursor.clone());
        }
    }
    Ok(next)
}

pub(super) fn run(
    api: &FakeGithub,
    request: &FetchRequest,
    config: &GithubConfig,
) -> Result<SourceOutcome> {
    let installations = installations(config)?;
    let schemes = Schemes::new(["github".to_owned()]);
    let request_enum = Request::Fetch(request.clone());
    let mut session = Session::new(&request_enum, &schemes);
    let run = fetch_with_api(request, config, &installations, api, &mut |message| {
        session.accept(message).map(|_| ())
    });
    Ok(session.finish(run))
}

pub(super) fn config(installations: Vec<u64>, budget: u32, floor: u32) -> GithubConfig {
    GithubConfig {
        installations,
        request_budget: budget,
        rate_limit_floor: floor,
    }
}

pub(super) fn pointers(outcome: &SourceOutcome) -> BTreeSet<String> {
    outcome
        .records
        .iter()
        .map(|record| record.pointer.clone())
        .collect()
}

pub(super) fn issue(number: u64, updated: &str) -> Value {
    json!({
        "number": number,
        "title": format!("Issue {number}"),
        "body": "the issue body",
        "user": {"login": "sami", "id": 1},
        "updated_at": updated
    })
}

pub(super) fn comment(id: u64, updated: &str) -> Value {
    json!({
        "id": id,
        "body": "a comment",
        "user": {"login": "bob", "id": 2},
        "updated_at": updated
    })
}

pub(super) fn cursor_of(outcome: &SourceOutcome, stream: &str) -> Result<Option<String>> {
    let result = outcome
        .streams
        .get(stream)
        .context("the repository has a stream result")?;
    assert_eq!(result.status, StreamStatus::Ok, "{:?}", result.error);
    Ok(result.cursor.clone())
}

/// A repository whose issues listing is empty under both window starts the tests use, and
/// whose default branch has no commit.
pub(super) fn quiet(fake: &FakeGithub, subject: &Repo) {
    let base = api_base();
    for since in [SINCE, WINDOW_END] {
        fake.page(&issues_url(&base, subject, since), page(Vec::new()));
    }
    fake.page(&head_url(&base, subject), page(Vec::new()));
}

pub(super) fn commit(sha: &str, date: &str) -> Value {
    json!({
        "sha": sha,
        "commit": {
            "message": format!("commit {sha}"),
            "author": {"name": "Sami", "date": date},
            "committer": {"date": date}
        },
        "author": {"login": "sami", "id": 1}
    })
}

/// `subject`'s default branch head is `head`.
pub(super) fn branch_at(fake: &FakeGithub, subject: &Repo, head: &Value) {
    fake.page(&head_url(&api_base(), subject), page(vec![head.clone()]));
}
