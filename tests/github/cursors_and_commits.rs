//! What one run's cursor carries to the next, and how the default branch's commits are read.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Context as _, Result};
use ctx_memory::source::Message;
use serde_json::{json, Value};

use crate::fixtures::{
    against, busy_comment, busy_issue, fetch_busy, guard, minute, pointers, run_fetch,
    run_fetch_until, scratch, stream_ends, Busy, BUSY, BUSY_STREAM,
};
use crate::support::fake_http::FakeHttp;

/// The cursor one stream proposed, which `ctx fetch` stores once its window is written.
fn proposed(messages: &[Message], stream: &str) -> Option<String> {
    stream_ends(messages)
        .into_iter()
        .find(|(name, ..)| name == stream)
        .and_then(|(_, _, cursor, _)| cursor)
}

#[test]
fn a_run_the_budget_stopped_reads_the_follow_up_it_did_not_reach_next_time() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [{ "full_name": "sjawhar/ctx-memory", "default_branch": "main" }],
        })
        .to_string(),
    )?;
    api.json(
        "/repos/sjawhar/ctx-memory/issues",
        200,
        &json!([{
            "number": 1,
            "title": "An issue",
            "body": "the issue body",
            "user": {"login": "sami", "id": 1},
            "updated_at": "2026-10-26T10:00:00Z"
        }])
        .to_string(),
    )?;
    api.json("/repos/sjawhar/ctx-memory/issues/1/comments", 200, "[]")?;
    api.json("/repos/sjawhar/ctx-memory/commits", 200, "[]")?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<(Option<String>, usize, Vec<Message>)> {
        let state = scratch("held")?;
        // One request is all the first run may spend, so it reads the issues feed and stops
        // before the comments listing with the cursor held.
        let first = run_fetch(
            json!({ "installations": [1_000_002], "request_budget": 1 }),
            state.clone(),
            BTreeMap::new(),
        )?;
        let held = proposed(&first, "github:sjawhar/ctx-memory");
        let boundary = api.requests()?.len();
        let cursors = held.clone().map_or_else(BTreeMap::new, |cursor| {
            BTreeMap::from([("github:sjawhar/ctx-memory".to_owned(), cursor)])
        });
        let second = run_fetch(json!({ "installations": [1_000_002] }), state, cursors)?;
        Ok((held, boundary, second))
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let (held, boundary, second) = outcome?;
    // The first run held its cursor at the window start it read from.
    assert_eq!(held.as_deref(), Some("2026-10-26T00:00:00Z"));
    let requests = api.requests()?;
    let after_hold: Vec<_> = requests.into_iter().skip(boundary).collect();
    assert!(
        after_hold
            .iter()
            .any(|request| request.path == "/repos/sjawhar/ctx-memory/issues/1/comments"),
        "the follow-up the held run never reached is read: {after_hold:?}"
    );
    assert_eq!(
        proposed(&second, "github:sjawhar/ctx-memory").as_deref(),
        Some("2026-10-27T00:00:00Z"),
        "the second run finished, so its cursor moves to the window end",
    );
    Ok(())
}

