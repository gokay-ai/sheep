//! Discovering the user's worktree and deciding whether it is safe to touch.
//!
//! Sheep restores files. The cost of being wrong is someone's uncommitted work,
//! so the checks in this module are refusals, not warnings, wherever the state
//! is ambiguous.

use crate::git::{canonical, Git};
use anyhow::{bail, Result};
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

        let root = canonical(Path::new(&git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
        ])?))?;
        let git_dir = canonical(Path::new(&git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
        ])?))?;
        let common_dir = canonical(Path::new(&git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?))?;

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
    /// A git repository inside the worktree that the user has not registered as
    /// a submodule. Sheep records it as a pointer, so it can neither capture
    /// nor restore anything inside it — and it will refuse to remove it.
    NestedRepositories(Vec<String>),
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
            Warning::NestedRepositories(paths) => write!(
                f,
                "{} nested git repositor{} ({}): recorded as a pointer only, so nothing inside {} captured or restored — and a restore that would remove {} will refuse instead.",
                paths.len(),
                if paths.len() == 1 { "y" } else { "ies" },
                paths.join(", "),
                if paths.len() == 1 { "it is" } else { "they are" },
                if paths.len() == 1 { "it" } else { "them" },
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

    // An untracked directory that is itself a repository is the case `ls-files
    // --stage` cannot see: it has no index entry until something commits it,
    // yet `git add -A` will record it as a gitlink the moment Sheep snapshots.
    // `--directory` stops git from listing its contents, so this stays cheap.
    let nested: Vec<String> = git
        .run_z(&["ls-files", "-o", "--directory", "--exclude-standard", "-z"])?
        .into_iter()
        .filter(|entry| entry.ends_with('/'))
        .filter(|entry| wt.root.join(entry.trim_end_matches('/')).join(".git").exists())
        .map(|entry| entry.trim_end_matches('/').to_string())
        .collect();
    if !nested.is_empty() {
        health.warnings.push(Warning::NestedRepositories(nested));
    }

    Ok(health)
}

/// Where Sheep keeps its state. Inside herdr this is handed to us; outside it,
/// we fall back to the platform's state directory so `sheep` works standalone.
///
/// Precedence, highest first: `HERDR_PLUGIN_STATE_DIR`, `SHEEP_STATE_DIR`,
/// `XDG_STATE_HOME`, then the platform's own home. On Windows that is
/// `%LOCALAPPDATA%` before `%USERPROFILE%`, and both before `HOME`: a Windows
/// user has `HOME` only when the shell invents one, so preferring it would mean
/// turns recorded from Git Bash were invisible from PowerShell.
pub fn state_dir() -> Result<PathBuf> {
    state_dir_from(|var| std::env::var(var).ok(), cfg!(windows))
}

