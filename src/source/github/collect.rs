//! GitHub repository collection and request governance.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::github::{api_base, Page, PageKind, Repo};
use crate::exclude::Exclusion;
use crate::record::Record;

use super::cursor_state::{is_sha, rfc3339, FeedKey, Position, Reading};
use super::records::{
    comment_built, comment_time, commit_built, commit_time, commits_url, compare_url, head_url,
    issue_built, issue_comments_url, issues_url, item_time, pull_review_comments_url,
    pull_reviews_url, review_built, review_comment_built, stream_id, Built, PAGE_SIZE,
};
use super::{FetchRequest, GithubApi};

/// One stream's records, warnings, and the cursor that follows them.
pub(super) struct Collected {
    pub(super) records: Vec<Record>,
    pub(super) warnings: Vec<String>,
    pub(super) held: usize,
    pub(super) cursor: String,
    /// Why the stream stopped before it read everything it planned, when it did.
    pub(super) stop: Option<Stop>,
}

/// Reads one repository's stream. A part of it the deployment excludes whole — every issue
/// and pull request with their follow-ups (`github:<owner>/<repo>#…`), or every commit
/// (`github:<owner>/<repo>@…`) — is not read at all: the fetch would drop all of it, and a
/// repository whose agents commit thousands of times an hour would spend the run's budget on
/// it.
pub(super) fn collect_repo<C: GithubApi + ?Sized>(
    installation: u64,
    repo: &Repo,
    request: &FetchRequest,
    exclusion: &Exclusion,
    governor: &mut Governor,
    api: &C,
) -> Result<Collected> {
    let stream = stream_id(repo);
    let window_end = request.window_end;
    let input_cursor = request.cursors.get(&stream).map(String::as_str);
    let position = Position::read(input_cursor, window_end, request.backfill_hours)?;
    let base = api_base();
    let mut collected = Collected {
        records: Vec::new(),
        warnings: Vec::new(),
        held: 0,
        cursor: rfc3339(window_end),
        stop: None,
    };
    let mut reader = Reader {
        api,
        installation,
        governor,
    };

    // Phase one: the issues feed carries both issues and pull requests, the oldest change first.
    // It stops early when the budget is no more than its open issues' own follow-ups will cost,
    // so a feed longer than the budget still leaves the run issues it can finish. GitHub pages
    // it by key (see `issues_url`), so following its next link skips nothing that changes
    // meanwhile.
    let reserve = |items: &[Value]| feed_reserve(items, &position);
    let (listed, feed_stop) = if exclusion.covers(&format!("{stream}#")) {
        (Vec::new(), None)
    } else {
        reader.drain(
            issues_url(&base, repo, &rfc3339(position.resume)),
            &reserve,
            Paging::Forward,
        )?
    };
    let issues = finishable(listed, &position, feed_stop.is_some());
    for item in &issues {
        push_built(&mut collected, issue_built(item, repo, window_end));
    }

    // Phase two: each issue's comments, reviews and review comments, one issue after another
    // in the order `finishable` gives, so a run that stops has finished a prefix of it. They
    // are read from where the stream is stored through: an issue the last run did not finish
    // can have comments anywhere after it.
    let mut finished = 0_usize;
    let mut stop = feed_stop;
    if matches!(feed_stop, None | Some(Stop::Truncated)) {
        for item in &issues {
            let halt = follow_issue(
                item,
                repo,
                position.stored,
                window_end,
                &mut reader,
                &mut collected,
            )?;
            if halt.is_some() {
                stop = halt;
                break;
            }
            finished += 1;
        }
    }
    if let Some(stop) = stop {
        // The feed resumes after the last issue the run finished. An issue changed after the
        // window end is not in this window, so the position never passes one.
        let resume = issues
            .iter()
            .take(finished)
            .filter_map(feed_key)
            .rfind(|(time, _)| *time <= window_end)
            .map(|(time, number)| (time, Some(number)));
        return Ok(stopped(
            collected,
            &position,
            resume,
            position.reading.clone(),
            input_cursor,
            stop,
            &stream,
        ));
    }

    // Phase three: the default branch's commits, once every issue is stored.
    let held_before_commits = collected.held;
    let commits = if exclusion.covers(&format!("{stream}@")) {
        Commits::default()
    } else {
        read_commits(&base, repo, &position, window_end, &mut reader)?
    };
    for item in &commits.items {
        push_built(&mut collected, commit_built(item, repo, window_end));
    }
    Ok(after_commits(
        collected,
        &position,
        commits,
        held_before_commits,
        input_cursor,
        &stream,
        window_end,
    ))
}

