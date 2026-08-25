//! Turn-boundary detection.
//!
//! Herdr publishes a status per pane. The naive reading of a turn boundary is a
//! `working` → `idle`/`done` transition, and that reading is wrong often enough
//! to matter: herdr infers status from what the pane paints, so it can call an
//! agent `done` while the agent is still mid-turn. A turn Sheep invents is worse
//! than a turn it misses — an invented one pollutes the list the user has to
//! pick from — so this module is built for precision and accepts the misses.
//!
//! The state machine here is pure: it takes sightings and the current time, and
//! it emits signals. Every corroboration that needs the socket (is the agent
//! process still there, does herdr still say the same thing) is the recorder's
//! job, and comes back in as a [`Verdict`]. That split is what makes the whole
//! detector testable against synthetic event sequences with no server and no
//! sleeping.
//!
//! The rule, in full:
//!
//! 1. A candidate opens only on `working` → `idle`/`done`, and only if this pane
//!    has actually been seen `working` since the last turn it recorded. A pane
//!    that merely appears at rest never produces anything.
//! 2. A candidate has to survive a **quiet window**: `settle` with no status
//!    change and no new output. Output is watched through `PaneInfo.revision`,
//!    which herdr bumps every time the pane paints; a still-working agent paints.
//! 3. Any move back to `working` withdraws the candidate outright. This is the
//!    false-`done` case, and withdrawing is the whole defence against it.
//! 4. `blocked` and `unknown` withdraw it too — an agent waiting on the user has
//!    not finished a turn, and `unknown` means herdr has lost the thread.
//! 5. The candidate remembers the working directory it opened in. If the pane
//!    moves before the window closes, the candidate is withdrawn rather than
//!    filed against a repository the agent never touched.
//! 6. When the window finally elapses the recorder gets [`Signal::Ripe`] and
//!    corroborates against the live session before anything is written. The
//!    corroboration can ask to wait, but only until `patience` runs out: a
//!    candidate that can never be corroborated is given up on, not retried for
//!    the rest of the day.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Herdr's per-pane agent status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl Status {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "idle" => Status::Idle,
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            "done" => Status::Done,
            _ => Status::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Done => "done",
            Status::Unknown => "unknown",
        }
    }

    /// `idle` and `done` are the two ways herdr says "not currently working".
    pub fn is_rest(&self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }
}

/// One look at a pane, however it arrived — a streamed event or a reconcile.
#[derive(Debug, Clone)]
pub struct Sighting {
    pub pane_id: String,
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub status: Status,
    /// Herdr's output counter for the pane. Monotonic; bumps on every paint.
    pub revision: u64,
}

/// Why a candidate boundary was taken back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawn {
    /// The pane went back to `working`: herdr's `done` was premature.
    StillWorking,
    /// The agent is waiting on the user, which is not the end of a turn.
    Blocked,
    /// Herdr lost track of the agent.
    LostAgent,
    /// The pane closed, exited, or released its agent.
    PaneGone,
    /// The pane's working directory changed while the candidate was open, so
    /// the tree we would snapshot is no longer the tree the agent worked in.
    MovedDirectory,
    /// The recorder could not corroborate the boundary within `patience`.
    Uncorroborated,
}

impl Withdrawn {
    pub fn as_str(&self) -> &'static str {
        match self {
            Withdrawn::StillWorking => "the pane went back to working — false done",
            Withdrawn::Blocked => "the agent is blocked on the user",
            Withdrawn::LostAgent => "herdr lost track of the agent",
            Withdrawn::PaneGone => "the pane is gone",
            Withdrawn::MovedDirectory => {
                "the pane changed directory — the work was done somewhere else"
            }
            Withdrawn::Uncorroborated => "it could not be corroborated within patience",
        }
    }
}

