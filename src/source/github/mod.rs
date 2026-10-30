//! Built-in GitHub source: the deployment's GitHub App reads every repository it is installed on.
//!
//! The app mints a read-only installation token for each installation named in
//! `CTX_GITHUB_INSTALLATIONS`, and no other installation is touched, so the one organization
//! the spec does not read is never called. One repository is one stream (`github:<owner>/<repo>`)
//! and its cursor is an `updated_at` high-water mark.
//!
//! The run guards the App's rate limit, which other clients of the App share, three ways. A
//! GraphQL pre-pass asks, for 50 repositories a query, when each last changed, so a repository
//! with nothing new since its stream's stored time costs no REST request. A per-run request
//! budget and a floor under GitHub's reported remaining rate limit bound the REST reads: when
//! either runs low, the run stops the stream with a warning. It reads each issue's follow-ups
//! before the next issue's,
//! so a stopped stream has finished a prefix of its feed, and its cursor names where the feed
//! resumes and the time it is stored through (`<resume>@<stored>`); a repository too large for
//! one run's budget therefore advances across runs, and the next run starts at the repository
//! the last one stopped at.

mod collect;
mod cursor_state;
mod records;

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::github::{GithubClient, Page, Repo};
use crate::credentials;

use super::{FetchRequest, Message, Op, Request, ResolveRequest, Source, PROTOCOL_VERSION};
use collect::{collect_repo, Governor, Stop};
use cursor_state::{Position, State};
use records::stream_id;

/// The installations to read, one id per comma, when the configuration names none.
const INSTALLATIONS_ENV: &str = "CTX_GITHUB_INSTALLATIONS";
/// How many paid requests one run may spend before it holds every unfinished cursor.
const DEFAULT_REQUEST_BUDGET: u32 = 600;
/// A run stops reading an installation once GitHub reports its `core` rate limit under this.
const DEFAULT_RATE_FLOOR: u32 = 2000;
/// How many repositories one pre-pass query asks about.
const ACTIVITY_BATCH: usize = 50;
/// What the pre-pass asks about each repository: the newest change to an issue, to a pull
/// request, to any branch, and the default branch's head.
const ACTIVITY_FIELDS: &str = "pushedAt \
     defaultBranchRef { target { oid ... on Commit { committedDate } } } \
     issues(first: 1, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { updatedAt } } \
     pullRequests(first: 1, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { updatedAt } }";

/// Built-in source for GitHub issues, pull requests, reviews, comments, and commits.
pub struct GithubSource;

impl GithubSource {
    /// Creates a GitHub source that acts as the deployment's GitHub App.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for GithubSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for GithubSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                let installations = installations(&config)?;
                let client = GithubClient::load(&credentials::Config::new())?;
                fetch_with_api(request, &config, &installations, &client, sink)
            }
            Request::Resolve(request) => emit_resolve_hello(request, sink),
        }
    }
}

/// What the source needs from GitHub, so a test can supply its own.
trait GithubApi {
    /// Mints a read-only token for `installation` and lists the repositories it covers, or
    /// `None` when GitHub refused that to protect a rate limit.
    fn repositories(&self, installation: u64) -> Result<Option<Vec<Repo>>>;
    /// `GET url` with `installation`'s token.
    fn get(&self, installation: u64, url: &str) -> Result<Page>;
    /// Posts one GraphQL request with `installation`'s token and returns GitHub's answer.
    fn graphql(&self, installation: u64, request: &Value) -> Result<Value>;
}

impl GithubApi for GithubClient {
    fn repositories(&self, installation: u64) -> Result<Option<Vec<Repo>>> {
        Self::repositories(self, installation)
    }

    fn get(&self, installation: u64, url: &str) -> Result<Page> {
        self.get_page(installation, url)
    }

