//! `sheep watch` — the recorder's command-line surface.
//!
//! The command itself is a supervisor. All it does is hold a subscription open,
//! hand it to the [`Recorder`], and put it back together when herdr takes it
//! away — a session shutdown, a live handoff, a socket that moved. It is meant
//! to be started once and left alone for a day.

use super::detect::Tuning;
use super::log::Log;
use super::recorder::{Config, Ended, LineBy, LiveSource, Recorder};
use super::session::Live;
use super::supervise::{Next, Supervisor};
use super::wire;
use crate::repo::{self, DEFAULT_MAX_FILES};
use anyhow::{bail, Result};
use clap::Args;
use std::time::{Duration, Instant};

/// How often to re-read herdr's own view of every agent, healing whatever the
/// stream missed while we were reconnecting.
const RECONCILE_EVERY: Duration = Duration::from_secs(30);

#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    /// Print detected turn boundaries instead of recording them.
    #[arg(long)]
    pub dry_run: bool,

    /// Seconds a pane must sit still — no status change, no new output —
    /// before a boundary is believed.
    ///
    /// The default is measured, not guessed: a live herdr 0.8.0 session was
    /// watched flipping a pane to `done` and back to `working` 9.2 seconds
    /// later while the agent was still mid-turn. Anything shorter records that
    /// as a turn.
    #[arg(long, default_value_t = 10.0, value_name = "SECONDS")]
    pub settle: f64,

    /// Stop extending the quiet window for a pane that will not stop painting,
    /// and decide on corroboration alone.
    #[arg(long, default_value_t = 120, value_name = "SECONDS")]
    pub patience: u64,

    /// How a pane's timeline is named.
    #[arg(long, value_enum, default_value_t = LineBy::Agent)]
    pub line_by: LineBy,

    /// Refuse to record a worktree with more tracked files than this.
    ///
    /// Named apart from the global `--max-files`, which `watch` cannot see:
    /// the recorder supervises a whole session rather than the one checkout
    /// the global flag describes.
    #[arg(long, default_value_t = DEFAULT_MAX_FILES, value_name = "N")]
    pub file_budget: usize,

    /// Mirror the log to stdout as well as the log file.
    #[arg(long)]
    pub verbose: bool,
}

impl WatchArgs {
    fn tuning(&self) -> Tuning {
        Tuning {
            settle: Duration::from_secs_f64(self.settle.max(0.0)),
            patience: Duration::from_secs(self.patience),
        }
    }
}

pub fn run(args: &WatchArgs) -> Result<()> {
    // Everything below is herdr's socket API and nothing else — `agent.list`,
    // `pane.get`, `pane.read`, `events.subscribe`, and the write-back that
    // tells an agent what a restore took back all go through `wire::connect`,
    // which has no non-unix implementation. Without this the loop opens, fails
    // to subscribe, retries until the supervisor gives up, and exits 0: a
    // recorder that reported "started", recorded nothing, and left an empty
    // timeline nobody could tell from a quiet one. Refusing at startup is the
    // difference between a feature that is missing and a claim that is false.
    if !cfg!(unix) {
        bail!(
            "`sheep watch` is not supported on Windows yet.\nherdr's session API is a unix \
             socket and Sheep has no transport for it on this platform, so there is no way to see \
             an agent finish a turn. `sheep snap` records a turn by hand."
        );
    }
    if !wire::inside_herdr() {
        bail!(
            "`sheep watch` records what herdr sees, so it has to run inside a herdr session.\nStart it from a herdr pane, or use `sheep snap` to record a turn by hand."
        );
    }

    let state = repo::state_dir()?;
    // A dry run has to leave nothing behind, log file included.
    let log = if args.dry_run { Log::to_stdout() } else { Log::open(&state, args.verbose)? };

    let config = Config {
        dry_run: args.dry_run,
        tuning: args.tuning(),
        line_by: args.line_by,
        file_budget: args.file_budget,
        state,
        reconcile_every: RECONCILE_EVERY,
    };

    log.info(format!(
        "watching{}: settle {:.1}s, patience {}s, timelines by {:?}{}",
        if config.dry_run { " (dry run — nothing will be written)" } else { "" },
        config.tuning.settle.as_secs_f64(),
        config.tuning.patience.as_secs(),
        config.line_by,
        match log.path().as_os_str().is_empty() {
            true => String::new(),
            false => format!(", log {}", log.path().display()),
        }
    ));

    let mut recorder = Recorder::new(Live, config, log);
    let mut supervisor = Supervisor::new();

    loop {
        let opened_at = Instant::now();
        match LiveSource::open() {
            Ok(mut source) => match recorder.pump(&mut source) {
                Ended::Disconnected => recorder.log().info("herdr closed the event stream"),
                Ended::Failed(why) => {
                    recorder.log().warn(format!("the event stream failed: {why}"))
                }
            },
            Err(err) => recorder.log().warn(format!("cannot subscribe to herdr: {err:#}")),
        }

        // A socket that is present but refusing is a handoff in flight and
        // worth waiting for. One that has stayed missing is a session that
        // really has ended, and the recorder should stop rather than spin.
        let present = wire::socket_path().is_some_and(|path| path.exists());
        match supervisor.after(Instant::now(), opened_at.elapsed(), present) {
            Next::Stop => {
                recorder.log().info(format!(
                    "the herdr session is gone; {} turn(s) recorded",
                    recorder.recorded()
                ));
                return Ok(());
            }
            Next::Retry(delay) => std::thread::sleep(delay),
        }
    }
}
