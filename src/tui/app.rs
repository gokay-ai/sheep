//! What the interface knows and what a keystroke does to it.
//!
//! This module holds no terminal and spawns no process. Keys arrive as [`Key`],
//! slow work leaves as [`Job`] and comes back as [`Reply`], which makes the
//! whole interaction — including a restore — drivable from a test with no TTY.
//!
//! The rule the state machine exists to enforce: **a restore is only reachable
//! from a plan that is on screen.** `Confirm` does nothing unless
//! [`PlanState::Ready`] holds a plan for the selected turn, and if the working
//! tree moved while the user was reading it, the worker answers
//! [`Reply::Stale`] with the new plan instead of restoring.

use crate::store::Turn;
use crate::tui::engine::{Job, Notice, Outcome, PlanView, Reply};
use std::collections::HashMap;

/// Keys, named by what the interface does with them rather than by scan code,
/// so `app` does not depend on which crossterm version is in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The timeline, docked beside the agent.
    Dock,
    /// The plan for one turn, and the one key that applies it.
    Rewind,
    Help,
}

#[derive(Debug, Clone)]
pub enum PlanState {
    Idle,
    Loading(u64),
    Ready(PlanView),
    Failed { seq: u64, message: String },
}

#[derive(Debug, Clone)]
pub enum PatchState {
    Idle,
    Loading(String),
    Ready { path: String, body: String },
    Failed { path: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub level: Level,
    pub lines: Vec<String>,
}

impl Status {
    pub fn new(level: Level, lines: impl IntoIterator<Item = String>) -> Self {
        Self { level, lines: lines.into_iter().collect() }
    }
    fn info(line: impl Into<String>) -> Self {
        Self::new(Level::Info, [line.into()])
    }
    fn bad(line: impl Into<String>) -> Self {
        Self::new(Level::Bad, [line.into()])
    }
}

/// A reason the interface cannot do its job at all — not a git worktree, or a
/// state directory it cannot read. Shown instead of an empty box.
#[derive(Debug, Clone)]
pub struct Fatal {
    pub headline: String,
    pub detail: String,
    pub remedy: Vec<String>,
}

pub struct App {
    pub line: String,
    pub repo: String,
    pub root: String,
    pub turns: Vec<Turn>,
    pub sel: usize,
    sel_seq: Option<u64>,
    pub mode: Mode,
    return_to: Mode,
    pub plan: PlanState,
    pub plan_sel: usize,
    pub patch: PatchState,
    pub show_patch: bool,
    /// First visible line of the patch preview.
    pub patch_scroll: u16,
    patch_cache: HashMap<String, String>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub fatal: Option<Fatal>,
    pub status: Option<Status>,
    pub restoring: bool,
    pub loading: bool,
    /// Tell the agent what was taken back. On by default; `--no-notify` and `n`
    /// turn it off.
    pub notify: bool,
    pub inside_herdr: bool,
    /// Wall clock, injected rather than read, so a rendering test is stable.
    pub now: u64,
    pub spinner: usize,
    pub quit: bool,
    req: u64,
    plan_req: u64,
    patch_req: u64,
    restore_req: u64,
    outbox: Vec<Job>,
}

impl App {
    pub fn new(repo: impl Into<String>, root: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            line: line.into(),
            repo: repo.into(),
            root: root.into(),
            turns: Vec::new(),
            sel: 0,
            sel_seq: None,
            mode: Mode::Dock,
            return_to: Mode::Dock,
            plan: PlanState::Idle,
            plan_sel: 0,
            patch: PatchState::Idle,
            show_patch: false,
            patch_scroll: 0,
            patch_cache: HashMap::new(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            fatal: None,
            status: None,
            restoring: false,
            loading: true,
            notify: true,
            inside_herdr: false,
            now: crate::shadow::now(),
            spinner: 0,
            quit: false,
            req: 0,
            plan_req: 0,
            patch_req: 0,
            restore_req: 0,
            outbox: Vec::new(),
        }
    }

