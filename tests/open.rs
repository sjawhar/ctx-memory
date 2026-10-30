use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::{Days, NaiveDate};
use ctx_memory::{
    config::TreeConfig,
    open::{open_items_with_filters, OpenFilter},
    tree::Tree,
};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-open-{label}-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_test_root(root: &Path) -> Result<()> {
    std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

fn write(root: &Path, relative: &str, content: &str) -> Result<()> {
    let path = root.join(relative);
    let parent = path
        .parent()
        .with_context(|| format!("finding parent for {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_ctx"))
}

fn ctx(tree: &Path, arguments: &[&str]) -> Result<Value> {
    let tree = tree.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "--json", "open"])
        .args(arguments)
        .output()
        .context("running ctx")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn fixture_tree() -> Result<PathBuf> {
    let root = test_root("filters")?;
    write(&root, "ctx.toml", "version = 1\n")?;
    write(
        &root,
        "people/bob.md",
        "---\nname: Bob Example\nrole: reviewer\n---\n# Profile\n",
    )?;
    write(
        &root,
        "decisions/review.md",
        "---\ntype: decision\nstatus: open\nowner: \"[[people/bob]]\"\n---\n[[people/bob]] is responsible for the review.\n",
    )?;
    Ok(root)
}

fn item_count(response: &Value) -> Option<usize> {
    response
        .get("data")
        .and_then(|data| data.get("items"))
        .and_then(Value::as_array)
        .map(Vec::len)
}

/// The paths of the open items a response lists, in its order.
fn paths(response: &Value) -> Result<Vec<String>> {
    Ok(response
        .get("data")
        .and_then(|data| data.get("items"))
        .and_then(Value::as_array)
        .context("open items are missing")?
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

/// A tree of open questions, some waiting on named people and some on nobody.
fn waiting_tree() -> Result<PathBuf> {
    let root = test_root("waiting")?;
    write(&root, "ctx.toml", "version = 1\n")?;
    write(
        &root,
        "people/ada.md",
        "---\nname: Ada Lovelace\n---\n# Profile\n",
    )?;
    write(
        &root,
        "questions/budget.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-01\nowner: Grace Hopper\n\
         waiting_on: [Ada Lovelace, Grace Hopper]\n\
         tracked_by: [\"https://example.com/ask/1\"]\n---\nWho approves the budget?\n",
    )?;
    write(
        &root,
        "questions/venue.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-02\nowner: Grace Hopper\n\
         waiting_on: [Grace Hopper]\n---\nWhich venue?\n",
    )?;
    write(
        &root,
        "questions/linked.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-03\nowner: unassigned\n\
         waiting_on: [\"[[people/ada]]\"]\n---\nWho signs?\n",
    )?;
    write(
        &root,
        "questions/nobody.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-04\nowner: Grace Hopper\n---\n\
         Nobody was asked.\n",
    )?;
    write(
        &root,
        "questions/answered.md",
        "---\ntype: question\nstatus: answered\nwaiting_on: [Ada Lovelace]\n---\nSettled.\n",
    )?;
    Ok(root)
}

/// `--waiting-on` keeps the open items that list one of the names, compared as `--owner`
/// compares an owner: the whole name, in any case. A record that waits on nobody, a closed
/// one, a part of a name and a person no record lists match nothing.
#[test]
fn waiting_on_filter_matches_a_listed_person_by_whole_name() -> Result<()> {
    let root = waiting_tree()?;
    let ada = ctx(&root, &["--waiting-on", "ada lovelace"]);
    let grace = ctx(&root, &["--waiting-on", "GRACE HOPPER"]);
    let either = ctx(&root, &["--waiting-on", "Ada Lovelace,Grace Hopper"]);
    let part = ctx(&root, &["--waiting-on", "Lovelace"]);
    let unknown = ctx(&root, &["--waiting-on", "Alan Turing"]);
    remove_test_root(&root)?;

    assert_eq!(paths(&ada?)?, ["questions/budget.md"]);
    assert_eq!(
        paths(&grace?)?,
        ["questions/budget.md", "questions/venue.md"]
    );
    assert_eq!(
        paths(&either?)?,
        ["questions/budget.md", "questions/venue.md"]
    );
    assert_eq!(paths(&part?)?, Vec::<String>::new());
    assert_eq!(paths(&unknown?)?, Vec::<String>::new());
    Ok(())
}

/// A `waiting_on` entry that links a person record matches that record's stem or link target,
/// as an owner link does.
#[test]
fn waiting_on_filter_follows_person_links() -> Result<()> {
    let root = waiting_tree()?;
    let stem = ctx(&root, &["--waiting-on", "ada"]);
    let target = ctx(&root, &["--waiting-on", "people/ada"]);
    remove_test_root(&root)?;

    assert_eq!(paths(&stem?)?, ["questions/linked.md"]);
    assert_eq!(paths(&target?)?, ["questions/linked.md"]);
    Ok(())
}

/// `--waiting-on` and `--owner` together keep only the items that pass both.
#[test]
fn waiting_on_and_owner_filters_must_both_match() -> Result<()> {
    let root = waiting_tree()?;
    let both = ctx(
        &root,
        &["--owner", "grace hopper", "--waiting-on", "ada lovelace"],
    );
    let owner_only = ctx(&root, &["--owner", "grace hopper"]);
    let disjoint = ctx(
        &root,
        &["--owner", "unassigned", "--waiting-on", "grace hopper"],
    );
    remove_test_root(&root)?;

    assert_eq!(paths(&both?)?, ["questions/budget.md"]);
    assert_eq!(
        paths(&owner_only?)?,
        [
            "questions/budget.md",
            "questions/venue.md",
            "questions/nobody.md"
        ]
    );
    assert_eq!(paths(&disjoint?)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn waiting_on_linked_requires_a_linked_filter() -> Result<()> {
    let root = fixture_tree()?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "--json", "open", "--waiting-on", "linked"])
        .output()
        .context("running ctx")?;
    remove_test_root(&root)?;

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("--waiting-on linked requires --linked"),
        "{value}"
    );
    Ok(())
}

/// Every JSON item carries `waiting_on` and `tracked_by`, as `[]` when the record has none, so
/// a reader can select on them without testing for the key.
#[test]
fn open_json_always_carries_waiting_on_and_tracked_by() -> Result<()> {
    let root = waiting_tree()?;
    let response = ctx(&root, &[]);
    remove_test_root(&root)?;
    let response = response?;
    let items = response
        .pointer("/data/items")
        .and_then(Value::as_array)
        .context("open items are missing")?;
    let item = |path: &str| -> Result<&Value> {
        items
            .iter()
            .find(|item| item.get("path").and_then(Value::as_str) == Some(path))
            .with_context(|| format!("{path} is not listed: {response}"))
    };

    let budget = item("questions/budget.md")?;
    assert_eq!(
        budget.get("waiting_on"),
        Some(&serde_json::json!(["Ada Lovelace", "Grace Hopper"]))
    );
    assert_eq!(
        budget.get("tracked_by"),
        Some(&serde_json::json!(["https://example.com/ask/1"]))
    );
    let nobody = item("questions/nobody.md")?;
    assert_eq!(nobody.get("waiting_on"), Some(&serde_json::json!([])));
    assert_eq!(nobody.get("tracked_by"), Some(&serde_json::json!([])));
    Ok(())
}

/// The text listing names who an item waits on and what tracks it, and says nothing of either
/// for an item that has neither.
#[test]
fn open_text_names_who_an_item_waits_on_and_what_tracks_it() -> Result<()> {
    let root = waiting_tree()?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "open", "--owner", "grace hopper"])
        .output()
        .context("running ctx open");
    remove_test_root(&root)?;
    let output = output?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");

    assert!(
        stdout.contains("waiting on Ada Lovelace, Grace Hopper"),
        "{stdout}"
    );
    assert!(
        stdout.contains("tracked by https://example.com/ask/1"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("waiting on").count(), 2, "{stdout}");
    assert_eq!(stdout.matches("tracked by").count(), 1, "{stdout}");
    Ok(())
}