    fn graphql(&self, installation: u64, request: &Value) -> Result<Value> {
        Self::graphql(self, installation, request)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubConfig {
    /// Installation ids to read; when empty the environment names them.
    #[serde(default)]
    installations: Vec<u64>,
    /// Paid requests one run may spend.
    #[serde(default = "default_budget")]
    request_budget: u32,
    /// A run stops reading an installation once GitHub reports its `core` rate limit under
    /// this.
    #[serde(default = "default_floor")]
    rate_limit_floor: u32,
}

const fn default_budget() -> u32 {
    DEFAULT_REQUEST_BUDGET
}

const fn default_floor() -> u32 {
    DEFAULT_RATE_FLOOR
}

fn parse_config(value: &Value) -> Result<GithubConfig> {
    if value.is_null() {
        return Ok(GithubConfig {
            installations: Vec::new(),
            request_budget: DEFAULT_REQUEST_BUDGET,
            rate_limit_floor: DEFAULT_RATE_FLOOR,
        });
    }
    serde_json::from_value(value.clone()).context("parsing GitHub source configuration")
}

/// The installations to read: the configuration's list, or the environment's.
fn installations(config: &GithubConfig) -> Result<Vec<u64>> {
    if !config.installations.is_empty() {
        return Ok(config.installations.clone());
    }
    let raw = std::env::var(INSTALLATIONS_ENV).map_err(|_| {
        anyhow!("set {INSTALLATIONS_ENV} to the GitHub App installation ids to read")
    })?;
    let mut ids = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id = trimmed
            .parse::<u64>()
            .with_context(|| format!("parsing installation id {trimmed}"))?;
        ids.push(id);
    }
    if ids.is_empty() {
        bail!("{INSTALLATIONS_ENV} names no installations");
    }
    Ok(ids)
}

fn emit_fetch_hello(
    request: &FetchRequest,
    streams: &[String],
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: true,
        streams: streams.to_vec(),
    })
}

fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: false,
        streams: Vec::new(),
    })
}

fn fetch_with_api<C: GithubApi + ?Sized>(
    request: &FetchRequest,
    config: &GithubConfig,
    installations: &[u64],
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut repos = Vec::new();
    let mut throttled = Vec::new();
    for &installation in installations {
        let listed = api
            .repositories(installation)
            .with_context(|| format!("listing repositories for installation {installation}"))?;
        match listed {
            Some(listed) => {
                for repo in listed {
                    repos.push((installation, repo));
                }
            }
            // GitHub refused to say what this installation covers, so none of its streams are
            // known this run. Their stored cursors stand untouched and the next fetch asks again.
            None => throttled.push(installation),
        }
    }
    let streams: Vec<String> = repos.iter().map(|(_, repo)| stream_id(repo)).collect();
    emit_fetch_hello(request, &streams, sink)?;
    for &installation in &throttled {
        sink(Message::Warning {
            stream: None,
            message: format!(
                "installation {installation}: GitHub refused its repository listing to protect its rate limit; its repositories are read by the next fetch"
            ),
        })?;
    }

    let (mut state, warnings) = State::load(&request.state_dir)?;
    for message in warnings {
        sink(Message::Warning {
            stream: None,
            message,
        })?;
    }
    let unchanged = unchanged_repositories(&repos, request, &state, api, sink)?;
    let mut governor = Governor::new(config.request_budget, config.rate_limit_floor);
    // The run starts at the repository the last run's budget stopped at and goes round, so the
    // budget reaches every repository in turn rather than the same ones first on every run. A
    // repository its installation's rate floor or GitHub's throttle held does not move that
    // start: its installation's next answer decides when it is read, and a start pinned to it
    // would read the other installations from their first repository on every run. A point in
    // an installation that did not answer this run is kept for the run that reads it.
    let previous = state.first.take();
    let start = previous
        .as_deref()
        .and_then(|first| repos.iter().position(|(_, repo)| stream_id(repo) == first))
        .unwrap_or(0);
    let unplanned = previous.filter(|first| !streams.contains(first));
    let mut first_stopped = None;
    for (installation, repo) in repos.iter().cycle().skip(start).take(repos.len()) {
        let stream = stream_id(repo);
        if let Some(head) = unchanged.get(&stream) {
            // Nothing the REST listings name changed since the stream's stored time, by the
            // pre-pass, so the window carries everything up to its end and costs no request.
            // The head the stream holds is the branch's head still, and the cursor keeps it.
            let cursor = Position {
                resume: request.window_end,
                after: None,
                stored: request.window_end,
                head: head.clone(),
            }
            .cursor();
            state.settle(&stream, &cursor);
            super::emit_stream(&stream, Ok((Vec::new(), Vec::new(), cursor)), sink)?;
            continue;
        }
        let collected = collect_repo(*installation, repo, request, &mut governor, api);
        match &collected {
            Ok(collected) if collected.stop.is_none() && collected.held == 0 => {
                state.settle(&stream, &collected.cursor);
            }
            Ok(collected) => {
                if collected.stop.is_some_and(Stop::spent_the_budget) && first_stopped.is_none() {
                    first_stopped = Some(stream.clone());
                }
                state.unsettle(&stream);
            }
            Err(_) => state.unsettle(&stream),
        }
        super::emit_stream(
            &stream,
            collected.map(|c| (c.warnings, c.records, c.cursor)),
            sink,
        )?;
    }
    state.first = first_stopped.or(unplanned);
    if throttled.is_empty() {
        // Every installation answered, so a stream that is missing from this run is a
        // repository the app no longer reads.
        state.retain(&streams);
    }
    state.save(&request.state_dir)
}

