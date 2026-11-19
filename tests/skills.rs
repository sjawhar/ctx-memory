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
                // `1` parses wherever a placeholder stands: a path, an id or a count.
                filled.push('1');
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
/// a skill leaves out when it only names the command (`ctx dossier`), each with `1`, which
/// parses as a path, an id or a count.
fn runs_only_locally(argv: &[String]) -> Result<bool> {
    let mut argv = argv.to_vec();
    loop {
        match parse_argv(&argv) {
            Ok(cli) => return Ok(cli.command.remote() == Remote::Local),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument && argv.len() < 8 => {
                argv.push("1".to_owned());
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

/// A session can drop the end of a long skill when its context is compacted. The ingest skill
/// stays under 8,192 bytes; a block only some cycles need goes to `references/`, named at the
/// step that needs it.
#[test]
fn the_ingest_skill_stays_under_8192_bytes() -> Result<()> {
    let bytes = skill("ctx-memory-ingest")?.len();
    assert!(
        bytes < 8_192,
        "the ingest skill is {bytes} bytes; move a block only some cycles need to references/"
    );
    Ok(())
}

/// A file the skill sends the session to read ships beside it: a pointer to a file that is not
/// there leaves the session to guess what it held.
#[test]
fn every_file_the_ingest_skill_names_ships_with_it() -> Result<()> {
    let text = skill("ctx-memory-ingest")?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/ctx-memory-ingest");
    let named: Vec<&str> = text
        .split("skill://ctx-memory-ingest/")
        .skip(1)
        .filter_map(|rest| rest.split(['`', ' ', ',', ')']).next())
        .collect();
    assert!(
        named.iter().any(|path| path.starts_with("references/")),
        "the ingest skill names no reference file, so this check reads nothing: {named:?}"
    );
    for path in named {
        assert!(
            root.join(path).is_file(),
            "the ingest skill names skill://ctx-memory-ingest/{path}, which is not in the skill"
        );
    }
    Ok(())
}

/// An agent's skill list shows each skill by its description, and Oh My Pi cuts a description
/// longer than 100 characters short while it has no compressed copy of it. The reading skill's
/// description, read from its front matter, stays whole at that width.
#[test]
fn the_reading_skill_description_fits_a_skill_list() -> Result<()> {
    let text = skill("ctx-memory")?;
    let front = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(front, _)| front)
        .context("the reading skill has no front matter")?;
    let front: serde_json::Value =
        serde_saphyr::from_str(front).context("parsing the reading skill's front matter")?;
    let description = front
        .get("description")
        .and_then(serde_json::Value::as_str)
        .context("the reading skill's front matter has no string description")?;
    let chars = description.chars().count();
    assert!(
        chars <= 100,
        "the reading skill's description is {chars} characters; a skill list cuts it after 100: {description:?}"
    );
    Ok(())
}
