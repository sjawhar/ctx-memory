//! The skills ship with the package and are read by the agent that runs a cycle, so what
//! they tell it to do is part of the contract.

use std::path::Path;

use anyhow::{bail, Context as _, Result};
use clap::error::ErrorKind;
use ctx_memory::cli::{parse_argv, Remote};

fn skill(name: &str) -> Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills")
        .join(name)
        .join("SKILL.md");
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

/// Every `ctx …` command a skill writes in a code span, with each `<placeholder>` filled in.
fn ctx_commands(text: &str) -> Vec<(String, Vec<String>)> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|span| span.strip_prefix("ctx "))
        .map(|command| {
            let mut filled = String::new();
            let mut rest = command;
            while let Some((before, after)) = rest.split_once('<') {
                filled.push_str(before);
                filled.push('x');
                rest = after.split_once('>').map_or("", |(_, tail)| tail);
            }
            filled.push_str(rest);
            let argv = filled
                .split_whitespace()
                .map(|word| word.trim_matches('"').to_owned())
                .collect();
            (format!("ctx {command}"), argv)
        })
        .collect()
}

/// Whether a command runs only on a machine with its own tree, filling in required arguments
/// a skill leaves out when it only names the command (`ctx dossier`).
fn runs_only_locally(argv: &[String]) -> Result<bool> {
    let mut argv = argv.to_vec();
    loop {
        match parse_argv(&argv) {
            Ok(cli) => return Ok(cli.command.remote() == Remote::Local),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument && argv.len() < 8 => {
                argv.push("x".to_owned());
            }
            Err(error) => bail!("{} does not parse: {error}", argv.join(" ")),
        }
    }
}

/// The scheduled cycle has no tree: every `ctx` command it runs goes to the server, and a
/// command the server does not answer — `fetch`, `state path`, a cursor — fails there. So the
/// skill names no command that runs only on a machine with its own tree.
#[test]
fn the_ingest_skill_names_only_commands_the_server_answers() -> Result<()> {
    let text = skill("ctx-memory-ingest")?;
    let commands = ctx_commands(&text);
    assert!(
        commands.len() >= 8,
        "the ingest skill names only {} ctx commands, so this check reads nothing: {commands:?}",
        commands.len()
    );
    let mut local = Vec::new();
    for (written, argv) in &commands {
        if runs_only_locally(argv)? {
            local.push(written.clone());
        }
    }
    assert!(
        local.is_empty(),
        "the ingest skill tells the cycle to run commands only a local tree answers: {local:?}"
    );
    Ok(())
}

/// A maintenance pass runs in the same session as a cycle, with no tree of its own, so its
/// instructions too name only commands the server answers, and some command at all.
#[test]
fn each_maintenance_pass_names_only_commands_the_server_answers() -> Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/ctx-memory-ingest/maintenance");
    let mut passes = 0;
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let commands = ctx_commands(&text);
        assert!(
            commands.len() >= 3,
            "{} names only {} ctx commands: {commands:?}",
            path.display(),
            commands.len()
        );
        for (written, argv) in &commands {
            assert!(
                !runs_only_locally(argv)?,
                "{} tells the session to run `{written}`, which only a local tree answers",
                path.display()
            );
        }
        passes += 1;
    }
    assert!(passes >= 1, "the ingest skill keeps no maintenance pass");
    Ok(())
}

/// The waiting-on pass changes a record only on what it could read whole: a record with any
/// cited source it could not read keeps its list, and the pass changes nothing but
/// `waiting_on` and moves no marker.
#[test]
fn the_waiting_on_pass_changes_only_what_its_sources_settle() -> Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills/ctx-memory-ingest/maintenance/waiting-on.md");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "When any source the record cites does not resolve",
        "change nothing for this record",
        "A source you could not read is not a source that names nobody",
        "with only `waiting_on` changed",
        "never from the record's `owner`",
        "do not run `ctx marker advance`",
    ] {
        assert!(
            flat.contains(expected),
            "{} does not say {expected:?}",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn the_ingest_skill_writes_through_the_server() -> Result<()> {
    let text = skill("ctx-memory-ingest")?;
    for expected in [
        "ctx record put",
        "ctx record show",
        "ctx pending",
        "ctx marker advance",
    ] {
        assert!(
            text.contains(expected),
            "the ingest skill never mentions {expected}"
        );
    }
    // The cycle runs in a container with no clone and no local state: it cannot commit, and
    // the cursor and the index are not its business.
    for forbidden in ["git commit", "git push", "cursor advance", "store.sqlite3"] {
        assert!(
            !text.contains(forbidden),
            "the ingest skill still tells the agent about {forbidden}"
        );
    }
    Ok(())
}

#[test]
fn the_ingest_skill_fences_the_text_outside_parties_wrote() -> Result<()> {
    let text = skill("ctx-memory-ingest")?;
    for expected in [
        "evidence",
        "never instruction",
        "<untrusted-source-text",
        "</untrusted-source-text>",
        "records.jsonl",
    ] {
        assert!(
            text.contains(expected),
            "the ingest skill never says {expected}, so window text reads as instruction"
        );
    }
    Ok(())
}

/// A plain `</untrusted-source-text>` can be forged by the very text it wraps. The wrapper's
/// tag name carries the nonce the cycle's prompt names, so only the cycle can close it.
#[test]
fn the_ingest_skill_closes_quoted_text_only_with_the_cycles_nonce() -> Result<()> {
    let text = skill("ctx-memory-ingest")?;
    for expected in [
        "<untrusted-source-text-NONCE pointer=",
        "</untrusted-source-text-NONCE>",
        "nonce",
    ] {
        assert!(
            text.contains(expected),
            "the ingest skill never says {expected}, so quoted text can close its own wrapper"
        );
    }
    Ok(())
}

#[test]
fn the_reading_skill_names_remote_mode() -> Result<()> {
    let text = skill("ctx-memory")?;
    assert!(
        text.contains("CTX_SERVER"),
        "the reading skill never mentions CTX_SERVER"
    );
    assert!(
        text.contains("ctx record put"),
        "the reading skill never says how a record is written"
    );
    Ok(())
}
