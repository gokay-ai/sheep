//! The operations Sheep performs on a worktree.
//!
//! Kept out of `main.rs` so the dangerous paths are unit-testable without
//! spawning a process, and so the daemon and the TUI can call exactly what the
//! CLI calls rather than a parallel implementation.

use crate::repo::{self, Worktree};
use crate::shadow::{self, RestorePlan, Shadow};
use crate::store::{Store, Turn, TurnKind};
use anyhow::{bail, Result};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct SnapMeta {
    pub agent: Option<String>,
    pub pane_id: Option<String>,
    pub note: Option<String>,
    pub prompt: Option<String>,
}

/// Record the working tree as a turn on `line`.
///
/// Returns `None` when the tree is byte-identical to the previous turn and
/// `allow_empty` is false — an agent that answered a question without editing
/// anything should not litter the timeline.
pub fn snap(
    wt: &Worktree,
    state: &Path,
    line: &str,
    max_files: usize,
    kind: TurnKind,
    meta: SnapMeta,
    allow_empty: bool,
) -> Result<Option<Turn>> {
    repo::inspect(wt, max_files)?.bail_if_unsafe()?;
    let shadow = Shadow::ensure(wt.clone(), state)?;
    let store = Store::open(state, &wt.id, line)?;

    let tree = shadow.write_tree("snap")?;
    let parent = shadow.head(line)?;

    if !allow_empty {
        if let Some(parent) = &parent {
            if shadow.tree_of(parent)? == tree {
                return Ok(None);
            }
        }
    }

    let (files, insertions, deletions) = match &parent {
        Some(p) => shadow.diffstat(&shadow.tree_of(p)?, &tree)?,
        None => (shadow.tree_size(&tree)?, 0, 0),
    };

    let mut turn = Turn {
        seq: store.next_seq()?,
        kind,
        commit: String::new(),
        tree: tree.clone(),
        parent,
        at: shadow::now(),
        files,
        insertions,
        deletions,
        pane_id: meta.pane_id,
        agent: meta.agent,
        prompt: meta.prompt,
        note: meta.note,
    };

    let snapshot = shadow.commit(line, &tree, &turn.subject())?;
    turn.commit = snapshot.commit;
    turn.at = snapshot.at;
    store.append(&turn)?;
    Ok(Some(turn))
}

/// Resolve `#7`, `7`, or a snapshot commit id to a commit on `line`.
pub fn resolve_target(shadow: &Shadow, store: &Store, line: &str, target: &str) -> Result<String> {
    let bare = target.trim_start_matches('#');
    if let Ok(seq) = bare.parse::<u64>() {
        return match store.find(seq)? {
            Some(turn) => Ok(turn.commit),
            None => bail!("no turn #{seq} on timeline `{line}`"),
        };
    }
    shadow.resolve(line, target)
}

pub struct Planned {
    pub shadow: Shadow,
    pub store: Store,
    pub commit: String,
    pub plan: RestorePlan,
}

/// Work out what a restore would do. Touches nothing.
pub fn plan(
    wt: &Worktree,
    state: &Path,
    line: &str,
    target: &str,
    max_files: usize,
) -> Result<Planned> {
    repo::inspect(wt, max_files)?.bail_if_unsafe()?;
    let shadow = Shadow::ensure(wt.clone(), state)?;
    let store = Store::open(state, &wt.id, line)?;
    let commit = resolve_target(&shadow, &store, line, target)?;
    let tree = shadow.tree_of(&commit)?;
    let plan = shadow.plan(&tree)?;
    Ok(Planned { shadow, store, commit, plan })
}

/// The working tree moved between the moment a plan was made and the moment it
/// was going to be applied.
///
/// This is the whole reason [`restore_expecting`] exists. A user reads a plan
/// that says three files, and while they are reading it the agent keeps
/// working; applying a freshly computed plan at that point would write nine.
/// The plan a user saw has to be the plan that runs, so a moved tree stops the
/// restore and hands back what the truth is now.
#[derive(Debug)]
pub struct StaleTree {
    pub expected: String,
    pub actual: String,
    /// The plan as it stands now, so a caller can show it instead of recomputing.
    pub plan: RestorePlan,
}

