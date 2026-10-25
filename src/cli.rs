//! Command-line parsing and dispatch for tree commands.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize as _;
use serde::Serialize;

use crate::config::{locate_tree, state_base, TreeConfig, UserConfig};
use crate::context::Context;
use crate::open::{open_items, OpenItem};
use crate::output::{self, Outcome};
use crate::search;
use crate::sweep::{self, Findings};
use crate::tree::Doc;

#[derive(Parser)]
#[command(name = "ctx", version, arg_required_else_help = true)]
pub struct Cli {
    /// Path to the markdown tree.
    #[arg(long, env = "CTX_MEMORY", global = true)]
    pub memory: Option<PathBuf>,

    /// Emit JSON instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug)]
pub struct GlobalArgs {
    pub memory: Option<PathBuf>,
    pub json: bool,
}

impl From<&Cli> for GlobalArgs {
    fn from(cli: &Cli) -> Self {
        Self {
            memory: cli.memory.clone(),
            json: cli.json,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Print the tree rules page.
    Rules,
    /// Report missing links, stale items, and missing provenance.
    Sweep,
    /// Search the tree, ranking direct sources ahead of summaries.
    Search {
        query: String,
        /// Include only direct-source records.
        #[arg(long)]
        verbatim: bool,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Find documents that mention an entity.
    Dossier { entity: String },
    /// List open decisions, commitments, and questions.
    Open,
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

impl Command {
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Rules => "rules".to_owned(),
            Self::Sweep => "sweep".to_owned(),
            Self::Search { .. } => "search".to_owned(),
            Self::Dossier { .. } => "dossier".to_owned(),
            Self::Open => "open".to_owned(),
            Self::External(arguments) => arguments
                .first()
                .and_then(|argument| argument.to_str())
                .map_or_else(|| "external".to_owned(), str::to_owned),
        }
    }
}

pub struct PluginEnv {
    pub tree_root: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub json: bool,
}

#[derive(Serialize)]
struct RulesPayload<'a> {
    path: String,
    bytes: usize,
    text: &'a str,
}

#[derive(Serialize)]
struct SearchPayload<'a> {
    query: &'a str,
    hits: Vec<search::Hit>,
}

#[derive(Serialize)]
struct DossierPayload<'entity, 'vitals, 'document> {
    entity: &'entity str,
    page: Option<DossierPage<'vitals, 'document>>,
    hits: Vec<search::Hit>,
}

#[derive(Serialize)]
struct DossierPage<'vitals, 'document> {
    path: String,
    vitals: BTreeMap<&'vitals str, &'document str>,
}

#[derive(Serialize)]
struct OpenPayload {
    items: Vec<OpenItem>,
}

/// Runs the selected command.
///
/// # Errors
///
/// Returns an error when tree discovery, loading, command execution, or external dispatch fails.
pub fn run(cli: &Cli) -> Result<Outcome> {
    if let Command::External(arguments) = &cli.command {
        let environment = plugin_environment(&GlobalArgs::from(cli))?;
        return run_external(arguments, &environment).map(|never| match never {});
    }

    let context = Context::load(&GlobalArgs::from(cli))?;
    match &cli.command {
        Command::Rules => run_rules(&context),
        Command::Sweep => {
            let findings = sweep::run(&context.tree)?;
            output::emit(context.json, "sweep", &findings, print_sweep)?;
            Ok(if findings.structural_clean() {
                Outcome::Pass
            } else {
                Outcome::StructuralDefects
            })
        }
        Command::Search {
            query,
            verbatim,
            limit,
        } => {
            let payload = SearchPayload {
                query,
                hits: search::run(&context.tree, query, *verbatim, *limit),
            };
            output::emit(context.json, "search", &payload, |payload| {
                search::print(&payload.hits, payload.query.split_whitespace().count());
            })?;
            Ok(Outcome::Pass)
        }
        Command::Dossier { entity } => run_dossier(&context, entity),
        Command::Open => {
            if !context.config.sources.is_empty() {
                context.state()?;
            }
            let payload = OpenPayload {
                items: open_items(&context.tree),
            };
            output::emit(context.json, "open", &payload, |payload| {
                print_open(&payload.items);
            })?;
            Ok(Outcome::Pass)
        }
        Command::External(_) => Err(anyhow!("external command dispatch failed")),
    }
}

