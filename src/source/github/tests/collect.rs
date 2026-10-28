//! GitHub collection and request-governance tests.

use super::fixtures::*;
#[test]
fn fetches_issues_prs_comments_reviews_and_default_branch_commits() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    fake.page(
        &issues_url(&base, &subject, SINCE),
        page(vec![
            json!({
                "number": 1,
                "title": "Fix the parser",
                "body": "ship the GitHub source",
                "user": {"login": "sami", "id": 1},
                "updated_at": "2026-10-25T10:00:00Z",
                "html_url": "https://github.com/sjawhar/ctx-memory/issues/1"
            }),
            json!({
                "number": 2,
                "title": "Add the source",
                "body": "the pull request body",
                "pull_request": {"url": "https://api.github.com/repos/sjawhar/ctx-memory/pulls/2"},
                "user": {"login": "sami", "id": 1},
                "updated_at": "2026-10-25T11:00:00Z",
                "html_url": "https://github.com/sjawhar/ctx-memory/pull/2"
            }),
        ]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 1),
        page(vec![json!({
            "id": 9001,
            "body": "a comment on the issue",
            "user": {"login": "bob", "id": 2},
            "updated_at": "2026-10-25T12:00:00Z"
        })]),
    );
    fake.page(
        &issue_comments_url(&base, &subject, 2),
        page(vec![json!({
            "id": 9002,
            "body": "a comment on the pull request",
            "user": {"login": "bob", "id": 2},
            "updated_at": "2026-10-25T12:30:00Z"
        })]),
    );
    fake.page(
        &pull_reviews_url(&base, &subject, 2),
        page(vec![json!({
            "id": 7001,
            "body": "looks good to me",
            "state": "APPROVED",
            "user": {"login": "carol", "id": 3},
            "submitted_at": "2026-10-25T13:00:00Z"
        })]),
    );
    fake.page(
        &pull_review_comments_url(&base, &subject, 2),
        page(vec![json!({
            "id": 8001,
            "body": "one inline note",
            "user": {"login": "carol", "id": 3},
            "updated_at": "2026-10-25T13:30:00Z"
        })]),
    );
    let merge = json!({
        "sha": "d56570d1234567",
        "commit": {
            "message": "Merge the lease work",
            "author": {"name": "Sami", "date": "2026-10-25T09:00:00Z"},
            "committer": {"date": "2026-10-25T09:00:00Z"}
        },
        "author": {"login": "sami", "id": 1}
    });
    branch_at(&fake, &subject, &merge);
    fake.page(
        &commits_url(&base, &subject, "d56570d1234567", SINCE),
        page(vec![merge.clone()]),
    );

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 600, 2000))?;

    assert_eq!(outcome.verbatim, Some(true));
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(
        pointers(&outcome),
        BTreeSet::from([
            "github:sjawhar/ctx-memory#1".to_owned(),
            "github:sjawhar/ctx-memory#2".to_owned(),
            "github:sjawhar/ctx-memory#1/comment/9001".to_owned(),
            "github:sjawhar/ctx-memory#2/comment/9002".to_owned(),
            "github:sjawhar/ctx-memory#2/review/7001".to_owned(),
            "github:sjawhar/ctx-memory#2/review-comment/8001".to_owned(),
            "github:sjawhar/ctx-memory@d56570d1234567".to_owned(),
        ])
    );
    assert_eq!(
        cursor_of(&outcome, &stream_id(&subject))?.as_deref(),
        Some("feed=2026-10-27T00:00:00Z,head=d56570d1234567@2026-10-27T00:00:00Z")
    );
    Ok(())
}

#[test]
fn stops_at_request_budget_and_names_only_what_it_finished() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let mut first = page(vec![issue(1, "2026-10-25T10:00:00Z")]);
    first.next = Some("page-2".to_owned());
    fake.page(&issues_url(&base, &subject, SINCE), first);
    let mut second = page(vec![issue(2, "2026-10-25T11:00:00Z")]);
    second.next = Some("page-3".to_owned());
    fake.page("page-2", second);
    fake.page("page-3", page(Vec::new()));
    fake.page(&issue_comments_url(&base, &subject, 1), page(Vec::new()));

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 3, 2000))?;

    // Three requests were paid for: two feed pages, then the first issue's comments, which
    // the feed left the last request for. The rest of the feed was never asked, and issue
    // 2, whose second may go on in the page not read, waits for the next run.
    assert_eq!(fake.get_log.borrow().len(), 3);
    assert!(!fake.get_log.borrow().iter().any(|url| url == "page-3"));
    // The cursor names what the run finished: the feed resumes after issue 1, and the
    // follow-ups are read from where the stream is stored through.
    assert_eq!(
        cursor_of(&outcome, &stream_id(&subject))?.as_deref(),
        Some("feed=2026-10-25T10:00:00Z,issue=1@2026-10-01T00:00:00Z")
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.message.contains("request budget")),
        "{:?}",
        outcome.warnings
    );
    assert_eq!(
        pointers(&outcome),
        BTreeSet::from(["github:sjawhar/ctx-memory#1".to_owned()])
    );
    Ok(())
}