/// Something the recorder has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// A turn just started here. Two things happen on this edge: the prompt is
    /// scraped while what the user typed is still the freshest thing on the
    /// screen, and the timeline gets a baseline if it does not have one — the
    /// tree *before* the agent touches anything is what a first turn has to be
    /// measured against.
    Started { pane_id: String, agent: Option<String>, cwd: Option<String> },
    /// A boundary is being considered. The recorder fingerprints the pane's
    /// processes now, so it can tell at [`Signal::Ripe`] whether anything moved.
    Candidate { pane_id: String },
    /// The quiet window elapsed. Corroborate, then record.
    ///
    /// `agent` and `cwd` are the ones the candidate opened with, not whatever
    /// the pane says now. A turn belongs to the directory the work happened in.
    Ripe {
        pane_id: String,
        agent: Option<String>,
        cwd: Option<String>,
        /// The pane never went quiet; the window ran out of patience instead.
        /// Worth logging, because it is the shape a mis-tracked pane has.
        noisy: bool,
    },
    /// A candidate was taken back before it could be recorded.
    Withdrawn { pane_id: String, why: Withdrawn },
}

/// The recorder's answer to a [`Signal::Ripe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Dealt with — recorded, or deliberately not worth recording. Done here.
    Settled,
    /// Corroboration says the pane is still busy. Wait for more quiet.
    Wait,
    /// This is not a boundary after all. Drop it without recording.
    Drop,
}

#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Quiet required — no status change, no new output — before a boundary is
    /// believed.
    pub settle: Duration,
    /// How long to keep extending the quiet window for a pane that will not
    /// stop painting. Past this the recorder decides on corroboration alone.
    pub patience: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { settle: Duration::from_secs(10), patience: Duration::from_secs(120) }
    }
}

#[derive(Debug)]
struct Candidate {
    /// The agent and working directory at the moment the boundary opened.
    ///
    /// Herdr re-sends a pane on `cd`, so the pane's *current* directory is not
    /// the one the turn happened in — a directory change during a ten-second
    /// window would otherwise file the turn against a repository the agent
    /// never touched, and leave the real one with nothing.
    agent: Option<String>,
    cwd: Option<String>,
    /// When the current quiet window closes.
    due: Instant,
    /// Stop restarting the window on new output after this. Also the ceiling
    /// on the whole wait, corroboration retries included.
    deadline: Instant,
    /// False once [`Signal::Ripe`] has been emitted and we are waiting for a
    /// verdict, so the same candidate cannot fire twice.
    armed: bool,
    /// Ran out of patience rather than going quiet.
    noisy: bool,
}

#[derive(Debug)]
struct Pane {
    agent: Option<String>,
    cwd: Option<String>,
    status: Status,
    revision: u64,
    /// Seen `working` since the last turn this pane recorded. Without it, a
    /// pane that is simply sitting at `idle` when Sheep starts would look like
    /// a finished turn the first time anything nudges it.
    worked: bool,
    candidate: Option<Candidate>,
    /// Screen-scraped, captured at the start of the turn. Never authoritative.
    prompt: Option<String>,
}

impl Pane {
    fn new(sighting: &Sighting) -> Self {
        Self {
            agent: sighting.agent.clone(),
            cwd: sighting.cwd.clone(),
            status: sighting.status,
            revision: sighting.revision,
            // A pane that is already `working` when we first see it has a turn
            // in flight, and its end is a real boundary.
            worked: sighting.status == Status::Working,
            candidate: None,
            prompt: None,
        }
    }
}

/// Whether a pane has moved away from where a candidate opened.
///
/// A sighting that simply does not carry a directory is not a move: herdr omits
/// the field rather than reporting a change, and treating silence as a move
/// would withdraw every candidate on a pane herdr happens to be terse about.
fn moved(opened_in: &Option<String>, now_in: &Option<String>) -> bool {
    match (opened_in, now_in) {
        (Some(before), Some(after)) => before != after,
        _ => false,
    }
}

/// The state machine. One per recorder.
#[derive(Debug)]
pub struct Detector {
    tuning: Tuning,
    panes: HashMap<String, Pane>,
}