/// Ends a stream once its commits are read, or the run stopped in them.
fn after_commits(
    mut collected: Collected,
    position: &Position,
    commits: Commits,
    held_before_commits: usize,
    input_cursor: Option<&str>,
    stream: &str,
    window_end: DateTime<Utc>,
) -> Collected {
    collected.warnings.extend(commits.warning);
    if let Some(stop) = commits.stop {
        // Every issue changed by the window end is stored, so the feed resumes there. The
        // commits go on from the page `reading` names, or are read again from the head the
        // stream holds when the run read no page of them.
        let resume = Some((window_end, None));
        return stopped(
            collected,
            position,
            resume,
            commits.reading,
            input_cursor,
            stop,
            stream,
        );
    }
    // A commit dated after the window end is held, and its head is not taken as stored, so the
    // next run compares from the head before it and reads it again.
    let head = if collected.held > held_before_commits {
        position.head.clone()
    } else {
        commits.head
    };
    if collected.held > 0 {
        collected.warnings.push(format!(
            "{} item(s) in {stream} changed after the window ended; the next fetch carries them",
            collected.held
        ));
    }
    collected.cursor = Position {
        resume: window_end,
        after: None,
        stored: window_end,
        head,
        reading: None,
    }
    .cursor();
    collected
}

fn feed_key(item: &Value) -> Option<FeedKey> {
    let number = item.get("number").and_then(Value::as_u64)?;
    Some((item_time(item, "updated_at")?, number))
}

/// What the issues listed so far still cost in follow-ups, counting only those the run may
/// finish, and nothing while those all share one second.
///
/// The feed stops listing once the budget left is no more than this. While every open issue
/// listed changed in the same second, that second may go on in the next page, and the run
/// could finish none of it (see `finishable`), so the feed lists on until a later second or
/// the end of the listing appears.
fn feed_reserve(items: &[Value], position: &Position) -> u32 {
    let open: Vec<(FeedKey, &Value)> = items
        .iter()
        .filter_map(|item| feed_key(item).map(|key| (key, item)))
        .filter(|(key, _)| position.open(*key))
        .collect();
    let first = open.iter().map(|((time, _), _)| *time).min();
    let last = open.iter().map(|((time, _), _)| *time).max();
    if first == last {
        return 0;
    }
    follow_up_cost(open.into_iter().map(|(_, item)| item))
}

/// The listed issues this run may finish, in the order it finishes them.
///
/// It drops the issues an earlier run finished (up to the cursor's position) and, when the
/// listing stopped early, the issues of the last second it reached, which may go on in a page
/// not read. The rest is ordered by `updated_at` and then by number, the order a position names,
/// so a run that stops can say exactly which issues it finished. GitHub's own order within a
/// second is not documented (on 2026-10-28 it was by descending id), and nothing here relies on
/// it: every issue of a second the run finishes from is listed before the run sorts them.
fn finishable(listed: Vec<Value>, position: &Position, cut_short: bool) -> Vec<Value> {
    let mut open: Vec<(FeedKey, Value)> = listed
        .into_iter()
        .filter_map(|item| feed_key(&item).map(|key| (key, item)))
        .filter(|(key, _)| position.open(*key))
        .collect();
    if cut_short {
        if let Some(last) = open.iter().map(|((time, _), _)| *time).max() {
            open.retain(|((time, _), _)| *time < last);
        }
    }
    open.sort_by_key(|(key, _)| *key);
    open.into_iter().map(|(_, item)| item).collect()
}

