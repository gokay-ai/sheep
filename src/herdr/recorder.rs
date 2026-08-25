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
use crate::store::{Store, TurnKind};
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Longest the loop sleeps with nothing pending. Keeps the reconcile timer and
/// the settle windows honest on a session that has gone quiet.
const TICK: Duration = Duration::from_secs(1);

/// How much of the screen to scrape for a prompt. One screen, no scrollback:
/// a prompt that has already scrolled away is not the one we want.
const PROMPT_LINES: u32 = 80;

/// How long a "this directory is not a git worktree" answer is trusted.
///
/// Short, because it is the answer that stops a pane being recorded at all:
/// somebody who runs `git init` in a directory an agent is already sitting in
/// should not have to restart the recorder to be seen.
const NOT_A_WORKTREE_TTL: Duration = Duration::from_secs(5 * 60);

/// How long a resolved worktree is trusted. `Worktree::discover` is four git
/// subprocesses, so this is worth caching — but a worktree can be removed, and
/// nothing else would ever notice.
const WORKTREE_TTL: Duration = Duration::from_secs(60 * 60);

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

impl LineBy {
    /// The timeline a pane records on.
    ///
    /// The rule lives here rather than inside the recorder because the herdr
    /// plugin has to arrive at the same string from the other side — a dock
    /// pane knows only what herdr put in its invocation context — and a rule
    /// stated in two places is a rule that drifts. `herdr-plugin/scripts/
    /// common.sh:sheep_target_line` is the other statement of it, and
    /// `tests/plugin_timeline.rs` runs the two against each other.
    ///
    /// Handed on raw: [`crate::store::slug`] is what makes a name safe as both
    /// a file name and a git ref, and both the turn log and the shadow
    /// repository call it, so cleaning it here as well would only produce a
    /// second, different answer.
    pub fn timeline(self, pane_id: &str, agent: Option<&str>) -> String {
        match self {
            LineBy::Agent => agent.unwrap_or("agent").to_string(),
            LineBy::Pane => pane_id.to_string(),
        }
    }
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

/// A `Worktree::discover` answer and when it was reached.
struct Resolved {
    worktree: Option<Worktree>,
    at: Instant,
}

impl Resolved {
    fn is_fresh(&self, now: Instant) -> bool {
        let ttl = match self.worktree {
            Some(_) => WORKTREE_TTL,
            None => NOT_A_WORKTREE_TTL,
        };
        now.duration_since(self.at) < ttl
    }
}

pub struct Recorder<S: Session> {
    session: S,
    config: Config,
    log: Log,
    detector: Detector,
    /// The pane's foreground processes when a candidate opened, so we can tell
    /// at the other end of the window whether anything started or finished.
    ///
    /// Entries live exactly as long as the candidate that made them. A
    /// fingerprint left behind by turn N would be compared against turn N+1,
    /// where a changed pid set is near certain and would cost a whole extra
    /// window — or, on a changed leader, the turn itself.
    fingerprints: HashMap<String, Processes>,
    /// `Worktree::discover` shells out to git and a pane's directory rarely
    /// moves, so answers are cached — but only for a while, and only for
    /// directories some pane still reports. See [`Resolved`].
    worktrees: HashMap<String, Resolved>,
    /// `(worktree id, timeline)` pairs already known to have something to
    /// rewind to. Bounded by the number of checkouts the user runs agents in.
    baselined: HashSet<(String, String)>,
    /// The worktree each pane's turn began in, resolved when the turn started
    /// and thrown away when it resolves.
    ///
    /// The detector already refuses to let a turn wander, but this is the layer
    /// that makes the consequence impossible rather than merely unlikely: a
    /// turn is recorded where its baseline went or it is not recorded at all.
    /// The two checks are independent, which is the point — the last review
    /// found a directory change the detector absorbed silently, and a backstop
    /// that shares its reasoning would have absorbed it too.
    started_in: HashMap<String, String>,
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
            baselined: HashSet::new(),
            started_in: HashMap::new(),
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

    /// Ask herdr what it currently thinks, and make our state match it.
    ///
    /// Run at start-up and periodically. Its job is the gap after a reconnect:
    /// events that happened while we were away are simply gone, so a pane that
    /// changed status — or stopped existing — in that window would otherwise be
    /// stuck on stale state. `pane.closed` and `pane.exited` are exactly the
    /// events a reconnect loses, so this has to work in both directions: panes
    /// that dropped off `agent.list` are forgotten, not merely left behind with
    /// a `worked` flag that stays true for the rest of the day.
    pub fn reconcile(&mut self) {
        let agents = match self.session.agents() {
            Ok(agents) => agents,
            Err(err) => {
                self.log.warn(format!("cannot list agents: {err:#}"));
                return;
            }
        };

        let live: HashSet<&str> = agents.iter().map(|s| s.pane_id.as_str()).collect();
        let stale: Vec<String> =
            self.detector.pane_ids().into_iter().filter(|id| !live.contains(id.as_str())).collect();
        for pane_id in stale {
            self.log.info(format!("{pane_id}: herdr no longer lists an agent here; forgetting it"));
            self.drop_pane(&pane_id);
        }

        let now = Instant::now();
        for sighting in &agents {
            self.observe(now, sighting);
        }

        // Directories nothing points at any more. Re-resolving one costs four
        // git subprocesses on the next turn that needs it, which is cheaper
        // than a map that only ever grows in a process meant to run for days.
        let wanted: HashSet<&str> = agents.iter().filter_map(|s| s.cwd.as_deref()).collect();
        self.worktrees.retain(|cwd, _| wanted.contains(cwd.as_str()));
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
        self.started_in.remove(pane_id);
    }

