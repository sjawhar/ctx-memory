//! Command-line parsing and dispatch for tree commands.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::context::Context;
use crate::cursor::{self, AdvanceOutcome, Cursors};
use crate::delta;
use crate::fetch::{self, FetchOptions};
use crate::git;
use crate::lease::{self, Lease};
use crate::marker::{self, Marker};
use crate::open::{
    self, apply_staleness, open_items_with_filters, parse_linked, LinkedFilter, OpenFilter,
    OpenItemView,
};
use crate::output::{self, Outcome, Sink};
use crate::plugin::{run_external, PluginEnv};
use crate::redact;
use crate::remote;
use crate::resolve::{self, Resolved};
use crate::schema::today_utc;
use crate::search;
use crate::server;
use crate::slack_login;
use crate::source::{
    self, FetchRequest, Request as SourceRequest, ResolveRequest, Session, SourceRef,
};
use crate::sweep;
use crate::tree::Doc;
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
        /// Keep the items that wait on one of these people, matched as `--owner` matches.
        #[arg(long = "waiting-on", value_name = "NAME", value_delimiter = ',')]
        waiting_on: Vec<String>,
        /// Keep the items whose `revisit_by` or `due` date has passed.
        #[arg(long)]
        overdue: bool,
    },
    /// Fetch all configured sources into a durable window.
    Fetch {
        #[arg(long, value_parser = parse_window_end)]
        window_end: Option<DateTime<Utc>>,
        #[arg(long = "source", value_name = "ID", num_args = 1..)]
        source: Vec<String>,
    },
    /// Inspect or write a window's record delta.
    Delta {
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        write: bool,
    },

    /// Inspect durable source cursors.
    Cursor {
        #[command(subcommand)]
        command: CursorCommand,
    },
    /// List the completed windows the agent has not handled yet.
    Pending {
        /// Handle at most this many windows, oldest first, and leave the rest for the next
        /// cycle. Omitted, every pending window is listed.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Inspect or advance the agent marker.
    Marker {
        #[command(subcommand)]
        command: MarkerCommand,
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
    /// Show a record, or write or remove one through the server.
    Record {
        #[command(subcommand)]
        command: RecordCommand,
    },
    /// Sign in to Slack and save the credential for the fetch job.
    Slack {
        #[command(subcommand)]
        command: SlackCommand,
    },
    /// Serve read commands and record writes over HTTP.
    Serve(server::ServeArgs),
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Subcommand)]
pub enum SlackCommand {
    /// Sign in through the browser and save the Slack credential.
    Login,
}

#[derive(Subcommand)]
pub enum RecordCommand {
    /// Print a record's whole text, frontmatter and body, as the tree holds it.
    Show {
        /// Tree-relative path of the record, such as `decisions/renewal.md`.
        path: String,
    },
    /// Write a record, reading it from standard input or from a file.
    Put {
        /// Tree-relative path of the record, such as `decisions/renewal.md`.
        path: String,
        /// `-` reads the record from standard input; anything else names a file.
        #[arg(default_value = "-")]
        source: String,
    },
    /// Remove a record.
    Rm {
        /// Tree-relative path of the record.
        path: String,
    },
}

#[derive(Subcommand)]
pub enum CiteCommand {
    /// Verify pointer resolution and quoted text. Exits 1 when a quote is broken or nothing
    /// could check it.
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

#[derive(Clone, Copy, Debug, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SourceOperation {
    Fetch,
    Resolve,
}

#[derive(Subcommand)]
pub enum StateCommand {
    /// Print the state directory, SQLite store path, and data directory.
    Path,
}

#[derive(Subcommand)]
pub enum CursorCommand {
    /// Print every stored source-stream cursor.
    Show,
}

