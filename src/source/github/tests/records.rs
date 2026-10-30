//! GitHub record and default-branch commit tests.

use super::fixtures::*;
/// A repository with no issue, whose default branch head is `head`, read once with the
/// commits `history` names since the cursor, and returned with the cursors it proposed.
fn a_branch_read_once(
    fake: &FakeGithub,
    subject: &Repo,
    head: &Value,
    history: Vec<Value>,
) -> Result<(FetchRequest, SourceOutcome)> {
    quiet(fake, subject);
    branch_at(fake, subject, head);
    let sha = head.get("sha").and_then(Value::as_str).unwrap_or_default();
    fake.page(
        &commits_url(&api_base(), subject, sha, SINCE),
        page(history),
    );
    let first_request = request(BTreeMap::from([(stream_id(subject), CURSOR.to_owned())]))?;
    let first = run(fake, &first_request, &config(vec![1], 600, 2000))?;
    assert_eq!(
        cursor_of(&first, &stream_id(subject))?,
        Some(format!("feed={WINDOW_END},head={sha}@{WINDOW_END}"))
    );
    Ok((first_request, first))
}

#[test]
fn a_branch_whose_head_has_not_moved_lists_no_commit() -> Result<()> {
    let base = api_base();
    let subject = repo("example-org", "steady");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let head = commit("aaaaaaa1", "2026-10-25T09:00:00Z");
    let (first_request, first) = a_branch_read_once(&fake, &subject, &head, vec![head.clone()])?;

    fake.get_log.borrow_mut().clear();
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    assert_eq!(
        *fake.get_log.borrow(),
        vec![
            issues_url(&base, &subject, WINDOW_END),
            head_url(&base, &subject)
        ]
    );
    assert_eq!(
        cursor_of(&second, &stream_id(&subject))?,
        Some(format!("feed={LATER_END},head=aaaaaaa1@{LATER_END}"))
    );
    Ok(())
}

#[test]
fn a_moved_head_is_read_as_a_comparison_whatever_its_commits_dates() -> Result<()> {
    let base = api_base();
    let subject = repo("example-org", "moved");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let old = commit("aaaaaaa1", "2026-10-25T09:00:00Z");
    let (first_request, first) = a_branch_read_once(&fake, &subject, &old, vec![old.clone()])?;

    // A merged branch brings a commit made before run one's window end, which no listing by
    // time from the stored time reaches, and the merge itself.
    let early = commit("ccccccc3", "2026-10-20T08:00:00Z");
    let merge = commit("bbbbbbb2", "2026-10-27T00:05:00Z");
    branch_at(&fake, &subject, &merge);
    fake.page(
        &compare_url(&base, &subject, "aaaaaaa1", "bbbbbbb2"),
        page(vec![early, merge]),
    );
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    let stored = pointers(&second);
    for pointer in [
        "github:example-org/moved@ccccccc3",
        "github:example-org/moved@bbbbbbb2",
    ] {
        assert!(stored.contains(pointer), "{pointer} is stored: {stored:?}");
    }
    assert_eq!(
        cursor_of(&second, &stream_id(&subject))?,
        Some(format!("feed={LATER_END},head=bbbbbbb2@{LATER_END}"))
    );
    Ok(())
}

#[test]
fn a_comparison_github_cannot_make_lists_the_new_head_since_the_stored_time() -> Result<()> {
    let base = api_base();
    let subject = repo("example-org", "unrelated");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let old = commit("aaaaaaa1", "2026-10-25T09:00:00Z");
    let (first_request, first) = a_branch_read_once(&fake, &subject, &old, vec![old.clone()])?;

    // The branch now holds a history that shares nothing with the stored head, and GitHub
    // answers the comparison `404`.
    let fresh = commit("bbbbbbb2", "2026-10-27T00:05:00Z");
    branch_at(&fake, &subject, &fresh);
    fake.page(
        &compare_url(&base, &subject, "aaaaaaa1", "bbbbbbb2"),
        Page {
            kind: PageKind::Missing,
            ..Page::default()
        },
    );
    fake.page(
        &commits_url(&base, &subject, "bbbbbbb2", WINDOW_END),
        page(vec![fresh]),
    );
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    assert!(
        pointers(&second).contains("github:example-org/unrelated@bbbbbbb2"),
        "{:?}",
        pointers(&second)
    );
    assert_eq!(
        cursor_of(&second, &stream_id(&subject))?,
        Some(format!("feed={LATER_END},head=bbbbbbb2@{LATER_END}"))
    );
    Ok(())
}