/// Registers one pull request whose review was submitted after the first run's window ended,
/// and a GraphQL API whose answer to the second run is stale: it names nothing after the first
/// run's window end.
fn a_review_held_over_two_runs(api: &FakeHttp) -> Result<()> {
    api.json(
        "/app/installations/1000002/access_tokens",
        201,
        &json!({ "token": "ghs_other", "expires_at": "2026-10-27T09:00:00Z" }).to_string(),
    )?;
    api.json(
        "/installation/repositories",
        200,
        &json!({
            "total_count": 1,
            "repositories": [{ "full_name": "sjawhar/ctx-memory", "default_branch": "main" }],
        })
        .to_string(),
    )?;
    // The review submitted while the first run was under way moves the pull request's
    // `updated_at`, so the second run's feed names it again.
    let pull = |updated: &str| {
        json!([{
            "number": 2,
            "title": "A pull request",
            "body": "the pull request body",
            "pull_request": {"url": "https://api.github.com/repos/sjawhar/ctx-memory/pulls/2"},
            "user": {"login": "sami", "id": 1},
            "updated_at": updated
        }])
        .to_string()
    };
    api.json(
        "/repos/sjawhar/ctx-memory/issues",
        200,
        &pull("2026-10-26T11:00:00Z"),
    )?;
    api.then_json(
        "/repos/sjawhar/ctx-memory/issues",
        200,
        &pull("2026-10-27T00:03:00Z"),
    )?;
    api.json("/repos/sjawhar/ctx-memory/issues/2/comments", 200, "[]")?;
    api.json("/repos/sjawhar/ctx-memory/pulls/2/comments", 200, "[]")?;
    api.json("/repos/sjawhar/ctx-memory/commits", 200, "[]")?;
    api.json(
        "/repos/sjawhar/ctx-memory/pulls/2/reviews",
        200,
        &json!([{
            "id": 7005,
            "body": "submitted while the fetch was running",
            "state": "APPROVED",
            "user": {"login": "carol", "id": 3},
            "submitted_at": "2026-10-27T00:03:00Z"
        }])
        .to_string(),
    )?;
    api.json(
        "/graphql",
        200,
        &json!({ "data": { "r0": activity("2026-10-26T11:00:00Z") } }).to_string(),
    )
}

#[test]
fn a_review_held_past_the_window_is_read_by_the_next_run_whatever_graphql_says() -> Result<()> {
    let _guard = guard();
    let api = FakeHttp::start()?;
    a_review_held_over_two_runs(&api)?;

    let credential = json!({
        "app_id": "1234567",
        "private_key": include_str!("../fixtures/jwt-signing-key.pem"),
    })
    .to_string();
    std::env::set_var("CTX_GITHUB_API", api.url(""));
    std::env::set_var("CTX_CREDENTIAL_GITHUB_APP", &credential);

    let outcome = (|| -> Result<(Vec<Message>, usize, Vec<Message>)> {
        let state = scratch("held-review")?;
        let first = run_fetch(
            json!({ "installations": [1_000_002] }),
            state.clone(),
            BTreeMap::new(),
        )?;
        let cursor = proposed(&first, "github:sjawhar/ctx-memory")
            .context("the first run proposed a cursor")?;
        let boundary = api.requests()?.len();
        let second = run_fetch_until(
            json!({ "installations": [1_000_002] }),
            state,
            BTreeMap::from([("github:sjawhar/ctx-memory".to_owned(), cursor)]),
            "2026-10-27T01:00:00Z",
        )?;
        Ok((first, boundary, second))
    })();

    std::env::remove_var("CTX_CREDENTIAL_GITHUB_APP");
    std::env::remove_var("CTX_GITHUB_API");

    let (first, boundary, second) = outcome?;
    let review = "github:sjawhar/ctx-memory#2/review/7005".to_owned();
    assert!(
        !pointers(&first).contains(&review),
        "the first run's window ends before the review: {:?}",
        pointers(&first)
    );
    let requests = api.requests()?;
    let reviews = requests
        .iter()
        .skip(boundary)
        .filter(|request| request.path == "/repos/sjawhar/ctx-memory/pulls/2/reviews")
        .count();
    assert_eq!(reviews, 1, "the stream that held a review is read again");
    assert!(
        pointers(&second).contains(&review),
        "the held review reaches the next window: {:?}",
        pointers(&second)
    );
    Ok(())
}

/// One repository's answer to the GraphQL pre-pass, every time in it `newest`. The repositories
/// these tests serve have no commit, so the default branch is `null`, as GitHub answers then.
fn activity(newest: &str) -> Value {
    json!({
        "pushedAt": newest,
        "defaultBranchRef": null,
        "issues": { "nodes": [{ "updatedAt": newest }] },
        "pullRequests": { "nodes": [{ "updatedAt": newest }] },
    })
}