#[test]
fn stops_when_rate_limit_remaining_is_low() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let mut first = page(vec![issue(1, "2026-10-25T10:00:00Z")]);
    first.rate_remaining = Some(1500);
    first.next = Some("page-2".to_owned());
    fake.page(&issues_url(&base, &subject, SINCE), first);
    fake.page("page-2", page(Vec::new()));

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 600, 2000))?;

    // The second page was never asked, because the rate limit had fallen under the floor.
    assert_eq!(fake.get_log.borrow().len(), 1);
    assert_eq!(
        cursor_of(&outcome, &stream_id(&subject))?.as_deref(),
        Some(CURSOR)
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.message.contains("rate limit")),
        "{:?}",
        outcome.warnings
    );
    Ok(())
}

/// GitHub keeps one rate limit per installation token, so a limit that is low on one
/// installation says nothing about another's. Today the run keeps one remaining count for
/// every installation, so once one installation's answer reports it under the floor, every later
/// repository is held, those of an installation with thousands of requests left among them.
/// Observed on the real surface (round-4 acceptance at d5655fc): the
/// other-org installation answered 1,996 remaining, and the run held two of the
/// other installation's repositories, which had changed, with "GitHub's remaining rate limit is low",
/// while the sjawhar installation had about 10,500 of 11,550 left and was never asked.
#[test]
fn a_low_rate_limit_on_one_installation_holds_no_repository_of_another() -> Result<()> {
    let base = api_base();
    let other_org = repo("other-org", "busy");
    let sjawhar = repo("sjawhar", "quiet");
    let fake = FakeGithub {
        repos: BTreeMap::from([
            (1_000_002, vec![other_org.clone()]),
            (1_000_001, vec![sjawhar.clone()]),
        ]),
        ..FakeGithub::default()
    };
    quiet(&fake, &other_org);
    quiet(&fake, &sjawhar);
    // The other-org installation is under the floor; the sjawhar one has plenty left.
    let mut low = page(Vec::new());
    low.rate_remaining = Some(1500);
    fake.page(&issues_url(&base, &other_org, SINCE), low);
    let mut plenty = page(Vec::new());
    plenty.rate_remaining = Some(9000);
    fake.page(&issues_url(&base, &sjawhar, SINCE), plenty);

    let cursors = BTreeMap::from([
        (stream_id(&other_org), CURSOR.to_owned()),
        (stream_id(&sjawhar), CURSOR.to_owned()),
    ]);
    let outcome = run(
        &fake,
        &request(cursors)?,
        &config(vec![1_000_002, 1_000_001], 600, 2000),
    )?;

    assert!(
        fake.get_log
            .borrow()
            .iter()
            .any(|url| url.contains("/repos/sjawhar/quiet/")),
        "the sjawhar repository was never read, though its installation's limit is high: {:?}",
        fake.get_log.borrow()
    );
    assert_ne!(
        cursor_of(&outcome, &stream_id(&sjawhar))?.as_deref(),
        Some(CURSOR),
        "{:?}",
        outcome.warnings
    );
    Ok(())
}

/// GitHub counts each resource apart (`X-RateLimit-Resource`): a low count of one resource
/// says nothing about `core`, which every REST listing is counted against. Only a low
/// `core` count holds the listings.
#[test]
fn a_low_count_of_another_resource_holds_no_rest_listing() -> Result<()> {
    let base = api_base();
    let subject = repo("sjawhar", "ctx-memory");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let mut first = page(vec![issue(1, "2026-10-25T10:00:00Z")]);
    first.rate_remaining = Some(100);
    first.rate_resource = Some("graphql".to_owned());
    first.next = Some("page-2".to_owned());
    fake.page(&issues_url(&base, &subject, SINCE), first);
    let mut second = page(Vec::new());
    second.rate_remaining = Some(1500);
    second.rate_resource = Some("core".to_owned());
    fake.page("page-2", second);

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    run(&fake, &request, &config(vec![1], 600, 2000))?;

    // The second page is asked for despite the low `graphql` count; its low `core` count
    // then holds the rest of the stream.
    assert_eq!(
        *fake.get_log.borrow(),
        vec![issues_url(&base, &subject, SINCE), "page-2".to_owned()]
    );
    Ok(())
}