    /// The interface could not start. Everything else is skipped and the
    /// message is what the user sees.
    pub fn dead(mut self, fatal: Fatal) -> Self {
        self.loading = false;
        self.fatal = Some(fatal);
        self
    }

    pub fn selected(&self) -> Option<&Turn> {
        self.turns.get(self.sel)
    }

    /// The pane whose agent gets told what was taken back: the selected turn's,
    /// falling back to the newest turn on the timeline that recorded one.
    ///
    /// Never `HERDR_PANE_ID` — that is the pane the dock itself is running in,
    /// which is not the agent whose files just moved.
    pub fn agent_pane(&self) -> Option<String> {
        self.selected()
            .and_then(|t| t.pane_id.clone())
            .or_else(|| self.turns.iter().find_map(|t| t.pane_id.clone()))
    }

    pub fn take_jobs(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.outbox)
    }

    pub fn reload(&mut self) {
        self.loading = true;
        self.outbox.push(Job::Reload);
    }

    pub fn tick(&mut self, now: u64) {
        self.now = now;
        self.spinner = self.spinner.wrapping_add(1);
    }

    // ---------------------------------------------------------------- keys

    pub fn on_key(&mut self, key: Key) {
        if self.fatal.is_some() {
            if matches!(key, Key::Char('q') | Key::Esc | Key::Char('Q')) {
                self.quit = true;
            }
            return;
        }
        match self.mode {
            Mode::Help => match key {
                Key::Char('q') => self.quit = true,
                _ => self.mode = self.return_to,
            },
            Mode::Dock => self.dock_key(key),
            Mode::Rewind => self.rewind_key(key),
        }
    }

    fn dock_key(&mut self, key: Key) {
        match key {
            Key::Char('q') | Key::Char('Q') => self.quit = true,
            Key::Char('?') => self.open_help(),
            Key::Char('n') => self.toggle_notify(),
            Key::Char('r') => self.reload(),
            Key::Down | Key::Char('j') => self.move_by(1),
            Key::Up | Key::Char('k') => self.move_by(-1),
            Key::PageDown => self.move_by(5),
            Key::PageUp => self.move_by(-5),
            Key::Home | Key::Char('g') => self.move_to(0),
            Key::End | Key::Char('G') => self.move_to(self.turns.len().saturating_sub(1)),
            Key::Enter | Key::Right | Key::Char('l') => self.open_rewind(),
            _ => {}
        }
    }

    fn rewind_key(&mut self, key: Key) {
        match key {
            Key::Char('q') | Key::Char('Q') => self.quit = true,
            Key::Char('?') => self.open_help(),
            Key::Char('n') => self.toggle_notify(),
            Key::Esc | Key::Left | Key::Char('h') => self.close_rewind(),
            Key::Down | Key::Char('j') => self.move_plan(1),
            Key::Up | Key::Char('k') => self.move_plan(-1),
            Key::PageDown => self.move_plan(10),
            Key::PageUp => self.move_plan(-10),
            Key::Home => self.plan_to(0),
            Key::End => self.plan_to(usize::MAX),
            Key::Char('d') | Key::Enter => self.toggle_patch(),
            Key::Char('J') => self.scroll_patch(4),
            Key::Char('K') => self.scroll_patch(-4),
            // Restore is deliberately a shift key. `enter` opens a diff, `y`
            // does nothing, and nothing on the way here can trigger a write.
            Key::Char('R') => self.confirm(),
            Key::Char('r') => {
                self.status =
                    Some(Status::info("press shift+R to restore — lower-case r is refresh"))
            }
            _ => {}
        }
    }

    fn open_help(&mut self) {
        if self.mode != Mode::Help {
            self.return_to = self.mode;
            self.mode = Mode::Help;
        }
    }

    fn toggle_notify(&mut self) {
        self.notify = !self.notify;
        self.status = Some(Status::info(if self.notify {
            "the agent will be told what a rewind took back"
        } else {
            "the agent will not be told about a rewind"
        }));
    }

