//! Repositories the deployment excludes by GitHub's id of each.

use super::fixtures::*;

/// GitHub's id of the repository the deployment excludes.
const EXCLUDED_ID: u64 = 9_000_000_001;
/// GitHub's id of the repository it reads.
const KEPT_ID: u64 = 9_000_000_002;

/// One installation holding two repositories, each with one issue: the one whose id is
/// `EXCLUDED_ID`, called `name` now, and `kept`.
fn installation(name: &str) -> (FakeGithub, Repo, Repo) {
    let base = api_base();
    let excluded = Repo {
        id: EXCLUDED_ID,
        ..repo("example-org", name)
    };
    let kept = Repo {
        id: KEPT_ID,
        ..repo("example-org", "kept")
    };
    let fake = FakeGithub {
        repos: BTreeMap::from([(1, vec![excluded.clone(), kept.clone()])]),
        ..FakeGithub::default()
    };
    for subject in [&excluded, &kept] {
        quiet(&fake, subject);
        fake.page(
            &issues_url(&base, subject, SINCE),
            page(vec![issue(1, "2026-10-25T10:00:00Z")]),
        );
        fake.page(&issue_comments_url(&base, subject, 1), page(Vec::new()));
    }
    (fake, excluded, kept)
}

/// A repository the deployment excludes by its id is never read, and stays excluded under the
/// name GitHub gives it after a rename or a transfer: the run lists no stream for it and sends
/// no request for it, so none of its issues becomes a record. A pointer pattern names a
/// repository by the name it had when the pattern was written, which a rename leaves behind.
/// Control: with no id excluded, the same repository, under either name, is read.
#[test]
fn a_repository_excluded_by_id_is_never_read_under_any_name() -> Result<()> {
    for name in ["private", "private-renamed"] {
        for exclude in [vec![EXCLUDED_ID], Vec::new()] {
            let (fake, excluded, kept) = installation(name);
            let cursors = BTreeMap::from([
                (stream_id(&excluded), CURSOR.to_owned()),
                (stream_id(&kept), CURSOR.to_owned()),
            ]);
            let config = GithubConfig {
                exclude_repository_ids: exclude.clone(),
                ..config(vec![1], 600, 2000)
            };
            let outcome = run(&fake, &request(cursors)?, &config)?;
            assert!(outcome.error.is_none(), "{:?}", outcome.error);

            let excluded_pointer = format!("{}#1", stream_id(&excluded));
            let kept_pointer = format!("{}#1", stream_id(&kept));
            let streams: BTreeSet<String> = outcome.streams.keys().cloned().collect();
            let read_excluded = fake
                .get_log
                .borrow()
                .iter()
                .any(|url| url.contains(&format!("/repos/example-org/{name}/")));
            if exclude.is_empty() {
                assert_eq!(
                    pointers(&outcome),
                    BTreeSet::from([excluded_pointer, kept_pointer]),
                    "with no id excluded, {name} is read"
                );
                assert!(streams.contains(&stream_id(&excluded)), "{streams:?}");
                assert!(read_excluded, "{:?}", fake.get_log.borrow());
            } else {
                assert_eq!(
                    pointers(&outcome),
                    BTreeSet::from([kept_pointer]),
                    "{name}, excluded by its id, makes no record"
                );
                assert_eq!(streams, BTreeSet::from([stream_id(&kept)]));
                assert!(!read_excluded, "{:?}", fake.get_log.borrow());
            }
        }
    }
    Ok(())
}

/// `CTX_GITHUB_EXCLUDE_REPOSITORY_IDS` adds ids to the configuration's, so neither list takes a
/// repository off the other, and an entry that is no id is an error, so a run never reads what
/// a mistyped setting meant to exclude.
#[test]
fn the_environment_adds_repository_ids_to_the_configurations() -> Result<()> {
    let configured = GithubConfig {
        exclude_repository_ids: vec![EXCLUDED_ID],
        ..config(vec![1], 600, 2000)
    };
    assert_eq!(
        excluded_repositories(&configured, Some(" 9000000003 ,, 9000000001"))?,
        BTreeSet::from([EXCLUDED_ID, 9_000_000_003])
    );
    assert_eq!(
        excluded_repositories(&configured, None)?,
        BTreeSet::from([EXCLUDED_ID])
    );
    assert_eq!(
        excluded_repositories(&config(vec![1], 600, 2000), Some(""))?,
        BTreeSet::new()
    );
    let error = excluded_repositories(&configured, Some("9000000003,example-org/private"))
        .err()
        .context("a name in place of an id was taken")?;
    assert!(
        format!("{error:#}").contains("CTX_GITHUB_EXCLUDE_REPOSITORY_IDS"),
        "{error:#}"
    );
    Ok(())
}
