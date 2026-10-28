//! GitHub API item conversion and URL construction.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::client;
use crate::api::github::Repo;
use crate::record::Record;

#[cfg(doc)]
use super::collect::Paging;

/// How many items a listing asks for per page.
const PER_PAGE: &str = "100";

/// What one API item became: a record, a held item beyond the window, or nothing usable.
pub(super) enum Built {
    Record(Box<Record>),
    Held,
    Skip,
}

/// The fields one record carries that differ by item kind.
struct Parts {
    pointer: String,
    ts: DateTime<Utc>,
    label: Option<String>,
    author: Option<String>,
    author_id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    text: String,
}

fn record(parts: Parts, repo: &Repo) -> Record {
    Record {
        pointer: parts.pointer,
        stream: stream_id(repo),
        ts: parts.ts,
        label: parts.label,
        author: parts.author,
        author_id: parts.author_id,
        thread: None,
        title: parts.title,
        url: parts.url,
        text: parts.text,
        aliases: Vec::new(),
        context: false,
        raw: Value::Null,
    }
}

pub(super) fn issue_built(item: &Value, repo: &Repo, window_end: DateTime<Utc>) -> Built {
    let Some(number) = item.get("number").and_then(Value::as_u64) else {
        return Built::Skip;
    };
    let Some(ts) = item_time(item, "updated_at") else {
        return Built::Skip;
    };
    if ts > window_end {
        return Built::Held;
    }
    let label = if item.get("pull_request").is_some() {
        "pull_request"
    } else {
        "issue"
    };
    Built::Record(Box::new(record(
        Parts {
            pointer: format!("github:{}/{}#{number}", repo.owner, repo.name),
            ts,
            label: Some(label.to_owned()),
            author: author_login(item),
            author_id: author_id(item),
            title: str_field(item, "title"),
            url: str_field(item, "html_url"),
            text: body_text(item),
        },
        repo,
    )))
}

pub(super) fn comment_built(
    item: &Value,
    repo: &Repo,
    number: u64,
    window_end: DateTime<Utc>,
) -> Built {
    let Some(id) = item.get("id").and_then(Value::as_u64) else {
        return Built::Skip;
    };
    let Some(ts) = comment_time(item) else {
        return Built::Skip;
    };
    if ts > window_end {
        return Built::Held;
    }
    Built::Record(Box::new(record(
        Parts {
            pointer: format!("github:{}/{}#{number}/comment/{id}", repo.owner, repo.name),
            ts,
            label: Some("comment".to_owned()),
            author: author_login(item),
            author_id: author_id(item),
            title: None,
            url: str_field(item, "html_url"),
            text: body_text(item),
        },
        repo,
    )))
}

pub(super) fn review_built(
    item: &Value,
    repo: &Repo,
    number: u64,
    window_end: DateTime<Utc>,
) -> Built {
    let Some(id) = item.get("id").and_then(Value::as_u64) else {
        return Built::Skip;
    };
    let Some(ts) = item_time(item, "submitted_at") else {
        return Built::Skip;
    };
    if ts > window_end {
        return Built::Held;
    }
    Built::Record(Box::new(record(
        Parts {
            pointer: format!("github:{}/{}#{number}/review/{id}", repo.owner, repo.name),
            ts,
            label: str_field(item, "state"),
            author: author_login(item),
            author_id: author_id(item),
            title: None,
            url: str_field(item, "html_url"),
            text: body_text(item),
        },
        repo,
    )))
}

pub(super) fn review_comment_built(
    item: &Value,
    repo: &Repo,
    number: u64,
    window_end: DateTime<Utc>,
) -> Built {
    let Some(id) = item.get("id").and_then(Value::as_u64) else {
        return Built::Skip;
    };
    let Some(ts) = comment_time(item) else {
        return Built::Skip;
    };
    if ts > window_end {
        return Built::Held;
    }
    Built::Record(Box::new(record(
        Parts {
            pointer: format!(
                "github:{}/{}#{number}/review-comment/{id}",
                repo.owner, repo.name
            ),
            ts,
            label: Some("review-comment".to_owned()),
            author: author_login(item),
            author_id: author_id(item),
            title: None,
            url: str_field(item, "html_url"),
            text: body_text(item),
        },
        repo,
    )))
}

