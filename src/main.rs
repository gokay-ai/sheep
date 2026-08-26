//! Command-line entry point. All behaviour lives in the library.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sheep::ops::{self, SnapMeta};
use sheep::repo::{self, Worktree, DEFAULT_MAX_FILES};
use sheep::shadow::RestorePlan;
use sheep::store::{Store, TurnKind};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "sheep", version, about = "Undo for AI coding agents.")]
struct Cli {
    /// Worktree to operate on. Defaults to the current directory.
    #[arg(long, short = 'C', global = true)]
    repo: Option<PathBuf>,
    /// Timeline to record against. One per agent pane; `default` when standalone.
    ///
    /// Also read from `SHEEP_LINE`, which is how a herdr pane learns which
    /// agent's timeline it belongs to: a pane's command line is fixed in the
    /// plugin manifest, so the only way to tell one dock from another is the
    /// environment the pane is opened with.
    #[arg(long, global = true, env = "SHEEP_LINE", default_value = "default")]
    line: String,
    /// Refuse to touch a worktree with more tracked files than this.
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_FILES)]
    max_files: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record the working tree as a new turn.
    Snap {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// Record even when nothing changed since the last turn.
        #[arg(long)]
        allow_empty: bool,
    },
    /// List recorded turns, newest first.
    Log {
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
    },
    /// Show what restoring a turn would do. Never writes anything.
    Diff { target: String },
    /// Restore the working tree to a turn. Dry run unless --yes is given.
    Restore {
        /// Turn to restore, as `#3` or a commit id.
        target: String,
        /// Apply the restore. Without this, only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
    /// Report whether this worktree is safe for Sheep to record and restore.
    Doctor,
    /// Shorten recorded history to what the retention policy allows.
    ///
    /// Dry run unless --yes is given. Rebuilds the kept turns as a fresh chain
    /// so the dropped ones can actually be collected; every kept turn still
    /// restores to exactly the same files.
    Gc {
        /// Newest turns to keep on each timeline.
        #[arg(long, default_value_t = 500)]
        keep: usize,
        /// Also drop turns older than this many days. 0 disables the age rule.
        #[arg(long, default_value_t = 30)]
        max_age_days: u64,
        /// Every timeline for this worktree, not just --line.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Watch the herdr session and record a turn whenever an agent finishes one.
    Watch(sheep::herdr::WatchArgs),
    /// Open Sheep's terminal interface: the timeline dock, or the rewind picker.
    Ui(sheep::tui::UiArgs),
}

fn main() {
    if let Err(err) = run() {
        eprintln!("sheep: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = match &cli.repo {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    // `watch` supervises a whole session rather than one checkout, and `ui`
    // resolves its own repository, so neither requires the current directory to
    // be a worktree.
    match &cli.command {
        Command::Watch(args) => return sheep::herdr::cli::run(args),
        Command::Ui(args) => return sheep::tui::cli::run(args, cli.repo.as_deref(), &cli.line),
        _ => {}
    }

    let wt = Worktree::discover(&cwd)?;
    let state = repo::state_dir()?;
    let line = &cli.line;

    match &cli.command {
        Command::Doctor => doctor(&wt, &state, cli.max_files),

        Command::Snap { agent, pane, note, allow_empty } => {
            let meta = SnapMeta {
                agent: agent.clone(),
                pane_id: pane.clone(),
                note: note.clone(),
                prompt: None,
            };
            match ops::snap(&wt, &state, line, cli.max_files, TurnKind::Manual, meta, *allow_empty)?
            {
                Some(turn) => println!("{}  {}", ops::short(&turn.commit), turn.subject()),
                None => println!("nothing changed since the last turn"),
            }
            Ok(())
        }

        Command::Log { limit } => {
            let turns = Store::open(&state, &wt.id, line)?.all()?;
            if turns.is_empty() {
                println!("no turns recorded on `{line}` yet");
                return Ok(());
            }
            for turn in turns.iter().rev().take(*limit) {
                println!(
                    "#{:<4} {}  {:<10} {:>4} files  +{:<6} -{:<6} {}",
                    turn.seq,
                    ops::short(&turn.commit),
                    turn.kind.label(),
                    turn.files,
                    turn.insertions,
                    turn.deletions,
                    turn.note.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }

        Command::Diff { target } => {
            let planned = ops::plan(&wt, &state, line, target, cli.max_files)?;
            print_plan(&planned.plan, &planned.commit);
            Ok(())
        }

        Command::Restore { target, yes } => {
            let planned = ops::plan(&wt, &state, line, target, cli.max_files)?;
            print_plan(&planned.plan, &planned.commit);
            if planned.plan.is_noop() {
                return Ok(());
            }
            if !yes {
                println!("\ndry run. re-run with --yes to apply.");
                return Ok(());
            }
            let done = ops::restore(&wt, &state, line, target, cli.max_files)?;
            println!("\nrestored {} path(s).", done.plan.touched());
            if let Some(note) = &done.bookkeeping_error {
                println!(
                    "note: your files are restored, but recording the new turn failed ({note})."
                );
            }
            if let Some(cp) = done.checkpoint {
                println!(
                    "previous state kept as turn #{} — `sheep restore #{}` puts it back.",
                    cp.seq, cp.seq
                );
            }
            Ok(())
        }

        Command::Gc { keep, max_age_days, all, yes } => {
            let policy = ops::Retention {
                keep: *keep,
                max_age_days: (*max_age_days > 0).then_some(*max_age_days),
            };
            let reports = if *all {
                ops::collect_all(&wt, &state, policy, *yes)?
            } else {
                vec![ops::collect(&wt, &state, line, policy, *yes)?]
            };
            let dropped: usize = reports.iter().map(|r| r.dropped).sum();
            for r in &reports {
                println!("{:<24} {} kept, {} dropped", r.line, r.kept, r.dropped);
            }
            if dropped == 0 {
                println!("nothing to collect");
            } else if *yes {
                let before: u64 = reports.first().map(|r| r.bytes_before).unwrap_or(0);
                let after: u64 = reports.last().map(|r| r.bytes_after).unwrap_or(0);
                println!("\nshadow repository {} -> {}", human(before), human(after));
            } else {
                println!("\ndry run. re-run with --yes to apply.");
            }
            Ok(())
        }

        Command::Watch(_) | Command::Ui(_) => unreachable!("handled above"),
    }
}

fn doctor(wt: &Worktree, state: &Path, max_files: usize) -> Result<()> {
    let health = repo::inspect(wt, max_files)?;
    println!("worktree   {}", wt.root.display());
    println!("id         {}", wt.id);
    println!("kind       {}", if wt.is_linked() { "linked worktree" } else { "main checkout" });
    println!("objects    {}", wt.objects_dir().display());
    println!("state      {}", state.display());
    println!("tracked    {} files", health.tracked_files);
    for warning in &health.warnings {
        println!("note       {warning}");
    }
    if health.is_safe() {
        println!("status     ready");
        Ok(())
    } else {
        for blocker in &health.blockers {
            println!("blocked    {blocker}");
        }
        bail!("this worktree is not safe to record right now");
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn print_plan(plan: &RestorePlan, commit: &str) {
    if plan.is_noop() {
        println!("already at {}: nothing to restore", ops::short(commit));
        return;
    }
    println!(
        "restore to {}  ·  {} file(s) written, {} removed",
        ops::short(commit),
        plan.write.len(),
        plan.remove.len()
    );
    for path in &plan.write {
        println!("  write   {path}");
    }
    for path in &plan.remove {
        println!("  remove  {path}");
    }
}