#[test]
fn a_commit_dated_after_the_window_end_keeps_the_stored_head() -> Result<()> {
    let base = api_base();
    let subject = repo("example-org", "skewed");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![subject.clone()])]),
        ..FakeGithub::default()
    };
    let old = commit("aaaaaaa1", "2026-10-25T09:00:00Z");
    let (first_request, first) = a_branch_read_once(&fake, &subject, &old, vec![old.clone()])?;

    // A commit whose clock ran ahead: dated after run two's window end.
    let ahead = commit("ddddddd4", "2026-10-28T00:00:00Z");
    branch_at(&fake, &subject, &ahead);
    fake.page(
        &compare_url(&base, &subject, "aaaaaaa1", "ddddddd4"),
        page(vec![ahead]),
    );
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    // The commit is held, and the next run compares from the head before it again.
    assert!(!pointers(&second).contains("github:example-org/skewed@ddddddd4"));
    assert_eq!(
        cursor_of(&second, &stream_id(&subject))?,
        Some(format!("feed={LATER_END},head=aaaaaaa1@{LATER_END}"))
    );
    Ok(())
}

#[test]
fn the_pre_pass_reads_a_repository_whose_head_moved_to_an_older_commit() -> Result<()> {
    let base = api_base();
    let steady = repo("example-org", "steady");
    let pushed = repo("example-org", "pushed");
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![steady.clone(), pushed.clone()])]),
        ..FakeGithub::default()
    };
    let head = commit("aaaaaaa1", "2026-10-25T09:00:00Z");
    for subject in [&steady, &pushed] {
        quiet(&fake, subject);
        branch_at(&fake, subject, &head);
        fake.page(
            &commits_url(&base, subject, "aaaaaaa1", SINCE),
            page(vec![head.clone()]),
        );
    }
    let first_request = request(
        [&steady, &pushed]
            .into_iter()
            .map(|subject| (stream_id(subject), CURSOR.to_owned()))
            .collect(),
    )?;
    let first = run(&fake, &first_request, &config(vec![1], 600, 2000))?;

    // Every time GitHub reports predates the cursor, but one branch was pushed to a commit
    // made before it: only the head's id tells.
    for subject in [&steady, &pushed] {
        fake.changed(subject, "2026-10-20T00:00:00Z");
    }
    fake.head_is(&steady, "aaaaaaa1");
    fake.head_is(&pushed, "eeeeeee5");
    let older = commit("eeeeeee5", "2026-10-21T00:00:00Z");
    branch_at(&fake, &pushed, &older);
    fake.page(
        &compare_url(&base, &pushed, "aaaaaaa1", "eeeeeee5"),
        page(vec![older]),
    );
    fake.get_log.borrow_mut().clear();
    let second = run(
        &fake,
        &next_request(&first_request, &first)?,
        &config(vec![1], 600, 2000),
    )?;

    let read = fake.get_log.borrow().clone();
    assert!(
        !read.iter().any(|url| url.contains("/example-org/steady/")),
        "{read:?}"
    );
    // The skipped stream's cursor still names the head it holds, so a later push of older
    // commits is still compared from it.
    assert_eq!(
        cursor_of(&second, &stream_id(&steady))?,
        Some(format!("feed={LATER_END},head=aaaaaaa1@{LATER_END}"))
    );
    assert!(
        pointers(&second).contains("github:example-org/pushed@eeeeeee5"),
        "{:?}",
        pointers(&second)
    );
    Ok(())
}