impl std::fmt::Display for StaleTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the working tree changed since this plan was made ({} is now {}); nothing was restored",
            &self.expected[..12.min(self.expected.len())],
            &self.actual[..12.min(self.actual.len())]
        )
    }
}

impl std::error::Error for StaleTree {}

/// A restore failed partway through.
///
/// [`Shadow::apply`] removes before it writes, and it has to: a path that
/// changes between a file and a directory cannot be written while the old shape
/// is still there. So a failure in the middle leaves a tree that is neither
/// state, and the only honest thing to do is say so — and try to undo it.
///
/// `recovered` is the difference between "your files are as they were" and
/// "your files are between two states, here is how to get back".
#[derive(Debug)]
pub struct RestoreFailed {
    pub recovered: bool,
    /// The checkpoint holding the state from before the attempt, when one was
    /// taken. Restoring it is the way back.
    pub checkpoint_seq: Option<u64>,
    pub cause: String,
    /// Set when putting the tree back failed as well.
    pub recovery_error: Option<String>,
}

impl std::fmt::Display for RestoreFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the restore failed: {}", self.cause)?;
        if self.recovered {
            return write!(f, ". Your files were put back as they were.");
        }
        write!(f, ". Your working tree is between two states")?;
        if let Some(err) = &self.recovery_error {
            write!(f, ", and putting it back failed too ({err})")?;
        }
        match self.checkpoint_seq {
            Some(seq) => write!(f, " — `sheep restore #{seq} --yes` returns it to how it was."),
            None => write!(f, "."),
        }
    }
}

impl std::error::Error for RestoreFailed {}

#[derive(Debug)]
pub struct Restored {
    pub plan: RestorePlan,
    pub commit: String,
    /// The turn holding the state that was replaced. Restoring it undoes this.
    pub checkpoint: Option<Turn>,
    /// The files were restored, but recording where we landed did not work —
    /// a full state directory, or a merge started in the second the restore
    /// took. The timeline is behind the disk until the next snapshot; the
    /// restore itself succeeded.
    pub bookkeeping_error: Option<String>,
}

/// Restore the working tree to `target`, recording a checkpoint first.
pub fn restore(
    wt: &Worktree,
    state: &Path,
    line: &str,
    target: &str,
    max_files: usize,
) -> Result<Restored> {
    restore_expecting(wt, state, line, target, max_files, None)
}

/// Restore, refusing if the working tree is no longer what `expect_tree` says.
///
/// A caller that showed a plan to a human passes the tree that plan was
/// computed against. The plan is then recomputed here — under the same guards,
/// immediately before anything is written — and if the tree moved, the restore
/// is abandoned with a [`StaleTree`] carrying the current plan. The check lives
/// inside the operation rather than in the caller so that the verified plan and
/// the applied plan are the same object, with no window between them.
pub fn restore_expecting(
    wt: &Worktree,
    state: &Path,
    line: &str,
    target: &str,
    max_files: usize,
    expect_tree: Option<&str>,
) -> Result<Restored> {
    let Planned { shadow, commit, plan, .. } = plan(wt, state, line, target, max_files)?;

    if let Some(expected) = expect_tree {
        if plan.current_tree != expected {
            return Err(StaleTree {
                expected: expected.to_string(),
                actual: plan.current_tree.clone(),
                plan,
            }
            .into());
        }
    }

    if plan.is_noop() {
        return Ok(Restored { plan, commit, checkpoint: None, bookkeeping_error: None });
    }

    let short = short(&commit);
    // Undo has to be undoable. This happens before a single byte of the working
    // tree changes.
    let checkpoint = snap(
        wt,
        state,
        line,
        max_files,
        TurnKind::Checkpoint,
        SnapMeta { note: Some(format!("before restore to {short}")), ..Default::default() },
        true,
    )?;

    if let Err(cause) = shadow.apply(&plan) {
        // The checkpoint tree is exactly what was on disk a moment ago, so a
        // plan from here to there is precisely the repair. Attempt it before
        // reporting anything: a user should have to think about a half-applied
        // tree only when we genuinely could not undo it.
        let mut failure = RestoreFailed {
            recovered: false,
            checkpoint_seq: checkpoint.as_ref().map(|c| c.seq),
            cause: format!("{cause:#}"),
            recovery_error: None,
        };
        if let Some(cp) = &checkpoint {
            match shadow.plan(&cp.tree).and_then(|back| shadow.apply(&back)) {
                Ok(()) => failure.recovered = true,
                Err(e) => failure.recovery_error = Some(format!("{e:#}")),
            }
        }
        return Err(failure.into());
    }

    // Record where we landed, so the timeline describes what is on disk.
    //
    // This runs after the files have already been written, so a failure here is
    // bookkeeping and not the restore. Returning an error would tell someone
    // their files are as they were when they are not, and send them to undo
    // something that worked.
    let bookkeeping_error = snap(
        wt,
        state,
        line,
        max_files,
        TurnKind::Manual,
        SnapMeta { note: Some(format!("restored to {short}")), ..Default::default() },
        true,
    )
    .err()
    .map(|e| format!("{e:#}"));

    Ok(Restored { plan, commit, checkpoint, bookkeeping_error })
}

