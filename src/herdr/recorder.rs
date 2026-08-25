//! The recorder: herdr's event stream in, turns on a timeline out.
//!
//! [`Detector`](super::detect::Detector) decides *when* a turn ended;
//! [`ops::snap`] decides *what* gets recorded. This module is the part in
//! between — it corroborates the detector's candidates against the live
//! session, maps a pane to a worktree, and survives everything that can go
//! wrong in a day of running.
//!
//! Two things it deliberately does not do: it never reimplements snapshotting,
//! and it never lets one pane's failure end the loop. A worktree mid-rebase, a
//! pane that closed under us, a socket that went away during a live handoff —
//! each is logged and the other panes keep recording.

use super::detect::{Detector, Sighting, Signal, Tuning, Verdict};
use super::log::Log;
use super::prompt;
use super::session::{self, Processes, Session};
use super::wire::{self, Subscription};
use crate::ops::{self, SnapMeta};
use crate::repo::Worktree;
use crate::store::TurnKind;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Longest the loop sleeps with nothing pending. Keeps the reconcile timer and
/// the settle windows honest on a session that has gone quiet.
const TICK: Duration = Duration::from_secs(1);

/// How much of the screen to scrape for a prompt. One screen, no scrollback:
/// a prompt that has already scrolled away is not the one we want.
const PROMPT_LINES: u32 = 80;

/// How long herdr should keep showing the turn number. Long enough to survive
/// a quiet afternoon, short enough that it disappears rather than lying when
/// the recorder is no longer running.
const TURN_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// How a pane's timeline is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LineBy {
    /// By agent (`claude`, `codex`). Survives a herdr restart, because pane ids
    /// do not: `w3K:p5` is reassigned every session. Turns are already filed
    /// per worktree, and herdr's own model is one worktree per agent, so this
    /// gives one timeline per agent per checkout — the thing a user rewinds.
    Agent,
    /// By pane id. Strictly one timeline per pane, at the cost of starting a
    /// fresh one every time herdr restarts.
    Pane,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Print boundaries instead of recording them. Writes nothing, anywhere.
    pub dry_run: bool,
    pub tuning: Tuning,
    pub line_by: LineBy,
    pub file_budget: usize,
    pub state: PathBuf,
    pub reconcile_every: Duration,
}

/// How a run of the event loop finished.
#[derive(Debug)]
pub enum Ended {
    /// Herdr closed the stream: shutdown, live handoff, or a dropped socket.
    Disconnected,
    /// The subscription itself failed.
    Failed(String),
}

/// One poll of the event stream.
pub enum Pump {
    Event(wire::Event),
    /// The timeout elapsed with nothing to report.
    Idle,
    Closed,
    Failed(String),
}

/// Where events come from. A trait so the recorder can be driven from a script.
pub trait Source {
    fn poll(&mut self, timeout: Duration) -> Pump;
}

/// The topics one subscription needs to see every agent in the session.
///
/// `pane.agent_status_changed` is per-pane, which would mean re-opening the
/// subscription every time a pane appears. `pane.updated` is parameterless and
/// carries the whole `PaneInfo` — status, cwd, agent and the output revision —
/// so a single subscription covers panes that do not exist yet.
pub fn topics() -> Vec<Value> {
    vec![
        wire::topic("pane.updated"),
        wire::topic("pane.created"),
        wire::topic("pane.closed"),
        wire::topic("pane.exited"),
        wire::topic("pane.agent_detected"),
    ]
}

/// The live event stream, read on its own thread.
///
/// [`Subscription::next_event`] blocks with no timeout — correct for a stream
/// that is silent most of the day, useless for a loop that also has to fire a
/// settle window on time. The thread turns the blocking read into something the
/// main loop can wait on with a deadline.
pub struct LiveSource {
    events: Receiver<std::result::Result<wire::Event, Option<String>>>,
}