/// GitHub ties an `ETag` to the token that read the page, and every fetch mints its own
/// installation token, so no stored `ETag` ever answered `304` on a later run (round-3
/// acceptance at b56738d: 627 conditions, 0 answers of `304`). An unchanged repository must
/// still cost no REST request on the next run: the GraphQL pre-pass says nothing changed since
/// its cursor. The acceptance test asserted `304` answers; with no condition sent any more, it
/// asserts that the second run sends no REST request at all.
#[test]
fn an_unchanged_listing_costs_nothing_on_the_next_run_under_its_new_token() -> Result<()> {
    let _guard = guard();
    let busy = Busy {
        activity: Some(activity("2026-10-20T00:00:00Z")),
        ..Busy::default()
    };
    let (address, shared) = busy.serve()?;

    let outcome = against(address, || {
        let state = scratch("unchanged")?;
        let first = run_fetch(
            json!({ "installations": [1_000_002] }),
            state.clone(),
            BTreeMap::new(),
        )?;
        let cursor = proposed(&first, BUSY_STREAM).context("the first run proposed a cursor")?;
        let boundary = shared
            .lock()
            .map_err(|_| anyhow!("the emulated GitHub was poisoned"))?
            .requests
            .len();
        let second = run_fetch_until(
            json!({ "installations": [1_000_002] }),
            state,
            BTreeMap::from([(BUSY_STREAM.to_owned(), cursor)]),
            "2026-10-27T00:15:00Z",
        )?;
        Ok((boundary, second))
    });

    let (boundary, second) = outcome?;
    let busy = shared
        .lock()
        .map_err(|_| anyhow!("the emulated GitHub was poisoned"))?;
    assert_eq!(busy.minted, 2, "each run minted its own token");
    let later: Vec<&String> = busy.requests.iter().skip(boundary).collect();
    assert!(
        later.iter().any(|target| target.starts_with("/graphql")),
        "the second run asked when the repository last changed: {later:?}"
    );
    assert!(
        !later.iter().any(|target| target.starts_with("/repos/")),
        "the second run paid for listings that had not changed: {later:?}"
    );
    assert_eq!(
        proposed(&second, BUSY_STREAM).as_deref(),
        Some("2026-10-27T00:15:00Z"),
        "the unchanged stream's cursor moves to the second run's window end"
    );
    Ok(())
}

/// A linear history `c000001` … of `count` commits, one minute apart from 2026-10-25T08:01,
/// and its newest commit.
fn linear(count: u64) -> (BTreeMap<String, (Vec<String>, String)>, String) {
    let mut commits = BTreeMap::new();
    let mut parent: Option<String> = None;
    for index in 1..=count {
        let sha = format!("c{index:06x}");
        commits.insert(
            sha.clone(),
            (parent.iter().cloned().collect(), minute(index)),
        );
        parent = Some(sha);
    }
    (commits, parent.unwrap_or_default())
}

fn busy_commit(sha: &str) -> String {
    format!("github:sjawhar/busy@{sha}")
}

/// A force push between two runs replaces the default branch with a history that shares only
/// its first commit with the one stored, and whose commits were made before the first run's
/// window ended. A listing by time from the stored time never reaches them; comparing the head
/// the first run stored with the new one lists them.
#[test]
fn a_force_push_between_runs_leaves_no_rewritten_commit_unread() -> Result<()> {
    let _guard = guard();
    let (mut commits, head) = linear(3);
    commits.insert(
        "f000001".to_owned(),
        (
            vec!["c000001".to_owned()],
            "2026-10-26T05:00:00Z".to_owned(),
        ),
    );
    commits.insert(
        "f000002".to_owned(),
        (
            vec!["f000001".to_owned()],
            "2026-10-26T06:00:00Z".to_owned(),
        ),
    );
    let busy = Busy {
        commits,
        main: Some(head),
        ..Busy::default()
    };
    let (address, shared) = busy.serve()?;

    let (first, second) = against(address, || {
        let state = scratch("force-push-between")?;
        let first = run_fetch(
            json!({ "installations": [1_000_002] }),
            state.clone(),
            BTreeMap::from([(BUSY_STREAM.to_owned(), "2026-10-01T00:00:00Z".to_owned())]),
        )?;
        let cursor = proposed(&first, BUSY_STREAM).context("the first run proposed a cursor")?;
        shared
            .lock()
            .map_err(|_| anyhow!("the emulated GitHub was poisoned"))?
            .main = Some("f000002".to_owned());
        let second = run_fetch_until(
            json!({ "installations": [1_000_002] }),
            state,
            BTreeMap::from([(BUSY_STREAM.to_owned(), cursor)]),
            "2026-10-27T00:15:00Z",
        )?;
        Ok((first, second))
    })?;

    for sha in ["c000001", "c000002", "c000003"] {
        assert!(
            pointers(&first).contains(&busy_commit(sha)),
            "{sha} is read"
        );
    }
    for sha in ["f000001", "f000002"] {
        assert!(
            pointers(&second).contains(&busy_commit(sha)),
            "the rewritten commit {sha} is read: {:?}",
            pointers(&second)
        );
    }
    Ok(())
}