impl Detector {
    pub fn new(tuning: Tuning) -> Self {
        Self { tuning, panes: HashMap::new() }
    }

    /// Feed one look at a pane.
    pub fn observe(&mut self, now: Instant, sighting: &Sighting) -> Vec<Signal> {
        let mut out = Vec::new();

        let Some(pane) = self.panes.get_mut(&sighting.pane_id) else {
            let fresh = Pane::new(sighting);
            let started = fresh.worked;
            self.panes.insert(sighting.pane_id.clone(), fresh);
            if started {
                out.push(Signal::Started {
                    pane_id: sighting.pane_id.clone(),
                    agent: sighting.agent.clone(),
                    cwd: sighting.cwd.clone(),
                });
            }
            return out;
        };

        if sighting.agent.is_some() {
            pane.agent = sighting.agent.clone();
        }
        if sighting.cwd.is_some() {
            pane.cwd = sighting.cwd.clone();
        }

        // Herdr's revision only ever climbs. Treating a repeat as new output
        // would restart the quiet window forever on a pane that merely renamed
        // itself.
        let painted = sighting.revision > pane.revision;
        pane.revision = pane.revision.max(sighting.revision);

        let was = pane.status;
        pane.status = sighting.status;

        match sighting.status {
            Status::Working => {
                if pane.candidate.take().is_some() {
                    // The defence against herdr's false `done`: whatever it
                    // said a moment ago, the agent is demonstrably still going.
                    out.push(Signal::Withdrawn {
                        pane_id: sighting.pane_id.clone(),
                        why: Withdrawn::StillWorking,
                    });
                }
                pane.worked = true;
                if was != Status::Working {
                    out.push(Signal::Started {
                        pane_id: sighting.pane_id.clone(),
                        agent: pane.agent.clone(),
                        cwd: pane.cwd.clone(),
                    });
                }
            }

            Status::Blocked => {
                if pane.candidate.take().is_some() {
                    out.push(Signal::Withdrawn {
                        pane_id: sighting.pane_id.clone(),
                        why: Withdrawn::Blocked,
                    });
                }
            }

            Status::Unknown => {
                if pane.candidate.take().is_some() {
                    out.push(Signal::Withdrawn {
                        pane_id: sighting.pane_id.clone(),
                        why: Withdrawn::LostAgent,
                    });
                }
                // Whatever was in flight is no longer something we can vouch for.
                pane.worked = false;
            }

            Status::Idle | Status::Done => {
                if was == Status::Working && pane.worked && pane.candidate.is_none() {
                    let deadline = now + self.tuning.patience;
                    pane.candidate = Some(Candidate {
                        agent: pane.agent.clone(),
                        cwd: pane.cwd.clone(),
                        // Patience is a ceiling on the whole wait, not just on
                        // how often the window may restart.
                        due: (now + self.tuning.settle).min(deadline),
                        deadline,
                        armed: true,
                        noisy: false,
                    });
                    out.push(Signal::Candidate { pane_id: sighting.pane_id.clone() });
                } else if let Some(candidate) = pane.candidate.as_mut() {
                    // A pane that moved is no longer describing the tree the
                    // turn happened in, and snapshotting the new one would file
                    // the turn against a repository nobody touched.
                    if moved(&candidate.cwd, &pane.cwd) {
                        pane.candidate = None;
                        out.push(Signal::Withdrawn {
                            pane_id: sighting.pane_id.clone(),
                            why: Withdrawn::MovedDirectory,
                        });
                    } else if painted && candidate.armed {
                        // Still at rest, but the pane painted again: an agent
                        // that is finished stops writing to the screen, so
                        // restart the window rather than believe the boundary.
                        if now < candidate.deadline {
                            candidate.due = (now + self.tuning.settle).min(candidate.deadline);
                        } else {
                            candidate.noisy = true;
                        }
                    }
                }
            }
        }

        out
    }