impl LiveSource {
    pub fn open() -> Result<Self> {
        let mut subscription = Subscription::open(&topics())?;
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || loop {
            let message = match subscription.next_event() {
                Ok(Some(event)) => Ok(event),
                Ok(None) => Err(None),
                Err(err) => Err(Some(format!("{err:#}"))),
            };
            let fatal = message.is_err();
            if tx.send(message).is_err() || fatal {
                return;
            }
        });
        Ok(Self { events })
    }
}

impl Source for LiveSource {
    fn poll(&mut self, timeout: Duration) -> Pump {
        match self.events.recv_timeout(timeout) {
            Ok(Ok(event)) => Pump::Event(event),
            Ok(Err(None)) => Pump::Closed,
            Ok(Err(Some(why))) => Pump::Failed(why),
            Err(RecvTimeoutError::Timeout) => Pump::Idle,
            Err(RecvTimeoutError::Disconnected) => Pump::Closed,
        }
    }
}

pub struct Recorder<S: Session> {
    session: S,
    config: Config,
    log: Log,
    detector: Detector,
    /// The pane's foreground processes when a candidate opened, so we can tell
    /// at the other end of the window whether anything started or finished.
    fingerprints: HashMap<String, Processes>,
    /// `Worktree::discover` shells out to git; a pane's directory does not move
    /// between turns. `None` records "this is not a git worktree", which is a
    /// normal state for plenty of panes and must not be re-checked every turn.
    worktrees: HashMap<String, Option<Worktree>>,
    /// Turns recorded this run, for the closing line in the log.
    recorded: u64,
}

impl<S: Session> Recorder<S> {
    pub fn new(session: S, config: Config, log: Log) -> Self {
        let detector = Detector::new(config.tuning);
        Self {
            session,
            config,
            log,
            detector,
            fingerprints: HashMap::new(),
            worktrees: HashMap::new(),
            recorded: 0,
        }
    }

    pub fn recorded(&self) -> u64 {
        self.recorded
    }

    pub fn log(&self) -> &Log {
        &self.log
    }

    /// Run until the stream ends. Called again after every reconnect, with the
    /// detector's state intact so a blip does not lose a turn in flight.
    pub fn pump(&mut self, source: &mut impl Source) -> Ended {
        self.reconcile();
        let mut last_reconcile = Instant::now();

        loop {
            let now = Instant::now();
            let wait = self
                .detector
                .next_deadline()
                .map(|due| due.saturating_duration_since(now))
                .unwrap_or(TICK)
                .min(TICK);

            match source.poll(wait) {
                Pump::Event(event) => self.on_event(Instant::now(), &event),
                Pump::Idle => {}
                Pump::Closed => return Ended::Disconnected,
                Pump::Failed(why) => return Ended::Failed(why),
            }

            let now = Instant::now();
            self.fire(now);

            if now.duration_since(last_reconcile) >= self.config.reconcile_every {
                last_reconcile = now;
                self.reconcile();
            }
        }
    }

    /// Ask herdr what it currently thinks, and feed that in as sightings.
    ///
    /// Run at start-up and periodically. Its job is the gap after a reconnect:
    /// events that happened while we were away are simply gone, and a pane that
    /// changed status in that window would otherwise be stuck on stale state.
    pub fn reconcile(&mut self) {
        match self.session.agents() {
            Ok(agents) => {
                let now = Instant::now();
                for sighting in agents {
                    self.observe(now, &sighting);
                }
            }
            Err(err) => self.log.warn(format!("cannot list agents: {err:#}")),
        }
    }