#[derive(Subcommand)]
pub enum MarkerCommand {
    /// Print the window the agent has handled up to.
    Show,
    /// Move the marker to a completed window after the commit that handled it.
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
            Self::Delta { .. } => "delta".to_owned(),
            Self::Cursor {
                command: CursorCommand::Show,
            } => "cursor show".to_owned(),
            Self::Pending { .. } => "pending".to_owned(),
            Self::Marker {
                command: MarkerCommand::Show,
            } => "marker show".to_owned(),
            Self::Marker {
                command: MarkerCommand::Advance { .. },
            } => "marker advance".to_owned(),
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
            Self::Record {
                command: RecordCommand::Show { .. },
            } => "record show".to_owned(),
            Self::Record {
                command: RecordCommand::Put { .. },
            } => "record put".to_owned(),
            Self::Record {
                command: RecordCommand::Rm { .. },
            } => "record rm".to_owned(),
            Self::Slack {
                command: SlackCommand::Login,
            } => "slack login".to_owned(),
            Self::Serve(_) => "serve".to_owned(),
            Self::External(arguments) => arguments
                .first()
                .and_then(|argument| argument.to_str())
                .map_or_else(|| "external".to_owned(), str::to_owned),
        }
    }

    /// How this command reaches a configured server: the one place that decides it.
    ///
    /// The server's `/v1/command` runs reads on behalf of a caller that holds no write lease
    /// and owns none of the files a write touches, so it runs only what answers
    /// [`Remote::Forward`] here, and the client forwards only that. Each variant says so for
    /// itself, and a new one has to decide: there is no catch-all arm that would let the next
    /// writing flag through the read door the way `delta --write` did.
    #[must_use]
    pub fn remote(&self) -> Remote<'_> {
        match self {
            Self::Rules
            | Self::Sweep
            | Self::Search { .. }
            | Self::Dossier { .. }
            | Self::Open { .. }
            | Self::Pending { .. }
            | Self::Resolve { .. }
            | Self::Marker {
                command: MarkerCommand::Show,
            }
            | Self::Cite {
                command: CiteCommand::Verify { .. },
            }
            | Self::Record {
                command: RecordCommand::Show { .. },
            }
            // Without `--write` the command prints the delta and writes nothing.
            | Self::Delta { write: false, .. } => Remote::Forward,
            Self::Record {
                command: RecordCommand::Put { path, source },
            } => Remote::Put { path, source },
            Self::Record {
                command: RecordCommand::Rm { path },
            } => Remote::Rm { path },
            Self::Marker {
                command: MarkerCommand::Advance { window },
            } => Remote::MarkerAdvance { window },
            // `--write` replaces the window's `delta.json`, `delta.md` and
            // `open-snapshot.json`, which the fetch job owns.
            Self::Delta { write: true, .. }
            // A fetch moves cursors and publishes a window, and a source run's scratch state,
            // the cursors and the state paths are this machine's own.
            | Self::Fetch { .. }
            | Self::Source { .. }
            | Self::Cursor { .. }
            | Self::State { .. }
            | Self::Slack { .. }
            | Self::Serve(_)
            // What an external subcommand does is its own business.
            | Self::External(_) => Remote::Local,
        }
    }
}

/// Where one command runs when a server is configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote<'a> {
    /// This machine's own work, never sent to a server, and refused by one however it is
    /// spelled.
    Local,
    /// A read the server runs through `/v1/command`, answering with what it printed.
    Forward,
    /// `record put`: the record goes to `/v1/records/put`, read from `source` (`-` is
    /// standard input).
    Put { path: &'a str, source: &'a str },
    /// `record rm`: `/v1/records/delete`.
    Rm { path: &'a str },
    /// `marker advance`: `/v1/marker/advance`, under the server's write lease.
    MarkerAdvance { window: &'a str },
}

#[derive(Serialize)]
struct RulesPayload<'a> {
    path: String,
    bytes: usize,
    text: &'a str,
}

#[derive(Serialize)]
struct RecordShowPayload<'a> {
    path: String,
    text: &'a str,
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
    data_dir: String,
}

#[derive(Serialize)]
struct FetchPayload<'a> {
    window: &'a str,
    exit_status: Status,
    sources: &'a BTreeMap<String, SourceManifest>,
    delta: FetchDeltaPayload,
    cursors: &'a AdvanceOutcome,
}

#[derive(Serialize)]
struct FetchDeltaPayload {
    summary: String,
    new_messages: usize,
}

#[derive(Serialize)]
struct CursorShowPayload {
    cursors: Cursors,
}

#[derive(Serialize)]
struct MarkerShowPayload {
    marker: Option<Marker>,
}

#[derive(Serialize)]
struct SourceRunPayload<'a> {
    source: &'a str,
    op: SourceOperation,
    messages: Vec<source::Message>,
    outcome: source::SourceOutcome,
}