    fn move_by(&mut self, delta: isize) {
        if self.turns.is_empty() {
            return;
        }
        let last = self.turns.len() - 1;
        let next = (self.sel as isize + delta).clamp(0, last as isize) as usize;
        self.move_to(next);
    }

    fn move_to(&mut self, index: usize) {
        if self.turns.is_empty() {
            return;
        }
        self.sel = index.min(self.turns.len() - 1);
        self.sel_seq = self.turns.get(self.sel).map(|t| t.seq);
    }

    fn move_plan(&mut self, delta: isize) {
        let PlanState::Ready(plan) = &self.plan else { return };
        if plan.files.is_empty() {
            return;
        }
        let last = plan.files.len() - 1;
        let next = (self.plan_sel as isize + delta).clamp(0, last as isize) as usize;
        self.plan_to(next);
    }

    fn plan_to(&mut self, index: usize) {
        let PlanState::Ready(plan) = &self.plan else { return };
        if plan.files.is_empty() {
            return;
        }
        self.plan_sel = index.min(plan.files.len() - 1);
        self.patch_scroll = 0;
        self.request_patch();
    }

    fn toggle_patch(&mut self) {
        self.show_patch = !self.show_patch;
        self.patch_scroll = 0;
        if self.show_patch {
            self.request_patch();
        }
    }

    fn scroll_patch(&mut self, delta: i32) {
        if !self.show_patch {
            return;
        }
        self.patch_scroll = (self.patch_scroll as i32 + delta).max(0) as u16;
    }

    // -------------------------------------------------------------- rewind

    pub fn open_rewind(&mut self) {
        if self.restoring {
            return;
        }
        if self.turns.is_empty() {
            self.status =
                Some(Status::info("nothing recorded yet — there is no turn to go back to"));
            return;
        }
        if let Some(blocker) = self.blockers.first() {
            self.status = Some(Status::new(
                Level::Bad,
                ["this worktree is not in a state Sheep will restore".to_string(), blocker.clone()],
            ));
            return;
        }
        let Some(seq) = self.selected().map(|t| t.seq) else { return };
        self.mode = Mode::Rewind;
        self.request_plan(seq);
    }

    fn close_rewind(&mut self) {
        self.mode = Mode::Dock;
        self.plan = PlanState::Idle;
        self.patch = PatchState::Idle;
        self.patch_cache.clear();
    }

    fn request_plan(&mut self, seq: u64) {
        self.req += 1;
        self.plan_req = self.req;
        self.plan = PlanState::Loading(seq);
        self.plan_sel = 0;
        self.patch = PatchState::Idle;
        self.patch_scroll = 0;
        self.patch_cache.clear();
        self.outbox.push(Job::Plan { req: self.plan_req, seq });
    }

    fn request_patch(&mut self) {
        if !self.show_patch {
            return;
        }
        let PlanState::Ready(plan) = &self.plan else { return };
        let Some(path) = plan.path(self.plan_sel).map(str::to_string) else { return };
        if let Some(body) = self.patch_cache.get(&path) {
            self.patch = PatchState::Ready { path, body: body.clone() };
            return;
        }
        let (current_tree, target_tree) = (plan.current_tree.clone(), plan.target_tree.clone());
        self.req += 1;
        self.patch_req = self.req;
        self.patch = PatchState::Loading(path.clone());
        self.outbox.push(Job::Patch { req: self.patch_req, path, current_tree, target_tree });
    }

    /// The only path to a write. Everything it needs is already on screen.
    fn confirm(&mut self) {
        if self.restoring {
            return;
        }
        let PlanState::Ready(plan) = &self.plan else {
            self.status = Some(Status::info("wait for the plan before restoring"));
            return;
        };
        if plan.is_noop() {
            self.status =
                Some(Status::info("the working tree already matches this turn — nothing to do"));
            return;
        }
        let (seq, expect_tree) = (plan.seq, plan.current_tree.clone());
        let pane = self.agent_pane();
        self.req += 1;
        self.restore_req = self.req;
        self.restoring = true;
        self.status = None;
        self.outbox.push(Job::Restore {
            req: self.restore_req,
            seq,
            expect_tree,
            pane,
            notify: self.notify,
        });
    }

