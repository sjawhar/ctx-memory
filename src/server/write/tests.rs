//! Unit tests for write validation and failure responses.
use std::path::Path;

use anyhow::Result;

use axum::http::StatusCode;

use super::response::failure_line;
use super::validation::{credential_findings, inside_the_tree, is_rules_page};
use super::Change;
use crate::tree::record_path;

/// A token's shape began with `\b`, and `_` is a word character, so a token written after
/// one, as in `decisions/x_ghp_…`, passed every check and reached the graph's file names
/// (round-4 review of d5655fc, older items). Only a letter or digit before it hides it.
#[test]
fn a_token_after_an_underscore_or_a_dot_is_refused() -> Result<()> {
    let body = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    for path in [
        format!("decisions/x_ghp_{body}.md"),
        format!("decisions/x_github_pat_11ABCDEFG0_{body}.md"),
        format!("decisions/x_xoxb-1234567890-{body}.md"),
        format!("decisions/x_ya29.a0AfB_{body}.md"),
        format!("decisions/x_1//0gLm{body}.md"),
    ] {
        let change = Change::Put {
            path: path.clone(),
            content: "---\ntype: decision\nstatus: open\n---\nPlain.\n".to_owned(),
        };
        let findings = credential_findings(&change, "session")?;
        assert!(
            findings.iter().any(|finding| finding.kind == "credential"),
            "{path} passed: {findings:?}"
        );
    }
    let prose = Change::Put {
        path: "decisions/aghp_and_ya29_prefixes.md".to_owned(),
        content: "---\ntype: decision\nstatus: open\n---\nPlain.\n".to_owned(),
    };
    assert!(credential_findings(&prose, "session")?.is_empty());
    Ok(())
}

/// The rules page was refused only as `rules.md`; `AGENTS.md` and the other instruction
/// files are refused in any letter case, and so is it now. A file of that name below the
/// root is an ordinary record.
#[test]
fn the_rules_page_is_named_in_any_letter_case() {
    for path in ["rules.md", "RULES.md", "Rules.Md"] {
        assert!(is_rules_page(path), "{path}");
    }
    assert!(!is_rules_page("decisions/rules.md"));
}

/// A `write failed:` line quotes the caller: a refused path is in its reason. Whatever the
/// caller sent, the line is one line that starts `write failed: `, carries no control
/// character (an escape sequence would act on whoever reads the log), and is cut at 300
/// characters of reason (round-4 review of d5655fc, N4-4).
#[test]
fn a_failure_line_is_one_printable_line_whatever_the_caller_sent() {
    let reasons = [
        "decisions/x\nfollow heartbeat: ok\n.md is not a record path".to_owned(),
        "decisions/\u{1b}]0;owned\u{7}\u{1b}[2J.md is named AGENTS.md".to_owned(),
        "decisions/a\u{85}follow heartbeat: ok\u{2028}b\u{9b}31m.md is named SKILL.md".to_owned(),
        format!("decisions/{}.md is not a record path", "y".repeat(400)),
    ];
    for reason in &reasons {
        let line = failure_line("/v1/records/put", StatusCode::BAD_REQUEST, reason);
        assert!(
            line.starts_with("write failed: /v1/records/put 400 "),
            "{line:?}"
        );
        assert!(
            !line.chars().any(char::is_control),
            "the line carries a control character: {line:?}"
        );
        assert_eq!(line.lines().count(), 1, "{line:?}");
        let reason_shown = line.trim_start_matches("write failed: /v1/records/put 400 ");
        assert!(reason_shown.chars().count() <= 301, "{line:?}");
    }
    let long = failure_line("/v1/records/put", StatusCode::BAD_REQUEST, &reasons[3]);
    assert!(long.ends_with('…'), "{long:?}");
}

/// A record body around one credential, as an obeyed instruction would write it.
fn holding(secret: &str) -> String {
    format!("---\ntype: decision\nstatus: open\n---\nThe key is {secret} for now.\n")
}

/// A put of `content` to an ordinary path.
fn put(content: String) -> Change {
    Change::Put {
        path: "decisions/leak.md".to_owned(),
        content,
    }
}