/// Runs the selected command, writing its output to `sink`.
///
/// # Errors
///
/// Returns an error when tree discovery, loading, command execution, or external dispatch fails.
pub fn run(cli: &Cli, sink: &mut Sink<'_>) -> Result<Outcome> {
    if let Command::External(arguments) = &cli.command {
        let environment = PluginEnv::resolve(&GlobalArgs::from(cli))?;
        return run_external(arguments, &environment).map(|never| match never {});
    }

    // `slack login` signs in and saves the credential; it loads no tree and reaches no server.
    if let Command::Slack {
        command: SlackCommand::Login,
    } = &cli.command
    {
        return slack_login::run(sink, cli.json);
    }

    if let Some(outcome) = remote::run(cli, sink)? {
        return Ok(outcome);
    }

    if let Some(root) = cli.memory.as_deref() {
        git::clone_when_empty(root)?;
    }
    let context = Context::load(&GlobalArgs::from(cli))?;
    if let Command::Serve(arguments) = &cli.command {
        return server::serve(context, arguments, sink);
    }
    run_loaded(cli, &context, sink)
}

/// Runs one parsed command against an already loaded tree.
///
/// # Errors
///
/// Returns an error when the command fails.
pub fn run_loaded(cli: &Cli, context: &Context, sink: &mut Sink<'_>) -> Result<Outcome> {
    run_loaded_with(cli, context, &[], sink)
}

/// Runs one parsed command against an already loaded tree, with drafts a remote caller sent
/// for `cite verify` to check as text: each stands for the file the caller named by its path.
///
/// # Errors
///
/// Returns an error when the command fails.
pub fn run_loaded_with(
    cli: &Cli,
    context: &Context,
    drafts: &[Doc],
    sink: &mut Sink<'_>,
) -> Result<Outcome> {
    dispatch(
        &cli.command,
        &mut Run {
            context,
            drafts,
            json: cli.json,
            sink,
        },
    )
}

/// One command's execution environment: the loaded tree, the drafts a remote caller sent, the
/// output format, and where the output goes.
pub struct Run<'ctx, 'sink> {
    context: &'ctx Context,
    drafts: &'ctx [Doc],
    json: bool,
    sink: &'ctx mut Sink<'sink>,
}

impl Run<'_, '_> {
    fn emit<T: Serialize>(
        &mut self,
        command: &str,
        data: &T,
        human: impl FnOnce(&T, &mut dyn Write) -> std::io::Result<()>,
    ) -> Result<()> {
        output::emit(self.sink, self.json, command, data, human)
    }
}

/// Runs one built-in command against an already loaded tree.
///
/// # Errors
///
/// Returns an error when the command fails.
fn dispatch(command: &Command, run: &mut Run<'_, '_>) -> Result<Outcome> {
    match command {
        Command::Rules => run_rules(run),
        Command::Sweep => {
            let findings = sweep::run(&run.context.tree)?;
            run.emit("sweep", &findings, sweep::print)?;
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
            let found = search::run(
                run.context.index()?,
                &run.context.tree,
                query,
                *verbatim,
                *limit,
            )?;
            run.emit("search", &found, search::print)?;
            Ok(Outcome::Pass)
        }
        Command::Dossier { entity } => run_dossier(run, entity),
        Command::Open {
            linked,
            owner,
            waiting_on,
            overdue,
        } => {
            let stale = if run.context.config.sources.is_empty() {
                BTreeMap::new()
            } else {
                delta::staleness(&run.context.data()?.windows(), &run.context.tree)?
            };
            let filter = OpenFilter {
                linked,
                owners: owner,
                waiting_on,
                overdue: *overdue,
            };
            let mut items = open_items_with_filters(&run.context.tree, &filter, today_utc())?;
            apply_staleness(&mut items, &stale);
            let payload = OpenPayload { items };
            run.emit("open", &payload, |payload, out| {
                open::print(&payload.items, out)
            })?;
            Ok(Outcome::Pass)
        }
        Command::Fetch { window_end, source } => run_fetch(
            run,
            &FetchOptions {
                window_end: window_end.unwrap_or_else(Utc::now),
                only: source.clone(),
            },
        ),
        Command::Delta { window, write } => run_delta(run, window.as_deref(), *write),
        Command::Cursor { command } => run_cursor(run, command),
        Command::Pending { limit } => run_pending(run, *limit),
        Command::Marker { command } => run_marker(run, command),
        Command::Resolve { thread, pointers } => run_resolve(run, pointers, *thread),
        Command::Cite {
            command: CiteCommand::Verify { live, files },
        } => run_verify(run, files, *live),
        Command::Source {
            command: SourceCommand::Run { id, op },
        } => run_source(run, id, *op),
        Command::State {
            command: StateCommand::Path,
        } => run_state_path(run),
        Command::Record {
            command: RecordCommand::Show { path },
        } => run_record_show(run, path),
        Command::Record { .. } => Err(anyhow!(
            "ctx record put and ctx record rm write through the server; set CTX_SERVER or \
             server in the user config"
        )),
        Command::Slack { .. } => Err(anyhow!(
            "slack login is dispatched before the tree commands"
        )),
        Command::Serve(_) => Err(anyhow!("serve is dispatched before the tree commands")),
        Command::External(_) => Err(anyhow!("external command dispatch failed")),
    }
}