    fn observe(&mut self, now: Instant, sighting: &Sighting) {
        // Sheep records agent turns, so a pane that has never had an agent is
        // not worth following. One we are already following is: herdr dropping
        // the attribution for a reply or two must not stop us watching the pane
        // paint, or a quiet window would close while the agent is still
        // working. Reading a pane is now allowed to answer "this pane exists
        // but I am not calling it an agent right now", so this decision lives
        // here, where it is a decision, rather than in the parser where it
        // looked like the pane was missing.
        if sighting.agent.is_none() && !self.detector.is_tracked(&sighting.pane_id) {
            return;
        }
        // The first time we lay eyes on an agent pane is the earliest — and so
        // the truest — baseline available, and it is worth taking even though
        // the pane may sit idle for hours afterwards.
        //
        // Taking it at the *start* of a turn instead loses a race that a live
        // session lost on the first try: herdr infers `working` from what the
        // pane paints, so by the time the edge arrives a fast agent has already
        // written its first file. The baseline then contains the turn's own
        // work, the boundary compares equal, and a real turn is silently not
        // recorded. `baseline` is keyed by timeline and does nothing twice, so
        // the call on the turn edge stays for the timeline that is new rather
        // than the pane.
        if !self.detector.is_tracked(&sighting.pane_id) {
            self.baseline(&sighting.pane_id, sighting.agent.as_deref(), sighting.cwd.as_deref());
        }
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
        let mut queue = VecDeque::from([signal]);
        while let Some(signal) = queue.pop_front() {
            match signal {
                Signal::Started { pane_id, agent, cwd } => {
                    let prompt = self.capture_prompt(&pane_id);
                    self.detector.set_prompt(&pane_id, prompt);
                    // Fix the worktree this turn belongs to before the agent
                    // has done anything, and take the baseline in it.
                    match cwd.as_deref().and_then(|cwd| self.worktree(cwd)) {
                        Some(worktree) => {
                            self.started_in.insert(pane_id.clone(), worktree.id.clone());
                        }
                        None => {
                            self.started_in.remove(&pane_id);
                        }
                    }
                    self.baseline(&pane_id, agent.as_deref(), cwd.as_deref());
                }
                Signal::Candidate { pane_id } => {
                    // Snapshot the process group *now*. The comparison at the
                    // far end of the window is what catches an agent still
                    // spawning and reaping tools while herdr calls it done.
                    //
                    // A read that fails clears the entry rather than leaving
                    // the last turn's group in the map to be compared against
                    // this one.
                    match self.session.processes(&pane_id) {
                        Ok(Some(processes)) => {
                            self.fingerprints.insert(pane_id, processes);
                        }
                        Ok(None) => {
                            self.fingerprints.remove(&pane_id);
                        }
                        Err(err) => {
                            self.log.warn(format!(
                                "{pane_id}: cannot fingerprint the pane's processes: {err:#}"
                            ));
                            self.fingerprints.remove(&pane_id);
                        }
                    }
                }
                Signal::Withdrawn { pane_id, why } => {
                    self.fingerprints.remove(&pane_id);
                    self.started_in.remove(&pane_id);
                    self.log.info(format!("{pane_id}: withdrawn — {}", why.as_str()));
                }
                Signal::Ripe { pane_id, agent, cwd, noisy } => {
                    let verdict = self.settle(&pane_id, agent.as_deref(), cwd.as_deref(), noisy);
                    // The fingerprint belongs to this candidate. Only a wait
                    // keeps it, and then only the freshly read one that
                    // `settle` left behind.
                    if verdict != Verdict::Wait {
                        self.fingerprints.remove(&pane_id);
                        self.started_in.remove(&pane_id);
                    }
                    queue.extend(self.detector.resolve(Instant::now(), &pane_id, verdict));
                }
            }
        }
    }

