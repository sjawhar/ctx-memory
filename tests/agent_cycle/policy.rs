//! What the cycle's session may run: its flags, its shell policy and its prompt.

use std::collections::BTreeSet;
use std::process::Command;

use anyhow::{bail, ensure, Context as _, Result};
use clap::error::ErrorKind;
use ctx_memory::cli::{parse_argv, Cli, Command as CtxCommand, Remote};
use serde_json::Value;

use crate::fixtures::{deploy, remove_scratch, repo, scratch};

fn policy() -> Result<Value> {
    serde_saphyr::from_str(&deploy("policy.yml")?).context("parsing deploy/agent/policy.yml")
}

/// The policy's bash rules, as `(pattern, approval)`.
fn bash_rules(policy: &Value) -> Result<Vec<(String, String)>> {
    let patterns = policy
        .pointer("/bash/patterns")
        .and_then(Value::as_array)
        .context("the policy has no bash.patterns list, so every command keeps the exec tier")?;
    patterns
        .iter()
        .map(|rule| {
            let pattern = rule.get("match").and_then(Value::as_str);
            let approval = rule.get("approval").and_then(Value::as_str);
            match (pattern, approval) {
                (Some(pattern), Some(approval)) => Ok((pattern.to_owned(), approval.to_owned())),
                _ => bail!("a bash rule lacks a match or an approval: {rule}"),
            }
        })
        .collect()
}

fn allowed_patterns(policy: &Value) -> Result<Vec<String>> {
    Ok(bash_rules(policy)?
        .into_iter()
        .filter(|(_, approval)| approval == "allow")
        .map(|(pattern, _)| pattern)
        .collect())
}

/// Whether omp's bash `allow` rule `pattern` admits `command`, decided the way omp decides
/// it at the release `deploy/agent/Dockerfile` pins
/// (`packages/coding-agent/src/tools/bash.ts:239-275,301-305`): a run of whitespace counts
/// as one space in both, `*` matches any run of characters, and the pattern has to cover
/// the whole command. omp also refuses an allow to a command that holds a shell control
/// character; no command checked here holds one.
fn admits(pattern: &str, command: &str) -> Result<bool> {
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let expression = normalize(pattern)
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    Ok(regex::Regex::new(&format!("^{expression}$"))?.is_match(&normalize(command)))
}

/// Parses `words` as ctx's arguments, adding a placeholder for each required argument a
/// pattern or a skill leaves out (`ctx search`).
fn parse_filled(words: &[&str]) -> Result<(Cli, Vec<String>)> {
    let mut argv: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
    loop {
        match parse_argv(&argv) {
            Ok(cli) => return Ok((cli, argv)),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument && argv.len() < 8 => {
                argv.push("x".to_owned());
            }
            Err(error) => bail!("`ctx {}` does not parse: {error}", argv.join(" ")),
        }
    }
}

#[test]
fn the_cycle_approves_no_tool_call_by_itself() -> Result<()> {
    let script = deploy("run-cycle.sh")?;
    for forbidden in ["yolo", "--auto-approve"] {
        assert!(
            !script.contains(forbidden),
            "the cycle still starts its session with {forbidden}"
        );
    }
    assert!(
        script.contains("--approval-mode write"),
        "the cycle does not name the approval mode its session runs under"
    );
    assert!(
        script.contains("--config /opt/ctx-memory/policy.yml"),
        "the cycle does not load the policy that bounds its shell"
    );
    Ok(())
}

#[test]
fn the_cycle_holds_only_the_tools_it_needs() -> Result<()> {
    let script = deploy("run-cycle.sh")?;
    assert!(
        script.contains("--tools bash,read,grep,glob,todo,ctx_record_put,ctx_record_rm"),
        "the cycle does not pass the tool allowlist; a session that can spawn subagents, \
         evaluate code or reach the network is not bounded by the shell policy"
    );
    Ok(())
}

#[test]
fn the_shell_runs_ctx_and_nothing_else() -> Result<()> {
    let policy = policy()?;
    let rules = bash_rules(&policy)?;
    assert!(
        !rules.is_empty(),
        "the policy has no bash rules, so every command keeps the exec tier"
    );
    for (pattern, approval) in &rules {
        assert!(
            approval != "allow" || pattern == "ctx" || pattern.starts_with("ctx "),
            "the policy allows {pattern}, which is not a ctx command"
        );
    }
    assert!(
        rules.iter().any(|(_, approval)| approval == "allow"),
        "the policy allows no command, so the cycle cannot run ctx"
    );
    for denied in ["eval", "task", "browser", "web_search"] {
        assert_eq!(
            policy
                .pointer(&format!("/tools/approval/{denied}"))
                .and_then(Value::as_str),
            Some("deny"),
            "the policy does not deny the {denied} tool"
        );
    }
    Ok(())
}