/// Parses a command line that does not carry the program name.
///
/// # Errors
///
/// Returns clap's error, which carries the message a local run would print.
pub fn parse_argv(argv: &[String]) -> std::result::Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("ctx".to_owned()).chain(argv.iter().cloned()))
}

/// Parses a command line, program name first, runs it, and reports the result the one way
/// `ctx` reports anything; returns the exit code.
///
/// - Help and version text are clap's answer, not an error: stdout, exit 0.
/// - A command line clap refuses: with `--json`, an error envelope on stdout; otherwise
///   clap's message on stderr. Exit 2.
/// - A command that fails: the same two forms, with the error chain. Exit 2.
///
/// The binary and the server's `/v1/command` both report through this, so a remote caller
/// cannot be answered differently from a local one.
pub fn run_reported<I, T>(
    argv: I,
    sink: &mut Sink<'_>,
    run: impl FnOnce(&Cli, &mut Sink<'_>) -> Result<Outcome>,
) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(error) => {
            let message = error.to_string();
            if !error.use_stderr() {
                let _ = write!(sink.out, "{message}");
                return u8::try_from(error.exit_code()).unwrap_or(2);
            }
            if argv.iter().any(|argument| argument == "--json") {
                report_error(sink, "ctx", message);
            } else {
                let _ = write!(sink.err, "{message}");
            }
            return 2;
        }
    };
    match run(&cli, sink) {
        Ok(outcome) => outcome.exit_code(),
        Err(error) => {
            let message = format!("{error:#}");
            if cli.json {
                report_error(sink, &cli.command.name(), message);
            } else if matches!(cli.command, Command::Serve(_) | Command::Fetch { .. }) {
                // The server and the fetch job run on Cloud Run, where each line of standard
                // error is its own log entry, and every other line they write there is one
                // line: so is the error that stops them, though it carries git's words or a
                // list of a tree's files.
                let _ = writeln!(sink.err, "{}", redact::flattened(&message));
            } else {
                // An error can carry a server's or a source's own words.
                let _ = writeln!(sink.err, "{}", output::printable(&message));
            }
            2
        }
    }
}

/// Writes an error envelope to stdout, or the reason it could not be written to stderr.
fn report_error(sink: &mut Sink<'_>, command: &str, message: String) {
    if let Err(error) = output::emit_error(&mut *sink.out, command, message) {
        let _ = writeln!(sink.err, "{error:#}");
    }
}

fn run_source(run: &mut Run<'_, '_>, id: &str, op: SourceOperation) -> Result<Outcome> {
    let context = run.context;
    let spec = context
        .config
        .sources
        .iter()
        .find(|source| source.id == id)
        .ok_or_else(|| anyhow!("source {id} is not configured"))?;
    if matches!(op, SourceOperation::Fetch) && spec.kind == "slack" {
        return Err(anyhow!(
            "ctx source run {id} --op fetch is refused: only ctx fetch may rotate the Slack credential"
        ));
    }
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
    let mut messages = Vec::new();
    let json = run.json;
    let stream = &mut *run.sink.out;
    let outcome = source.run(&request, &mut |message| {
        if !json {
            output::write_json(&mut *stream, &message)?;
            writeln!(stream)?;
        }
        session.accept(message.clone()).map(|_| {
            messages.push(message);
        })
    });
    let payload = SourceRunPayload {
        source: id,
        op,
        messages,
        outcome: session.finish(outcome),
    };
    if let Some(error) = payload.outcome.error.as_deref() {
        writeln!(run.sink.err, "source {id}: {}", output::printable(error))?;
    }
    for (stream, result) in &payload.outcome.streams {
        if let Some(error) = result.error.as_deref() {
            writeln!(
                run.sink.err,
                "source {id} stream {}: {}",
                output::printable(stream),
                output::printable(error)
            )?;
        }
    }
    run.emit("source run", &payload, |_, _| Ok(()))?;
    Ok(if payload.outcome.has_errors() {
        Outcome::SourceErrors
    } else {
        Outcome::Pass
    })
}

