//! Command-line parsing and dispatch for tree commands.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize as _;
use serde::Serialize;

use crate::config::{locate_tree, state_base, TreeConfig, UserConfig};
use crate::context::Context;
use crate::cursor::{self, AdvanceOutcome, Cursors};
use crate::fetch::{self, FetchOptions};
use crate::open::{open_items_with_filters, parse_linked, LinkedFilter, OpenItemView};
use crate::output::{self, Outcome};
use crate::resolve::{self, Resolved, Verification};
use crate::schema::today_utc;
use crate::search;
use crate::source::{
    self, FetchRequest, Request as SourceRequest, ResolveRequest, Session, SourceRef,
};
use crate::sweep::{self, Findings};
use crate::windows::{self, SourceManifest, Status, Window};

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
    Open {
        #[arg(
            long,
            value_name = "FIELD=VALUE[,VALUE…]",
            value_parser = parse_linked
        )]
        linked: Vec<LinkedFilter>,
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        owner: Vec<String>,
    },
    /// Fetch all configured sources into a durable window.
    Fetch {
        #[arg(long, value_parser = parse_window_end)]
        window_end: Option<DateTime<Utc>>,
        #[arg(long = "source", value_name = "ID", num_args = 1..)]
        source: Vec<String>,
    },
    /// Inspect or advance durable source cursors.
    Cursor {
        #[command(subcommand)]
        command: CursorCommand,
    },
    /// Resolve citation pointers from the local store.
    Resolve {
        #[arg(long)]
        thread: bool,
        pointers: Vec<String>,
    },
    /// Verify citations in tree documents.
    Cite {
        #[command(subcommand)]
        command: CiteCommand,
    },
    /// Run a configured source and print its protocol stream.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Print the active tree state paths.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Subcommand)]