/// The repositories in which a REST read would list nothing, by the GraphQL pre-pass.
///
/// A REST read lists the issues and pull requests changed since the stream's stored time, the
/// follow-ups of those alone, and the default branch's commits the stream does not hold. One
/// query asks [`ACTIVITY_BATCH`] repositories for the newest `updatedAt` of an issue and of a
/// pull request, `pushedAt`, and the default branch head's `oid` and `committedDate`. A
/// repository has nothing to list when all those times are older than its stored time and its
/// head is the one its cursor names (or the cursor names none). GitHub charges 1 point a query
/// (`rateLimit { cost }`), whatever the batch holds, in the runs measured.
///
/// That holds because GitHub moves an issue's or pull request's `updatedAt` for every follow-up
/// a REST read lists. Probed with the app's token on one busy repository (GraphQL
/// `pullRequests(first: 25, orderBy: UPDATED_AT DESC)` with each one's newest comment, review,
/// review-thread comment, commit and timeline item): `updatedAt` equalled, to the second, the
/// newest issue comment, the newest review, and a review with its review comment (a second
/// apart), and came 5 s after a pushed commit's time. The REST feed's `updated_at` equalled
/// GraphQL's `updatedAt` for all 25 pull requests compared. `pushedAt` alone does not cover the
/// default branch: on a few repositories it was older than the head's commit time, so the head
/// is asked too, and its `oid` catches a push of commits dated before the stored time.
///
/// Only a stream that is settled — whose cursor is the one a run proposed after reading it to
/// the end with nothing held — is asked about; any other is read through REST. A batch that
/// GitHub answers with an error, or without every repository, is read through REST. Each
/// unchanged stream comes back with the head its cursor names, which its next cursor keeps.
fn unchanged_repositories<C: GithubApi + ?Sized>(
    repos: &[(u64, Repo)],
    request: &FetchRequest,
    state: &State,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<BTreeMap<String, Option<String>>> {
    let mut candidates: BTreeMap<u64, Vec<(&Repo, Position)>> = BTreeMap::new();
    for (installation, repo) in repos {
        let stream = stream_id(repo);
        let Some(cursor) = request.cursors.get(&stream) else {
            continue;
        };
        if state.settled.get(&stream) != Some(cursor) {
            continue;
        }
        if let Ok(position) =
            Position::read(Some(cursor), request.window_end, request.backfill_hours)
        {
            candidates
                .entry(*installation)
                .or_default()
                .push((repo, position));
        }
    }
    let mut unchanged = BTreeMap::new();
    for (installation, listed) in candidates {
        for batch in listed.chunks(ACTIVITY_BATCH) {
            let asked: Vec<&Repo> = batch.iter().map(|(repo, _)| *repo).collect();
            match newest_changes(api, installation, &asked) {
                Ok(activity) => {
                    for ((repo, position), activity) in batch.iter().zip(activity) {
                        let quiet = activity.newest.is_none_or(|time| time < position.stored);
                        let same_head = position
                            .head
                            .as_ref()
                            .is_none_or(|head| activity.head.as_ref() == Some(head));
                        if quiet && same_head {
                            unchanged.insert(stream_id(repo), position.head.clone());
                        }
                    }
                }
                Err(error) => sink(Message::Warning {
                    stream: None,
                    message: format!(
                        "installation {installation}: GitHub's GraphQL API did not say when {} repositories last changed, so each is read through its REST listings: {error:#}",
                        batch.len()
                    ),
                })?,
            }
        }
    }
    Ok(unchanged)
}

/// What the pre-pass learned of one repository: its newest change, and its default branch head.
struct Activity {
    newest: Option<DateTime<Utc>>,
    head: Option<String>,
}

/// Asks GitHub when each of `repos` last changed, in their order.
fn newest_changes<C: GithubApi + ?Sized>(
    api: &C,
    installation: u64,
    repos: &[&Repo],
) -> Result<Vec<Activity>> {
    let mut parameters = Vec::with_capacity(repos.len());
    let mut fields = Vec::with_capacity(repos.len());
    let mut variables = serde_json::Map::new();
    for (index, repo) in repos.iter().enumerate() {
        parameters.push(format!("$o{index}: String!, $n{index}: String!"));
        fields.push(format!(
            "r{index}: repository(owner: $o{index}, name: $n{index}) {{ {ACTIVITY_FIELDS} }}"
        ));
        variables.insert(format!("o{index}"), Value::String(repo.owner.clone()));
        variables.insert(format!("n{index}"), Value::String(repo.name.clone()));
    }
    let query = format!(
        "query({}) {{ {} }}",
        parameters.join(", "),
        fields.join(" ")
    );
    let answer = api.graphql(
        installation,
        &json!({ "query": query, "variables": variables }),
    )?;
    if let Some(errors) = answer
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let text: String = Value::Array(errors.clone())
            .to_string()
            .chars()
            .take(300)
            .collect();
        bail!("it answered with errors: {text}");
    }
    let data = answer
        .get("data")
        .and_then(Value::as_object)
        .context("its answer carries no data")?;
    repos
        .iter()
        .enumerate()
        .map(|(index, repo)| {
            let node = data
                .get(&format!("r{index}"))
                .filter(|node| node.is_object())
                .with_context(|| {
                    format!("its answer says nothing of {}/{}", repo.owner, repo.name)
                })?;
            newest_change(node)
                .with_context(|| format!("reading its answer for {}/{}", repo.owner, repo.name))
        })
        .collect()
}

