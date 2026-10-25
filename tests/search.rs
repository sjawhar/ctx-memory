use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-search-{label}-{}-{suffix}",
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
        .args(["--memory", tree, "--json"])
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

fn write_config(root: &Path, vitals: &str) -> Result<()> {
    write(
        root,
        "ctx.toml",
        &format!("version = 1\n[dossier]\nvitals = [{vitals}]\n"),
    )
}

#[test]
fn dossier_finds_documents_by_frontmatter_name() -> Result<()> {
    let root = test_root("frontmatter-name")?;
    write_config(&root, "")?;
    write(
        &root,
        "people/alice.md",
        "---\nname: Alice Example\n---\n# Profile\n",
    )?;
    write(
        &root,
        "meetings/review.md",
        "The group asked Alice Example to review the proposal. [[people/alice]]\n",
    )?;

    let response = ctx(&root, &["dossier", "Alice Example"]);
    remove_test_root(&root)?;
    let response = response?;
    let data = response.get("data").context("dossier data is missing")?;

    assert_eq!(
        data.get("page").and_then(|page| page.get("path")),
        Some(&serde_json::json!("people/alice.md"))
    );
    let hit_paths = data
        .get("hits")
        .and_then(Value::as_array)
        .context("dossier hits are missing")?
        .iter()
        .filter_map(|hit| hit.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(hit_paths.contains(&"people/alice.md"));
    assert!(hit_paths.contains(&"meetings/review.md"));
    Ok(())
}

#[test]
fn empty_dossier_is_an_error() -> Result<()> {
    let root = test_root("empty")?;
    write_config(&root, "")?;

    let tree = root.to_str().context("test tree path is not valid UTF-8")?;
    let output = binary()
        .args(["--memory", tree, "dossier", "  "])
        .output()
        .context("running ctx")?;
    remove_test_root(&root)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("dossier entity must not be empty"));
    Ok(())
}

#[test]
fn ambiguous_entity_lists_candidates_instead_of_choosing() -> Result<()> {
    let root = test_root("ambiguous")?;
    write_config(&root, "")?;
    write(
        &root,
        "people/alice-a.md",
        "---\nname: Alice Anderson\n---\n# Profile\n",
    )?;
    write(
        &root,
        "people/alice-b.md",
        "---\nname: Alice Baker\n---\n# Profile\n",
    )?;

    let response = ctx(&root, &["dossier", "alice"]);
    remove_test_root(&root)?;
    let response = response?;
    let data = response.get("data").context("dossier data is missing")?;

    assert_eq!(data.get("page"), Some(&Value::Null));
    assert_eq!(
        data.get("candidates"),
        Some(&serde_json::json!([
            "people/alice-a.md",
            "people/alice-b.md"
        ]))
    );
    Ok(())
}

#[test]
fn dossier_json_carries_the_page_vitals() -> Result<()> {
    let root = test_root("vitals")?;
    write_config(&root, "\"status\"")?;
    write(
        &root,
        "people/alice.md",
        "---\nname: Alice Example\nstatus: active\n---\n# Profile\n",
    )?;

    let response = ctx(&root, &["dossier", "Alice Example"]);
    remove_test_root(&root)?;
    let response = response?;

    assert_eq!(
        response
            .get("data")
            .and_then(|data| data.get("page"))
            .and_then(|page| page.get("vitals"))
            .and_then(|vitals| vitals.get("status")),
        Some(&serde_json::json!("active"))
    );
    Ok(())
}