/// Ends a stream that stopped. It keeps what it fetched, and its cursor names where the feed
/// resumes, with the time the stream is stored through, the head it holds and the commit
/// listing it is part way through, when that moves the stream on; otherwise it holds the
/// cursor the run was given.
fn stopped(
    mut collected: Collected,
    position: &Position,
    resume: Option<(DateTime<Utc>, Option<u64>)>,
    reading: Option<Reading>,
    input_cursor: Option<&str>,
    stop: Stop,
    stream: &str,
) -> Collected {
    let next = resume
        .map(|(time, after)| Position {
            resume: time,
            after,
            stored: position.stored,
            head: position.head.clone(),
            reading,
        })
        .filter(|next| {
            (next.resume, next.after) > (position.resume, position.after)
                || next.reading != position.reading
        });
    if let Some(next) = next {
        collected.cursor = next.cursor();
        collected
            .warnings
            .push(stop.warning(stream, Some(&next.describe())));
    } else {
        collected.cursor = input_cursor.map_or_else(|| rfc3339(position.stored), str::to_owned);
        collected.warnings.push(stop.warning(stream, None));
    }
    collected.stop = Some(stop);
    collected
}

/// What the commits phase read: the commits, the head they belong to, why it stopped, where
/// the next run goes on in a listing it stopped in, and what the stream's owner should know
/// about it.
#[derive(Default)]
struct Commits {
    items: Vec<Value>,
    head: Option<String>,
    stop: Option<Stop>,
    reading: Option<Reading>,
    warning: Option<String>,
}

/// Reads the default branch's commits the stream does not hold yet.
///
/// It reads the branch head first (`commits?sha=<branch>&per_page=1`, whose first item is the
/// head: probed 2026-10-28 on `cli/cli`, it matched `branches/trunk`), and every later request
/// names that commit rather than the branch. One commit's history cannot change, so a push or a
/// force push while the run reads moves nothing under it, and no page can skip a commit.
///
/// When the cursor names the head the last run stored, a different head is read as
/// `compare/<stored head>...<head>`: every commit the branch has now that the stored head's
/// history lacks, oldest first, whatever its date. That is how a force push is caught and
/// re-read: the stored head is then no longer an ancestor, GitHub answers `status: diverged`,
/// and the comparison lists the new history from the merge base, including rewritten commits
/// dated before the stored time, which a listing by time would never reach. It also catches
/// commits that arrive with an older date, such as a merged pull request's own commits. A
/// comparison is paged by page number (probed 2026-10-28: `compare/9be5608…93d24ce?per_page=2`
/// answered `rel="next"` and `rel="last"`, oldest commit first) over two fixed commits, so
/// following its next link skips nothing. GitHub may still list fewer commits than the
/// answer's `total_commits` (`PyGithub` documents a cap of 10,000 for a paged comparison), and
/// then the rest is read as the head's history since the stored time (see `short_comparison`).
///
/// With no stored head — a first read, or a cursor an earlier build wrote — or when GitHub
/// cannot compare (`404`: the two share no history, or the stored head is gone), it lists the
/// head's history since the stored time (`read_history`).
///
/// A listing longer than the run's budget goes on in the next run. The cursor names the
/// listing and how many pages of it the run read (`reading`), and the next run reads the same
/// listing over the same commits from the page after, rather than the new head's from its top:
/// listed from a moving head, the first pages would hold the same commits run after run, and a
/// backlog longer than one run's budget would never be read past them. When the listing ends,
/// its target is stored as the head, and the next run compares that with the branch's head.
fn read_commits<C: GithubApi + ?Sized>(
    base: &str,
    repo: &Repo,
    position: &Position,
    window_end: DateTime<Utc>,
    reader: &mut Reader<'_, C>,
) -> Result<Commits> {
    if let Some(reading) = &position.reading {
        let resumed = if reading.comparison {
            read_comparison(base, repo, position, reading, window_end, reader)?
        } else {
            read_history(base, repo, position, reading, window_end, reader)?
        };
        if let Some(commits) = resumed {
            return Ok(commits);
        }
    }
    let opening = match reader.page(&head_url(base, repo), 0, false)? {
        Ok(page) => page,
        Err(stop) => {
            return Ok(Commits {
                stop: Some(stop),
                ..Commits::default()
            })
        }
    };
    let Some(head) = opening
        .items
        .first()
        .and_then(|item| item.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        // An empty repository (`409`) or an empty branch has no commit to read.
        return Ok(Commits::default());
    };
    if !is_sha(&head) {
        bail!("GitHub named {head} as the head of {}", repo.default_branch);
    }
    if position.head.as_deref() == Some(head.as_str()) {
        return Ok(Commits {
            head: Some(head),
            ..Commits::default()
        });
    }
    if position.head.is_some() {
        let from = Reading {
            target: head.clone(),
            pages: 0,
            comparison: true,
        };
        if let Some(commits) = read_comparison(base, repo, position, &from, window_end, reader)? {
            return Ok(commits);
        }
    }
    let from = Reading {
        target: head,
        pages: 0,
        comparison: false,
    };
    read_history(base, repo, position, &from, window_end, reader)?.with_context(|| {
        format!(
            "GitHub has no history for {}, which it just named as the head of {}",
            from.target, repo.default_branch
        )
    })
}

