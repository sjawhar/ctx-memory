//! GitHub cursor and persisted-state tests.

use super::fixtures::*;
#[test]
fn a_stream_the_budget_stopped_is_read_in_full_by_the_next_run_whatever_graphql_says() -> Result<()>
{
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    fake.page(
        &issues_url(&base, &subject, SINCE),
        page(vec![
            issue(1, "2026-10-25T10:00:00Z"),
            issue(2, "2026-10-25T10:30:00Z"),
        ]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 1),
        page(vec![comment(9001, "2026-10-25T12:00:00Z")]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 2),
        page(vec![comment(9002, "2026-10-25T12:30:00Z")]),
    );
    fake.page(&head_url(&base, &subject), page(Vec::new()));
    // The GraphQL API says nothing changed since long before the cursor, which is wrong
    // for a stream the last run did not finish: only a REST read stores issue 2's comment.
    fake.changed(&subject, "2026-09-01T00:00:00Z");

    // Run one pays for the issues feed alone, then runs out of budget before it finishes
    // any issue, so it holds the stream's cursor.
    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let first = run(&fake, &request, &config(vec![1], 1, 2000))?;
    assert_eq!(
        cursor_of(&first, &stream_id(&subject))?.as_deref(),
        Some(CURSOR)
    );

    // Run two is given that held cursor, so it reads the feed again in full.
    fake.get_log.borrow_mut().clear();
    let second = run(&fake, &request, &config(vec![1], 600, 2000))?;

    assert!(
        fake.get_log
            .borrow()
            .contains(&issue_comments_url(&base, &subject, 2)),
        "{:?}",
        fake.get_log.borrow()
    );
    assert!(
        pointers(&second).contains("github:sjawhar/ctx-memory#2/comment/9002"),
        "{:?}",
        pointers(&second)
    );
    assert_eq!(
        cursor_of(&second, &stream_id(&subject))?.as_deref(),
        Some(WINDOW_END)
    );
    Ok(())
}

#[test]
fn a_stream_that_failed_is_read_through_rest_by_the_next_run_whatever_graphql_says() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let stream = stream_id(&subject);
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    quiet(&fake, &subject);

    // Run one finishes and settles the stream at its window end.
    let first_request = request(BTreeMap::from([(stream.clone(), CURSOR.to_owned())]))?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;
    assert_eq!(cursor_of(&first, &stream)?.as_deref(), Some(WINDOW_END));

    // Run two finds a change, reads the first feed page, and fails on the second, as a
    // dropped connection would. Its cursor stays where run one left it.
    fake.changed(&subject, "2026-10-27T00:05:00Z");
    let mut issues = page(vec![issue(1, "2026-10-27T00:05:00Z")]);
    issues.next = Some("page-2".to_owned());
    fake.page(&issues_url(&base, &subject, WINDOW_END), issues);
    fake.page(&issue_comments_url(&base, &subject, 1), page(Vec::new()));
    let second_request = next_request(&first_request, &first)?;
    let second = run(&fake, &second_request, &config(vec![1], 600, 2000))?;
    let failed = second
        .streams
        .get(&stream)
        .context("the repository has a stream result")?;
    assert_eq!(failed.status, StreamStatus::Error);

    // Run three has run one's cursor again, and the GraphQL API's answer is stale: the
    // failed read is still read through REST, and the issue reaches the window.
    fake.changed(&subject, "2026-09-01T00:00:00Z");
    fake.page("page-2", page(Vec::new()));
    fake.get_log.borrow_mut().clear();
    let third = run(&fake, &second_request, &config(vec![1], 600, 2000))?;

    assert!(
        fake.get_log
            .borrow()
            .contains(&issues_url(&base, &subject, WINDOW_END)),
        "{:?}",
        fake.get_log.borrow()
    );
    assert!(
        pointers(&third).contains("github:sjawhar/ctx-memory#1"),
        "{:?}",
        pointers(&third)
    );
    Ok(())
}