pub(super) fn commit_built(item: &Value, repo: &Repo, window_end: DateTime<Utc>) -> Built {
    let Some(sha) = item.get("sha").and_then(Value::as_str) else {
        return Built::Skip;
    };
    let Some(ts) = commit_time(item) else {
        return Built::Skip;
    };
    if ts > window_end {
        return Built::Held;
    }
    let message = item
        .get("commit")
        .and_then(|commit| commit.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Built::Record(Box::new(record(
        Parts {
            pointer: format!("github:{}/{}@{sha}", repo.owner, repo.name),
            ts,
            label: Some("commit".to_owned()),
            author: commit_author(item),
            author_id: author_id(item),
            title: None,
            url: str_field(item, "html_url"),
            text: message,
        },
        repo,
    )))
}

pub(super) fn stream_id(repo: &Repo) -> String {
    format!("github:{}/{}", repo.owner, repo.name)
}

pub(super) fn item_time(item: &Value, field: &str) -> Option<DateTime<Utc>> {
    let raw = item.get(field).and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// A comment's time: when it was last edited, or when it was written.
pub(super) fn comment_time(item: &Value) -> Option<DateTime<Utc>> {
    item_time(item, "updated_at").or_else(|| item_time(item, "created_at"))
}

/// A commit's time: when it was committed, or when it was authored.
pub(super) fn commit_time(item: &Value) -> Option<DateTime<Utc>> {
    let commit = item.get("commit")?;
    let raw = commit
        .get("committer")
        .and_then(|committer| committer.get("date"))
        .and_then(Value::as_str)
        .or_else(|| {
            commit
                .get("author")
                .and_then(|author| author.get("date"))
                .and_then(Value::as_str)
        })?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn author_login(item: &Value) -> Option<String> {
    item.get("user")
        .and_then(|user| user.get("login"))
        .and_then(Value::as_str)
        .filter(|login| !login.is_empty())
        .map(str::to_owned)
}

/// A commit's author: the GitHub account, or the name git recorded.
fn commit_author(item: &Value) -> Option<String> {
    author_login(item).or_else(|| {
        item.get("commit")
            .and_then(|commit| commit.get("author"))
            .and_then(|author| author.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    })
}

fn author_id(item: &Value) -> Option<String> {
    let user = item.get("user")?;
    match user.get("id") {
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

fn str_field(item: &Value, field: &str) -> Option<String> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn body_text(item: &Value) -> String {
    item.get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The issues feed: issues and pull requests changed since `since`, oldest change first.
///
/// The order changes while a run reads it, yet following GitHub's next link loses nothing,
/// because GitHub pages this listing by key: the next link carries `after=<cursor>`, the base64
/// of `cursor:v2:` and a `MessagePack` pair of the last item's `updated_at` in milliseconds and
/// its id, and the next page starts after that item whatever moved meanwhile, with items
/// changed in the same instant ordered by id. Probed on 2026-10-28:
/// `GET /repos/cli/cli/issues?state=all&sort=updated&direction=asc&since=2026-10-20T00:00:00Z&per_page=5`
/// answered `Link: <…&after=Y3Vyc29yOnYyOpLPAAABoVi_FEDPAAAAAWFqnzw%3D&page=2>; rel="next"`,
/// which decodes to `[1792490280000, 5929344828]`: 2026-10-20T09:58:00Z and the id of #14754,
/// the page's last item. A cursor built the same way for the first item of a `per_page=3`
/// page answered the second to fourth items, and the same cursor beside `page=2` answered the
/// same, so `after` decides the page and `page` does not. A since-and-skip-seen scheme would be
/// worse here: `since` has whole seconds and no id to break ties.
pub(super) fn issues_url(base: &str, repo: &Repo, since: &str) -> String {
    format!(
        "{base}/repos/{}/{}/issues?state=all&sort=updated&direction=asc&since={}&per_page={PER_PAGE}",
        repo.owner,
        repo.name,
        client::percent_encode(since)
    )
}

/// All of one issue's comments.
///
/// GitHub lists them in ascending id order whatever `sort` and `direction` say, and pages them
/// by page number, so they are read last page first ([`Paging::Backward`]). Probed on
/// 2026-10-28: `GET /repos/cli/cli/issues/14090/comments?per_page=3` with
/// `sort=updated&direction=asc`, with `sort=updated&direction=desc`, and with no sort each
/// answered ids 5205732659, 5205778319, 5228377485, with
/// `Link: <…&page=2>; rel="next", <…&page=54>; rel="last"`. The listing names no `since`:
/// GitHub's `since` keeps the comments last updated after it, so an edit of an older comment
/// brings it into the listing at its id's place and moves every later comment one place on,
/// onto a page that may be read already. Listed whole, the listing changes only by deletion,
/// which the backward read handles, and `follow_issue` drops the comments changed before the
/// time the stream is stored through. That costs one request per 100 comments of a thread that
/// changed, rather than per 100 of its comments that changed.
pub(super) fn issue_comments_url(base: &str, repo: &Repo, number: u64) -> String {
    format!(
        "{base}/repos/{}/{}/issues/{number}/comments?per_page={PER_PAGE}",
        repo.owner, repo.name
    )
}

/// One pull request's reviews, oldest first, paged by page number.
///
/// A submitted review is never deleted — only a pending one can be, and only its author sees
/// it — so the listing only grows at its end and following the next link skips nothing.
pub(super) fn pull_reviews_url(base: &str, repo: &Repo, number: u64) -> String {
    format!(
        "{base}/repos/{}/{}/pulls/{number}/reviews?per_page={PER_PAGE}",
        repo.owner, repo.name
    )
}

/// All of one pull request's review comments, in ascending id order.
///
/// GitHub honours `sort` here and pages by page number. Under `sort=updated` an edit would move
/// a comment to the end and shift the next page's first comment onto a page already read;
/// under `sort=created`, which is id order, an edit of a listed comment moves nothing, and the
/// listing is read last page first ([`Paging::Backward`]) so a deletion hides nothing either.
/// It names no `since`, for the reason `issue_comments_url` gives. Probed on 2026-10-28: on
/// `GET /repos/cli/cli/pulls/14673/comments`, `sort=updated&direction=desc` answered newest
/// first (ids 4295608262, 4295607645, …) where `sort=created&direction=asc` and no sort
/// answered 4293684492, 4293684591, …; the `Link` header names `page=2` and `rel="last"`.
pub(super) fn pull_review_comments_url(base: &str, repo: &Repo, number: u64) -> String {
    format!(
        "{base}/repos/{}/{}/pulls/{number}/comments?sort=created&direction=asc&per_page={PER_PAGE}",
        repo.owner, repo.name
    )
}

/// The history of commit `head` (a sha, never a branch name, so the listing cannot change while
/// it is read) since `since`, newest first.
pub(super) fn commits_url(base: &str, repo: &Repo, head: &str, since: &str) -> String {
    format!(
        "{base}/repos/{}/{}/commits?sha={}&since={}&per_page={PER_PAGE}",
        repo.owner,
        repo.name,
        client::percent_encode(head),
        client::percent_encode(since)
    )
}

/// The default branch's head: the first commit of its listing.
pub(super) fn head_url(base: &str, repo: &Repo) -> String {
    format!(
        "{base}/repos/{}/{}/commits?sha={}&per_page=1",
        repo.owner,
        repo.name,
        client::percent_encode(&repo.default_branch)
    )
}

/// The commits `head` has that `stored`'s history lacks, oldest first.
pub(super) fn compare_url(base: &str, repo: &Repo, stored: &str, head: &str) -> String {
    format!(
        "{base}/repos/{}/{}/compare/{stored}...{head}?per_page={PER_PAGE}",
        repo.owner, repo.name
    )
}