/// How many of `pages`, from the first, hold no commit dated after the window end: such a
/// commit is held for a later window, so a stopped listing reads its page again.
fn clean_pages(pages: &[Vec<Value>], window_end: DateTime<Utc>) -> u32 {
    let clean = pages
        .iter()
        .take_while(|page| {
            !page
                .iter()
                .any(|item| commit_time(item).is_some_and(|time| time > window_end))
        })
        .count();
    u32::try_from(clean).unwrap_or(u32::MAX)
}

/// Reads `from.target`'s history since the stored time, newest first, from the page after
/// `from.pages`, or `None` when GitHub no longer has that commit (`404`).
///
/// A run the budget stops names the pages it read in `reading`, up to the first page that held
/// a commit dated after the window end. A run that reads to the end stores the target as the
/// head.
fn read_history<C: GithubApi + ?Sized>(
    base: &str,
    repo: &Repo,
    position: &Position,
    from: &Reading,
    window_end: DateTime<Utc>,
    reader: &mut Reader<'_, C>,
) -> Result<Option<Commits>> {
    let url = commits_url(base, repo, &from.target, &rfc3339(position.stored));
    let listing = reader.numbered(&url, from.pages + 1)?;
    if listing.missing {
        return Ok(None);
    }
    let reading = listing.stop.map(|_| Reading {
        target: from.target.clone(),
        pages: from.pages + clean_pages(&listing.pages, window_end),
        comparison: false,
    });
    Ok(Some(Commits {
        items: listing.pages.into_iter().flatten().collect(),
        head: Some(from.target.clone()),
        stop: listing.stop,
        reading,
        warning: None,
    }))
}

/// Reads the comparison of the stored head with `from.target`, oldest first, from the page
/// after `from.pages`, or `None` when there is no stored head or GitHub cannot compare the two
/// (`404`). A run the budget stops names the pages it read, as `read_history` does. A
/// comparison GitHub lists fewer commits of than its `total_commits` ends in
/// `short_comparison`; the pages earlier runs read were full ones.
fn read_comparison<C: GithubApi + ?Sized>(
    base: &str,
    repo: &Repo,
    position: &Position,
    from: &Reading,
    window_end: DateTime<Utc>,
    reader: &mut Reader<'_, C>,
) -> Result<Option<Commits>> {
    let Some(stored) = position.head.as_deref() else {
        return Ok(None);
    };
    let listing = reader.numbered(
        &compare_url(base, repo, stored, &from.target),
        from.pages + 1,
    )?;
    if listing.missing {
        return Ok(None);
    }
    let pages = from.pages + clean_pages(&listing.pages, window_end);
    let total = listing.total;
    let stop = listing.stop;
    let items: Vec<Value> = listing.pages.into_iter().flatten().collect();
    if stop.is_some() {
        return Ok(Some(Commits {
            items,
            head: Some(from.target.clone()),
            stop,
            reading: Some(Reading {
                target: from.target.clone(),
                pages,
                comparison: true,
            }),
            warning: None,
        }));
    }
    let earlier = u64::from(from.pages).saturating_mul(PAGE_SIZE);
    let listed = earlier.saturating_add(u64::try_from(items.len()).unwrap_or(u64::MAX));
    if let Some(total) = total.filter(|total| *total > listed) {
        let since = reader.numbered(
            &commits_url(base, repo, &from.target, &rfc3339(position.stored)),
            1,
        )?;
        return Ok(Some(short_comparison(
            Comparison {
                listed: items,
                earlier,
                total,
                head: from.target.clone(),
            },
            since,
            position,
            repo,
        )));
    }
    Ok(Some(Commits {
        items,
        head: Some(from.target.clone()),
        ..Commits::default()
    }))
}

