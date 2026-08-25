//! The half of the interface that is allowed to be slow.
//!
//! Planning a restore on a large checkout costs about a second of `git`, and a
//! frame that takes a second is a frozen program. So every call into
//! [`crate::ops`] happens on a worker thread and comes back as a [`Reply`] the
//! event loop applies between frames; the render path never spawns a process.
//!
//! [`execute`] is deliberately a plain function over a [`Ctx`]: the worker
//! thread is a `while let` around it, and a test can call it directly against a
//! real temporary repository with no terminal in the picture.
//!
//! Nothing here reimplements a restore. `ops::plan` and `ops::restore` are the
//! only paths that touch a user's files.

use crate::git::Git;
use crate::herdr::wire;
use crate::ops;
use crate::repo::{self, Worktree};
use crate::shadow::Shadow;
use crate::store::{Store, Turn};
use serde_json::json;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// Everything a job needs to know about which worktree it is acting on.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub wt: Worktree,
    pub state: PathBuf,
    pub line: String,
    pub max_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Write,
    Remove,
}

/// A restore plan flattened into something the list widget can index into.
#[derive(Debug, Clone, Default)]
pub struct PlanView {
    pub seq: u64,
    pub commit: String,
    pub target_tree: String,
    /// The tree the plan was computed against. If the working tree has moved on
    /// since, this no longer matches and the plan on screen is stale.
    pub current_tree: String,
    pub files: Vec<(Action, String)>,
    pub written: usize,
    pub removed: usize,
}

impl PlanView {
    pub fn is_noop(&self) -> bool {
        self.files.is_empty()
    }
    pub fn touched(&self) -> usize {
        self.files.len()
    }
    pub fn path(&self, index: usize) -> Option<&str> {
        self.files.get(index).map(|(_, p)| p.as_str())
    }
}

fn view(seq: u64, commit: String, plan: &crate::shadow::RestorePlan) -> PlanView {
    let mut files: Vec<(Action, String)> = Vec::with_capacity(plan.touched());
    files.extend(plan.write.iter().map(|p| (Action::Write, p.clone())));
    files.extend(plan.remove.iter().map(|p| (Action::Remove, p.clone())));
    PlanView {
        seq,
        commit,
        target_tree: plan.target_tree.clone(),
        current_tree: plan.current_tree.clone(),
        written: plan.write.len(),
        removed: plan.remove.len(),
        files,
    }
}

/// What happened to the agent write-back after a restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The agent in this pane was told what was taken back.
    Sent(String),
    /// Deliberately not sent.
    Off,
    /// Nothing to send to: no pane recorded, or not running under herdr.
    Skipped(String),
    /// herdr was there and refused.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub seq: u64,
    pub commit: String,
    pub written: usize,
    pub removed: usize,
    /// The turn holding the state that was replaced. Restoring it undoes this.
    pub checkpoint: Option<u64>,
    pub notice: Notice,
}

#[derive(Debug, Clone)]
pub enum Job {
    /// Turn log plus the safety report. Runs `git`; used at start-up and on `r`.
    Reload,
    /// Turn log only. Cheap enough to run on a timer so the dock stays live.
    Poll,
    Plan {
        req: u64,
        seq: u64,
    },
    Patch {
        req: u64,
        path: String,
        current_tree: String,
        target_tree: String,
    },
    Restore {
        req: u64,
        seq: u64,
        expect_tree: String,
        pane: Option<String>,
        notify: bool,
    },
}

#[derive(Debug, Clone)]
pub enum Reply {
    Loaded {
        turns: Vec<Turn>,
        blockers: Vec<String>,
        warnings: Vec<String>,
    },
    Turns(Vec<Turn>),
    Broken(String),
    Planned {
        req: u64,
        plan: PlanView,
    },
    PlanFailed {
        req: u64,
        seq: u64,
        message: String,
    },
    Patched {
        req: u64,
        path: String,
        body: String,
    },
    PatchFailed {
        req: u64,
        path: String,
        message: String,
    },
    /// The working tree moved between showing the plan and confirming it. The
    /// user gets the new plan instead of a restore they did not look at.
    Stale {
        req: u64,
        plan: PlanView,
    },
    Restored {
        req: u64,
        outcome: Outcome,
    },
    RestoreFailed {
        req: u64,
        message: String,
    },
}