/// `ctx <name>` runs `ctx-<name>`, so a rule that admits any first word admits any program a
/// name can spell. Each allow names one built-in subcommand that the server answers, and a
/// wildcard stands only for that subcommand's arguments.
#[test]
fn the_shell_admits_only_the_ctx_subcommands_the_cycle_needs() -> Result<()> {
    let allowed = allowed_patterns(&policy()?)?;
    let mut subcommands = BTreeSet::new();
    for pattern in &allowed {
        let words: Vec<&str> = pattern.split_whitespace().collect();
        let literal = match words.split_last() {
            Some((&"*", rest)) => rest,
            _ => words.as_slice(),
        };
        ensure!(
            literal.len() >= 2,
            "the allow `{pattern}` admits any first word, so it runs any ctx-<name> program"
        );
        ensure!(
            literal.first() == Some(&"ctx"),
            "the allow `{pattern}` is not a ctx command"
        );
        ensure!(
            literal.iter().all(|word| !word.contains('*')),
            "the allow `{pattern}` has a wildcard inside its command words, so it admits \
             another command, an external subcommand among them"
        );
        let (cli, _) = parse_filled(literal.get(1..).unwrap_or_default())?;
        ensure!(
            !matches!(cli.command, CtxCommand::External(_)),
            "the allow `{pattern}` names no built-in subcommand, so it runs a ctx-<name> program"
        );
        ensure!(
            cli.command.remote() != Remote::Local,
            "the allow `{pattern}` admits a command that runs on the cycle's own machine"
        );
        subcommands.insert(cli.command.name());
    }
    let expected: BTreeSet<String> = [
        "cite verify",
        "dossier",
        "marker advance",
        "open",
        "pending",
        "record show",
        "resolve",
        "rules",
        "search",
        "sweep",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        subcommands, expected,
        "the policy admits a different set of subcommands than the cycle needs"
    );

    for probe in [
        "ctx a/../../../bin/sh /workspace/x.sh",
        "ctx a/../../../usr/local/bin/omp -p --approval-mode yolo go",
        "ctx hello",
        "ctx fetch",
        "ctx delta --write",
        "ctx state path",
        "ctx cursor show",
        "ctx source run slack",
        "ctx serve",
        "ctx slack login",
    ] {
        let words: Vec<&str> = probe.split_whitespace().skip(1).collect();
        let (cli, _) = parse_filled(&words)?;
        ensure!(
            cli.command.remote() == Remote::Local,
            "the probe `{probe}` is not a local command, so it proves nothing here"
        );
        for pattern in &allowed {
            ensure!(
                !admits(pattern, probe)?,
                "the allow `{pattern}` admits `{probe}`"
            );
        }
    }
    Ok(())
}

/// A narrower policy must not starve the cycle: every `ctx` command the skills tell it to
/// run at the shell gets through. The two record writes are the exception, and must not get
/// through: both skills name `ctx record put <path>` and `ctx record rm <path>`, the ingest
/// skill as what its record tools run and the general skill for sessions that have a shell
/// write, but the cycle writes only through `ctx_record_put` and `ctx_record_rm`, which run
/// ctx with no shell. A shell rule for them would be surface with no use: `ctx record put
/// <path> <file>` reads any file the session's user can read into a record.
#[test]
fn the_shell_admits_every_ctx_command_the_skills_name() -> Result<()> {
    let allowed = allowed_patterns(&policy()?)?;
    let placeholder = regex::Regex::new("<[^>]*>")?;
    let mut checked = 0;
    let mut writes = 0;
    for skill in ["ctx-memory-ingest", "ctx-memory"] {
        let path = repo().join("skills").join(skill).join("SKILL.md");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        for span in text.split('`').skip(1).step_by(2) {
            let Some(command) = span.strip_prefix("ctx ") else {
                continue;
            };
            let filled = placeholder.replace_all(command, "x");
            let words: Vec<&str> = filled
                .split_whitespace()
                .map(|word| word.trim_matches('"'))
                .collect();
            let (cli, argv) = parse_filled(&words)?;
            let line = format!("ctx {}", argv.join(" "));
            let mut admitted = false;
            for pattern in &allowed {
                admitted |= admits(pattern, &line)?;
            }
            if matches!(cli.command.name().as_str(), "record put" | "record rm") {
                ensure!(
                    !admitted,
                    "the policy admits `ctx {command}` (run as `{line}`) at the shell, where \
                     the cycle writes only through its record tools"
                );
                writes += 1;
                continue;
            }
            ensure!(
                admitted,
                "the policy refuses `ctx {command}` (run as `{line}`), which the {skill} skill \
                 tells the cycle to run"
            );
            checked += 1;
        }
    }
    ensure!(
        checked >= 10 && writes >= 2,
        "the skills name only {checked} ctx commands and {writes} record writes, so this check \
         reads nothing"
    );
    Ok(())
}

