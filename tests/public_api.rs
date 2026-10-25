use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use ctx_memory::{config::TreeConfig, open::open_items, tree::Tree};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-{label}-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_test_root(root: &Path) -> Result<()> {
    std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))
}

#[test]
fn preserves_unknown_frontmatter_and_reports_open_items() -> Result<()> {
    let root = test_root("public-api")?;
    let decisions = root.join("decisions");
    std::fs::create_dir(&decisions).with_context(|| format!("creating {}", decisions.display()))?;
    std::fs::write(
        decisions.join("example.md"),
        "---\ntype: decision\nstatus: open\nrevisit_by: 2026-01-01\nquote: a quote here\nops_channel: C0EXAMPLE01\ntranscript_swept: 2026-01-01\n---\nA decision body.\n",
    )
    .with_context(|| format!("writing {}", decisions.display()))?;

    let tree = Tree::load(&root, &TreeConfig::default())?;
    remove_test_root(&root)?;

    let doc = tree
        .docs
        .first()
        .context("the decision document was not loaded")?;
    assert_eq!(doc.front.extra_str("ops_channel"), Some("C0EXAMPLE01"));
    assert_eq!(doc.front.extra_str("transcript_swept"), Some("2026-01-01"));
    assert_eq!(
        doc.front.deadline(),
        Some(NaiveDate::from_ymd_opt(2026, 1, 1).context("valid fixed test date")?)
    );

    let items = open_items(&tree);
    assert_eq!(items.len(), 1);
    let item = items.first().context("an open item was not returned")?;
    assert_eq!(item.path, "decisions/example.md");
    assert_eq!(item.kind, "decision");
    assert!(item.days_overdue.is_some());
    Ok(())
}

#[test]
fn folder_aliases_require_an_explicit_file_name() -> Result<()> {
    let root = test_root("folder-alias")?;
    let decisions = root.join("decisions");
    std::fs::create_dir(&decisions).with_context(|| format!("creating {}", decisions.display()))?;
    std::fs::write(decisions.join("index.md"), "# Decisions\n")
        .with_context(|| format!("writing {}", decisions.display()))?;

    let tree = Tree::load(&root, &TreeConfig::default())?;
    remove_test_root(&root)?;
    let doc = tree
        .docs
        .first()
        .context("the index document was not loaded")?;

    assert_eq!(doc.link_names(&[]), vec!["decisions/index"]);
    assert_eq!(
        doc.link_names(&["index.md".to_owned()]),
        vec!["decisions/index", "decisions"]
    );
    Ok(())
}
