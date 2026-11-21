//! Repositories the deployment excludes by GitHub's id of each.

use super::fixtures::*;

/// GitHub's id of the repository the deployment excludes.
const EXCLUDED_ID: u64 = 9_000_000_001;
/// GitHub's id of the repository it reads.
const KEPT_ID: u64 = 9_000_000_002;

/// One installation holding two repositories, each with one issue: the one whose id is
/// `EXCLUDED_ID`, `owner/name` now, and `kept`. The pre-pass knows both as unchanged since long
/// before the window, so a settled stream of either is asked about and not read through REST.
fn installation(owner: &str, name: &str) -> (FakeGithub, Repo, Repo) {
    let base = api_base();
    let excluded = Repo {
        id: EXCLUDED_ID,
        ..repo(owner, name)
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
        fake.changed(subject, "2026-09-01T00:00:00Z");
    }
    (fake, excluded, kept)
}

/// Whether a GraphQL request the source sent asks about a repository called `owner/name`.
fn asked_about(fake: &FakeGithub, owner: &str, name: &str) -> bool {
    fake.graphql_log.borrow().iter().any(|request| {
        request
            .get("variables")
            .and_then(Value::as_object)
            .is_some_and(|variables| {
                (0..variables.len() / 2).any(|index| {
                    variables.get(&format!("o{index}")) == Some(&json!(owner))
                        && variables.get(&format!("n{index}")) == Some(&json!(name))
                })
            })
    })
}

/// A repository the deployment excludes by its id is never read, and stays excluded under the
/// name GitHub gives it after a rename or a transfer to another owner. A first run, with no id
/// excluded, reads both repositories and settles their streams. The second, with the id excluded,
/// lists no stream for the repository, asks the GraphQL pre-pass nothing of it, sends no REST
/// request about it and keeps no settled state for it, though a second installation's refused
/// listing keeps the run from forgetting the streams it did not plan, so none of its issues
/// becomes a record. Control: a second run with no id excluded asks the pre-pass about it and
/// lists its stream.
#[test]
fn a_repository_excluded_by_id_is_never_read_under_any_name() -> Result<()> {
    for (owner, name) in [
        ("example-org", "private"),
        ("example-org", "private-renamed"),
        ("other-org", "private"),
    ] {
        for exclude in [vec![EXCLUDED_ID], Vec::new()] {
            let (mut fake, excluded, kept) = installation(owner, name);
            fake.throttled = BTreeSet::from([2]);
            let cursors = BTreeMap::from([
                (stream_id(&excluded), CURSOR.to_owned()),
                (stream_id(&kept), CURSOR.to_owned()),
            ]);
            let first_request = request(cursors)?;
            let first = run(&fake, &first_request, &config(vec![1, 2], 600, 2000))?;
            assert!(first.error.is_none(), "{:?}", first.error);
            assert!(
                pointers(&first).contains(&format!("{}#1", stream_id(&excluded))),
                "the first run, with no id excluded, reads {owner}/{name}"
            );
            let rest_before = fake.get_log.borrow().len();

            let second_config = GithubConfig {
                exclude_repository_ids: exclude.clone(),
                ..config(vec![1, 2], 600, 2000)
            };
            let second = run(
                &fake,
                &next_request(&first_request, &first)?,
                &second_config,
            )?;
            assert!(second.error.is_none(), "{:?}", second.error);
            let streams: BTreeSet<String> = second.streams.keys().cloned().collect();
            let read_by_rest = fake
                .get_log
                .borrow()
                .iter()
                .skip(rest_before)
                .any(|url| url.contains(&format!("/repos/{owner}/{name}/")));
            let settled = std::fs::read_to_string(first_request.state_dir.join(STATE_FILE))?;
            let case = format!("{owner}/{name}, excluded {}", !exclude.is_empty());
            if exclude.is_empty() {
                assert!(
                    asked_about(&fake, owner, name),
                    "{case}: the pre-pass is reached"
                );
                assert!(
                    streams.contains(&stream_id(&excluded)),
                    "{case}: {streams:?}"
                );
            } else {
                assert_eq!(streams, BTreeSet::from([stream_id(&kept)]), "{case}");
                assert!(
                    !asked_about(&fake, owner, name),
                    "{case}: {:?}",
                    fake.graphql_log
                );
                assert!(!read_by_rest, "{case}: {:?}", fake.get_log.borrow());
                assert!(
                    pointers(&second).is_empty(),
                    "{case}: {:?}",
                    pointers(&second)
                );
                assert!(
                    !settled.contains(&stream_id(&excluded)),
                    "{case}: the state names it: {settled}"
                );
            }
        }
    }
    Ok(())
}

/// A repository excluded by id is logged by its stream at each run, and a warning names it when
/// its current List-ID, which follows its name, is not one `CTX_GMAIL_EXCLUDE_LIST_IDS` names:
/// after a rename its notification mail would be stored. Control: with its current List-ID
/// named, in any case, no warning names it; the drop is still logged.
#[test]
fn a_repository_excluded_by_id_under_a_new_name_is_warned_of() -> Result<()> {
    for (declared, warned) in [
        ("private.example-org.github.com", true),
        (
            "Private-Renamed.Example-Org.github.com,other.example-org.github.com",
            false,
        ),
    ] {
        let (fake, excluded, kept) = installation("example-org", "private-renamed");
        let cursors = BTreeMap::from([
            (stream_id(&excluded), CURSOR.to_owned()),
            (stream_id(&kept), CURSOR.to_owned()),
        ]);
        let config = GithubConfig {
            exclude_repository_ids: vec![EXCLUDED_ID],
            ..config(vec![1], 600, 2000)
        };
        crate::source::content_net::tests::take_logged();
        let outcome = run_declaring(&fake, &request(cursors)?, &config, Some(declared))?;
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let named = outcome
            .warnings
            .iter()
            .filter(|warning| warning.message.contains("example-org/private-renamed"))
            .collect::<Vec<_>>();
        assert_eq!(named.len(), usize::from(warned), "{declared}: {named:?}");
        assert!(
            named.iter().all(|warning| warning
                .message
                .contains("private-renamed.example-org.github.com")),
            "{named:?}"
        );
        assert_eq!(
            crate::source::content_net::tests::take_logged(),
            [format!(
                r#"{{"event":"withheld","reason":"private-repository-id","pointer":"{}"}}"#,
                stream_id(&excluded)
            )]
        );
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