/// An external subcommand's name becomes `ctx-<name>`, which `Command::new` reads as a path
/// as soon as it holds a `/`. Given a `ctx-a` directory to walk through, a name made of
/// `..` steps reaches any program on the machine.
#[test]
fn an_external_subcommand_name_cannot_name_a_path() -> Result<()> {
    let root = scratch("traversal")?;
    std::fs::create_dir(root.join("ctx-a"))?;
    let name = format!("a{}/bin/sh", "/..".repeat(48));
    let output = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .current_dir(&root)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &root)
        .env_remove("CTX_SERVER")
        .env_remove("CTX_MEMORY")
        .args([name.as_str(), "-c", "printf '%s-%s' escaped ctx"])
        .output()
        .context("running ctx with a path for a subcommand name")?;
    remove_scratch(&root)?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !text.contains("escaped-ctx"),
        "ctx ran /bin/sh through the subcommand name {name}: {text}"
    );
    ensure!(
        !output.status.success(),
        "ctx accepted the subcommand name {name}: {text}"
    );
    ensure!(
        text.contains("is not a command name"),
        "ctx did not refuse the subcommand name for what it is: {text}"
    );
    Ok(())
}

/// At the pinned omp, `read` and `grep` fetch any `https://` path at the read tier, which
/// `approvalMode: write` approves; `fetch.enabled` is the one switch that refuses it
/// (`tools/read.ts:1720-1724`, `tools/fetch.ts:1736-1739`, default `true` at
/// `tools/settings.ts:728-731`).
#[test]
fn the_session_reads_no_url() -> Result<()> {
    assert_eq!(
        policy()?.pointer("/fetch/enabled").and_then(Value::as_bool),
        Some(false),
        "the policy leaves fetch.enabled at omp's default, so `read https://…` sends a request \
         anywhere the session names"
    );
    Ok(())
}

#[test]
fn the_model_key_leaves_the_session_environment() -> Result<()> {
    let script = deploy("run-cycle.sh")?;
    assert!(
        script.contains("unset ANTHROPIC_API_KEY"),
        "the cycle leaves the model key in the environment its session inherits"
    );
    let models = deploy("models.yml")?;
    assert!(
        models.contains("!cat /run/ctx-agent/model-key"),
        "the model credential does not come from the file the script writes"
    );
    Ok(())
}

/// The wrapper the session puts around quoted text has to be one the quoted text cannot
/// close: its tag name carries a nonce drawn for the cycle after the window was written, so
/// a `</untrusted-source-text>` inside a message is just more of the message.
#[test]
fn each_cycle_names_a_fresh_wrapper_nonce_in_its_prompt() -> Result<()> {
    let script = deploy("run-cycle.sh")?;
    let line = script
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("nonce="))
        .context("run-cycle.sh draws no wrapper nonce")?;
    ensure!(
        line.contains("/dev/urandom"),
        "the nonce is not drawn from /dev/urandom: {line}"
    );
    let draw = || -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("set -eu; {line}; printf %s \"$nonce\""))
            .output()
            .context("running the nonce line")?;
        ensure!(
            output.status.success(),
            "the nonce line failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    let (first, second) = (draw()?, draw()?);
    ensure!(
        first.len() == 32 && first.chars().all(|c| c.is_ascii_hexdigit()),
        "the nonce {first:?} is not 128 bits written in hex"
    );
    ensure!(first != second, "two cycles drew the same nonce {first}");
    for tag in [
        "<untrusted-source-text-$nonce ",
        "</untrusted-source-text-$nonce>",
    ] {
        ensure!(
            script.contains(tag),
            "the prompt does not name the wrapper {tag}, so the session cannot use it"
        );
    }
    Ok(())
}