#[test]
fn a_listing_reads_every_page_its_link_names() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    fake.page(
        &issues_url(&base, &subject, SINCE),
        page(vec![json!({
            "number": 2,
            "title": "A pull request",
            "body": "the pull request body",
            "pull_request": {"url": "https://api.github.com/repos/sjawhar/ctx-memory/pulls/2"},
            "user": {"login": "sami", "id": 1},
            "updated_at": "2026-10-25T11:00:00Z"
        })]),
    );
    fake.page(&issue_comments_url(&base, &subject, 2), page(Vec::new()));
    fake.page(
        &pull_review_comments_url(&base, &subject, 2),
        page(Vec::new()),
    );
    fake.page(&head_url(&base, &subject), page(Vec::new()));
    let review = |id: u64, submitted: &str| {
        json!({
            "id": id,
            "body": "a review",
            "state": "COMMENTED",
            "user": {"login": "carol", "id": 3},
            "submitted_at": submitted
        })
    };
    let mut reviews = page(vec![review(7001, "2026-10-25T13:00:00Z")]);
    reviews.next = Some("reviews-page-2".to_owned());
    fake.page(&pull_reviews_url(&base, &subject, 2), reviews);
    fake.page(
        "reviews-page-2",
        page(vec![review(7002, "2026-10-25T13:30:00Z")]),
    );

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 600, 2000))?;

    let stored = pointers(&outcome);
    for pointer in [
        "github:sjawhar/ctx-memory#2/review/7001",
        "github:sjawhar/ctx-memory#2/review/7002",
    ] {
        assert!(stored.contains(pointer), "{pointer} is stored: {stored:?}");
    }
    Ok(())
}

#[test]
fn a_repository_larger_than_the_budget_advances_across_runs_and_loses_nothing() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let stream = stream_id(&subject);
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let one = issue(1, "2026-10-25T10:00:00Z");
    let two = issue(2, "2026-10-25T11:00:00Z");
    let three = issue(3, "2026-10-25T12:00:00Z");
    fake.page(
        &issues_url(&base, &subject, SINCE),
        page(vec![one, two.clone(), three.clone()]),
    );
    // The feed resumes at the second of the last issue a run finished, and skips that issue
    // and any before it in the same second.
    fake.page(
        &issues_url(&base, &subject, "2026-10-25T10:00:00Z"),
        page(vec![issue(1, "2026-10-25T10:00:00Z"), two, three.clone()]),
    );
    fake.page(
        &issues_url(&base, &subject, "2026-10-25T11:00:00Z"),
        page(vec![issue(2, "2026-10-25T11:00:00Z"), three]),
    );
    fake.page(&issues_url(&base, &subject, WINDOW_END), page(Vec::new()));
    // Each issue's comments are kept from where the stream is stored through, not from
    // where the feed resumes: the comment on issue 2 is older than the point run two
    // resumes the feed at, and only a run that keeps comments from the window's start
    // stores it.
    fake.page(
        &issue_comments_url(&base, &subject, 1),
        page(vec![comment(9001, "2026-10-25T10:00:00Z")]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 2),
        page(vec![comment(9002, "2026-10-20T08:00:00Z")]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 3),
        page(vec![comment(9003, "2026-10-25T12:00:00Z")]),
    );
    let early = json!({
        "sha": "c0ffee1234567",
        "commit": {
            "message": "An early commit",
            "author": {"name": "Sami", "date": "2026-10-02T09:00:00Z"},
            "committer": {"date": "2026-10-02T09:00:00Z"}
        },
        "author": {"login": "sami", "id": 1}
    });
    branch_at(&fake, &subject, &early);
    fake.page(
        &commits_url(&base, &subject, "c0ffee1234567", SINCE),
        page(vec![early.clone()]),
    );

    let mut request = request(BTreeMap::from([(stream.clone(), CURSOR.to_owned())]))?;
    let mut stored = BTreeSet::new();
    let mut cursors = Vec::new();
    // Two requests a run cover the feed and one issue's comments, so the third run finishes
    // the last issue but not the commits, and the fourth finishes the stream.
    for budget in [2, 2, 2, 600] {
        let outcome = run(&fake, &request, &config(vec![1], budget, 2000))?;
        stored.extend(pointers(&outcome));
        let cursor = cursor_of(&outcome, &stream)?.context("an Ok stream has a cursor")?;
        request.cursors.insert(stream.clone(), cursor.clone());
        cursors.push(cursor);
    }

    assert_eq!(
        cursors,
        vec![
            "feed=2026-10-25T10:00:00Z,issue=1@2026-10-01T00:00:00Z".to_owned(),
            "feed=2026-10-25T11:00:00Z,issue=2@2026-10-01T00:00:00Z".to_owned(),
            "2026-10-27T00:00:00Z@2026-10-01T00:00:00Z".to_owned(),
            "feed=2026-10-27T00:00:00Z,head=c0ffee1234567@2026-10-27T00:00:00Z".to_owned(),
        ]
    );
    for pointer in [
        "github:sjawhar/ctx-memory#1/comment/9001",
        "github:sjawhar/ctx-memory#2/comment/9002",
        "github:sjawhar/ctx-memory#3/comment/9003",
        "github:sjawhar/ctx-memory@c0ffee1234567",
    ] {
        assert!(stored.contains(pointer), "{pointer} is stored: {stored:?}");
    }
    Ok(())
}