    fn on_event(&mut self, now: Instant, event: &wire::Event) {
        match event.kind.as_str() {
            "pane_updated" | "pane_created" => {
                if let Some(sighting) = event.data.get("pane").and_then(session::sighting) {
                    self.observe(now, &sighting);
                }
            }
            "pane_closed" | "pane_exited" => {
                if let Some(pane_id) = event.pane_id() {
                    self.drop_pane(pane_id);
                }
            }
            "pane_agent_detected" => {
                let released = event.data.get("released").and_then(Value::as_bool).unwrap_or(false);
                let Some(pane_id) = event.pane_id() else {
                    return;
                };
                if released {
                    self.drop_pane(pane_id);
                } else if let Ok(Some(sighting)) = self.session.pane(pane_id) {
                    self.observe(now, &sighting);
                }
            }
            _ => {}
        }
    }

    fn drop_pane(&mut self, pane_id: &str) {
        for signal in self.detector.forget(pane_id) {
            self.act(signal);
        }
        self.fingerprints.remove(pane_id);
    }

    fn observe(&mut self, now: Instant, sighting: &Sighting) {
        for signal in self.detector.observe(now, sighting) {
            self.act(signal);
        }
    }

    fn fire(&mut self, now: Instant) {
        for signal in self.detector.tick(now) {
            self.act(signal);
        }
    }

    fn act(&mut self, signal: Signal) {
        match signal {
            Signal::Started { pane_id } => {
                let prompt = self.capture_prompt(&pane_id);
                self.detector.set_prompt(&pane_id, prompt);
            }
            Signal::Candidate { pane_id } => {
                // Snapshot the process group *now*. The comparison at the far
                // end of the window is what catches an agent that is still
                // spawning and reaping tools while herdr calls it done.
                if let Ok(Some(processes)) = self.session.processes(&pane_id) {
                    self.fingerprints.insert(pane_id, processes);
                }
            }
            Signal::Withdrawn { pane_id, why } => {
                self.fingerprints.remove(&pane_id);
                self.log.info(format!("{pane_id}: withdrawn — {}", why.as_str()));
            }
            Signal::Ripe { pane_id, agent, cwd, noisy } => {
                let verdict = self.settle(&pane_id, agent.as_deref(), cwd.as_deref(), noisy);
                self.detector.resolve(Instant::now(), &pane_id, verdict);
            }
        }
    }

    fn capture_prompt(&self, pane_id: &str) -> Option<String> {
        match self.session.screen(pane_id, PROMPT_LINES) {
            Ok(Some(screen)) => prompt::scrape(&screen),
            Ok(None) => None,
            Err(err) => {
                self.log.warn(format!("{pane_id}: cannot read the pane: {err:#}"));
                None
            }
        }
    }