fn run_rules(context: &Context) -> Result<Outcome> {
    let document = context
        .tree
        .rules()
        .ok_or_else(|| anyhow!("rules.md is missing from the tree root"))?;
    let payload = RulesPayload {
        path: document.rel.to_string_lossy().into_owned(),
        bytes: document.raw.len(),
        text: &document.raw,
    };
    output::emit(context.json, "rules", &payload, |payload| {
        println!("{}", payload.path);
        print!("\n{}", payload.text);
    })?;
    Ok(Outcome::Pass)
}

fn run_dossier(context: &Context, entity: &str) -> Result<Outcome> {
    if entity.trim().is_empty() {
        bail!("dossier entity must not be empty");
    }
    let payload = DossierPayload {
        entity,
        page: dossier_page(
            &context.tree.docs,
            &context.tree.index_by_name(),
            entity,
            &context.config.dossier.vitals,
        ),
        hits: search::dossier(&context.tree, entity),
    };
    output::emit(context.json, "dossier", &payload, |payload| {
        search::print_dossier(payload.entity, &payload.hits);
    })?;
    Ok(Outcome::Pass)
}

fn dossier_page<'vitals, 'document>(
    documents: &'document [Doc],
    index: &std::collections::HashMap<String, usize>,
    entity: &str,
    vital_fields: &'vitals [String],
) -> Option<DossierPage<'vitals, 'document>> {
    let position = *index.get(entity)?;
    let document = documents.get(position)?;
    let vitals = vital_fields
        .iter()
        .filter_map(|field| front_vital(document, field).map(|value| (field.as_str(), value)))
        .collect();
    Some(DossierPage {
        path: document.rel.to_string_lossy().into_owned(),
        vitals,
    })
}

fn front_vital<'a>(document: &'a Doc, field: &str) -> Option<&'a str> {
    let front = &document.front;
    match field {
        "type" => front.kind.as_deref(),
        "status" => front.status.as_deref(),
        "title" => front.title.as_deref(),
        "name" => front.name.as_deref(),
        "owner" => front.owner.as_deref(),
        "what" => front.what.as_deref(),
        "question" => front.question.as_deref(),
        _ => front.extra_str(field),
    }
}

fn plugin_environment(global: &GlobalArgs) -> Result<PluginEnv> {
    let user = UserConfig::load()?;
    let tree_root = resolve_plugin_tree(global, &user);
    let state_dir = tree_root.as_deref().and_then(|root| {
        let config = TreeConfig::load(root).ok()?;
        let id = config.id?;
        let path = state_base(&user).ok()?.join(id);
        absolute_path(path).ok()
    });
    Ok(PluginEnv {
        tree_root,
        state_dir,
        json: global.json,
    })
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("reading the current directory for plugin state")?
        .join(path))
}

fn resolve_plugin_tree(global: &GlobalArgs, user: &UserConfig) -> Option<PathBuf> {
    if let Some(root) = global.memory.as_deref() {
        return root.canonicalize().ok();
    }
    let cwd = std::env::current_dir().ok()?;
    locate_tree(None, &cwd, user)
        .ok()
        .and_then(|root| root.canonicalize().ok())
}

