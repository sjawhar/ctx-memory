//! `/v1/records/put-batch`: records written together, checked against the tree with every one
//! of them in it, and committed in one commit or not at all.

use anyhow::Result;
use serde_json::{json, Value};

use crate::fixtures::{good_record, put_body};
use crate::support::{fetch_window, fixture_clone, git, remote_of, remove_fixture, Server};
use crate::validation::finding_kinds;

const BATCH: &str = "/v1/records/put-batch";

/// A good record under its own title that links `target`.
fn linking(title: &str, target: &str) -> String {
    good_record().replace("Keep the echo cadence", title) + &format!("\nSee [[{target}]].\n")
}

fn batch_body(records: &[(&str, &str)], session: &str) -> Value {
    let records: Vec<Value> = records
        .iter()
        .map(|(path, content)| json!({ "path": path, "content": content }))
        .collect();
    json!({ "records": records, "session": session })
}

/// Two new records that link each other can never be written one at a time: whichever goes
/// first links a file that does not exist yet, and the server refuses it. Written as one
/// batch, each link is checked against the tree with both records in it, and both land in one
/// commit.
#[test]
fn a_pair_that_links_each_other_is_refused_one_at_a_time_and_lands_as_a_batch() -> Result<()> {
    let tree = fixture_clone("write-batch-pair")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let start = git(&bare, &["rev-parse", "main"])?;
    let first = linking("The first of a pair", "decisions/pair-b");
    let second = linking("The second of a pair", "decisions/pair-a");
    let server = Server::start(&tree, &[])?;
    let alone_first = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/pair-a.md", &first, "session-pair"),
    )?;
    let alone_second = server.post_as(
        "ada@example.com",
        "/v1/records/put",
        &put_body("decisions/pair-b.md", &second, "session-pair"),
    )?;
    let after_singles = git(&bare, &["rev-parse", "main"])?;
    let together = server.post_as(
        "ada@example.com",
        BATCH,
        &batch_body(
            &[
                ("decisions/pair-a.md", &first),
                ("decisions/pair-b.md", &second),
            ],
            "session-pair",
        ),
    )?;
    drop(server);
    let head = git(&bare, &["rev-parse", "main"])?;
    let parent = git(&bare, &["rev-parse", "main~1"])?;
    let message = git(&bare, &["log", "-1", "--format=%B", "main"])?;
    let author = git(&bare, &["log", "-1", "--format=%an <%ae>", "main"])?;
    let changed = git(&bare, &["show", "--name-only", "--format=", "main"])?;
    remove_fixture(&tree)?;

    for alone in [&alone_first, &alone_second] {
        assert_eq!(alone.status, 422, "unexpected body: {}", alone.body);
        assert_eq!(finding_kinds(alone), vec!["gap"], "{}", alone.body);
    }
    assert_eq!(after_singles, start, "a refused single write moved main");
    assert_eq!(together.status, 200, "unexpected body: {}", together.body);
    assert_eq!(together.text("commit")?, head);
    assert_eq!(parent, start, "the batch is not one commit on top of main");
    assert_eq!(
        changed.lines().collect::<Vec<_>>(),
        vec!["decisions/pair-a.md", "decisions/pair-b.md"]
    );
    // A commit-msg hook on the machine that runs the tests may add trailers of its own.
    assert!(
        message.starts_with(
            "put 2 records\n\ndecisions/pair-a.md\ndecisions/pair-b.md\n\nSession: session-pair"
        ),
        "{message}"
    );
    assert_eq!(author, "ada <ada@example.com>");
    Ok(())
}

/// One record a check refuses refuses the whole batch: nothing is committed or left on disk,
/// and each finding names the record it is about.
#[test]
fn a_batch_with_one_bad_record_writes_nothing() -> Result<()> {
    let tree = fixture_clone("write-batch-bad")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let start = git(&bare, &["rev-parse", "main"])?;
    let server = Server::start(&tree, &[])?;
    let reply = server.post_as(
        "ada@example.com",
        BATCH,
        &batch_body(
            &[
                ("decisions/good.md", &good_record()),
                (
                    "decisions/bad.md",
                    &linking("A record with a dangling link", "decisions/nowhere"),
                ),
            ],
            "session-bad",
        ),
    )?;
    drop(server);
    let head = git(&bare, &["rev-parse", "main"])?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    assert_eq!(reply.status, 422, "unexpected body: {}", reply.body);
    let findings = reply
        .field("findings")?
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(findings.len(), 1, "unexpected findings: {findings:?}");
    assert_eq!(
        findings
            .first()
            .and_then(|finding| finding.get("path"))
            .and_then(Value::as_str),
        Some("decisions/bad.md")
    );
    assert_eq!(finding_kinds(&reply), vec!["gap"]);
    assert_eq!(head, start, "a refused batch moved main");
    assert_eq!(status, "", "the refused batch left files behind: {status}");
    Ok(())
}

/// A batch names each record once, and at least one; a record path inside another record of
/// the same batch cannot be written either. The server refuses each before it touches the tree.
#[test]
fn a_batch_that_cannot_be_written_as_named_is_refused_before_the_tree_is_touched() -> Result<()> {
    let tree = fixture_clone("write-batch-shape")?;
    fetch_window(&tree)?;
    let bare = remote_of(&tree)?;
    let start = git(&bare, &["rev-parse", "main"])?;
    let record = good_record();
    let server = Server::start(&tree, &[])?;
    let twice = server.post_as(
        "ada@example.com",
        BATCH,
        &batch_body(
            &[
                ("decisions/twice.md", &record),
                ("decisions//twice.md", &record),
            ],
            "session-shape",
        ),
    )?;
    let empty = server.post_as("ada@example.com", BATCH, &batch_body(&[], "session-shape"))?;
    let nested = server.post_as(
        "ada@example.com",
        BATCH,
        &batch_body(
            &[
                ("decisions/outer.md", &record),
                ("decisions/outer.md/inner.md", &record),
            ],
            "session-shape",
        ),
    )?;
    drop(server);
    let head = git(&bare, &["rev-parse", "main"])?;
    let status = git(&tree, &["status", "--porcelain", "--untracked-files=all"])?;
    remove_fixture(&tree)?;

    assert_eq!(twice.status, 400, "unexpected body: {}", twice.body);
    assert!(
        twice
            .text("error")?
            .contains("decisions/twice.md is named twice"),
        "{}",
        twice.body
    );
    assert_eq!(empty.status, 400, "unexpected body: {}", empty.body);
    assert!(
        empty.text("error")?.contains("names no record"),
        "{}",
        empty.body
    );
    assert_eq!(nested.status, 422, "unexpected body: {}", nested.body);
    assert_eq!(
        finding_kinds(&nested),
        vec!["not-a-record"],
        "{}",
        nested.body
    );
    assert_eq!(head, start, "a refused batch moved main");
    assert_eq!(status, "", "a refused batch left files behind: {status}");
    Ok(())
}
