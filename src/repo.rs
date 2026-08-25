//! Discovering the user's worktree and deciding whether it is safe to touch.
//!
//! Sheep restores files. The cost of being wrong is someone's uncommitted work,
//! so the checks in this module are refusals, not warnings, wherever the state
//! is ambiguous.

use crate::git::{canonical, Git};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A git worktree Sheep is willing to track.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// Top level of the working tree — the directory whose files we snapshot.
    pub root: PathBuf,
    /// This worktree's own git dir. For a linked worktree this is
    /// `<main>/.git/worktrees/<name>`, not `<main>/.git`.
    pub git_dir: PathBuf,
    /// The shared git dir. For a linked worktree this is the *main* repo's
    /// `.git`, which is where the object database actually lives — the
    /// distinction that makes worktree-per-agent setups work at all.
    pub common_dir: PathBuf,
    /// Stable id derived from `root`. Names the shadow repo and the turn log.
    pub id: String,
}

impl Worktree {
    pub fn discover(start: &Path) -> Result<Self> {
        let git = Git::discover(start);
        if !git.ok(&["rev-parse", "--is-inside-work-tree"]) {
            bail!(
                "{} is not inside a git worktree.\nSheep records turns as git trees, so it needs a repository to record them against.",
                start.display()
            );
        }
        if git.run(&["rev-parse", "--is-bare-repository"])? == "true" {
            bail!("{} is a bare repository: there is no working tree to restore.", start.display());
        }

        let root = canonical(Path::new(
            &git.run(&["rev-parse", "--path-format=absolute", "--show-toplevel"])?,
        ))?;
        let git_dir = canonical(Path::new(
            &git.run(&["rev-parse", "--path-format=absolute", "--git-dir"])?,
        ))?;
        let common_dir = canonical(Path::new(
            &git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?,
        ))?;

        let id = worktree_id(&root);
        Ok(Self { root, git_dir, common_dir, id })
    }

    pub fn git(&self) -> Git {
        Git::discover(&self.root)
    }

    /// The object database every snapshot borrows from.
    pub fn objects_dir(&self) -> PathBuf {
        self.common_dir.join("objects")
    }

    /// True when this is a linked worktree rather than the main checkout.
    pub fn is_linked(&self) -> bool {
        self.git_dir != self.common_dir
    }
}

/// A short, stable, filesystem-safe id for a worktree path.
pub fn worktree_id(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(24)
        .collect();
    format!("{name}-{hex}")
}

/// Something that makes a snapshot or a restore unsafe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// A merge or rebase left conflict stages in the index. Snapshotting would
    /// silently flatten the conflict to whatever is currently in the file, and
    /// restoring would destroy the in-progress resolution.
    UnmergedPaths(Vec<String>),
    /// A rebase, merge, cherry-pick or bisect is mid-flight.
    OperationInProgress(String),
    /// The worktree holds more tracked files than the budget allows.
    TooLarge { files: usize, limit: usize },
}

impl std::fmt::Display for Blocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocker::UnmergedPaths(paths) => write!(
                f,
                "unresolved merge conflicts in {} file(s) (first: {}). Resolve them before Sheep records or restores this tree.",
                paths.len(),
                paths.first().map(String::as_str).unwrap_or("?")
            ),
            Blocker::OperationInProgress(op) => {
                write!(f, "a {op} is in progress. Sheep will not touch a tree mid-operation.")
            }
            Blocker::TooLarge { files, limit } => write!(
                f,
                "{files} tracked files exceeds the {limit}-file budget. Raise it with --max-files if you mean it."
            ),
        }
    }
}

/// Something worth telling the user about that does not stop the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Submodules are recorded as gitlinks: the pointer is restored, the
    /// submodule's own contents are not.
    Submodules(usize),
    /// Ignored files are never captured, so they are never restored either.
    /// This is the property that keeps `node_modules` and `.env` safe.
    IgnoredFilesPresent,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::Submodules(n) => write!(
                f,
                "{n} submodule(s): Sheep records the commit pointer, not the submodule's working tree."
            ),
            Warning::IgnoredFilesPresent => write!(
                f,
                "gitignored files present: Sheep never captures or overwrites them."
            ),
        }
    }
}

pub const DEFAULT_MAX_FILES: usize = 60_000;

#[derive(Debug, Default)]
pub struct Health {
    pub blockers: Vec<Blocker>,
    pub warnings: Vec<Warning>,
    pub tracked_files: usize,
}

impl Health {
    pub fn is_safe(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn bail_if_unsafe(&self) -> Result<()> {
        if let Some(blocker) = self.blockers.first() {
            bail!("refusing to continue: {blocker}");
        }
        Ok(())
    }
}

pub fn inspect(wt: &Worktree, max_files: usize) -> Result<Health> {
    let git = wt.git();
    let mut health = Health::default();

    let unmerged = git.run_z(&["ls-files", "-u", "-z"])?;
    if !unmerged.is_empty() {
        // `ls-files -u -z` emits "<mode> <sha> <stage>\tpath" records.
        let paths: Vec<String> = unmerged
            .iter()
            .filter_map(|rec| rec.split_once('\t').map(|(_, p)| p.to_string()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        health.blockers.push(Blocker::UnmergedPaths(paths));
    }

    for (marker, label) in [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ] {
        if wt.git_dir.join(marker).exists() {
            health.blockers.push(Blocker::OperationInProgress(label.into()));
            break;
        }
    }

    let tracked = git.run_z(&["ls-files", "-z"])?;
    health.tracked_files = tracked.len();
    if tracked.len() > max_files {
        health.blockers.push(Blocker::TooLarge { files: tracked.len(), limit: max_files });
    }

    let gitlinks = git
        .run(&["ls-files", "--stage"])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("160000"))
        .count();
    if gitlinks > 0 {
        health.warnings.push(Warning::Submodules(gitlinks));
    }

    if !git.run_z(&["ls-files", "-o", "-i", "--exclude-standard", "-z"])?.is_empty() {
        health.warnings.push(Warning::IgnoredFilesPresent);
    }

    Ok(health)
}

/// Where Sheep keeps its state. Inside herdr this is handed to us; outside it,
/// we fall back to the platform's state directory so `sheep` works standalone.
pub fn state_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    if let Ok(dir) = std::env::var("SHEEP_STATE_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let base = if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").context("neither HERDR_PLUGIN_STATE_DIR nor HOME is set")?;
        PathBuf::from(home).join(".local").join("state")
    };
    Ok(base.join("sheep"))
}