/// `--overdue` keeps the items whose deadline has passed and drops those due later and those
/// with no date.
#[test]
fn overdue_lists_only_items_past_their_deadline() -> Result<()> {
    let root = test_root("overdue")?;
    write(&root, "ctx.toml", "version = 1\n")?;
    write(
        &root,
        "questions/late.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2000-01-01\nwaiting_on: [Ada Lovelace]\n---\nLate.\n",
    )?;
    write(
        &root,
        "commitments/late.md",
        "---\ntype: commitment\nstatus: open\ndue: 2001-01-01\n---\nAlso late.\n",
    )?;
    write(
        &root,
        "questions/later.md",
        "---\ntype: question\nstatus: open\nrevisit_by: 2999-01-01\nwaiting_on: [Ada Lovelace]\n---\nLater.\n",
    )?;
    write(
        &root,
        "questions/undated.md",
        "---\ntype: question\nstatus: open\n---\nNo date.\n",
    )?;
    let overdue = ctx(&root, &["--overdue"]);
    let waiting_and_overdue = ctx(&root, &["--overdue", "--waiting-on", "Ada Lovelace"]);
    remove_test_root(&root)?;

    assert_eq!(
        paths(&overdue?)?,
        ["questions/late.md", "commitments/late.md"]
    );
    assert_eq!(paths(&waiting_and_overdue?)?, ["questions/late.md"]);
    Ok(())
}

