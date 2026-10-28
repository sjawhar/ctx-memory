use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use serde_json::Value;

static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> Result<PathBuf> {
    let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ctx-memory-resolve-{label}-test-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

fn remove_scratch(path: &Path) -> Result<()> {
    fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// Writes a tree whose one decision quotes `quote` from the echo source.
fn tree(root: &Path, quote: &str) -> Result<PathBuf> {
    tree_with(
        root,
        &format!("source: echo:C0EXAMPLE01/1\ntier: 1\nquote: {quote}\n"),
    )
}

/// Writes a tree whose one decision carries `provenance`: its source, tier and quote lines.
fn tree_with(root: &Path, provenance: &str) -> Result<PathBuf> {
    let tree = root.join("tree");
    let decisions = tree.join("decisions");
    fs::create_dir_all(&decisions).with_context(|| format!("creating {}", decisions.display()))?;
    fs::write(
        tree.join("ctx.toml"),
        format!(
            "version = 1\nid = \"example\"\ndata_dir = {:?}\n\n[[sources]]\nid = \"echo\"\nkind = \"echo\"\nscheme = \"echo\"\nstreams = [\"C0EXAMPLE01\"]\n",
            root.join("data").display().to_string()
        ),
    )
    .with_context(|| format!("writing {}", tree.join("ctx.toml").display()))?;
    fs::write(
        decisions.join("example.md"),
        format!(
            "---\ntype: decision\nstatus: open\nrevisit_by: 2026-12-01\n{provenance}---\nA decision body.\n"
        ),
    )
    .with_context(|| format!("writing {}", decisions.join("example.md").display()))?;
    Ok(tree)
}

/// Writes one completed window holding `text` for the cited pointer.
fn window(root: &Path, id: &str, generated_at: &str, text: &str) -> Result<()> {
    let dir = root.join("data/windows").join(id);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(
        dir.join("manifest.json"),
        format!(
            "{{\"version\":1,\"window\":\"{id}\",\"generated_at\":\"{generated_at}\",\"window_end\":\"{generated_at}\",\"sources\":{{}},\"warnings\":[],\"exit_status\":\"ok\"}}"
        ),
    )
    .with_context(|| format!("writing {}", dir.join("manifest.json").display()))?;
    fs::write(
        dir.join("records.jsonl"),
        format!(
            "{{\"source\":\"echo\",\"record\":{{\"pointer\":\"echo:C0EXAMPLE01/1\",\"stream\":\"C0EXAMPLE01\",\"ts\":\"2026-10-25T12:00:00Z\",\"author\":\"Alice\",\"text\":\"{text}\",\"raw\":null}}}}\n"
        ),
    )
    .with_context(|| format!("writing {}", dir.join("records.jsonl").display()))?;
    fs::write(dir.join("rendered.md"), "# window\n")
        .with_context(|| format!("writing {}", dir.join("rendered.md").display()))?;
    Ok(())
}

/// Declares the echo source verbatim, as a fetch's protocol hello does.
fn sources(root: &Path) -> Result<()> {
    let path = root.join("data/sources.json");
    fs::create_dir_all(root.join("data"))
        .with_context(|| format!("creating {}", root.display()))?;
    fs::write(
        &path,
        "{\"version\":1,\"sources\":{\"echo\":{\"verbatim\":true}}}",
    )
    .with_context(|| format!("writing {}", path.display()))
}

fn verify(tree: &Path, state: &Path) -> Result<Output> {
    let tree = tree.to_str().context("tree path is not valid UTF-8")?;
    Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--json", "--memory", tree, "cite", "verify"])
        .env("CTX_STATE_DIR", state)
        .env_remove("CTX_DATA_DIR")
        .output()
        .context("running ctx cite verify")
}

fn data(output: &Output) -> Result<Value> {
    let value: Value = serde_json::from_slice(&output.stdout).context("decoding verify output")?;
    value
        .get("data")
        .cloned()
        .context("verify output has a data envelope")
}

fn broken(output: &Output) -> Result<Vec<String>> {
    Ok(data(output)?
        .get("broken")
        .and_then(Value::as_array)
        .context("verify output lists broken citations")?
        .iter()
        .filter_map(|broken| broken.get("why").and_then(Value::as_str).map(str::to_owned))
        .collect())
}

/// The number of pointers whose quoted text the run actually checked.
fn checked(output: &Output) -> Result<u64> {
    data(output)?
        .get("checked")
        .and_then(Value::as_u64)
        .context("verify output counts checked citations")
}

/// The words the cited message holds; every quote the length tests make appears in it.
const SIGNED: &str = "ok, the vendor signed the renewal; vendors sign yearly, ok";

/// Runs `ctx cite verify` on a decision with `provenance`, citing a message that says
/// [`SIGNED`], and returns the run and its data.
fn verify_provenance(label: &str, provenance: &str) -> Result<(Output, Value)> {
    let root = scratch(label)?;
    let tree = tree_with(&root, provenance)?;
    sources(&root)?;
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        SIGNED,
    )?;
    let verified = verify(&tree, &root.join("state"));
    remove_scratch(&root)?;
    let verified = verified?;
    let data = data(&verified)?;
    Ok((verified, data))
}

/// Each `unchecked` finding as its pointer and reason.
fn unchecked(data: &Value) -> Vec<(String, String)> {
    data.get("unchecked")
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .map(|finding| {
                    let text = |key: &str| {
                        finding
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    };
                    (text("pointer"), text("why"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn quotes_unchecked(data: &Value) -> Option<u64> {
    data.get("quotes_unchecked").and_then(Value::as_u64)
}

/// Matching is a substring search, so `ok` is found inside `book` and a two-word fragment
/// inside most messages: a quote any of whose fragments holds fewer than 3 words and fewer
/// than 12 letters and digits proves nothing about its source, even where it appears. It
/// checks nothing, so it cannot bear out a tier 1 record, and the record is refused.
#[test]
fn a_quote_too_short_to_be_evidence_does_not_bear_out_a_tier_one_record() -> Result<()> {
    // One word; two words of 9 letters; two words of 11 letters; and a fragment of one word
    // after one long enough.
    for quote in [
        "ok",
        "the vendor",
        "vendors sign",
        "the vendor signed ... ok",
    ] {
        let (verified, data) = verify_provenance(
            "short-quote",
            &format!("source: echo:C0EXAMPLE01/1\ntier: 1\nquote: {quote}\n"),
        )?;

        assert_eq!(verified.status.code(), Some(1), "{quote:?}: {data}");
        assert_eq!(broken(&verified)?, Vec::<String>::new(), "{quote:?}");
        let findings = unchecked(&data);
        assert!(
            findings.len() == 1
                && findings.iter().all(|(pointer, why)| pointer == "tier 1"
                    && why.contains("at least 3 words or 12 letters and digits")),
            "{quote:?} bore out a tier 1 record: {findings:?}"
        );
        assert_eq!(quotes_unchecked(&data), Some(1), "{quote:?}: {data}");
    }
    Ok(())
}

/// At the minimum — three words, or two words of 12 letters, in every fragment — a quote that
/// appears in its source is checked, and bears out a tier 1 record.
#[test]
fn a_quote_of_three_words_or_twelve_letters_bears_out_a_tier_one_record() -> Result<()> {
    for quote in [
        "the vendor signed",
        "vendor signed",
        "the vendor signed ... sign yearly, ok",
    ] {
        let (verified, data) = verify_provenance(
            "long-quote",
            &format!("source: echo:C0EXAMPLE01/1\ntier: 1\nquote: {quote}\n"),
        )?;

        assert_eq!(verified.status.code(), Some(0), "{quote:?}: {data}");
        assert_eq!(unchecked(&data), Vec::new(), "{quote:?}");
        assert_eq!(quotes_unchecked(&data), Some(0), "{quote:?}: {data}");
    }
    Ok(())
}

/// A short quote is not refused for its length alone: at tier 3 it is a summary's, which need
/// not be checked, and the run counts it among the quotes nothing checked.
#[test]
fn a_short_quote_at_tier_three_is_unchecked_and_not_refused() -> Result<()> {
    let (verified, data) = verify_provenance(
        "short-quote-tier-three",
        "source: echo:C0EXAMPLE01/1\ntier: 3\nquote: ok\n",
    )?;

    assert_eq!(verified.status.code(), Some(0), "{data}");
    assert_eq!(unchecked(&data), Vec::new());
    assert_eq!(quotes_unchecked(&data), Some(1), "{data}");
    Ok(())
}

/// A short quote beside one that bears the record's tier out is counted unchecked, and the
/// record stands on the other.
#[test]
fn a_short_quote_beside_one_that_holds_is_unchecked_and_not_refused() -> Result<()> {
    let (verified, data) = verify_provenance(
        "short-quote-beside",
        "tier: 1\nquotes:\n  - quote: the vendor signed\n    source: echo:C0EXAMPLE01/1\n  - quote: ok\n    source: echo:C0EXAMPLE01/1\n",
    )?;

    assert_eq!(verified.status.code(), Some(0), "{data}");
    assert_eq!(unchecked(&data), Vec::new());
    assert_eq!(quotes_unchecked(&data), Some(1), "{data}");
    Ok(())
}

#[test]
fn cite_verify_accepts_an_older_version() -> Result<()> {
    let root = scratch("older-version")?;
    let tree = tree(&root, "the first text")?;
    sources(&root)?;
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        "the first text",
    )?;
    window(
        &root,
        "20261025T130000Z-0000",
        "2026-10-25T13:00:00Z",
        "the edited text",
    )?;

    let verified = verify(&tree, &root.join("state"))?;
    let store = root.join("state/example/store.sqlite3").is_file();
    let findings = broken(&verified)?;
    let checked = checked(&verified)?;
    remove_scratch(&root)?;

    assert!(
        store,
        "the store is rebuilt from the windows when it is missing"
    );
    assert_eq!(findings, Vec::<String>::new());
    assert_eq!(
        checked, 1,
        "the verbatim flag from sources.json makes the quote checkable"
    );
    assert_eq!(verified.status.code(), Some(0));
    Ok(())
}

#[test]
fn cite_verify_rejects_a_quote_no_version_holds() -> Result<()> {
    let root = scratch("no-version")?;
    let tree = tree(&root, "a text no version holds")?;
    sources(&root)?;
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        "the first text",
    )?;
    window(
        &root,
        "20261025T130000Z-0000",
        "2026-10-25T13:00:00Z",
        "the edited text",
    )?;

    let verified = verify(&tree, &root.join("state"))?;
    let findings = broken(&verified)?;
    remove_scratch(&root)?;

    assert_eq!(verified.status.code(), Some(1));
    assert_eq!(
        findings,
        vec!["quote does not appear in the resolved text".to_owned()]
    );
    Ok(())
}

/// A machine that does not fetch gets the source's verbatim declaration with the share, after
/// its own index was built. The next `ctx cite verify` must check the quote against the
/// declaration the share now carries, instead of skipping it as an undeclared source's.
#[test]
fn cite_verify_reads_a_declaration_that_reached_the_share_later() -> Result<()> {
    let root = scratch("late-declaration")?;
    let tree = tree(&root, "a text no version holds")?;
    let state = root.join("state");
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        "the first text",
    )?;
    // The index is built while nothing has declared the source.
    let first = verify(&tree, &state)?;
    let skipped = checked(&first)?;

    // The fetch job's declaration reaches the share, as a pull brings it.
    sources(&root)?;
    let second = verify(&tree, &state)?;
    let findings = broken(&second)?;
    let checked = checked(&second)?;
    remove_scratch(&root)?;

    // A quote nothing could check fails `cite verify`, as it fails the writer.
    assert_eq!(first.status.code(), Some(1));
    assert_eq!(skipped, 0, "an undeclared source's quote is not checked");
    assert_eq!(
        checked, 1,
        "the declaration on the share makes the quote checkable"
    );
    assert_eq!(
        findings,
        vec!["quote does not appear in the resolved text".to_owned()]
    );
    assert_eq!(second.status.code(), Some(1));
    Ok(())
}

/// Runs `ctx resolve` against the tree, with the echo source on `PATH` only when asked.
fn resolve(tree: &Path, state: &Path, pointer: &str, with_source: bool) -> Result<Output> {
    let tree = tree.to_str().context("tree path is not valid UTF-8")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_ctx"));
    command
        .args(["--json", "--memory", tree, "resolve", pointer])
        .env("CTX_STATE_DIR", state)
        .env_remove("CTX_DATA_DIR");
    if with_source {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let inherited = std::env::var("PATH").context("PATH is not set")?;
        command.env("PATH", format!("{}:{inherited}", fixtures.display()));
    } else {
        command.env("PATH", "/nonexistent");
    }
    command.output().context("running ctx resolve")
}

/// Every file under the shared data directory, with its bytes, so a read's effect is visible.
fn shared_files(data: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut found = Vec::new();
    let mut pending = vec![data.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes =
                    fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                found.push((path, bytes));
            }
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

fn resolved_text(value: &Value) -> Option<String> {
    value
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[test]
fn a_live_resolve_survives_a_rebuilt_store() -> Result<()> {
    let root = scratch("live-resolve")?;
    let tree = tree(&root, "the first text")?;
    let state = root.join("state");

    let live = resolve(&tree, &state, "echo:C0EXAMPLE01/9", true)?;
    fs::remove_file(state.join("example/store.sqlite3"))
        .context("removing the local index after the live resolve")?;
    let offline = resolve(&tree, &state, "echo:C0EXAMPLE01/9", false)?;
    let live_data = data(&live)?;
    let offline_data = data(&offline)?;
    remove_scratch(&root)?;

    assert_eq!(live.status.code(), Some(0));
    assert!(resolved_text(&live_data).is_some_and(|text| text.contains("echo echo:C0EXAMPLE01/9")));
    assert_eq!(
        offline.status.code(),
        Some(0),
        "the evidence a live resolve kept survives the index being rebuilt"
    );
    assert!(
        resolved_text(&offline_data).is_some_and(|text| text.contains("echo echo:C0EXAMPLE01/9")),
        "{offline_data:?}"
    );
    Ok(())
}

#[test]
fn a_read_never_writes_the_shared_data_directory() -> Result<()> {
    let root = scratch("read-only-share")?;
    let tree = tree(&root, "the first text")?;
    let state = root.join("state");
    // No sources.json: a read must neither create one nor change one.
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        "the first text",
    )?;
    let before = shared_files(&root.join("data"))?;

    let resolved = resolve(&tree, &state, "echo:C0EXAMPLE01/9", true)?;
    let verified = verify(&tree, &state)?;
    let after = shared_files(&root.join("data"))?;
    remove_scratch(&root)?;

    assert_eq!(resolved.status.code(), Some(0));
    assert_eq!(verified.status.code(), Some(0));
    assert_eq!(
        after, before,
        "ctx resolve and ctx cite verify leave the shared data directory untouched"
    );
    Ok(())
}

/// A message whose text would drive a terminal: it sets the window title (`OSC 0`, ended by
/// `BEL`), clears the screen (`CSI 2J`), returns the carriage and backs over a character, and
/// starts a sequence with the one-character C1 `CSI` (U+009B). Written with JSON escapes, as a
/// window stores it.
const HOSTILE: &str = "\\u001b]0;owned\\u0007the vendor \\u001b[2Jagreed\\r\\b to \\u009b31mrenew";

/// Whether `text` holds a control character other than a tab or a newline.
fn controls(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| character.is_control() && !matches!(character, '\t' | '\n'))
        .collect()
}

/// Window text comes from anyone who can post in a source. `ctx resolve` prints it to a
/// person's terminal or into a pipe an agent reads, and prints it without the controls that
/// would drive the terminal; `--json` keeps the text whole, every control escaped.
#[test]
fn resolve_prints_none_of_the_window_s_control_characters() -> Result<()> {
    let root = scratch("control-text")?;
    let tree = tree(&root, "the first text")?;
    let state = root.join("state");
    window(
        &root,
        "20261025T120000Z-0000",
        "2026-10-25T12:00:00Z",
        HOSTILE,
    )?;
    let tree_text = tree.to_str().context("tree path is not valid UTF-8")?;
    let text = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["--memory", tree_text, "resolve", "echo:C0EXAMPLE01/1"])
        .env("CTX_STATE_DIR", &state)
        .env_remove("CTX_DATA_DIR")
        .env("PATH", "/nonexistent")
        .output()
        .context("running ctx resolve");
    let json = resolve(&tree, &state, "echo:C0EXAMPLE01/1", false);
    remove_scratch(&root)?;
    let (text, json) = (text?, json?);

    let printed = String::from_utf8_lossy(&text.stdout).into_owned();
    assert_eq!(text.status.code(), Some(0), "{printed}");
    // The controls go; what followed each is plain text, which drives nothing. The carriage
    // return shows as a space, as the quote check reads it, and the backspace goes.
    assert!(
        printed.contains("]0;ownedthe vendor [2Jagreed  to 31mrenew"),
        "the text around the controls is missing: {printed:?}"
    );
    assert_eq!(controls(&printed), Vec::<char>::new(), "{printed:?}");

    let raw = String::from_utf8_lossy(&json.stdout).into_owned();
    assert_eq!(json.status.code(), Some(0), "{raw}");
    assert_eq!(
        controls(&raw),
        Vec::<char>::new(),
        "the JSON carries a raw control: {raw:?}"
    );
    let kept = resolved_text(&data(&json)?).unwrap_or_default();
    assert!(
        ['\u{1b}', '\u{7}', '\u{9b}']
            .iter()
            .all(|control| kept.contains(*control)),
        "the JSON text is not whole: {kept:?}"
    );
    Ok(())
}