/// How much of a patch the preview will hold. A generated lockfile can be a
/// hundred thousand lines and nobody reads past the first screen.
const PATCH_LINE_LIMIT: usize = 600;

pub fn execute(ctx: &Ctx, job: Job) -> Reply {
    match job {
        Job::Reload => match load(ctx) {
            Ok(reply) => reply,
            Err(e) => Reply::Broken(format!("{e:#}")),
        },
        Job::Poll => match Store::open(&ctx.state, &ctx.wt.id, &ctx.line).and_then(|s| s.all()) {
            Ok(mut turns) => {
                turns.reverse();
                Reply::Turns(turns)
            }
            Err(e) => Reply::Broken(format!("{e:#}")),
        },
        Job::Plan { req, seq } => match plan(ctx, seq) {
            Ok(plan) => Reply::Planned { req, plan },
            Err(e) => Reply::PlanFailed { req, seq, message: format!("{e:#}") },
        },
        Job::Patch { req, path, current_tree, target_tree } => {
            match patch(ctx, &current_tree, &target_tree, &path) {
                Ok(body) => Reply::Patched { req, path, body },
                Err(e) => Reply::PatchFailed { req, path, message: format!("{e:#}") },
            }
        }
        Job::Restore { req, seq, expect_tree, pane, notify } => {
            match restore(ctx, seq, &expect_tree, pane.as_deref(), notify) {
                Ok(Ok(outcome)) => Reply::Restored { req, outcome },
                Ok(Err(plan)) => Reply::Stale { req, plan },
                Err(e) => Reply::RestoreFailed { req, message: format!("{e:#}") },
            }
        }
    }
}

fn load(ctx: &Ctx) -> anyhow::Result<Reply> {
    let health = repo::inspect(&ctx.wt, ctx.max_files)?;
    let mut turns = Store::open(&ctx.state, &ctx.wt.id, &ctx.line)?.all()?;
    turns.reverse(); // newest first: the dock reads top-down like a log
    Ok(Reply::Loaded {
        turns,
        blockers: health.blockers.iter().map(ToString::to_string).collect(),
        warnings: health.warnings.iter().map(ToString::to_string).collect(),
    })
}

fn plan(ctx: &Ctx, seq: u64) -> anyhow::Result<PlanView> {
    let planned = ops::plan(&ctx.wt, &ctx.state, &ctx.line, &format!("#{seq}"), ctx.max_files)?;
    Ok(view(seq, planned.commit, &planned.plan))
}