/// The lookup behind [`state_dir`], with the environment and the platform
/// passed in so both can be tested without touching either.
fn state_dir_from(env: impl Fn(&str) -> Option<String>, windows: bool) -> Result<PathBuf> {
    // The variables consulted, in order, each with what is appended to it. An
    // empty suffix means the variable names the state directory itself.
    //
    // As data rather than as a chain of `if let`s, because the error is
    // generated from the same list: the message used to name two of the four
    // and omit `SHEEP_STATE_DIR`, which is the one that would have fixed it.
    let mut sources: Vec<(&str, &[&str])> = vec![
        ("HERDR_PLUGIN_STATE_DIR", &[]),
        ("SHEEP_STATE_DIR", &[]),
        ("XDG_STATE_HOME", &["sheep"]),
    ];
    if windows {
        sources.push(("LOCALAPPDATA", &["sheep"]));
        sources.push(("USERPROFILE", &["AppData", "Local", "sheep"]));
    }
    sources.push(("HOME", &[".local", "state", "sheep"]));

    for (var, suffix) in &sources {
        // Empty is unset. A variable exported as `""` would otherwise put the
        // state directory somewhere relative to whatever the process's working
        // directory happened to be.
        let Some(value) = env(var).filter(|value| !value.is_empty()) else { continue };
        let mut path = PathBuf::from(value);
        path.extend(*suffix);
        return Ok(path);
    }

    let names: Vec<&str> = sources.iter().map(|(var, _)| *var).collect();
    bail!(
        "cannot work out where to keep Sheep's state: none of {} is set.\nSet SHEEP_STATE_DIR to a directory Sheep may write to.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::state_dir_from;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Every variable `state_dir` consults, so that a test can walk them
    /// without repeating the order the function itself defines.
    const UNIX: [&str; 4] = ["HERDR_PLUGIN_STATE_DIR", "SHEEP_STATE_DIR", "XDG_STATE_HOME", "HOME"];
    const WINDOWS_ONLY: [&str; 2] = ["LOCALAPPDATA", "USERPROFILE"];

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |var: &str| map.get(var).cloned()
    }

    #[test]
    fn the_state_directory_is_named_by_the_first_variable_that_is_set() {
        let all = env(&[
            ("HERDR_PLUGIN_STATE_DIR", "/herdr"),
            ("SHEEP_STATE_DIR", "/sheep"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/a"),
        ]);
        assert_eq!(state_dir_from(&all, false).unwrap(), PathBuf::from("/herdr"));

        let without_herdr =
            env(&[("SHEEP_STATE_DIR", "/sheep"), ("XDG_STATE_HOME", "/xdg"), ("HOME", "/home/a")]);
        assert_eq!(state_dir_from(&without_herdr, false).unwrap(), PathBuf::from("/sheep"));

        let xdg = env(&[("XDG_STATE_HOME", "/xdg"), ("HOME", "/home/a")]);
        assert_eq!(state_dir_from(&xdg, false).unwrap(), PathBuf::from("/xdg/sheep"));

        let home = env(&[("HOME", "/home/a")]);
        assert_eq!(
            state_dir_from(&home, false).unwrap(),
            PathBuf::from("/home/a/.local/state/sheep")
        );
    }

    #[test]
    fn a_variable_set_to_nothing_counts_as_unset() {
        // Exported as `""` — which is what a shell script does when it passes
        // an unset variable on — the state directory would otherwise be
        // relative to whatever directory the process happened to start in.
        let empty = env(&[("SHEEP_STATE_DIR", ""), ("HOME", "/home/a")]);
        assert_eq!(
            state_dir_from(&empty, false).unwrap(),
            PathBuf::from("/home/a/.local/state/sheep"),
            "an empty variable must not name a relative state directory"
        );
    }

    #[test]
    fn windows_has_somewhere_to_put_it_without_home() {
        // The case that used to be HOME-or-bust. A Windows shell that never
        // sets HOME left `sheep` with nowhere to record, and an error naming
        // two variables, neither of which Windows has.
        let local = env(&[("LOCALAPPDATA", "C:\\Users\\a\\AppData\\Local")]);
        assert_eq!(
            state_dir_from(&local, true).unwrap(),
            PathBuf::from("C:\\Users\\a\\AppData\\Local").join("sheep")
        );

        let profile = env(&[("USERPROFILE", "C:\\Users\\a")]);
        assert_eq!(
            state_dir_from(&profile, true).unwrap(),
            PathBuf::from("C:\\Users\\a").join("AppData").join("Local").join("sheep")
        );

        // A Git Bash `HOME` must not win over the location every other shell on
        // the machine agrees on, or turns recorded in one are invisible in the
        // other.
        let both = env(&[("LOCALAPPDATA", "C:\\Local"), ("HOME", "/c/Users/a")]);
        assert_eq!(state_dir_from(&both, true).unwrap(), PathBuf::from("C:\\Local").join("sheep"));

        // And none of it changes anything anywhere else.
        assert_eq!(
            state_dir_from(env(&[("LOCALAPPDATA", "C:\\Local"), ("HOME", "/home/a")]), false)
                .unwrap(),
            PathBuf::from("/home/a/.local/state/sheep")
        );
    }

    #[test]
    fn the_error_names_every_variable_it_looked_at() {
        // The bug this replaces: the message named `HERDR_PLUGIN_STATE_DIR` and
        // `HOME`, and left out `SHEEP_STATE_DIR` — the one variable a user
        // could have set to get out of it.
        for (windows, expected) in [
            (false, UNIX.to_vec()),
            (true, UNIX.iter().chain(WINDOWS_ONLY.iter()).copied().collect()),
        ] {
            let err = state_dir_from(env(&[]), windows)
                .expect_err("with nothing set there is nowhere to record");
            let message = format!("{err:#}");
            for var in &expected {
                assert!(
                    message.contains(var),
                    "the error must name {var}, the user's way out of it: {message}"
                );
            }
        }
    }

    #[test]
    fn a_worktree_id_separates_two_checkouts_with_the_same_name() {
        // Two `fix/` branches checked out side by side is the ordinary shape of
        // a worktree-per-agent session, and the directory name alone does not
        // tell them apart.
        assert_ne!(
            super::worktree_id(std::path::Path::new("/a/fix")),
            super::worktree_id(std::path::Path::new("/b/fix"))
        );
    }
}