fn run_fetch(run: &mut Run<'_, '_>, options: &FetchOptions) -> Result<Outcome> {
    let outcome = fetch::fetch(run.context, options)?;
    let payload = FetchPayload {
        window: outcome.window.id.as_str(),
        exit_status: outcome.manifest.exit_status,
        sources: &outcome.manifest.sources,
        delta: FetchDeltaPayload {
            summary: delta::summary(&outcome.delta),
            new_messages: outcome.delta.new_messages.len(),
        },
        cursors: &outcome.cursors,
    };
    run.emit("fetch", &payload, print_fetch)?;
    Ok(if outcome.errored.is_empty() {
        Outcome::Pass
    } else {
        Outcome::SourceErrors
    })
}

fn run_delta(run: &mut Run<'_, '_>, requested: Option<&str>, write: bool) -> Result<Outcome> {
    // `--write` replaces the window's `delta.json`, `delta.md` and `open-snapshot.json`, which
    // the fetch writes under its lease, on the share every machine reads. So a write takes the
    // fetch lease first, and computes nothing while another copy holds it.
    let lease = if write {
        lease::for_role("fetch").context(
            "ctx delta --write replaces files the fetch writes, so it takes the fetch lease",
        )?
    } else {
        None
    };
    let context = run.context;
    let data = context.data()?;
    let directory = windows::resolve_arg(requested.unwrap_or("newest"), &data.windows())?;
    let window = Window::open(directory)?;
    let previous = windows::previous(&data.windows(), &window.id)?;
    let schemes = source::configured_schemes(&context.config.sources)?;
    let owners = delta::Owners::discover(&context.tree, &context.config.sources);
    let snapshot = delta::open_snapshot(&context.tree, Utc::now());
    let computed = delta::compute(
        &context.tree,
        &schemes,
        &owners,
        &window,
        previous.as_ref(),
        snapshot.generated_at,
        context.embedding()?.embedder(),
    )?;
    if write {
        // The compute can wait on Vertex AI for longer than the lease survives unrenewed.
        lease
            .as_ref()
            .map_or(Ok(()), Lease::check)
            .context("before writing the delta")?;
        delta::write(&window, &snapshot, &computed)?;
        give_back(lease, run.sink.err);
    }
    // A delta quotes each new message, which is a source's text.
    run.emit("delta", &computed, |delta, out| {
        writeln!(out, "{}", output::printable(&delta::render(delta)))
    })?;
    Ok(Outcome::Pass)
}

/// Gives up a lease a local command took for its write, so the role's own copy can take it
/// at once. A failure costs that copy the takeover wait and nothing else.
fn give_back(lease: Option<Lease>, err: &mut dyn Write) {
    if let Some(lease) = lease {
        if let Err(error) = lease.release() {
            let _ = writeln!(
                err,
                "releasing the lease: {}",
                redact::one_line(&format!("{error:#}"))
            );
        }
    }
}

fn run_cursor(run: &mut Run<'_, '_>, command: &CursorCommand) -> Result<Outcome> {
    match command {
        CursorCommand::Show => {
            let payload = CursorShowPayload {
                cursors: cursor::show(run.context)?,
            };
            run.emit("cursor show", &payload, |payload, out| {
                cursor::print_cursors(&payload.cursors, out)
            })?;
        }
    }
    Ok(Outcome::Pass)
}

fn run_pending(run: &mut Run<'_, '_>, limit: Option<usize>) -> Result<Outcome> {
    let payload = marker::pending(run.context, limit)?;
    run.emit("pending", &payload, marker::print_pending)?;
    Ok(Outcome::Pass)
}

fn run_marker(run: &mut Run<'_, '_>, command: &MarkerCommand) -> Result<Outcome> {
    match command {
        MarkerCommand::Show => {
            let payload = MarkerShowPayload {
                marker: marker::show(run.context)?,
            };
            run.emit("marker show", &payload, |payload, out| {
                marker::print(payload.marker.as_ref(), out)
            })?;
        }
        MarkerCommand::Advance { window } => {
            // The marker is on the share, and the server moves it under its write lease. A
            // move made here takes that lease while it writes, and moves nothing while another
            // copy, such as a running server, holds it.
            let lease = lease::for_role("server").context(
                "ctx marker advance moves the marker the server moves, so it takes the server lease",
            )?;
            let moved = marker::advance(run.context, window, &marker::caller());
            give_back(lease, run.sink.err);
            let payload = MarkerShowPayload {
                marker: Some(moved?),
            };
            run.emit("marker advance", &payload, |payload, out| {
                marker::print(payload.marker.as_ref(), out)
            })?;
        }
    }
    Ok(Outcome::Pass)
}