/// A comparison that listed fewer commits than its `total_commits`: what this run listed, and
/// how many earlier runs listed.
struct Comparison {
    listed: Vec<Value>,
    earlier: u64,
    total: u64,
    head: String,
}

/// Ends a comparison GitHub cut short, with `since`, the head's history since the stored time.
///
/// The comparison's commits are the ones the head has that the stored head lacks. Each commit
/// of `since` dated after the stored time is one of them: the stored head's history holds no
/// commit dated after the stored time, because a run that read one held it and kept the head
/// before it. So when the commits the comparison listed and those of `since` add up to its
/// `total_commits`, every commit is read and the head is stored. Otherwise some commit the
/// comparison left out is dated at or before the stored time, where `since` does not reach,
/// and the stream keeps the head it was given, with a warning: the next run compares from it
/// again rather than from a head past commits never read. That holds until GitHub lists the
/// whole comparison; each run still stores every commit it reads. A `since` listing the budget
/// stops is read again by the next run, from the comparison: a resumed history listing could
/// end without the commits the comparison left out and store a head past them.
fn short_comparison(
    comparison: Comparison,
    since: Numbered,
    position: &Position,
    repo: &Repo,
) -> Commits {
    let Comparison {
        listed,
        earlier,
        total,
        head,
    } = comparison;
    let compared = earlier.saturating_add(u64::try_from(listed.len()).unwrap_or(u64::MAX));
    let mut seen: BTreeSet<String> = listed
        .iter()
        .filter_map(|item| item.get("sha").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut covered = compared;
    let mut items = listed;
    let stop = since.stop;
    for item in since.pages.into_iter().flatten() {
        let Some(sha) = item.get("sha").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(sha.to_owned()) {
            continue;
        }
        if commit_time(&item).is_some_and(|time| time > position.stored) {
            covered = covered.saturating_add(1);
        }
        items.push(item);
    }
    if stop.is_some() || covered >= total {
        return Commits {
            items,
            head: Some(head),
            stop,
            reading: None,
            warning: None,
        };
    }
    let stored_head = position.head.as_deref().unwrap_or("the stored head");
    Commits {
        items,
        head: position.head.clone(),
        stop: None,
        reading: None,
        warning: Some(format!(
            "{}: GitHub listed {compared} of the {total} commits between {stored_head} and {head}, and those left out are older than {}, so the stream keeps {stored_head} and the next fetch compares from it again",
            stream_id(repo),
            rfc3339(position.stored)
        )),
    }
}

/// What one stream reads GitHub with: the client and installation, and the run's governor.
struct Reader<'r, C: ?Sized> {
    api: &'r C,
    installation: u64,
    governor: &'r mut Governor,
}