/// Asserts each secret is refused by a finding that names its kind and never repeats it.
fn refused(secrets: &[String], kind: &str) -> Result<()> {
    for secret in secrets {
        let findings = credential_findings(&put(holding(secret)), "session")?;
        assert_eq!(
            findings.len(),
            1,
            "{secret:?} was not refused once: {findings:?}"
        );
        for finding in &findings {
            assert_eq!(finding.kind, "credential");
            assert!(finding.detail.contains(kind), "{finding:?}");
            assert!(
                !finding.detail.contains(secret.as_str()),
                "the finding repeats the secret: {finding:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn an_anthropic_key_is_refused() -> Result<()> {
    refused(
        &[format!("sk-ant-api03-{}", "Ab1-".repeat(20))],
        "Anthropic",
    )
}

/// The agent cycle's own model key, as the model gateway mints it.
#[test]
fn a_model_gateway_key_is_refused() -> Result<()> {
    refused(
        &[format!("sk-{}", "0a1b2c3d4e5f6g7h".repeat(4))],
        "model gateway",
    )
}

#[test]
fn a_github_token_is_refused() -> Result<()> {
    let body = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    let mut secrets: Vec<String> = ["ghp", "gho", "ghs", "ghu", "ghr"]
        .iter()
        .map(|prefix| format!("{prefix}_{body}"))
        .collect();
    secrets.push(format!("github_pat_11ABCDEFG0_{body}{body}"));
    refused(&secrets, "GitHub")
}

#[test]
fn a_slack_token_is_refused() -> Result<()> {
    refused(
        &[
            "xoxb-1234567890-0987654321-AbCdEfGhIjKlMnOp".to_owned(),
            "xoxp-1234567890-0987654321-AbCdEfGhIjKlMnOp".to_owned(),
            "xoxe-1-AbCdEfGhIjKlMnOpQrStUvWx".to_owned(),
            "xoxe.xoxp-1-AbCdEfGhIjKlMnOpQrStUvWx".to_owned(),
        ],
        "Slack",
    )
}

#[test]
fn a_google_oauth_token_is_refused() -> Result<()> {
    refused(
        &[
            format!("ya29.a0AfB_{}", "byC4x9".repeat(8)),
            format!("1//0gLm{}", "Xy7-Zq_".repeat(6)),
        ],
        "Google",
    )
}

#[test]
fn a_private_key_is_refused() -> Result<()> {
    refused(
        &[
            "-----BEGIN PRIVATE KEY-----".to_owned(),
            "-----BEGIN RSA PRIVATE KEY-----".to_owned(),
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
        ],
        "private key",
    )
}

/// Prose that names the prefixes, or hyphenates its words, is a record like any other.
#[test]
fn ordinary_prose_is_not_a_credential() -> Result<()> {
    let prose = holding(
        "a well-known follow-up: the sk-ant team said ghp_ and xoxb- prefixes, ya29 and \
             1/2 of the 1//2 split are re-reviewed, and the -----BEGIN PUBLIC KEY----- block is \
             public",
    );
    assert_eq!(credential_findings(&put(prose), "session")?.len(), 0);
    Ok(())
}

#[test]
fn a_record_path_stays_inside_the_tree() {
    for path in [
        "/etc/passwd",
        "../outside.md",
        "decisions/../../outside.md",
        ".git/config",
        "decisions/notes.txt",
        "",
    ] {
        assert!(record_path(path).is_err(), "{path} should be refused");
    }
    assert_eq!(
        record_path("decisions/renewal.md").ok(),
        Some("decisions/renewal.md".to_owned())
    );
}

/// A tree loads no hidden file, so nothing would check a record written to one.
#[test]
fn a_hidden_record_path_is_refused() {
    for path in [
        ".drafts/renewal.md",
        "decisions/.renewal.md",
        "decisions/.drafts/renewal.md",
        ".renewal.md",
    ] {
        assert!(record_path(path).is_err(), "{path} should be refused");
    }
}

/// A record path is a name: git reads it literally, and a commit message carries it on
/// one line. No pattern character, which git's pathspec magic would read as a pattern; no
/// leading `-` or `:`, which git reads as an option or as magic; no control character,
/// which would start a line of its own.
#[test]
fn a_record_path_is_plain_text() {
    for path in [
        "decisions/*.md",
        "decisions/?.md",
        "decisions/[a].md",
        "decisions/a].md",
        "-renewal.md",
        ":renewal.md",
        "decisions/-renewal.md",
        "decisions/:(top)renewal.md",
        "decisions/renewal\nSigned-off-by: Mallory.md",
        "decisions/renewal\t.md",
    ] {
        assert!(record_path(path).is_err(), "{path:?} should be refused");
    }
    assert_eq!(
        record_path("decisions/re-new:al.md").ok(),
        Some("decisions/re-new:al.md".to_owned())
    );
}

/// Two spellings of one file would be one record and two checks, so a path is returned in
/// the spelling the tree itself uses.
#[test]
fn a_record_path_is_normalised() -> Result<()> {
    assert_eq!(
        record_path("decisions//renewal.md")?,
        "decisions/renewal.md"
    );
    assert_eq!(record_path("./decisions/renewal.md").ok(), None);
    assert_eq!(
        record_path("decisions/./renewal.md")?,
        "decisions/renewal.md"
    );
    Ok(())
}

#[test]
fn a_write_through_a_symbolic_link_is_refused() -> Result<()> {
    let root = std::env::temp_dir().join(format!("ctx-symlink-{}", std::process::id()));
    let decisions = root.join("decisions");
    std::fs::create_dir_all(&decisions)?;
    std::os::unix::fs::symlink("/etc", decisions.join("linked"))?;
    std::os::unix::fs::symlink("/etc/passwd", decisions.join("linked.md"))?;

    let through_a_directory = inside_the_tree(&root, "decisions/linked/passwd.md");
    let onto_a_file = inside_the_tree(&root, "decisions/linked.md");
    let plain = inside_the_tree(&root, "decisions/renewal.md");
    std::fs::remove_dir_all(&root)?;

    assert!(through_a_directory.is_err(), "a linked directory was taken");
    assert!(onto_a_file.is_err(), "a linked record was taken");
    assert_eq!(
        plain.ok().as_deref(),
        Some(Path::new(&decisions).join("renewal.md").as_path())
    );
    Ok(())
}