pub enum CiteCommand {
    /// Verify pointer resolution and quoted text.
    Verify {
        #[arg(long)]
        live: bool,
        files: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum SourceCommand {
    /// Run a configured source.
    Run {
        id: String,
        #[arg(long, value_enum, default_value_t = SourceOperation::Fetch)]
        op: SourceOperation,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SourceOperation {
    Fetch,
    Resolve,
}

#[derive(Subcommand)]
pub enum StateCommand {
    /// Print the state directory and SQLite store path.
    Path,
}

#[derive(Subcommand)]
pub enum CursorCommand {
    /// Print every stored source-stream cursor.
    Show,
    /// Advance successful source-stream cursors from one completed window.
    Advance { window: String },
}

impl Command {
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Rules => "rules".to_owned(),
            Self::Sweep => "sweep".to_owned(),
            Self::Search { .. } => "search".to_owned(),
            Self::Dossier { .. } => "dossier".to_owned(),
            Self::Open { .. } => "open".to_owned(),
            Self::Fetch { .. } => "fetch".to_owned(),
            Self::Cursor {
                command: CursorCommand::Show,
            } => "cursor show".to_owned(),
            Self::Cursor {
                command: CursorCommand::Advance { .. },
            } => "cursor advance".to_owned(),
            Self::Resolve { .. } => "resolve".to_owned(),
            Self::Cite {
                command: CiteCommand::Verify { .. },
            } => "cite verify".to_owned(),
            Self::Source {
                command: SourceCommand::Run { .. },
            } => "source run".to_owned(),
            Self::State {
                command: StateCommand::Path,
            } => "state path".to_owned(),
            Self::External(arguments) => arguments
                .first()
                .and_then(|argument| argument.to_str())
                .map_or_else(|| "external".to_owned(), str::to_owned),
        }
    }
}

pub struct PluginEnv {
    pub tree_root: Option<PathBuf>,
    pub state_base: Option<PathBuf>,
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
struct OpenPayload {
    items: Vec<OpenItemView>,
}

#[derive(Serialize)]
struct ResolvePayload {
    results: Vec<Resolved>,
}

#[derive(Serialize)]
struct StatePathPayload {
    state_dir: String,
    store: String,
}

#[derive(Serialize)]
struct FetchPayload<'a> {
    window: &'a str,
    exit_status: Status,
    sources: &'a BTreeMap<String, SourceManifest>,
}

#[derive(Serialize)]
struct CursorShowPayload {
    cursors: Cursors,
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
        Command::Open { linked, owner } => {
            if !context.config.sources.is_empty() {
                context.state()?;
            }
            let payload = OpenPayload {
                items: open_items_with_filters(&context.tree, linked, owner)?,
            };
            output::emit(context.json, "open", &payload, |payload| {
                print_open(&payload.items);
            })?;
            Ok(Outcome::Pass)
        }
        Command::Fetch { window_end, source } => run_fetch(
            &context,
            &FetchOptions {
                window_end: window_end.unwrap_or_else(Utc::now),
                only: source.clone(),
            },
        ),
        Command::Cursor { command } => run_cursor(&context, command),
        Command::Resolve { thread, pointers } => run_resolve(&context, pointers, *thread),
        Command::Cite {
            command: CiteCommand::Verify { live, files },
        } => run_verify(&context, files, *live),
        Command::Source {
            command: SourceCommand::Run { id, op },
        } => run_source(&context, id, *op),
        Command::State {
            command: StateCommand::Path,
        } => run_state_path(&context),
        Command::External(_) => Err(anyhow!("external command dispatch failed")),
    }
}

fn run_source(context: &Context, id: &str, op: SourceOperation) -> Result<Outcome> {
    let spec = context
        .config
        .sources
        .iter()
        .find(|source| source.id == id)
        .ok_or_else(|| anyhow!("source {id} is not configured"))?;
    let scheme = source::scheme(spec)?;
    let source = source::open(spec, &scheme)?;
    let source_ref = SourceRef {
        id: spec.id.clone(),
        kind: spec.kind.clone(),
        scheme: scheme.clone(),
    };
    let state_dir = context.state()?.source_state(id);
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let request = match op {
        SourceOperation::Fetch => SourceRequest::Fetch(FetchRequest {
            protocol: source::PROTOCOL_VERSION,
            source: source_ref,
            config: source::config_json(spec)?,
            window_end: Utc::now(),
            backfill_hours: context.config.ingest.backfill_hours,
            cursors: BTreeMap::new(),
            discovered: spec
                .discover_field
                .as_deref()
                .map_or_else(Vec::new, |field| {
                    source::discover_streams(&context.tree, field)
                }),
            state_dir,
        }),
        SourceOperation::Resolve => SourceRequest::Resolve(ResolveRequest {
            protocol: source::PROTOCOL_VERSION,
            source: source_ref,
            config: source::config_json(spec)?,
            pointers: Vec::new(),
            state_dir,
        }),
    };
    let schemes = source::configured_schemes(&context.config.sources)?;
    let mut session = Session::new(&request, &schemes);
    let run = source.run(&request, &mut |message| {
        println!("{}", serde_json::to_string(&message)?);
        session.accept(message).map(|_| ())
    });
    let outcome = session.finish(run);
    if let Some(error) = outcome.error.as_deref() {
        eprintln!("source {id}: {error}");
    }
    for (stream, result) in &outcome.streams {
        if let Some(error) = result.error.as_deref() {
            eprintln!("source {id} stream {stream}: {error}");
        }
    }
    Ok(if outcome.has_errors() {
        Outcome::SourceErrors
    } else {
        Outcome::Pass
    })
}

fn run_fetch(context: &Context, options: &FetchOptions) -> Result<Outcome> {
    let outcome = fetch::fetch(context, options)?;
    let payload = FetchPayload {
        window: outcome.window.id.as_str(),
        exit_status: outcome.manifest.exit_status,
        sources: &outcome.manifest.sources,
    };
    output::emit(context.json, "fetch", &payload, print_fetch)?;
    Ok(if outcome.errored.is_empty() {
        Outcome::Pass
    } else {
        Outcome::SourceErrors
    })
}

fn run_cursor(context: &Context, command: &CursorCommand) -> Result<Outcome> {
    match command {
        CursorCommand::Show => {
            let payload = CursorShowPayload {
                cursors: cursor::show(context)?,
            };
            output::emit(context.json, "cursor show", &payload, print_cursors)?;
        }
        CursorCommand::Advance { window } => {
            let state = context.state()?;
            let directory = windows::resolve_arg(window, &state.windows())?;
            let window = Window::open(directory)?;
            let payload = cursor::advance(context, &window)?;
            output::emit(context.json, "cursor advance", &payload, print_advance)?;
        }
    }
    Ok(Outcome::Pass)
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
    let payload = search::dossier(&context.tree, entity, &context.config)?;
    output::emit(context.json, "dossier", &payload, search::print_dossier)?;
    Ok(Outcome::Pass)
}

fn run_resolve(context: &Context, pointers: &[String], thread: bool) -> Result<Outcome> {
    let payload = ResolvePayload {
        results: resolve::resolve(context, pointers, thread)?,
    };
    let source_errors = payload.results.iter().any(|result| result.source_error);
    output::emit(context.json, "resolve", &payload, print_resolved)?;
    Ok(if source_errors {
        Outcome::SourceErrors
    } else {
        Outcome::Pass
    })
}

fn run_verify(context: &Context, files: &[PathBuf], live: bool) -> Result<Outcome> {
    let verification = resolve::verify(context, files, live)?;
    output::emit(
        context.json,
        "cite verify",
        &verification,
        print_verification,
    )?;
    Ok(if verification.source_errors {
        Outcome::SourceErrors
    } else if verification.broken.is_empty() {
        Outcome::Pass
    } else {
        Outcome::StructuralDefects
    })
}

fn run_state_path(context: &Context) -> Result<Outcome> {
    let state = context.state()?;
    let payload = StatePathPayload {
        state_dir: state.root().to_string_lossy().into_owned(),
        store: state.store().to_string_lossy().into_owned(),
    };
    output::emit(context.json, "state path", &payload, |payload| {
        println!("{}", payload.state_dir);
        println!("{}", payload.store);
    })?;
    Ok(Outcome::Pass)
}

fn plugin_environment(global: &GlobalArgs) -> Result<PluginEnv> {
    let user = UserConfig::load()?;
    let tree_root = resolve_plugin_tree(global, &user);
    let state_base = tree_root.as_deref().and_then(|root| {
        let config = TreeConfig::load(root).ok()?;
        config.id?;
        absolute_path(state_base(&user).ok()?).ok()
    });
    Ok(PluginEnv {
        tree_root,
        state_base,
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
    if let Some(state_base) = &environment.state_base {
        command.env("CTX_STATE_DIR", state_base);
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

fn parse_window_end(value: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| format!("window end must be RFC 3339: {error}"))
}

fn print_fetch(payload: &FetchPayload<'_>) {
    println!("{}", payload.window);
    for (source, manifest) in payload.sources {
        println!("{source}: {}", status_label(manifest.status));
    }
}

fn print_cursors(payload: &CursorShowPayload) {
    for (source, streams) in &payload.cursors.sources {
        for (stream, cursor) in streams {
            println!("{source} {stream} {} {}", cursor.cursor, cursor.window);
        }
    }
}

fn print_advance(payload: &AdvanceOutcome) {
    for advanced in &payload.advanced {
        println!(
            "advanced {} {} {}",
            advanced.source, advanced.stream, advanced.cursor
        );
    }
    for skipped in &payload.skipped {
        println!(
            "skipped {} {} {}",
            skipped.source, skipped.stream, skipped.why
        );
    }
}

const fn status_label(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::Error => "error",
    }
}

fn print_rules_findings(findings: &Findings) {
    if findings.rules_missing {
        println!("{}", "RULES — missing root rules.md".bold());
        println!();
    }
    if findings.rules_wrong_type {
        println!("{}", "RULES — rules.md must have type: rules".bold());
        println!();
    }
    if let Some(bytes) = findings.rules_oversize {
        println!(
            "{}",
            format!("RULES — rules.md exceeds 8192 bytes ({bytes})").bold()
        );
        println!();
    }
    if !findings.schema_violations.is_empty() {
        println!(
            "{}",
            format!(
                "SCHEMAS — records that do not match their type ({})",
                findings.schema_violations.len()
            )
            .bold()
        );
        for violation in &findings.schema_violations {
            println!(
                "  {}{} — {}",
                violation.path, violation.pointer, violation.message
            );
        }
        println!();
    }
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

    if !findings.ambiguous.is_empty() {
        println!(
            "{}",
            format!(
                "AMBIGUOUS — links with multiple files ({})",
                findings.ambiguous.len()
            )
            .bold()
        );
        for ambiguous in &findings.ambiguous {
            println!(
                "  {}  <- {}  candidates={}",
                ambiguous.target.yellow(),
                ambiguous.from.dimmed(),
                ambiguous.candidates.join(", ")
            );
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

    print_rules_findings(findings);

    if findings.clean() {
        println!("{}", "no findings".green());
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

fn print_open(items: &[OpenItemView]) {
    if items.is_empty() {
        println!("nothing open");
        return;
    }
    let today = today_utc();
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

fn print_resolved(payload: &ResolvePayload) {
    for result in &payload.results {
        println!("{}", result.input);
        if let Some(error) = result.error.as_deref() {
            println!("  error: {error}");
        } else if let Some(text) = result.text.as_deref() {
            println!("{text}");
        } else {
            println!("  not found");
        }
        println!();
    }
}

fn print_verification(verification: &Verification) {
    println!("checked {}", verification.checked);
    for broken in &verification.broken {
        println!(
            "{}:{}: {} — {}",
            broken.file, broken.line, broken.pointer, broken.why
        );
    }
    for (scheme, skipped) in &verification.skipped {
        println!("skipped {scheme}: {skipped}");
    }
}
