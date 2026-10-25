use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
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