    /// Give a timeline something to rewind *to* before its first turn lands.
    ///
    /// `ops::snap` can only tell that nothing changed by comparing against the
    /// previous turn, so on an empty timeline the first boundary always records
    /// — `1 file(s) +0 -0` — whether or not the agent did anything. That is the
    /// mechanism by which a phantom boundary becomes a turn on disk.
    ///
    /// Taking the baseline at the *start* of a turn fixes it at the root rather
    /// than papering over it: the tree before the agent touches anything is
    /// both the honest thing to compare the turn against and the thing a user
    /// rewinding their first turn actually wants to land on. It is recorded as
    /// a checkpoint, because that is what it is — a state kept so you can get
    /// back to it — and never as a turn, because no agent finished one.
    fn baseline(&mut self, pane_id: &str, agent: Option<&str>, cwd: Option<&str>) {
        if self.config.dry_run {
            return;
        }
        let Some(cwd) = cwd else { return };
        let Some(worktree) = self.worktree(cwd) else { return };
        let line = self.timeline(pane_id, agent);
        let key = (worktree.id.clone(), line.clone());
        if self.baselined.contains(&key) {
            return;
        }

        match Store::open(&self.config.state, &worktree.id, &line).and_then(|store| store.all()) {
            Ok(turns) if !turns.is_empty() => {
                self.baselined.insert(key);
                return;
            }
            Ok(_) => {}
            Err(err) => {
                self.log.warn(format!("{pane_id}: cannot read the timeline {line}: {err:#}"));
                return;
            }
        }

        let meta = SnapMeta {
            agent: agent.map(str::to_string),
            pane_id: Some(pane_id.to_string()),
            prompt: None,
            note: Some("baseline, before the first recorded turn".into()),
        };
        match ops::snap(
            &worktree,
            &self.config.state,
            &line,
            self.config.file_budget,
            TurnKind::Checkpoint,
            meta,
            true,
        ) {
            Ok(turn) => {
                self.baselined.insert(key);
                if let Some(turn) = turn {
                    self.log.info(format!(
                        "{pane_id}: baseline #{} on {line} — {} file(s) in {}",
                        turn.seq,
                        turn.files,
                        worktree.root.display()
                    ));
                }
            }
            // Not fatal, and not marked done: a worktree that is mid-rebase now
            // may well be recordable by the next turn.
            Err(err) => self.log.warn(format!("{pane_id}: cannot take a baseline: {err:#}")),
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
            Ok(None) => {
                self.log.info(format!("{pane_id}: the pane is gone; skipped"));
                return Verdict::Drop;
            }
            Ok(Some(fresh)) if !fresh.status.is_rest() => {
                self.log.info(format!(
                    "{pane_id}: herdr now says {} — not a boundary",
                    fresh.status.as_str()
                ));
                return Verdict::Drop;
            }
            // The detector withdraws a candidate whose pane moves, but only
            // for moves it saw. Asking outright closes the case where the
            // event went missing across a reconnect.
            Ok(Some(fresh)) if fresh.cwd.as_deref().is_some_and(|now| now != cwd) => {
                self.log.info(format!(
                    "{pane_id}: the pane is now in {} but the turn happened in {cwd}; skipped",
                    fresh.cwd.unwrap_or_default()
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
        // guess about — and which is also the answer to "herdr has stopped
        // attributing an agent to this pane". Whether an agent is running is a
        // question about processes, not about what herdr is willing to call
        // one, so an attribution that flaps costs nothing here.
        let processes = match self.session.processes(pane_id) {
            Ok(Some(processes)) => processes,
            Ok(None) => {
                self.log.info(format!("{pane_id}: the pane is gone; skipped"));
                return Verdict::Drop;
            }
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

        let Some(worktree) = self.worktree(cwd) else {
            // Not a git worktree. Normal — plenty of panes are not — so this is
            // not an error and must not be reported as one.
            return Verdict::Drop;
        };

        // The last line of defence, and deliberately not sharing any reasoning
        // with the detector's: whatever the events said, a turn is recorded in
        // the worktree its baseline went to or it is not recorded.
        if let Some(started_in) = self.started_in.get(pane_id) {
            if started_in != &worktree.id {
                self.log.warn(format!(
                    "{pane_id}: this turn started in {started_in} but would be recorded in {} — refusing",
                    worktree.id
                ));
                return Verdict::Drop;
            }
        }

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

    /// The timeline a pane records on. See [`LineBy::timeline`].
    fn timeline(&self, pane_id: &str, agent: Option<&str>) -> String {
        self.config.line_by.timeline(pane_id, agent)
    }

    fn worktree(&mut self, cwd: &str) -> Option<Worktree> {
        let now = Instant::now();
        if let Some(cached) = self.worktrees.get(cwd) {
            if cached.is_fresh(now) {
                return cached.worktree.clone();
            }
        }
        let worktree = Worktree::discover(std::path::Path::new(cwd)).ok();
        self.worktrees.insert(cwd.to_string(), Resolved { worktree: worktree.clone(), at: now });
        worktree
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "'"))
}