#[test]
fn a_feed_longer_than_the_budget_stops_where_its_issues_can_still_be_finished() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let stream = stream_id(&subject);
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let mut first = page(vec![issue(1, "2026-10-25T10:00:00Z")]);
    first.next = Some("feed-2".to_owned());
    fake.page(&issues_url(&base, &subject, SINCE), first);
    for (index, updated) in [(2, "2026-10-25T11:00:00Z"), (3, "2026-10-25T12:00:00Z")] {
        let mut next = page(vec![issue(index, updated)]);
        next.next = Some(format!("feed-{}", index + 1));
        fake.page(&format!("feed-{index}"), next);
    }
    fake.page("feed-4", page(Vec::new()));
    fake.page(
        &issue_comments_url(&base, &subject, 1),
        page(vec![comment(9001, "2026-10-25T10:00:00Z")]),
    );

    // Three requests: had the run read the feed to its end, nothing would be left for any
    // issue's comments, and every run would stop at the same place.
    let request = request(BTreeMap::from([(stream.clone(), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 3, 2000))?;

    assert!(
        pointers(&outcome).contains("github:sjawhar/ctx-memory#1/comment/9001"),
        "{:?}",
        pointers(&outcome)
    );
    assert_eq!(
        cursor_of(&outcome, &stream)?.as_deref(),
        Some("feed=2026-10-25T10:00:00Z,issue=1@2026-10-01T00:00:00Z")
    );
    assert!(!fake.get_log.borrow().iter().any(|url| url == "feed-3"));
    Ok(())
}

/// A stream the run's budget held is read by the next run at the same budget, when nothing
/// changed in the repositories read before it. Observed on the real surface
/// (final acceptance at b0a545d): at `request_budget = 40`, two runs
/// read the same 20 repositories and held the same 341.
#[test]
fn a_stream_the_budget_held_is_read_by_the_next_run_at_the_same_budget() -> Result<()> {
    let first_repo = repo("sjawhar", "first");
    let second_repo = repo("sjawhar", "second");
    let held_repo = repo("sjawhar", "held");
    let fake = FakeGithub {
        repos: BTreeMap::from([(
            1,
            vec![first_repo.clone(), second_repo.clone(), held_repo.clone()],
        )]),
        ..FakeGithub::default()
    };
    for subject in [&first_repo, &second_repo, &held_repo] {
        quiet(&fake, subject);
    }
    let cursors = [&first_repo, &second_repo, &held_repo]
        .into_iter()
        .map(|subject| (stream_id(subject), CURSOR.to_owned()))
        .collect();

    // Run one: four requests read the first two repositories and hold the third.
    let first_request = request(cursors)?;
    let first = run(&fake, &first_request, &config(vec![1], 4, 2000))?;
    assert_eq!(
        cursor_of(&first, &stream_id(&held_repo))?.as_deref(),
        Some(CURSOR)
    );

    // Run two, fifteen minutes later, starts from the cursors run one left.
    let second_request = next_request(&first_request, &first)?;
    let second = run(&fake, &second_request, &config(vec![1], 4, 2000))?;

    assert_eq!(
        cursor_of(&second, &stream_id(&held_repo))?.as_deref(),
        Some(LATER_END),
        "the repository run one held was held again: requests {:?}",
        fake.get_log.borrow()
    );
    Ok(())
}

#[test]
fn a_run_starts_at_the_repository_the_last_run_held() -> Result<()> {
    // Every repository changes between the runs, so the pre-pass saves no request: only
    // where the run starts decides whether the held repository is ever read.
    let names = ["first", "second", "held"];
    let subjects: Vec<Repo> = names.iter().map(|name| repo("sjawhar", name)).collect();
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, subjects.clone())]),
        ..FakeGithub::default()
    };
    for subject in &subjects {
        quiet(&fake, subject);
        fake.changed(subject, "2026-10-27T00:05:00Z");
    }
    let held = stream_id(&repo("sjawhar", "held"));
    let first_request = request(
        subjects
            .iter()
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    let first = run(&fake, &first_request, &config(vec![1], 4, 2000))?;
    assert_eq!(cursor_of(&first, &held)?.as_deref(), Some(CURSOR));

    let second_request = next_request(&first_request, &first)?;
    let second = run(&fake, &second_request, &config(vec![1], 4, 2000))?;

    assert_eq!(cursor_of(&second, &held)?.as_deref(), Some(LATER_END));
    // The first repository is read after the held one, and the budget holds the second.
    assert_eq!(
        cursor_of(&second, &stream_id(&repo("sjawhar", "first")))?.as_deref(),
        Some(LATER_END)
    );
    assert_eq!(
        cursor_of(&second, &stream_id(&repo("sjawhar", "second")))?.as_deref(),
        Some(WINDOW_END)
    );
    Ok(())
}

