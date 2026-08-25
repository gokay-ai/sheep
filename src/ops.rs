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

pub struct Restored {
    pub plan: RestorePlan,
    pub commit: String,
    /// The turn holding the state that was replaced. Restoring it undoes this.
    pub checkpoint: Option<Turn>,
}

/// Restore the working tree to `target`, recording a checkpoint first.
pub fn restore(
    wt: &Worktree,
    state: &Path,
    line: &str,
    target: &str,
    max_files: usize,
) -> Result<Restored> {
    let Planned { shadow, commit, plan, .. } = plan(wt, state, line, target, max_files)?;
    if plan.is_noop() {
        return Ok(Restored { plan, commit, checkpoint: None });
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

    shadow.apply(&plan)?;

    // Record where we landed, so the timeline always describes what is on disk.
    snap(
        wt,
        state,
        line,
        max_files,
        TurnKind::Manual,
        SnapMeta { note: Some(format!("restored to {short}")), ..Default::default() },
        true,
    )?;

    Ok(Restored { plan, commit, checkpoint })
}

pub fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}