    /// Fire any candidate whose quiet window has closed.
    pub fn tick(&mut self, now: Instant) -> Vec<Signal> {
        let mut out = Vec::new();
        for (pane_id, pane) in self.panes.iter_mut() {
            let Some(candidate) = pane.candidate.as_mut() else {
                continue;
            };
            if !candidate.armed || candidate.due > now {
                continue;
            }
            candidate.armed = false;
            out.push(Signal::Ripe {
                pane_id: pane_id.clone(),
                agent: candidate.agent.clone(),
                cwd: candidate.cwd.clone(),
                noisy: candidate.noisy || now >= candidate.deadline,
            });
        }
        out
    }

    /// When the recorder next has to wake up, if it has anything pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.panes
            .values()
            .filter_map(|p| p.candidate.as_ref())
            .filter(|c| c.armed)
            .map(|c| c.due)
            .min()
    }

    /// Answer a [`Signal::Ripe`], and say what the answer implies.
    ///
    /// A [`Verdict::Wait`] arriving after `patience` has run out is not a wait
    /// at all: the recorder has been unable to corroborate this boundary for
    /// the entire window it was given. Retrying it every `settle` for the rest
    /// of the day would keep the loop blocked on a server that is not
    /// answering while every other pane goes unrecorded, so the candidate is
    /// given up instead — loudly.
    #[must_use]
    pub fn resolve(&mut self, now: Instant, pane_id: &str, verdict: Verdict) -> Vec<Signal> {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Vec::new();
        };
        match verdict {
            Verdict::Settled => {
                pane.candidate = None;
                pane.worked = false;
                pane.prompt = None;
            }
            Verdict::Drop => {
                pane.candidate = None;
                pane.worked = false;
            }
            Verdict::Wait => {
                let Some(candidate) = pane.candidate.as_mut() else {
                    return Vec::new();
                };
                if now >= candidate.deadline {
                    pane.candidate = None;
                    pane.worked = false;
                    return vec![Signal::Withdrawn {
                        pane_id: pane_id.to_string(),
                        why: Withdrawn::Uncorroborated,
                    }];
                }
                candidate.armed = true;
                // `deadline` is deliberately left where it is. It is the
                // ceiling on the whole wait, and moving it here is exactly
                // what would make the retry loop unbounded.
                candidate.due = (now + self.tuning.settle).min(candidate.deadline);
                candidate.noisy = false;
            }
        }
        Vec::new()
    }

    /// Forget a pane that closed, exited, or released its agent.
    pub fn forget(&mut self, pane_id: &str) -> Vec<Signal> {
        match self.panes.remove(pane_id) {
            Some(pane) if pane.candidate.is_some() => {
                vec![Signal::Withdrawn { pane_id: pane_id.to_string(), why: Withdrawn::PaneGone }]
            }
            _ => Vec::new(),
        }
    }

    /// Store the best-effort prompt captured at the start of a turn.
    pub fn set_prompt(&mut self, pane_id: &str, prompt: Option<String>) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if prompt.is_some() {
                pane.prompt = prompt;
            }
        }
    }

    pub fn prompt(&self, pane_id: &str) -> Option<&str> {
        self.panes.get(pane_id).and_then(|p| p.prompt.as_deref())
    }

    pub fn tracked(&self) -> usize {
        self.panes.len()
    }

    /// Every pane the detector is holding state for.
    ///
    /// The recorder prunes against this: `agent.list` is herdr's own authority
    /// on which panes have agents, and one that has dropped off it is a pane
    /// whose `worked` flag would otherwise sit there being true for ever.
    pub fn pane_ids(&self) -> Vec<String> {
        self.panes.keys().cloned().collect()
    }

    /// Whether a candidate is currently pending for this pane.
    pub fn is_pending(&self, pane_id: &str) -> bool {
        self.panes.get(pane_id).is_some_and(|p| p.candidate.is_some())
    }
}