/// Each run starts where the last one stopped, so a repository the budget held is read by
/// the next run. Since a low rate limit holds only its own installation, one run can hold an
/// installation's repositories under the floor and then run out of budget in another's. It
/// records the first repository it stopped in, in its own order, as where to start: the held
/// installation's, which comes first. So the next run starts there again, the other
/// installation is read from its start, and the budget holds the same repository on every
/// run while the first installation stays under its floor. Observed on the real surface
/// (round-5 acceptance at d9cb9f7, floor 13,000): run A held every
/// sjawhar repository and ran out of budget at `other-org/example-repo`, and
/// its state named `first = github:sjawhar/vertminer-gpu`, the rate-held repository.
#[test]
fn a_repository_the_budget_held_is_read_next_while_another_installation_is_under_its_floor(
) -> Result<()> {
    let base = api_base();
    let low = repo("sjawhar", "low");
    let names = ["first", "second", "held"];
    let subjects: Vec<Repo> = names.iter().map(|name| repo("other-org", name)).collect();
    let fake = FakeGithub {
        repos: BTreeMap::from([(2, vec![low.clone()]), (1, subjects.clone())]),
        ..FakeGithub::default()
    };
    // The sjawhar installation answers under the floor on every run.
    quiet(&fake, &low);
    for since in [SINCE, WINDOW_END] {
        let mut under = page(Vec::new());
        under.rate_remaining = Some(1500);
        fake.page(&issues_url(&base, &low, since), under);
    }
    // Every other-org repository changes between the runs, so the pre-pass saves no
    // request: only where the run starts decides whether the held repository is read.
    for subject in &subjects {
        quiet(&fake, subject);
        fake.changed(subject, "2026-10-27T00:05:00Z");
    }
    let held = stream_id(&repo("other-org", "held"));
    let first_request = request(
        std::iter::once(&low)
            .chain(&subjects)
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    // One request for the sjawhar repository, which its floor then holds, and two for each
    // of `first` and `second`: the budget holds `held`.
    let first = run(&fake, &first_request, &config(vec![2, 1], 5, 2000))?;
    assert_eq!(cursor_of(&first, &held)?.as_deref(), Some(CURSOR));

    let second_request = next_request(&first_request, &first)?;
    let second = run(&fake, &second_request, &config(vec![2, 1], 5, 2000))?;

    assert_eq!(
        cursor_of(&second, &held)?.as_deref(),
        Some(LATER_END),
        "the repository the budget held was held again: {:?}",
        second.warnings
    );
    Ok(())
}

#[test]
fn an_unchanged_repository_costs_no_request_on_the_next_run() -> Result<()> {
    let subject = repo("sjawhar", "quiet");
    let stream = stream_id(&subject);
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    quiet(&fake, &subject);
    fake.changed(&subject, "2026-10-20T00:00:00Z");
    let first_request = request(BTreeMap::from([(stream.clone(), CURSOR.to_owned())]))?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;
    // A stream no run has settled is not asked about: its first read is a REST read.
    assert_eq!(fake.asked(), Vec::<usize>::new());
    assert_eq!(fake.get_log.borrow().len(), 2);

    fake.get_log.borrow_mut().clear();
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    assert_eq!(fake.asked(), vec![1]);
    assert_eq!(*fake.get_log.borrow(), Vec::<String>::new());
    assert_eq!(cursor_of(&second, &stream)?.as_deref(), Some(LATER_END));
    Ok(())
}

#[test]
fn a_repository_changed_at_or_after_its_cursor_is_read_through_rest() -> Result<()> {
    let changed = repo("sjawhar", "changed");
    let same_second = repo("sjawhar", "same-second");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![changed.clone(), same_second.clone()])]),
        ..FakeGithub::default()
    };
    for subject in [&changed, &same_second] {
        quiet(&fake, subject);
    }
    let first_request = request(
        [&changed, &same_second]
            .into_iter()
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;

    // One repository changed after run one's window end, the other in its very second: a
    // change at the stored time itself may not be stored yet, so both are read.
    fake.changed(&changed, "2026-10-27T00:10:00Z");
    fake.changed(&same_second, WINDOW_END);
    fake.get_log.borrow_mut().clear();
    run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    let base = api_base();
    for subject in [&changed, &same_second] {
        assert!(
            fake.get_log
                .borrow()
                .contains(&issues_url(&base, subject, WINDOW_END)),
            "{} is read: {:?}",
            subject.name,
            fake.get_log.borrow()
        );
    }
    Ok(())
}