fn run_rules(run: &mut Run<'_, '_>) -> Result<Outcome> {
    let document = run
        .context
        .tree
        .rules()
        .ok_or_else(|| anyhow!("rules.md is missing from the tree root"))?;
    let payload = RulesPayload {
        path: document.rel.to_string_lossy().into_owned(),
        bytes: document.raw.len(),
        text: &document.raw,
    };
    // The rules page is the tree's text, printed without its control characters.
    run.emit("rules", &payload, |payload, out| {
        writeln!(out, "{}", output::printable(&payload.path))?;
        write!(out, "\n{}", output::printable(payload.text))
    })?;
    Ok(Outcome::Pass)
}

fn run_record_show(run: &mut Run<'_, '_>, path: &str) -> Result<Outcome> {
    let document = run.context.tree.record(path)?;
    let payload = RecordShowPayload {
        path: document.rel.to_string_lossy().into_owned(),
        text: &document.raw,
    };
    // The record is the tree's text, printed whole without its control characters.
    run.emit("record show", &payload, |payload, out| {
        write!(out, "{}", output::printable(payload.text))
    })?;
    Ok(Outcome::Pass)
}

fn run_dossier(run: &mut Run<'_, '_>, entity: &str) -> Result<Outcome> {
    let payload = search::dossier(&run.context.tree, entity, &run.context.config)?;
    run.emit("dossier", &payload, search::print_dossier)?;
    Ok(Outcome::Pass)
}

fn run_resolve(run: &mut Run<'_, '_>, pointers: &[String], thread: bool) -> Result<Outcome> {
    let payload = ResolvePayload {
        results: resolve::resolve(run.context, pointers, thread)?,
    };
    let source_errors = payload.results.iter().any(|result| result.source_error);
    run.emit("resolve", &payload, |payload, out| {
        resolve::print_resolved(&payload.results, out)
    })?;
    Ok(if source_errors {
        Outcome::SourceErrors
    } else {
        Outcome::Pass
    })
}

fn run_verify(run: &mut Run<'_, '_>, files: &[PathBuf], live: bool) -> Result<Outcome> {
    let verification = resolve::verify(run.context, files, run.drafts, live)?;
    run.emit("cite verify", &verification, resolve::print_verification)?;
    // A quote nothing could check is not a checked one, and the writer refuses it as it
    // refuses a broken one: the exit code says so to a cycle that reads only the code.
    Ok(if verification.source_errors {
        Outcome::SourceErrors
    } else if verification.broken.is_empty() && verification.unchecked.is_empty() {
        Outcome::Pass
    } else {
        Outcome::StructuralDefects
    })
}

fn run_state_path(run: &mut Run<'_, '_>) -> Result<Outcome> {
    let state = run.context.state()?;
    let payload = StatePathPayload {
        state_dir: state.root().to_string_lossy().into_owned(),
        store: state.store().to_string_lossy().into_owned(),
        data_dir: run.context.data()?.root().to_string_lossy().into_owned(),
    };
    run.emit("state path", &payload, |payload, out| {
        writeln!(out, "{}", payload.state_dir)?;
        writeln!(out, "{}", payload.store)?;
        writeln!(out, "{}", payload.data_dir)
    })?;
    Ok(Outcome::Pass)
}

fn parse_window_end(value: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| format!("window end must be RFC 3339: {error}"))
}

fn print_fetch(payload: &FetchPayload<'_>, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "{}", payload.window)?;
    for (source, manifest) in payload.sources {
        write!(
            out,
            "{}: {}",
            output::printable(source),
            manifest.status.label()
        )?;
        // Why the source failed, on the same line with any credential taken out, so the fetch
        // job's log says what to do and not only that it failed (a revoked Slack token names
        // `ctx slack login`). Each stream's own error stays in the window's manifest.
        if let Some(error) = &manifest.error {
            write!(out, ": {}", crate::redact::one_line(error))?;
        }
        writeln!(out)?;
    }
    cursor::print_advance(payload.cursors, out)
}
