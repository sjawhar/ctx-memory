//! Command-line parsing and dispatch for read-only tree commands.

use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize as _;

use crate::open::{open_items, OpenItem};
use crate::search;
use crate::sweep::{self, Findings};
use crate::tree::Tree;

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

#[derive(Subcommand)]
pub enum Command {
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
}

pub enum Outcome {
    Pass,
    StructuralDefects,
}

impl Outcome {
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::StructuralDefects => 1,
        }
    }
}

/// Runs the selected read command against the requested tree.
///
/// # Errors
///
/// Returns an error when the tree cannot be loaded or a command cannot complete.
pub fn run(cli: &Cli) -> Result<Outcome> {
    let tree_root = cli
        .memory
        .clone()
        .unwrap_or_else(|| PathBuf::from("memory"));
    let tree = Tree::load(&tree_root)?;
    match &cli.command {
        Command::Sweep => {
            let findings = sweep::run(&tree)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else {
                print_sweep(&findings);
            }
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
            let hits = search::run(&tree, query, *verbatim, *limit);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                search::print(&hits, query.split_whitespace().count());
            }
            Ok(Outcome::Pass)
        }
        Command::Dossier { entity } => {
            let pages = search::dossier(&tree, entity);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&pages)?);
            } else {
                search::print_dossier(entity, &pages);
            }
            Ok(Outcome::Pass)
        }
        Command::Open => {
            let items = open_items(&tree);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                print_open(&items);
            }
            Ok(Outcome::Pass)
        }
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