impl<C: GithubApi + ?Sized> Reader<'_, C> {
    fn drain(
        &mut self,
        first: String,
        reserve: &dyn Fn(&[Value]) -> u32,
        paging: Paging,
    ) -> Result<(Vec<Value>, Option<Stop>)> {
        match paging {
            Paging::Forward => self.forward(first, reserve),
            Paging::Backward => self.backward(first),
        }
    }

    /// Follows `Link: rel="next"` from `first`. Before each page it stops when the budget left
    /// is no more than `reserve` says the items gathered so far still need.
    fn forward(
        &mut self,
        first: String,
        reserve: &dyn Fn(&[Value]) -> u32,
    ) -> Result<(Vec<Value>, Option<Stop>)> {
        let mut items = Vec::new();
        let mut url = Some(first);
        while let Some(current) = url {
            let page = match self.page(&current, reserve(&items), false)? {
                Ok(page) => page,
                Err(stop) => return Ok((items, Some(stop))),
            };
            items.extend(page.items);
            url = page.next;
        }
        Ok((items, None))
    }

    /// Reads a listing in ascending id order that GitHub pages by page number: page 1, which
    /// names the last page, then the last page down to page 2, then page 1 again.
    ///
    /// The listing must hold every item, not only those a filter such as `since` keeps: an edit
    /// can bring an item into a filtered listing at its id's place, which moves every later item
    /// one place on, so the last item of a page not yet read moves onto a page already read.
    /// Unfiltered, an edit moves no item, and a new item lands at the end. A deletion moves every
    /// later item one place back, so the first item of a page not yet read can move onto a page
    /// already read. Read from the last page down, an item can only move to a page that is read
    /// after it moved: an item on page `p` or earlier when page `p` is read is on a page before
    /// `p` unless it is read then, and so on down to page 1, which is read last. Every item that
    /// was there when page 1 was first read is therefore read. One created since was created
    /// after the run began, so after its window end, and the next run, which lists from the time
    /// it stores, reads it. Each item comes back once, the copy changed last, in id order.
    fn backward(&mut self, first: String) -> Result<(Vec<Value>, Option<Stop>)> {
        let opening = match self.page(&first, 0, false)? {
            Ok(page) => page,
            Err(stop) => return Ok((Vec::new(), Some(stop))),
        };
        let mut items = opening.items;
        if opening.next.is_none() {
            return Ok((items, None));
        }
        let last = opening.last.with_context(|| {
            format!("GitHub named a next page of {first} but no last page, so the listing cannot be read from its end")
        })?;
        let count = page_number(&last).with_context(|| {
            format!("GitHub's last page of {first} names no page number: {last}")
        })?;
        let mut urls: Vec<String> = (2..=count)
            .rev()
            .map(|number| with_page(&last, number))
            .collect();
        urls.push(first);
        for url in urls {
            match self.page(&url, 0, false)? {
                Ok(page) => items.extend(page.items),
                Err(stop) => return Ok((unique_by_id(items), Some(stop))),
            }
        }
        Ok((unique_by_id(items), None))
    }

    /// Reads a listing GitHub pages by number, `url`'s pages from `from` on, each page's items
    /// apart. It follows `Link: rel="next"` to the end, or until the run must stop; a `404`
    /// ends it as missing.
    fn numbered(&mut self, url: &str, from: u32) -> Result<Numbered> {
        let mut numbered = Numbered::default();
        let mut next = Some(at_page(url, from));
        while let Some(current) = next {
            match self.page(&current, 0, true)? {
                Err(stop) => {
                    numbered.stop = Some(stop);
                    break;
                }
                Ok(page) if page.kind == PageKind::Missing => {
                    numbered.missing = true;
                    break;
                }
                Ok(page) => {
                    numbered.total = numbered.total.or(page.total);
                    numbered.pages.push(page.items);
                    next = page.next;
                }
            }
        }
        Ok(numbered)
    }

    /// Reads one page, unless the run must stop first: the budget or the rate limit is spent,
    /// or the budget left is no more than `needed`. A page GitHub refused stops the listing. A
    /// `404` is an error unless `missing_ok`, when the caller gets the missing page.
    fn page(&mut self, url: &str, needed: u32, missing_ok: bool) -> Result<Result<Page, Stop>> {
        if let Some(stop) = self.governor.blocked(self.installation) {
            return Ok(Err(stop));
        }
        if needed > 0 && self.governor.budget <= needed {
            return Ok(Err(Stop::Truncated));
        }
        let page = self.api.get(self.installation, url)?;
        self.governor.observe(self.installation, &page);
        match page.kind {
            // GitHub refused the pace, so the rest of this listing waits for the next fetch.
            PageKind::Throttled => Ok(Err(Stop::Throttled)),
            PageKind::Missing if !missing_ok => bail!("reading {url} failed: 404 Not Found"),
            PageKind::Items | PageKind::Missing => Ok(Ok(page)),
        }
    }
}

/// How a listing's pages are followed, which decides whether an item can slip between two
/// page requests while GitHub changes the listing.
#[derive(Clone, Copy)]
pub(super) enum Paging {
    /// Follow `Link: rel="next"`: right for a listing that GitHub pages by key, or that only
    /// grows at its end or moves items to later pages.
    Forward,
    /// Read the last page first, down to page 1 ([`Reader::backward`]): right for a listing in
    /// ascending id order, paged by page number, from which an item can be deleted.
    Backward,
}

/// The `page` number a listing URL names.
fn page_number(url: &str) -> Option<u32> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|param| param.strip_prefix("page="))?
        .parse()
        .ok()
}

/// What [`Reader::numbered`] read: each page's items, in page order, and why it ended early.
#[derive(Default)]
struct Numbered {
    pages: Vec<Vec<Value>>,
    /// A comparison's `total_commits`, from the first page read.
    total: Option<u64>,
    stop: Option<Stop>,
    /// GitHub answered `404`: the commit the listing names is gone.
    missing: bool,
}