    /// Corroborate a candidate boundary and, if it holds, record the turn.
    fn settle(
        &mut self,
        pane_id: &str,
        agent: Option<&str>,
        cwd: Option<&str>,
        noisy: bool,
    ) -> Verdict {
        let Some(cwd) = cwd else {
            self.log.info(format!("{pane_id}: no working directory reported; skipped"));
            return Verdict::Drop;
        };

        // Corroboration one: ask herdr directly rather than trusting the last
        // event we happened to see.
        match self.session.pane(pane_id) {
            Ok(None) => return Verdict::Drop,
            Ok(Some(fresh)) if !fresh.status.is_rest() => {
                self.log.info(format!(
                    "{pane_id}: herdr now says {} — not a boundary",
                    fresh.status.as_str()
                ));
                return Verdict::Drop;
            }
            Ok(Some(_)) => {}
            Err(err) => {
                self.log.warn(format!("{pane_id}: cannot re-read the pane: {err:#}"));
                return Verdict::Wait;
            }
        }

        // Corroboration two: the kernel's opinion, which herdr's status is a
        // guess about.
        let processes = match self.session.processes(pane_id) {
            Ok(Some(processes)) => processes,
            Ok(None) => return Verdict::Drop,
            Err(err) => {
                self.log.warn(format!("{pane_id}: cannot read process info: {err:#}"));
                return Verdict::Wait;
            }
        };
        if !processes.agent_is_running() {
            self.log.info(format!(
                "{pane_id}: no agent process in the pane's foreground group; skipped"
            ));
            return Verdict::Drop;
        }
        if let Some(before) = self.fingerprints.get(pane_id) {
            if before.leader != processes.leader {
                self.log
                    .info(format!("{pane_id}: the foreground program changed under us; skipped"));
                self.fingerprints.insert(pane_id.to_string(), processes);
                return Verdict::Drop;
            }
            if before.pids() != processes.pids() {
                // Something started or finished inside the agent's process
                // group while herdr called it done. That is an agent running
                // tools, not an agent that has stopped.
                self.log.info(format!("{pane_id}: still spawning processes; waiting"));
                self.fingerprints.insert(pane_id.to_string(), processes);
                return Verdict::Wait;
            }
        }
        let summary = format!("{}({})", processes.leader_name(), processes.running.len());
        self.fingerprints.insert(pane_id.to_string(), processes);

        let Some(worktree) = self.worktree(cwd) else {
            // Not a git worktree. Normal — plenty of panes are not — so this is
            // not an error and must not be reported as one.
            return Verdict::Drop;
        };

        let line = self.timeline(pane_id, agent);
        let scraped = self.detector.prompt(pane_id).map(str::to_string);

        if self.config.dry_run {
            self.log.info(format!(
                "boundary  {pane_id}  {}  line={line}  worktree={}  procs={summary}{}  prompt={}",
                agent.unwrap_or("-"),
                worktree.root.display(),
                if noisy { "  (never went quiet)" } else { "" },
                scraped.as_deref().map(quote).unwrap_or_else(|| "-".into()),
            ));
            return Verdict::Settled;
        }

        let meta = SnapMeta {
            agent: agent.map(str::to_string),
            pane_id: Some(pane_id.to_string()),
            prompt: scraped,
            note: None,
        };
        match ops::snap(
            &worktree,
            &self.config.state,
            &line,
            self.config.file_budget,
            TurnKind::Turn,
            meta,
            false,
        ) {
            Ok(Some(turn)) => {
                self.recorded += 1;
                self.log.info(format!(
                    "{pane_id}: recorded #{} on {line} — {} file(s) +{} -{} in {}{}",
                    turn.seq,
                    turn.files,
                    turn.insertions,
                    turn.deletions,
                    worktree.root.display(),
                    if noisy { "  (never went quiet)" } else { "" },
                ));
                if let Err(err) = self.session.report_turn(pane_id, turn.seq, TURN_TTL) {
                    self.log.warn(format!("{pane_id}: cannot report the turn number: {err:#}"));
                }
            }
            Ok(None) => {
                self.log.info(format!("{pane_id}: nothing changed on {line}; not recorded"));
            }
            Err(err) => {
                // One worktree being unrecordable — mid-rebase, unmerged paths,
                // over budget — is not a reason to stop watching the others.
                self.log.warn(format!("{pane_id}: cannot record: {err:#}"));
            }
        }
        Verdict::Settled
    }

    /// The timeline a pane records on.
    fn timeline(&self, pane_id: &str, agent: Option<&str>) -> String {
        match self.config.line_by {
            LineBy::Agent => timeline_name(agent.unwrap_or("agent")),
            LineBy::Pane => timeline_name(pane_id),
        }
    }

    fn worktree(&mut self, cwd: &str) -> Option<Worktree> {
        if let Some(found) = self.worktrees.get(cwd) {
            return found.clone();
        }
        let found = Worktree::discover(std::path::Path::new(cwd)).ok();
        self.worktrees.insert(cwd.to_string(), found.clone());
        found
    }
}

/// A timeline name that is safe as both a file name and a git ref.
///
/// The shadow repository keeps one ref per timeline, and git refuses a ref
/// whose name contains a colon — which every herdr pane id does (`w3K:p5`).
/// The same character class the turn log uses, applied before either sees it,
/// keeps the ref and the file agreeing on what a timeline is called.
fn timeline_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "agent".to_string() } else { cleaned }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "'"))
}