#[test]
fn only_listed_installations_are_called() -> Result<()> {
    let other_org = repo("other-org", "memory");
    let sjawhar = repo("sjawhar", "ctx-memory");
    let other = repo("unlisted-org", "private-repo");
    let fake = FakeGithub {
        repos: BTreeMap::from([
            (1_000_002, vec![other_org.clone()]),
            (1_000_001, vec![sjawhar.clone()]),
            (1_000_003, vec![other.clone()]),
        ]),
        ..FakeGithub::default()
    };
    for subject in [&other_org, &sjawhar] {
        quiet(&fake, subject);
    }

    let cursors = BTreeMap::from([
        (stream_id(&other_org), CURSOR.to_owned()),
        (stream_id(&sjawhar), CURSOR.to_owned()),
    ]);
    let request = request(cursors)?;
    let outcome = run(
        &fake,
        &request,
        &config(vec![1_000_002, 1_000_001], 600, 2000),
    )?;

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    // Only the two configured installations were asked for repositories.
    assert_eq!(
        *fake.installations_called.borrow(),
        vec![1_000_002, 1_000_001]
    );
    // The excluded organization's repository was never read.
    assert!(
        !fake
            .get_log
            .borrow()
            .iter()
            .any(|url| url.contains("unlisted-org")),
        "{:?}",
        fake.get_log.borrow()
    );
    let streams: BTreeSet<&String> = outcome.streams.keys().collect();
    assert_eq!(
        streams,
        BTreeSet::from([&stream_id(&other_org), &stream_id(&sjawhar)])
    );
    Ok(())
}

#[test]
fn head_url_percent_encodes_the_branch() {
    // One repository's default branch is literally `v3.3.1+qvar`. A raw `+` in a
    // query string is read as a space, so GitHub answers the commits listing with
    // `404 Not Found` and the whole repository stream errors (observed in a real fetch,
    // 2026-10-27). The branch must be percent-encoded in the `sha` parameter.
    let subject = Repo {
        owner: "other-org".to_owned(),
        name: "coq-elpi".to_owned(),
        default_branch: "v3.3.1+qvar".to_owned(),
    };
    let url = head_url(&api_base(), &subject);
    assert!(
        !url.contains("sha=v3.3.1+qvar"),
        "the branch's `+` must be encoded, not sent raw: {url}"
    );
    assert!(
        url.contains("v3.3.1%2Bqvar"),
        "the branch must be percent-encoded in the sha parameter: {url}"
    );
}

#[test]
fn a_default_branch_with_a_plus_still_reads_its_commits() -> Result<()> {
    let base = api_base();
    let subject = Repo {
        owner: "other-org".to_owned(),
        name: "coq-elpi".to_owned(),
        default_branch: "v3.3.1+qvar".to_owned(),
    };
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    fake.page(&issues_url(&base, &subject, SINCE), page(Vec::new()));
    // The fake answers the encoded URL only, so a raw `+` would leave the stream errored,
    // exactly as GitHub's 404 did in a real fetch.
    let bump = json!({
        "sha": "abc1234",
        "commit": {
            "message": "Bump the qvar branch",
            "author": {"name": "Sami", "date": "2026-10-25T09:00:00Z"},
            "committer": {"date": "2026-10-25T09:00:00Z"}
        },
        "author": {"login": "sami", "id": 1}
    });
    fake.page(
        &format!("{base}/repos/other-org/coq-elpi/commits?sha=v3.3.1%2Bqvar&per_page=1"),
        page(vec![bump.clone()]),
    );
    fake.page(
            &format!(
                "{base}/repos/other-org/coq-elpi/commits?sha=abc1234&since=2026-10-01T00%3A00%3A00Z&per_page=100"
            ),
            page(vec![bump]),
        );

    let request = request(BTreeMap::from([(stream_id(&subject), CURSOR.to_owned())]))?;
    let outcome = run(&fake, &request, &config(vec![1], 600, 2000))?;

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(
        pointers(&outcome),
        BTreeSet::from(["github:other-org/coq-elpi@abc1234".to_owned()])
    );
    assert_eq!(
        cursor_of(&outcome, &stream_id(&subject))?.as_deref(),
        Some("feed=2026-10-27T00:00:00Z,head=abc1234@2026-10-27T00:00:00Z")
    );
    Ok(())
}