/// Two runs of `sjawhar/busy`: the first stores the linear history `c000001`…`c000003` and its
/// head; then 150 commits `d000001`… dated by `dated` land on top of it, and the comparison
/// from the stored head lists only the oldest 100 of them, as GitHub cuts a long one short.
/// Returns the second run's messages.
fn a_comparison_cut_short(label: &str, dated: fn(u64) -> String) -> Result<Vec<Message>> {
    let (commits, head) = linear(3);
    let busy = Busy {
        commits,
        main: Some(head),
        ..Busy::default()
    };
    let (address, shared) = busy.serve()?;
    against(address, || {
        let state = scratch(label)?;
        let first = run_fetch(
            json!({ "installations": [1_000_002] }),
            state.clone(),
            BTreeMap::from([(BUSY_STREAM.to_owned(), "2026-10-01T00:00:00Z".to_owned())]),
        )?;
        let cursor = proposed(&first, BUSY_STREAM).context("the first run proposed a cursor")?;
        assert!(cursor.contains("head=c000003"), "{cursor}");
        {
            let mut busy = shared
                .lock()
                .map_err(|_| anyhow!("the emulated GitHub was poisoned"))?;
            let mut parent = "c000003".to_owned();
            for index in 1..=150 {
                let sha = format!("d{index:06x}");
                busy.commits
                    .insert(sha.clone(), (vec![parent.clone()], dated(index)));
                parent = sha;
            }
            busy.main = Some(parent);
            busy.compare_cap = Some(100);
        }
        run_fetch_until(
            json!({ "installations": [1_000_002] }),
            state,
            BTreeMap::from([(BUSY_STREAM.to_owned(), cursor)]),
            "2026-10-27T00:15:00Z",
        )
    })
}

/// GitHub answers a comparison with `total_commits`, and may list fewer commits than that
/// across its pages. A run that stored the head after such an answer would never read the
/// rest: the next comparison starts from that head (round-4 review of d5655fc, N4-5). The
/// commits it did not list are read from the head's history since the stored time.
#[test]
fn a_comparison_github_cuts_short_leaves_no_commit_unread() -> Result<()> {
    let _guard = guard();
    let second = a_comparison_cut_short("compare-short", |index| {
        format!("2026-10-27T00:{:02}:{:02}Z", 1 + index / 60, index % 60)
    })?;

    let stored = pointers(&second);
    let missing: Vec<String> = (1..=150)
        .map(|index| format!("d{index:06x}"))
        .filter(|sha| !stored.contains(&busy_commit(sha)))
        .collect();
    assert!(missing.is_empty(), "commits never read: {missing:?}");
    let cursor = proposed(&second, BUSY_STREAM).context("the second run proposed a cursor")?;
    assert!(cursor.contains("head=d000096"), "{cursor}");
    Ok(())
}