#[test]
fn the_pre_pass_asks_fifty_repositories_a_query() -> Result<()> {
    let subjects: Vec<Repo> = (0..51)
        .map(|index| repo("sjawhar", &format!("repo-{index}")))
        .collect();
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, subjects.clone())]),
        ..FakeGithub::default()
    };
    for subject in &subjects {
        quiet(&fake, subject);
        fake.changed(subject, "2026-10-20T00:00:00Z");
    }
    let first_request = request(
        subjects
            .iter()
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;

    fake.get_log.borrow_mut().clear();
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    assert_eq!(fake.asked(), vec![50, 1]);
    assert_eq!(*fake.get_log.borrow(), Vec::<String>::new());
    for subject in &subjects {
        assert_eq!(
            cursor_of(&second, &stream_id(subject))?.as_deref(),
            Some(LATER_END)
        );
    }
    Ok(())
}

/// Runs a settled pair of quiet repositories once more, after `break_answer` spoils the
/// GraphQL API's answer, and returns the REST requests that run sent and its warnings.
fn a_spoiled_pre_pass(break_answer: impl Fn(&FakeGithub)) -> Result<(Vec<String>, String)> {
    let one = repo("sjawhar", "one");
    let two = repo("sjawhar", "two");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![one.clone(), two.clone()])]),
        ..FakeGithub::default()
    };
    for subject in [&one, &two] {
        quiet(&fake, subject);
        fake.changed(subject, "2026-10-20T00:00:00Z");
    }
    let first_request = request(
        [&one, &two]
            .into_iter()
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;
    break_answer(&fake);
    fake.get_log.borrow_mut().clear();
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;
    let warnings = second
        .warnings
        .iter()
        .map(|warning| warning.message.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let read = fake.get_log.borrow().clone();
    Ok((read, warnings))
}

#[test]
fn a_graphql_answer_with_errors_reads_its_whole_batch_through_rest() -> Result<()> {
    let (read, warnings) = a_spoiled_pre_pass(|fake| {
        *fake.graphql_errors.borrow_mut() = Some(json!([
            {"type": "NOT_FOUND", "path": ["r1"], "message": "Could not resolve to a Repository"}
        ]));
    })?;

    let base = api_base();
    for name in ["one", "two"] {
        let url = issues_url(&base, &repo("sjawhar", name), WINDOW_END);
        assert!(read.contains(&url), "{name} is read: {read:?}");
    }
    assert!(warnings.contains("GraphQL"), "{warnings}");
    Ok(())
}

#[test]
fn a_graphql_answer_that_leaves_a_repository_out_reads_its_whole_batch_through_rest() -> Result<()>
{
    let (read, warnings) = a_spoiled_pre_pass(|fake| {
        fake.activity.borrow_mut().remove("sjawhar/two");
    })?;

    let base = api_base();
    for name in ["one", "two"] {
        let url = issues_url(&base, &repo("sjawhar", name), WINDOW_END);
        assert!(read.contains(&url), "{name} is read: {read:?}");
    }
    assert!(warnings.contains("sjawhar/two"), "{warnings}");
    Ok(())
}

#[test]
fn a_review_held_past_the_window_is_stored_by_the_next_run_whatever_graphql_says() -> Result<()> {
    // Run one holds a review submitted after its window ended. The GraphQL API's answer to
    // run two is stale and names nothing after run one's window end, so only the rule that
    // a stream which held an item is read through REST brings the review in.
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let pull = |updated: &str| {
        json!({
            "number": 2,
            "title": "A pull request",
            "body": "the pull request body",
            "pull_request": {"url": "https://api.github.com/repos/sjawhar/ctx-memory/pulls/2"},
            "user": {"login": "sami", "id": 1},
            "updated_at": updated
        })
    };
    for (since, updated) in [
        (SINCE, "2026-10-25T11:00:00Z"),
        (WINDOW_END, "2026-10-27T00:03:00Z"),
    ] {
        fake.page(
            &issues_url(&base, &subject, since),
            page(vec![pull(updated)]),
        );
        fake.page(&issue_comments_url(&base, &subject, 2), page(Vec::new()));
        fake.page(
            &pull_review_comments_url(&base, &subject, 2),
            page(Vec::new()),
        );
    }
    fake.page(&head_url(&base, &subject), page(Vec::new()));
    fake.page(
        &pull_reviews_url(&base, &subject, 2),
        page(vec![json!({
            "id": 7005,
            "body": "submitted while the fetch was running",
            "state": "APPROVED",
            "user": {"login": "carol", "id": 3},
            "submitted_at": "2026-10-27T00:03:00Z"
        })]),
    );
    fake.changed(&subject, "2026-10-25T11:00:00Z");

    let first_request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;
    assert!(
        !pointers(&first).contains("github:sjawhar/ctx-memory#2/review/7005"),
        "run one holds the review: {:?}",
        pointers(&first)
    );
    assert_eq!(
        cursor_of(&first, &stream_id(&subject))?.as_deref(),
        Some(WINDOW_END)
    );

    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    assert!(
        pointers(&second).contains("github:sjawhar/ctx-memory#2/review/7005"),
        "{:?}",
        pointers(&second)
    );
    Ok(())
}

#[test]
fn the_state_file_forgets_repositories_the_app_left_and_the_old_etag_file_is_deleted() -> Result<()>
{
    let subject = repo("sjawhar", "ctx-memory");
    let gone = repo("sjawhar", "gone");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone(), gone.clone()])]),
        ..FakeGithub::default()
    };
    for repository in [&subject, &gone] {
        quiet(&fake, repository);
    }
    let request = request(BTreeMap::from([
        (stream_id(&subject), CURSOR.to_owned()),
        (stream_id(&gone), CURSOR.to_owned()),
    ]))?;
    let old = request.state_dir.join(OLD_ETAGS_FILE);
    std::fs::write(&old, r#"{"streams": {}}"#)?;
    let first = run(&fake, &request, &config(vec![1], 600, 2000))?;
    assert!(!old.exists(), "{} is deleted", old.display());
    let saved = std::fs::read_to_string(request.state_dir.join(STATE_FILE))?;
    assert!(saved.contains("sjawhar/gone"), "{saved}");

    // The app is removed from one repository.
    let second_fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    quiet(&second_fake, &subject);
    let mut next = next_request(&request, &first)?;
    next.cursors.remove(&stream_id(&gone));
    run(&second_fake, &next, &config(vec![1], 600, 2000))?;

    let saved = std::fs::read_to_string(request.state_dir.join(STATE_FILE))?;
    assert!(saved.contains("sjawhar/ctx-memory"), "{saved}");
    assert!(
        !saved.contains("sjawhar/gone"),
        "a repository the app no longer reads is dropped: {saved}"
    );
    Ok(())
}