/// An item is overdue the day after its deadline, not on the day itself.
#[test]
fn an_item_due_today_is_not_overdue() -> Result<()> {
    let today = NaiveDate::from_ymd_opt(2026, 10, 30).context("fixed date is valid")?;
    let yesterday = today
        .checked_sub_days(Days::new(1))
        .context("yesterday is a date")?;
    let root = test_root("overdue-boundary")?;
    write(
        &root,
        "questions/today.md",
        &format!("---\ntype: question\nstatus: open\nrevisit_by: {today}\n---\nToday.\n"),
    )?;
    write(
        &root,
        "questions/yesterday.md",
        &format!("---\ntype: question\nstatus: open\nrevisit_by: {yesterday}\n---\nYesterday.\n"),
    )?;
    let tree = Tree::load(&root, &TreeConfig::default());
    remove_test_root(&root)?;
    let tree = tree?;

    let overdue = open_items_with_filters(
        &tree,
        &OpenFilter {
            overdue: true,
            ..OpenFilter::default()
        },
        today,
    )?;
    let listed: Vec<(&str, Option<i64>)> = overdue
        .iter()
        .map(|item| (item.path.as_str(), item.days_overdue))
        .collect();
    assert_eq!(listed, [("questions/yesterday.md", Some(1))]);

    let all = open_items_with_filters(&tree, &OpenFilter::default(), today)?;
    assert_eq!(all.len(), 2);
    Ok(())
}

#[test]
fn linked_filter_matches_on_any_frontmatter_field() -> Result<()> {
    let root = fixture_tree()?;

    let matching = ctx(
        &root,
        &[
            "--linked",
            "role=other,reviewer",
            "--linked",
            "name=Bob Example",
        ],
    );
    let excluded = ctx(&root, &["--linked", "role=other"]);
    remove_test_root(&root)?;

    assert_eq!(item_count(&matching?), Some(1));
    assert_eq!(item_count(&excluded?), Some(0));
    Ok(())
}

#[test]
fn ambiguous_linked_targets_do_not_match_filters() -> Result<()> {
    let root = fixture_tree()?;
    write(
        &root,
        "people/alternate/bob.md",
        "---\nname: Other Bob\nrole: reviewer\n---\n# Profile\n",
    )?;
    write(
        &root,
        "decisions/ambiguous.md",
        "---\ntype: decision\nstatus: open\n---\n[[bob]] is responsible for the ambiguous review.\n",
    )?;

    let response = ctx(&root, &["--linked", "role=reviewer"]);
    remove_test_root(&root)?;
    let response = response?;
    let paths = response
        .get("data")
        .and_then(|data| data.get("items"))
        .and_then(Value::as_array)
        .context("open items are missing")?
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();

    assert_eq!(paths, ["decisions/review.md"]);
    Ok(())
}

#[test]
fn owner_linked_requires_a_linked_filter() -> Result<()> {
    let root = fixture_tree()?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "--json", "open", "--owner", "linked"])
        .output()
        .context("running ctx")?;
    remove_test_root(&root)?;

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("--owner linked requires --linked")));
    Ok(())
}

#[test]
fn owner_filter_matches_the_resolved_target_stem_or_name() -> Result<()> {
    let root = fixture_tree()?;

    let stem_match = ctx(&root, &["--owner", "people/bob"]);
    let name_match = ctx(&root, &["--owner", "bob"]);
    let excluded = ctx(&root, &["--owner", "carol"]);
    remove_test_root(&root)?;

    assert_eq!(item_count(&stem_match?), Some(1));
    assert_eq!(item_count(&name_match?), Some(1));
    assert_eq!(item_count(&excluded?), Some(0));
    Ok(())
}

#[test]
fn owner_filter_matches_literal_owner_in_starter_tree() -> Result<()> {
    let starter = Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
    let response = ctx(&starter, &["--owner", "alice"])?;
    let paths = response
        .get("data")
        .and_then(|data| data.get("items"))
        .and_then(Value::as_array)
        .context("starter open items are missing")?
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();

    assert_eq!(paths, ["decisions/record-provenance.md"]);
    Ok(())
}

#[test]
fn owner_linked_matches_the_filtered_target() -> Result<()> {
    let root = fixture_tree()?;

    let response = ctx(&root, &["--linked", "role=reviewer", "--owner", "linked"]);
    remove_test_root(&root)?;
    let response = response?;

    assert_eq!(item_count(&response), Some(1));
    assert_eq!(
        response
            .get("data")
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("unreviewed")),
        Some(&Value::Null)
    );
    Ok(())
}

/// An open item's title is the record's text, which can quote anyone. `ctx open` prints it
/// without its control characters (`CSI 2J`, `BEL`, the C1 `CSI`); what followed each stays.
#[test]
fn piped_open_output_carries_none_of_the_tree_s_controls() -> Result<()> {
    let root = test_root("no-control")?;
    write(&root, "ctx.toml", "version = 1\n")?;
    write(
        &root,
        "decisions/renew.md",
        "---\ntype: decision\nstatus: open\ntitle: \"Renew \\e[2J\\a the \\u009b31mcontract\"\n---\nOpen.\n",
    )?;
    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "open"])
        .output()
        .context("running ctx open");
    remove_test_root(&root)?;
    let output = output?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Renew [2J the 31mcontract"),
        "the open item is missing: {stdout:?}"
    );
    let controls: Vec<char> = stdout
        .chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect();
    assert_eq!(controls, Vec::<char>::new(), "{stdout:?}");
    Ok(())
}