/// One repository's answer: the newest of its times, or `None` when it has no issue, pull
/// request or commit, and its default branch head. A field the query asked for and the answer
/// lacks is an error.
fn newest_change(node: &Value) -> Result<Activity> {
    let mut times = vec![nullable_time(node, "pushedAt")?];
    let mut head = None;
    let branch = node
        .get("defaultBranchRef")
        .context("no defaultBranchRef")?;
    if !branch.is_null() {
        let target = branch.get("target").context("no default branch target")?;
        times.push(Some(required_time(target, "committedDate")?));
        let oid = target
            .get("oid")
            .and_then(Value::as_str)
            .context("no default branch oid")?;
        head = Some(oid.to_owned());
    }
    for connection in ["issues", "pullRequests"] {
        let nodes = node
            .get(connection)
            .and_then(|found| found.get("nodes"))
            .and_then(Value::as_array)
            .with_context(|| format!("no {connection} nodes"))?;
        for item in nodes {
            times.push(Some(required_time(item, "updatedAt")?));
        }
    }
    Ok(Activity {
        newest: times.into_iter().flatten().max(),
        head,
    })
}

/// A time field that GitHub answers with `null` when there is none.
fn nullable_time(node: &Value, field: &str) -> Result<Option<DateTime<Utc>>> {
    match node.get(field) {
        Some(Value::Null) => Ok(None),
        Some(_) => required_time(node, field).map(Some),
        None => bail!("no {field}"),
    }
}

fn required_time(node: &Value, field: &str) -> Result<DateTime<Utc>> {
    let raw = node
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("no {field}"))?;
    Ok(DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("parsing {field} {raw}"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests;