/// When the commits a short comparison left out are older than the stored time, the history
/// since that time does not reach them either, so the run holds the head it was given: the
/// next run compares from it again rather than from a head past commits never read.
#[test]
fn a_comparison_cut_short_past_older_commits_holds_the_stored_head() -> Result<()> {
    let _guard = guard();
    let second = a_comparison_cut_short("compare-short-old", |index| {
        format!("2026-10-26T10:{:02}:{:02}Z", index / 60, index % 60)
    })?;

    let cursor = proposed(&second, BUSY_STREAM).context("the second run proposed a cursor")?;
    assert!(
        cursor.contains("head=c000003"),
        "the head moved past commits never read: {cursor}"
    );
    let stored = pointers(&second);
    for index in 1..=100 {
        let sha = format!("d{index:06x}");
        assert!(stored.contains(&busy_commit(&sha)), "{sha} is read");
    }
    Ok(())
}

/// A force push during the read: the default branch drops its ten newest commits right after
/// the run's first request for commits. A listing that names the branch moves every later
/// commit ten places up, so ten commits that stay on the branch fall between its first and
/// second page. A listing that names the head read first cannot change.
#[test]
fn a_force_push_during_the_read_hides_no_commit_that_stays_on_the_branch() -> Result<()> {
    let _guard = guard();
    let (commits, head) = linear(150);
    let busy = Busy {
        commits,
        main: Some(head),
        change: Some((
            format!("{BUSY}/commits"),
            Box::new(|busy: &mut Busy| busy.main = Some(format!("c{:06x}", 140))),
        )),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "force-push-during")?;

    for index in 1..=140 {
        let pointer = busy_commit(&format!("c{index:06x}"));
        assert!(stored.contains(&pointer), "{pointer} is read");
    }
    Ok(())
}

/// 150 issues changed in one second, each with a comment, read at 50 requests a run. A run
/// lists the whole second (two pages), finishes 48 issues in the order of their numbers and
/// names the last one, so the next run goes on after it. Four runs read every comment: 150
/// comment listings, two feed pages a run and the branch head come to 159 requests, more than
/// three runs' 150. A position that names only a time sent every run back to the start of the
/// second, so the same issues were read on every run and the rest never.
#[test]
fn a_second_with_more_issues_than_one_runs_budget_is_finished_across_runs() -> Result<()> {
    let _guard = guard();
    let busy = Busy {
        issues: (1..=150)
            .map(|number| busy_issue(number, "2026-10-26T12:00:00Z"))
            .collect(),
        comments: (1..=150)
            .map(|number| {
                (
                    number,
                    vec![busy_comment(10_000 + number, "2026-10-26T12:00:00Z")],
                )
            })
            .collect(),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let (stored, cursors) = against(address, || {
        let state = scratch("same-second")?;
        let mut cursors =
            BTreeMap::from([(BUSY_STREAM.to_owned(), "2026-10-01T00:00:00Z".to_owned())]);
        let mut stored = BTreeSet::new();
        let mut proposals = Vec::new();
        for _ in 0..6 {
            let messages = run_fetch(
                json!({ "installations": [1_000_002], "request_budget": 50 }),
                state.clone(),
                cursors.clone(),
            )?;
            stored.extend(pointers(&messages));
            let cursor = proposed(&messages, BUSY_STREAM).context("a cursor is proposed")?;
            proposals.push(cursor.clone());
            if cursor == "2026-10-27T00:00:00Z" {
                break;
            }
            cursors.insert(BUSY_STREAM.to_owned(), cursor);
        }
        Ok((stored, proposals))
    })?;

    for number in 1..=150 {
        let pointer = format!("github:sjawhar/busy#{number}/comment/{}", 10_000 + number);
        assert!(
            stored.contains(&pointer),
            "{pointer} is read: cursors {cursors:?}"
        );
    }
    assert_eq!(
        cursors,
        vec![
            "feed=2026-10-26T12:00:00Z,issue=48@2026-10-01T00:00:00Z".to_owned(),
            "feed=2026-10-26T12:00:00Z,issue=96@2026-10-01T00:00:00Z".to_owned(),
            "feed=2026-10-26T12:00:00Z,issue=144@2026-10-01T00:00:00Z".to_owned(),
            "2026-10-27T00:00:00Z".to_owned(),
        ]
    );
    Ok(())
}
