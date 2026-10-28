//! A run reads every page of a listing, even when GitHub's data changes between two pages.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{json, Value};

use crate::fixtures::{
    busy_comment, busy_issue, fetch_busy, guard, id_of, minute, Busy, Change, BUSY,
};

/// GitHub pages the issues feed by key: its next link names the last item read, so an issue
/// changed between two page requests moves to the end without pushing another issue onto a
/// page already read. Following the next link therefore reads every issue (probed on the real
/// API, 2026-10-28; see `issues_url`).
#[test]
fn an_issue_changed_while_the_feed_is_read_hides_no_other_issue() -> Result<()> {
    let _guard = guard();
    let busy = Busy {
        issues: (1..=150)
            .map(|number| busy_issue(number, &minute(number)))
            .collect(),
        change: Some((
            format!("{BUSY}/issues"),
            Box::new(|busy: &mut Busy| {
                if let Some(issue) = busy.issues.get_mut(9) {
                    *issue = busy_issue(10, "2026-10-26T23:00:00Z");
                }
            }),
        )),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-feed")?;

    for number in 1..=150 {
        let pointer = format!("github:sjawhar/busy#{number}");
        assert!(stored.contains(&pointer), "{pointer} is read");
    }
    Ok(())
}

/// An issue's comments are in id order and paged by page number. A comment deleted from a
/// page already read moves every later comment one place back, so the first comment of the
/// next page lands on the page already read. Read from the last page down, it is still read.
#[test]
fn a_comment_deleted_while_an_issues_comments_are_read_hides_no_other_comment() -> Result<()> {
    let _guard = guard();
    let busy = Busy {
        issues: vec![busy_issue(1, "2026-10-26T10:00:00Z")],
        comments: BTreeMap::from([(
            1,
            (1..=150).map(|id| busy_comment(id, &minute(id))).collect(),
        )]),
        change: Some((
            format!("{BUSY}/issues/1/comments"),
            Box::new(|busy: &mut Busy| {
                if let Some(comments) = busy.comments.get_mut(&1) {
                    comments.retain(|comment| id_of(comment) != 10);
                }
            }),
        )),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-comments")?;

    for id in (1..=150).filter(|id| *id != 10) {
        let pointer = format!("github:sjawhar/busy#1/comment/{id}");
        assert!(stored.contains(&pointer), "{pointer} is read");
    }
    Ok(())
}

/// A pull request's review comments, listed by `updated_at`, move when one is edited: the
/// edited comment goes to the end and the first comment of the next page lands on a page
/// already read. Listed in id order and read from the last page down, every comment is read.
#[test]
fn a_review_comment_edited_while_a_pull_requests_comments_are_read_hides_no_other_comment(
) -> Result<()> {
    let _guard = guard();
    let mut pull = busy_issue(2, "2026-10-26T10:00:00Z");
    if let Some(fields) = pull.as_object_mut() {
        fields.insert(
            "pull_request".to_owned(),
            json!({ "url": "https://api.github.com/repos/sjawhar/busy/pulls/2" }),
        );
    }
    let busy = Busy {
        issues: vec![pull],
        review_comments: BTreeMap::from([(
            2,
            (1..=150).map(|id| busy_comment(id, &minute(id))).collect(),
        )]),
        change: Some((
            format!("{BUSY}/pulls/2/comments"),
            Box::new(|busy: &mut Busy| {
                if let Some(comment) = busy
                    .review_comments
                    .get_mut(&2)
                    .and_then(|comments| comments.get_mut(9))
                {
                    *comment = busy_comment(10, "2026-10-26T23:00:00Z");
                }
            }),
        )),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-review-comments")?;

    for id in 1..=150 {
        let pointer = format!("github:sjawhar/busy#2/review-comment/{id}");
        assert!(stored.contains(&pointer), "{pointer} is read");
    }
    Ok(())
}

/// The comments of a thread with three pages, one of them (comment 50) last changed before the
/// stored time, and that one edited right after the run reads the last page.
fn three_pages_and_an_old_comment_edited(listing: &str) -> (Vec<Value>, Option<(String, Change)>) {
    let comments = (1..=300)
        .map(|id| {
            if id == 50 {
                busy_comment(id, "2026-09-01T00:00:00Z")
            } else {
                busy_comment(id, &minute(id))
            }
        })
        .collect();
    let edit: Change = if listing.contains("/pulls/") {
        Box::new(|busy: &mut Busy| {
            if let Some(comment) = busy
                .review_comments
                .get_mut(&2)
                .and_then(|comments| comments.get_mut(49))
            {
                *comment = busy_comment(50, "2026-10-26T23:00:00Z");
            }
        })
    } else {
        Box::new(|busy: &mut Busy| {
            if let Some(comment) = busy
                .comments
                .get_mut(&1)
                .and_then(|comments| comments.get_mut(49))
            {
                *comment = busy_comment(50, "2026-10-26T23:00:00Z");
            }
        })
    };
    (comments, Some((format!("{listing}?page=3"), edit)))
}

/// The ids of `1..=300` whose pointer under `prefix` the run did not store.
fn never_read(stored: &BTreeSet<String>, prefix: &str) -> Vec<u64> {
    (1..=300)
        .filter(|id| !stored.contains(&format!("{prefix}{id}")))
        .collect()
}

/// A comment listing filtered by `since` grows when a comment older than `since` is edited:
/// the edited comment joins the listing at its id's place, and every later comment moves one
/// place on, so once the last page is read the last comment of the page before it moves onto
/// that page and is never read. The run ended `Ok`, its cursor moved past that comment, and the
/// next run lists from a time later than the comment's own, so it was lost for good
/// (round-4 review of d5655fc, N4-1). Listed whole, the listing changes only by deletion.
#[test]
fn an_old_comment_edited_while_an_issues_comments_are_read_hides_no_other_comment() -> Result<()> {
    let _guard = guard();
    let (comments, change) =
        three_pages_and_an_old_comment_edited(&format!("{BUSY}/issues/1/comments"));
    let busy = Busy {
        issues: vec![busy_issue(1, "2026-10-26T23:00:00Z")],
        comments: BTreeMap::from([(1, comments)]),
        change,
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-old-comment")?;

    let missing = never_read(&stored, "github:sjawhar/busy#1/comment/");
    assert!(missing.is_empty(), "comments never read: {missing:?}");
    Ok(())
}

/// The same for a pull request's review comments, which are listed the same way.
#[test]
fn an_old_review_comment_edited_while_a_pull_requests_comments_are_read_hides_no_other_comment(
) -> Result<()> {
    let _guard = guard();
    let mut pull = busy_issue(2, "2026-10-26T23:00:00Z");
    if let Some(fields) = pull.as_object_mut() {
        fields.insert(
            "pull_request".to_owned(),
            json!({ "url": "https://api.github.com/repos/sjawhar/busy/pulls/2" }),
        );
    }
    let (comments, change) =
        three_pages_and_an_old_comment_edited(&format!("{BUSY}/pulls/2/comments"));
    let busy = Busy {
        issues: vec![pull],
        review_comments: BTreeMap::from([(2, comments)]),
        change,
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-old-review-comment")?;

    let missing = never_read(&stored, "github:sjawhar/busy#2/review-comment/");
    assert!(
        missing.is_empty(),
        "review comments never read: {missing:?}"
    );
    Ok(())
}

/// A comment last changed before the time the stream is stored through was stored by an
/// earlier run, so a run that reads the whole listing does not send it again as an arrival.
#[test]
fn a_comment_unchanged_since_the_stored_time_is_not_sent_again() -> Result<()> {
    let _guard = guard();
    let busy = Busy {
        issues: vec![busy_issue(1, "2026-10-26T10:00:00Z")],
        comments: BTreeMap::from([(
            1,
            vec![
                busy_comment(1, "2026-09-30T23:59:59Z"),
                busy_comment(2, "2026-10-01T00:00:00Z"),
                busy_comment(3, "2026-10-25T08:03:00Z"),
            ],
        )]),
        ..Busy::default()
    };
    let (address, _) = busy.serve()?;

    let stored = fetch_busy(address, "busy-unchanged-comment")?;

    assert!(
        !stored.contains("github:sjawhar/busy#1/comment/1"),
        "{stored:?}"
    );
    assert!(
        stored.contains("github:sjawhar/busy#1/comment/2"),
        "{stored:?}"
    );
    assert!(
        stored.contains("github:sjawhar/busy#1/comment/3"),
        "{stored:?}"
    );
    Ok(())
}
