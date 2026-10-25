use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use ctx_memory::{config::TreeConfig, sweep, tree::Tree};

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn test_root(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-sweep-{label}-{}-{suffix}",
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

fn findings(root: &Path) -> Result<sweep::Findings> {
    let tree = Tree::load(root, &TreeConfig::default())?;
    sweep::run(&tree)
}

#[test]
fn md_suffix_relative_and_case_variants_resolve() -> Result<()> {
    let root = test_root("link-variants")?;
    write(&root, "targets/Foo.md", "# Target\n")?;
    write(
        &root,
        "sub/x.md",
        "[[targets/Foo.md]]\n[[../targets/Foo]]\n[[targets/foo]]\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert!(result?.gaps.is_empty());
    Ok(())
}

#[test]
fn unique_basename_resolves_and_shared_basename_is_ambiguous() -> Result<()> {
    let root = test_root("basename-links")?;
    write(&root, "a/note.md", "# Note\n")?;
    write(&root, "reference.md", "[[note]]\n")?;
    let unique = findings(&root)?;
    assert!(unique.gaps.is_empty());

    write(&root, "b/note.md", "# Another note\n")?;
    let duplicate = findings(&root);
    remove_test_root(&root)?;

    let duplicate = duplicate?;
    assert!(duplicate.gaps.is_empty());
    let ambiguity = duplicate
        .ambiguous
        .first()
        .context("shared basename was not reported as ambiguous")?;
    assert_eq!(ambiguity.target, "note");
    assert_eq!(ambiguity.from, "reference.md");
    assert_eq!(ambiguity.candidates, vec!["a/note.md", "b/note.md"]);
    Ok(())
}

#[test]
fn self_link_does_not_save_an_orphan() -> Result<()> {
    let root = test_root("self-link")?;
    write(&root, "self.md", "[[self]]\n")?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.orphans, vec!["self.md"]);
    Ok(())
}

#[test]
fn gitignored_and_hidden_files_are_not_records() -> Result<()> {
    let root = test_root("ignored-files")?;
    write(&root, ".gitignore", "build/\n")?;
    write(&root, ".obsidian/hidden.md", "# Hidden\n")?;
    write(&root, "build/generated.md", "# Generated\n")?;
    write(&root, "visible.md", "# Visible\n")?;
    let tree = Tree::load(&root, &TreeConfig::default());
    remove_test_root(&root)?;

    let tree = tree?;
    assert_eq!(tree.docs.len(), 1);
    let document = tree
        .docs
        .first()
        .context("visible Markdown document was not loaded")?;
    assert_eq!(document.rel, Path::new("visible.md"));
    Ok(())
}

#[test]
fn crlf_records_are_checked() -> Result<()> {
    let root = test_root("crlf-record")?;
    write(
        &root,
        "decision.md",
        "---\r\ntype: decision\r\ntier: external\r\n---\r\nNo quote.\r\n",
    )?;
    let result = findings(&root);
    remove_test_root(&root)?;

    assert_eq!(result?.unquoted, vec!["decision.md"]);
    Ok(())
}

#[test]
fn crlf_paragraphs_do_not_hide_an_unsourced_negative() -> Result<()> {
    let crlf_root = test_root("negative-crlf")?;
    write(
        &crlf_root,
        "claim.md",
        "They were never told.\r\n\r\nThis paragraph mentions email but not a source.\r\n",
    )?;
    let crlf = findings(&crlf_root);
    remove_test_root(&crlf_root)?;

    let lf_root = test_root("negative-lf")?;
    write(
        &lf_root,
        "claim.md",
        "They were never told.\n\nThis paragraph mentions email but not a source.\n",
    )?;
    let lf = findings(&lf_root);
    remove_test_root(&lf_root)?;

    assert_eq!(crlf?.negatives, lf?.negatives);
    Ok(())
}