/// `url` asking for page `number`: its own `page` set to it, or one added. Page 1 is `url` as
/// it is, the way the first request of a listing names it.
fn at_page(url: &str, number: u32) -> String {
    if number <= 1 {
        return url.to_owned();
    }
    if page_number(url).is_some() {
        return with_page(url, number);
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}page={number}")
}

/// `url` with its `page` number set to `number`.
fn with_page(url: &str, number: u32) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let params: Vec<String> = query
        .split('&')
        .map(|param| {
            if param.starts_with("page=") {
                format!("page={number}")
            } else {
                param.to_owned()
            }
        })
        .collect();
    format!("{path}?{}", params.join("&"))
}

/// Each item once, by id: the copy changed last, in id order. An item without an id becomes no
/// record, so it is dropped.
fn unique_by_id(items: Vec<Value>) -> Vec<Value> {
    let mut by_id: BTreeMap<u64, Value> = BTreeMap::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_u64) else {
            continue;
        };
        match by_id.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(item);
            }
            Entry::Occupied(mut slot) => {
                if comment_time(&item) >= comment_time(slot.get()) {
                    slot.insert(item);
                }
            }
        }
    }
    by_id.into_values().collect()
}

/// Reads one issue's comments and, for a pull request, its reviews and review comments.
/// Returns why it stopped, when it did not finish them.
///
/// The comment listings are read whole (see `issue_comments_url`), and only the comments
/// changed at or after `stored`, the time the stream is stored through, become records: an
/// earlier run stored the others.
fn follow_issue<C: GithubApi + ?Sized>(
    item: &Value,
    repo: &Repo,
    stored: DateTime<Utc>,
    window_end: DateTime<Utc>,
    reader: &mut Reader<'_, C>,
    collected: &mut Collected,
) -> Result<Option<Stop>> {
    let Some(number) = item.get("number").and_then(Value::as_u64) else {
        return Ok(None);
    };
    let base = api_base();
    let mut steps = vec![FollowUp {
        url: issue_comments_url(&base, repo, number),
        kind: FollowKind::IssueComment(number),
        paging: Paging::Backward,
    }];
    if item.get("pull_request").is_some() {
        steps.push(FollowUp {
            url: pull_reviews_url(&base, repo, number),
            kind: FollowKind::Review(number),
            paging: Paging::Forward,
        });
        steps.push(FollowUp {
            url: pull_review_comments_url(&base, repo, number),
            kind: FollowKind::ReviewComment(number),
            paging: Paging::Backward,
        });
    }
    for step in steps {
        let (items, stop) = reader.drain(step.url, &no_reserve, step.paging)?;
        for item in items
            .iter()
            .filter(|item| !step.kind.stored_before(item, stored))
        {
            push_built(collected, step.kind.build(item, repo, window_end));
        }
        if stop.is_some() {
            return Ok(stop);
        }
    }
    Ok(None)
}

fn push_built(collected: &mut Collected, built: Built) {
    match built {
        Built::Record(record) => collected.records.push(*record),
        Built::Held => collected.held += 1,
        Built::Skip => {}
    }
}

/// One follow-up listing an issue needs after the issues feed.
struct FollowUp {
    url: String,
    kind: FollowKind,
    paging: Paging,
}

/// The kind of a follow-up listing, which decides how its items become records.
enum FollowKind {
    IssueComment(u64),
    Review(u64),
    ReviewComment(u64),
}

impl FollowKind {
    fn build(&self, item: &Value, repo: &Repo, window_end: DateTime<Utc>) -> Built {
        match self {
            Self::IssueComment(number) => comment_built(item, repo, *number, window_end),
            Self::Review(number) => review_built(item, repo, *number, window_end),
            Self::ReviewComment(number) => review_comment_built(item, repo, *number, window_end),
        }
    }

    /// Whether `item` is a comment last changed before `stored`, which an earlier run stored.
    /// Reviews are listed whole and kept as they are.
    fn stored_before(&self, item: &Value, stored: DateTime<Utc>) -> bool {
        match self {
            Self::IssueComment(_) | Self::ReviewComment(_) => {
                comment_time(item).is_some_and(|time| time < stored)
            }
            Self::Review(_) => false,
        }
    }
}