#[test]
fn a_state_file_this_build_cannot_read_costs_a_rest_read_of_every_repository() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let stream = stream_id(&subject);
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    quiet(&fake, &subject);
    fake.changed(&subject, "2026-10-20T00:00:00Z");
    let mut request = request(BTreeMap::from([(stream.clone(), WINDOW_END.to_owned())]))?;
    request.window_end = at(LATER_END)?;
    let path = request.state_dir.join(STATE_FILE);
    std::fs::write(&path, "settled: not json")?;

    let outcome = run(&fake, &request, &config(vec![1], 600, 2000))?;

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert!(
        fake.get_log
            .borrow()
            .contains(&issues_url(&base, &subject, WINDOW_END)),
        "{:?}",
        fake.get_log.borrow()
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.message.contains(STATE_FILE)),
        "{:?}",
        outcome.warnings
    );
    assert_eq!(cursor_of(&outcome, &stream)?.as_deref(), Some(LATER_END));
    // The run rewrote it in the form this build reads.
    let saved = std::fs::read_to_string(&path)?;
    assert!(saved.contains("\"settled\""), "{saved}");
    Ok(())
}

#[test]
fn cursors_an_earlier_build_wrote_still_parse_and_new_ones_say_what_they_hold() -> Result<()> {
    let end = at(WINDOW_END)?;
    let read = |cursor: &str| Position::read(Some(cursor), end, 24);
    // A bare time, which a finished stream wrote.
    assert_eq!(
        read(CURSOR)?,
        Position {
            resume: at(CURSOR)?,
            after: None,
            stored: at(CURSOR)?,
            head: None
        }
    );
    // `<resume>@<stored>`, which a stopped stream wrote: the feed resumes at that time, a
    // second before the issue it had not finished, and skips nothing.
    assert_eq!(
        read("2026-10-25T09:59:59Z@2026-10-01T00:00:00Z")?,
        Position {
            resume: at("2026-10-25T09:59:59Z")?,
            after: None,
            stored: at(CURSOR)?,
            head: None
        }
    );
    let stopped = "feed=2026-10-25T10:00:00Z,issue=7,head=abc1234@2026-10-01T00:00:00Z";
    let position = read(stopped)?;
    assert_eq!(
        position,
        Position {
            resume: at("2026-10-25T10:00:00Z")?,
            after: Some(7),
            stored: at(CURSOR)?,
            head: Some("abc1234".to_owned())
        }
    );
    assert_eq!(position.cursor(), stopped);
    // Issue 7 and those before it in its second are finished; issue 8 there is not.
    assert!(!position.open((at("2026-10-25T10:00:00Z")?, 7)));
    assert!(position.open((at("2026-10-25T10:00:00Z")?, 8)));
    assert!(position.open((at("2026-10-25T10:00:01Z")?, 1)));
    for bad in [
        "feed=2026-10-25T10:00:00Z,colour=blue@2026-10-01T00:00:00Z",
        "feed=2026-10-25T10:00:00Z,head=../../x@2026-10-01T00:00:00Z",
        "feed=yesterday@2026-10-01T00:00:00Z",
    ] {
        assert!(read(bad).is_err(), "{bad} is refused");
    }
    Ok(())
}