/// The patch a restore would apply to one path, read out of the shadow repo.
///
/// `diff-tree` rather than `diff`: plumbing, so it cannot fire a hook, and it
/// compares two trees without needing an index or a working tree at all.
fn patch(ctx: &Ctx, current_tree: &str, target_tree: &str, path: &str) -> anyhow::Result<String> {
    let shadow = Shadow::ensure(ctx.wt.clone(), &ctx.state)?;
    let out = Git::bare(&shadow.git_dir).output(&[
        "diff-tree",
        "-r",
        "-p",
        "--no-renames",
        "--no-color",
        "--unified=3",
        current_tree,
        target_tree,
        "--",
        path,
    ])?;
    if !out.status.success() {
        anyhow::bail!("git diff-tree failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let body = String::from_utf8_lossy(&out.stdout);
    // A preview panel is eight to twenty rows. Four of them spent on
    // `diff --git` / `index` / `---` / `+++` is a quarter of the evidence gone,
    // and none of it says anything the file list did not already say.
    let interesting = body.lines().filter(|l| {
        !(l.starts_with("diff --git")
            || l.starts_with("index ")
            || l.starts_with("--- ")
            || l.starts_with("+++ ")
            || l.starts_with("old mode")
            || l.starts_with("new mode"))
    });
    let mut lines: Vec<&str> = interesting.clone().take(PATCH_LINE_LIMIT).collect();
    if interesting.count() > PATCH_LINE_LIMIT {
        lines.push("…");
        lines.push("(preview stops at 600 lines)");
    }
    Ok(lines.join("\n"))
}

/// `Ok(Err(plan))` means the working tree changed under the plan the user was
/// looking at; they get the new one rather than a restore they did not confirm.
#[allow(clippy::type_complexity)]
fn restore(
    ctx: &Ctx,
    seq: u64,
    expect_tree: &str,
    pane: Option<&str>,
    notify: bool,
) -> anyhow::Result<Result<Outcome, PlanView>> {
    let fresh = plan(ctx, seq)?;
    if fresh.current_tree != expect_tree {
        return Ok(Err(fresh));
    }

    let done = ops::restore(&ctx.wt, &ctx.state, &ctx.line, &format!("#{seq}"), ctx.max_files)?;
    let checkpoint = done.checkpoint.as_ref().map(|t| t.seq);
    let written = done.plan.write.len();
    let removed = done.plan.remove.len();

    let notice = if !notify {
        Notice::Off
    } else {
        match pane {
            Some(pane) => {
                let text = rewind_message(seq, &done.commit, written, removed, checkpoint);
                tell_agent(pane, &text)
            }
            None => Notice::Skipped("no agent pane recorded on this timeline".into()),
        }
    };

    Ok(Ok(Outcome { seq, commit: done.commit, written, removed, checkpoint, notice }))
}

/// What the agent is told after its worktree moved underneath it.
///
/// This is the whole point of the plugin. An agent whose files were rewound and
/// which was not told keeps editing from a memory of a tree that no longer
/// exists, and every edit after that is built on a file it has not read. The
/// message names the turn, the size of the change, and the way back.
pub fn rewind_message(
    seq: u64,
    commit: &str,
    written: usize,
    removed: usize,
    checkpoint: Option<u64>,
) -> String {
    let mut text = format!(
        "[sheep] Your working tree was rewound to turn #{seq} ({}). \
         {} path(s) changed on disk: {written} rewritten, {removed} deleted. \
         Anything you wrote after turn #{seq} is no longer on disk — re-read any file before you edit it, \
         and do not re-apply the reverted changes unless you are asked to.",
        ops::short(commit),
        written + removed,
    );
    if let Some(cp) = checkpoint {
        text.push_str(&format!(
            " The state from just before the rewind was kept as turn #{cp}; `sheep restore #{cp} --yes` puts it back."
        ));
    }
    text
}

/// Hand the message to herdr. Outside a herdr session this is a no-op by
/// construction: `try_request` answers `Ok(None)` rather than failing, so the
/// interface behaves identically in a plain terminal.
fn tell_agent(pane: &str, text: &str) -> Notice {
    match wire::try_request("agent.prompt", json!({ "target": pane, "text": text })) {
        Ok(Some(_)) => Notice::Sent(pane.to_string()),
        Ok(None) => Notice::Skipped("not running inside herdr".into()),
        Err(e) => Notice::Failed(format!("{e:#}")),
    }
}

/// The worker thread and the two channels the event loop talks to it through.
pub struct Worker {
    jobs: Sender<Job>,
    pub replies: Receiver<Reply>,
}

impl Worker {
    /// Spawn the worker. When no job arrives within `poll`, it re-reads the turn
    /// log by itself, so a dock left open next to a working agent grows new
    /// turns without anyone pressing a key. That tick never runs `git`.
    pub fn spawn(ctx: Ctx, poll: Duration) -> Self {
        let (jobs, inbox) = std::sync::mpsc::channel::<Job>();
        let (outbox, replies) = std::sync::mpsc::channel::<Reply>();
        std::thread::spawn(move || {
            let mut seen: Option<(usize, String)> = None;
            loop {
                match inbox.recv_timeout(poll) {
                    Ok(job) => {
                        let reply = execute(&ctx, job);
                        if let Reply::Loaded { turns, .. } = &reply {
                            seen = Some(signature(turns));
                        }
                        if outbox.send(reply).is_err() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Reply::Turns(turns) = execute(&ctx, Job::Poll) {
                            let now = signature(&turns);
                            if seen.as_ref() != Some(&now) {
                                seen = Some(now);
                                if outbox.send(Reply::Turns(turns)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { jobs, replies }
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

fn signature(turns: &[Turn]) -> (usize, String) {
    (turns.len(), turns.first().map(|t| t.commit.clone()).unwrap_or_default())
}