/// The fewest requests the follow-ups of `issues` cost: one comments listing per issue, and
/// a reviews and a review comments listing more per pull request.
fn follow_up_cost<'a>(issues: impl IntoIterator<Item = &'a Value>) -> u32 {
    issues.into_iter().fold(0_u32, |cost, issue| {
        let each = if issue.get("pull_request").is_some() {
            3
        } else {
            1
        };
        cost.saturating_add(each)
    })
}

/// A listing whose items need no further requests.
const fn no_reserve(_items: &[Value]) -> u32 {
    0
}

/// Why a stream stopped before it finished.
#[derive(Clone, Copy)]
pub(super) enum Stop {
    Budget,
    /// The issues feed stopped while the budget could still pay for its issues' follow-ups.
    Truncated,
    RateLimit,
    Throttled,
}

impl Stop {
    /// Says why `stream` stopped, and where the next fetch continues: from `resume` when the
    /// run moved the feed on, or from the cursor it was given otherwise.
    fn warning(self, stream: &str, resume: Option<&str>) -> String {
        let why = match self {
            Self::Budget => "the run's request budget ran out",
            Self::Truncated => "the run's request budget does not cover the rest of its issues",
            Self::RateLimit => "GitHub's remaining rate limit is low",
            Self::Throttled => "GitHub refused the request to protect its rate limit",
        };
        resume.map_or_else(
            || format!("{stream}: {why}; its cursor is held for the next fetch"),
            |resume| format!("{stream}: {why}; the next fetch continues from {resume}"),
        )
    }

    /// Whether the run's own request budget stopped the stream, rather than its
    /// installation's rate limit or GitHub's throttle.
    pub(super) const fn spent_the_budget(self) -> bool {
        matches!(self, Self::Budget | Self::Truncated)
    }
}

/// The rate-limit resource GitHub counts a REST request against, and names when an answer
/// carries no `X-RateLimit-Resource`.
const REST_RESOURCE: &str = "core";

/// Tracks the run's request budget and the remaining rate limit GitHub reports for each
/// installation and resource.
///
/// GitHub keeps a rate limit per installation token's installation, and within it one per
/// resource (`X-RateLimit-Resource`): `core` for the REST listings, `graphql` for the pre-pass.
/// So a low count on one installation holds only that installation's repositories, and a
/// count of one resource never holds requests of another. Every request a [`Reader`] sends is
/// a REST request, so it is held by its installation's `core` count. The pre-pass's GraphQL
/// answers are not read here.
///
/// GitHub's answers for one installation do not always come from one counter: in one observed
/// run, within one minute and with one token, one repository's `/issues` answered `remaining 14407,
/// reset 03:15:34Z` and the next request, to its commits, `remaining 13790, reset 03:47:48Z`;
/// another installation's repository showed the same with 11,521 and 10,484. A count under the
/// floor is still a count GitHub keeps, so it holds that installation for the rest of the run.
pub(super) struct Governor {
    budget: u32,
    floor: u32,
    remaining: BTreeMap<u64, BTreeMap<String, u32>>,
}

impl Governor {
    pub(super) const fn new(budget: u32, floor: u32) -> Self {
        Self {
            budget,
            floor,
            remaining: BTreeMap::new(),
        }
    }

    /// Why the next REST request for `installation` must not be sent, or `None` when it may.
    fn blocked(&self, installation: u64) -> Option<Stop> {
        if self.budget == 0 {
            return Some(Stop::Budget);
        }
        match self
            .remaining
            .get(&installation)
            .and_then(|resources| resources.get(REST_RESOURCE))
        {
            Some(remaining) if *remaining < self.floor => Some(Stop::RateLimit),
            _ => None,
        }
    }

    /// Charges a page against the budget and learns the rate limit of the installation and
    /// resource it was counted against.
    fn observe(&mut self, installation: u64, page: &Page) {
        self.budget = self.budget.saturating_sub(1);
        if let Some(remaining) = page.rate_remaining {
            let resource = page.rate_resource.as_deref().unwrap_or(REST_RESOURCE);
            let resources = self.remaining.entry(installation).or_default();
            match resources.get_mut(resource) {
                Some(count) => *count = remaining,
                None => {
                    resources.insert(resource.to_owned(), remaining);
                }
            }
        }
    }
}