    // --------------------------------------------------------------- replies

    pub fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Loaded { turns, blockers, warnings } => {
                self.loading = false;
                self.blockers = blockers;
                self.warnings = warnings;
                self.set_turns(turns);
            }
            Reply::Turns(turns) => {
                self.loading = false;
                self.set_turns(turns);
            }
            Reply::Broken(message) => {
                self.loading = false;
                self.status = Some(Status::bad(message));
            }
            Reply::Planned { req, plan } => {
                if req == self.plan_req {
                    self.plan_sel = 0;
                    self.patch_cache.clear();
                    self.plan = PlanState::Ready(plan);
                    self.request_patch();
                }
            }
            Reply::PlanFailed { req, seq, message } => {
                if req == self.plan_req {
                    self.plan = PlanState::Failed { seq, message };
                }
            }
            Reply::Patched { req, path, body } => {
                if req == self.patch_req {
                    self.patch_cache.insert(path.clone(), body.clone());
                    self.patch = PatchState::Ready { path, body };
                }
            }
            Reply::PatchFailed { req, path, message } => {
                if req == self.patch_req {
                    self.patch = PatchState::Failed { path, message };
                }
            }
            Reply::Stale { req, plan } => {
                if req == self.restore_req {
                    self.restoring = false;
                    self.plan_sel = 0;
                    self.patch_cache.clear();
                    self.plan = PlanState::Ready(plan);
                    self.status = Some(Status::new(
                        Level::Bad,
                        [
                            "the working tree changed while this plan was on screen".to_string(),
                            "nothing was restored. this is the new plan — read it and press shift+R again."
                                .to_string(),
                        ],
                    ));
                }
            }
            Reply::Restored { req, outcome } => {
                if req == self.restore_req {
                    self.restoring = false;
                    self.mode = Mode::Dock;
                    self.plan = PlanState::Idle;
                    self.patch = PatchState::Idle;
                    self.patch_cache.clear();
                    self.status = Some(Status::new(Level::Good, restored_lines(&outcome)));
                    self.reload();
                }
            }
            Reply::RestoreFailed { req, message } => {
                if req == self.restore_req {
                    self.restoring = false;
                    self.status = Some(Status::new(
                        Level::Bad,
                        ["the restore did not happen — nothing was written".to_string(), message],
                    ));
                }
            }
        }
    }

    fn set_turns(&mut self, turns: Vec<Turn>) {
        self.turns = turns;
        // Selection follows the turn, not the row: new turns arriving at the
        // top of the list must not move the cursor onto a different snapshot.
        self.sel = match self.sel_seq {
            Some(seq) => self.turns.iter().position(|t| t.seq == seq).unwrap_or(0),
            None => 0,
        };
        self.sel_seq = self.turns.get(self.sel).map(|t| t.seq);
    }
}

/// The sentences shown after a restore. Separate from `App` so the wording is
/// testable on its own.
pub fn restored_lines(outcome: &Outcome) -> Vec<String> {
    let mut lines = vec![format!(
        "restored to #{} · {} files written, {} removed",
        outcome.seq, outcome.written, outcome.removed
    )];
    match outcome.checkpoint {
        Some(cp) => lines.push(format!(
            "the tree you had is turn #{cp} — press enter on it, or `sheep restore #{cp} --yes`"
        )),
        None => lines.push("nothing needed checkpointing".into()),
    }
    lines.push(match &outcome.notice {
        Notice::Sent(pane) => format!("the agent in pane {pane} was told what was taken back"),
        Notice::Off => "the agent was not told (notify is off — press n)".into(),
        Notice::Skipped(why) => format!("the agent was not told: {why}"),
        Notice::Failed(err) => format!("could not tell the agent: {err}"),
    });
    lines
}