pub fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}

/// How much history a timeline keeps.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Newest turns to keep. The newest turn is always kept.
    pub keep: usize,
    /// Drop turns older than this many days, subject to `keep`.
    pub max_age_days: Option<u64>,
}

impl Default for Retention {
    fn default() -> Self {
        // Enough to cover days of work at a realistic turn rate, small enough
        // that a machine left running for a year does not accumulate a history
        // nobody will ever scroll to.
        Self { keep: 500, max_age_days: Some(30) }
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    pub line: String,
    pub kept: usize,
    pub dropped: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Shorten one timeline to what `policy` allows.
///
/// Trimming the turn log alone would free nothing: every old commit stays
/// reachable through the parent chain. So the kept turns are rebuilt as a fresh
/// chain — same trees, so every one of them still restores to the same bytes —
/// and only then can the rest be collected.
///
/// `apply == false` reports what would happen and changes nothing.
pub fn collect(
    wt: &Worktree,
    state: &Path,
    line: &str,
    policy: Retention,
    apply: bool,
) -> Result<Collected> {
    let shadow = Shadow::ensure(wt.clone(), state)?;
    let store = Store::open(state, &wt.id, line)?;
    let turns = store.all()?;

    let mut report = Collected {
        line: line.to_string(),
        bytes_before: shadow.size_bytes(),
        ..Default::default()
    };
    if turns.is_empty() {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }

    let cutoff =
        policy.max_age_days.map(|days| shadow::now().saturating_sub(days.saturating_mul(86_400)));
    let floor = turns.len().saturating_sub(policy.keep.max(1));

    let kept: Vec<Turn> = turns
        .iter()
        .enumerate()
        .filter(|(i, turn)| *i >= floor && cutoff.is_none_or(|c| turn.at >= c))
        .map(|(_, turn)| turn.clone())
        .collect();
    // Never leave a timeline empty: an age policy that outruns every turn would
    // otherwise delete a history the user can still see in the interface.
    let mut kept = if kept.is_empty() { vec![turns[turns.len() - 1].clone()] } else { kept };

    report.kept = kept.len();
    report.dropped = turns.len() - kept.len();
    if report.dropped == 0 || !apply {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }

    let chain: Vec<(String, String, u64)> =
        kept.iter().map(|t| (t.tree.clone(), t.subject(), t.at)).collect();
    let rewritten = shadow.rechain(line, &chain)?;

    // The commit ids changed, so the log has to carry the new ones or a restore
    // would look up a commit that is no longer reachable.
    for (turn, commit) in kept.iter_mut().zip(rewritten.iter()) {
        turn.parent = None;
        turn.commit = commit.clone();
    }
    for i in 1..kept.len() {
        kept[i].parent = Some(kept[i - 1].commit.clone());
    }
    store.rewrite(&kept)?;
    shadow.collect()?;

    report.bytes_after = shadow.size_bytes();
    Ok(report)
}

/// Shorten every timeline recorded for `wt`.
pub fn collect_all(
    wt: &Worktree,
    state: &Path,
    policy: Retention,
    apply: bool,
) -> Result<Vec<Collected>> {
    Store::lines_for(state, &wt.id)?
        .into_iter()
        .map(|line| collect(wt, state, &line, policy, apply))
        .collect()
}