/// Executes a `ctx-<name>` command from `PATH` with the resolved ctx environment.
///
/// # Errors
///
/// Returns an error when no command name is supplied or when the executable cannot be started.
pub fn run_external(arguments: &[OsString], environment: &PluginEnv) -> Result<Infallible> {
    let (name, arguments) = arguments
        .split_first()
        .ok_or_else(|| anyhow!("external command name is missing"))?;
    let name = name
        .to_str()
        .ok_or_else(|| anyhow!("external command name is not valid UTF-8"))?;
    let executable = format!("ctx-{name}");
    let mut command = ProcessCommand::new(&executable);
    command
        .env_remove("CTX_MEMORY")
        .env_remove("CTX_STATE_DIR")
        .env_remove("CTX_JSON");
    command.args(arguments);
    if let Some(root) = &environment.tree_root {
        command.env("CTX_MEMORY", root);
    }
    if let Some(state_dir) = &environment.state_dir {
        command.env("CTX_STATE_DIR", state_dir);
    }
    if environment.json {
        command.env("CTX_JSON", "1");
    }
    command.env("CTX_VERSION", env!("CARGO_PKG_VERSION"));
    command.env(
        "CTX_BIN",
        std::env::current_exe().context("locating the ctx executable")?,
    );

    let error = command.exec();
    if error.kind() == std::io::ErrorKind::NotFound {
        bail!("unknown command `{name}` (no {executable} on PATH)");
    }
    Err(error).with_context(|| format!("running {executable}"))
}

fn print_sweep(findings: &Findings) {
    println!("{} files\n", findings.files);

    if !findings.gaps.is_empty() {
        println!(
            "{}",
            format!("GAPS — links with no file ({})", findings.gaps.len()).bold()
        );
        for gap in &findings.gaps {
            println!("  {}  <- {}", gap.target.yellow(), gap.from.dimmed());
        }
        println!();
    }

    let (overdue, undated): (Vec<_>, Vec<_>) = findings
        .stale
        .iter()
        .partition(|stale| stale.overdue_days.is_some());
    for (title, items) in [
        ("STALE — open past its date", &overdue),
        ("UNDATED — open with no revisit_by/due date", &undated),
    ] {
        if items.is_empty() {
            continue;
        }
        println!("{}", format!("{title} ({})", items.len()).bold());
        for item in items {
            let owner = item.owner.as_deref().unwrap_or("unassigned");
            println!(
                "  {}  [{}] {}  owner={}",
                item.path.red(),
                item.status,
                item.why.yellow(),
                owner.dimmed()
            );
        }
        println!();
    }

    for (title, items) in [
        ("UNTIERED — no authority tier", &findings.untiered),
        ("UNQUOTED — no verbatim quote", &findings.unquoted),
        (
            "NEGATIVES — direct source not identified",
            &findings.negatives,
        ),
        ("ORPHANS — no document links here", &findings.orphans),
    ] {
        if items.is_empty() {
            continue;
        }
        println!("{}", format!("{title} ({})", items.len()).bold());
        for item in items {
            println!("  {item}");
        }
        println!();
    }

    if findings.clean() {
        println!("{}", "no gaps, nothing stale".green());
    } else if findings.structural_clean() {
        println!(
            "{}",
            format!(
                "structure clean — {} stale item(s) need review",
                findings.stale.len()
            )
            .green()
        );
    }
}

fn print_open(items: &[OpenItem]) {
    if items.is_empty() {
        println!("nothing open");
        return;
    }
    let today = Utc::now().date_naive();
    for item in items {
        let when = item.days_overdue.map_or_else(
            || {
                item.deadline.map_or_else(
                    || match item.opened.map(|opened| (today - opened).num_days()) {
                        Some(age) => format!("no date (opened {age}d ago)"),
                        None => "no date".to_owned(),
                    },
                    |deadline| deadline.format("%a %Y-%m-%d").to_string(),
                )
            },
            |days| format!("overdue {days}d"),
        );
        println!(
            "[{}] {}  {}",
            item.kind.bold(),
            when.red(),
            item.owner.as_deref().unwrap_or("unassigned").dimmed()
        );
        println!("     {}", item.title);
        println!("     {}\n", item.path.dimmed());
    }
}
