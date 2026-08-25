//! The shadow repository: where Sheep records agent turns.
//!
//! The single most important property of this module is what it does *not* do.
//! Sheep never writes into the user's `.git`. It creates its own bare
//! repository under the state directory and points that repository's
//! `objects/info/alternates` at the user's object database, so unchanged file
//! contents are borrowed rather than copied. Snapshots are built with a
//! throwaway index file, so the user's index, HEAD, branches, stash and reflog
//! are never read for writing and never modified. Uninstalling Sheep is
//! `rm -rf` on one directory.
//!
//! Everything here uses git plumbing (`add`, `write-tree`, `commit-tree`,
//! `read-tree`, `checkout-index`), which by design does not run repository
//! hooks.

use crate::git::Git;
use crate::repo::Worktree;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Commit identity. Pinned so Sheep works on machines with no `user.email`.
const IDENT_NAME: &str = "sheep";
const IDENT_EMAIL: &str = "sheep@localhost";

pub struct Shadow {
    pub git_dir: PathBuf,
    pub worktree: Worktree,
    tmp_dir: PathBuf,
}

/// One recorded point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub parent: Option<String>,
    pub at: u64,
}

/// What a restore would do, computed before anything is touched.
#[derive(Debug, Clone, Default)]
pub struct RestorePlan {
    /// Present in the target, absent or different on disk: will be written.
    pub write: Vec<String>,
    /// Present on disk, absent in the target: will be removed.
    pub remove: Vec<String>,
    pub target_tree: String,
    pub current_tree: String,
}

impl RestorePlan {
    pub fn is_noop(&self) -> bool {
        self.write.is_empty() && self.remove.is_empty()
    }
    pub fn touched(&self) -> usize {
        self.write.len() + self.remove.len()
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Shadow {
    /// Open — creating if necessary — the shadow repository for `wt`.
    ///
    /// Idempotent: safe to call on every snapshot. The alternates file is
    /// rewritten each time so that moving the repository, or a worktree gaining
    /// its own alternates, is picked up rather than silently breaking later.
    pub fn ensure(wt: Worktree, state_dir: &Path) -> Result<Self> {
        let git_dir = state_dir.join("shadow").join(format!("{}.git", wt.id));
        let tmp_dir = state_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("cannot create {}", tmp_dir.display()))?;

        if !git_dir.join("HEAD").exists() {
            std::fs::create_dir_all(&git_dir)
                .with_context(|| format!("cannot create {}", git_dir.display()))?;
            Git::discover(state_dir).run(&[
                "init",
                "--bare",
                "--quiet",
                git_dir.to_str().context("shadow path is not valid UTF-8")?,
            ])?;
        }

        let shadow = Self { git_dir, worktree: wt, tmp_dir };
        shadow.write_alternates()?;
        shadow.mirror_local_excludes()?;
        shadow.write_marker()?;
        Ok(shadow)
    }

    /// Borrow the user's object database instead of copying it.
    ///
    /// This is what keeps the state directory small: a snapshot of a 500 MB
    /// checkout writes only the blobs that actually changed. The cost is a read
    /// dependency on the user's objects, which is why [`Self::verify`] exists.
    fn write_alternates(&self) -> Result<()> {
        let info = self.git_dir.join("objects").join("info");
        std::fs::create_dir_all(&info)?;

        let mut lines: Vec<String> = vec![self.worktree.objects_dir().display().to_string()];
        // If the user's repo itself borrows from somewhere (a --reference clone,
        // a git-alternates setup), we must borrow from there too or we would
        // resolve only half the objects.
        let upstream = self.worktree.objects_dir().join("info").join("alternates");
        if let Ok(existing) = std::fs::read_to_string(&upstream) {
            for line in existing.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let path = PathBuf::from(line);
                let resolved = if path.is_absolute() {
                    path
                } else {
                    self.worktree.objects_dir().join(path)
                };
                lines.push(resolved.display().to_string());
            }
        }

        let body = format!("{}\n", lines.join("\n"));
        let target = info.join("alternates");
        if std::fs::read_to_string(&target).ok().as_deref() != Some(body.as_str()) {
            std::fs::write(&target, body)
                .with_context(|| format!("cannot write {}", target.display()))?;
        }
        Ok(())
    }

    /// `git add` consults `$GIT_DIR/info/exclude`, and our `$GIT_DIR` is the
    /// shadow. Without mirroring, a repository-local exclude the user wrote
    /// would be ignored and Sheep would start capturing files they had
    /// deliberately excluded.
    fn mirror_local_excludes(&self) -> Result<()> {
        let src = self.worktree.common_dir.join("info").join("exclude");
        let dst = self.git_dir.join("info").join("exclude");
        std::fs::create_dir_all(dst.parent().expect("info dir has a parent"))?;
        match std::fs::read(&src) {
            Ok(body) => std::fs::write(&dst, body)?,
            Err(_) => {
                let _ = std::fs::remove_file(&dst);
            }
        }
        Ok(())
    }

    /// A human-readable note next to the shadow repo saying which checkout it
    /// belongs to. The directory name is a hash; without this, garbage
    /// collecting by hand is guesswork.
    fn write_marker(&self) -> Result<()> {
        let marker = self.git_dir.join("sheep-worktree.txt");
        let body = format!("{}\n", self.worktree.root.display());
        if std::fs::read_to_string(&marker).ok().as_deref() != Some(body.as_str()) {
            std::fs::write(marker, body)?;
        }
        Ok(())
    }

    fn git(&self) -> Git {
        Git::scoped(&self.git_dir, &self.worktree.root)
            .with_env("GIT_AUTHOR_NAME", IDENT_NAME)
            .with_env("GIT_AUTHOR_EMAIL", IDENT_EMAIL)
            .with_env("GIT_COMMITTER_NAME", IDENT_NAME)
            .with_env("GIT_COMMITTER_EMAIL", IDENT_EMAIL)
    }

    fn scratch_index(&self, tag: &str) -> PathBuf {
        self.tmp_dir.join(format!("{}-{}-{}.idx", self.worktree.id, tag, std::process::id()))
    }

    /// Hash the current working tree into a git tree object without committing
    /// it and without touching the user's index.
    ///
    /// Ignored files are not captured — `git add -A` honours `.gitignore` — so
    /// `node_modules`, build output and `.env` are outside Sheep's reach in
    /// both directions: never recorded, never overwritten.
    pub fn write_tree(&self, tag: &str) -> Result<String> {
        let index = self.scratch_index(tag);
        let _ = std::fs::remove_file(&index);
        let git = self.git().with_index(&index);
        git.run(&["add", "-A", "--", "."])
            .context("failed to stage the working tree into Sheep's scratch index")?;
        let tree = git.run(&["write-tree"])?;
        let _ = std::fs::remove_file(&index);
        Ok(tree)
    }

    /// Record `tree` as a commit on `refs/sheep/<line>`.
    pub fn commit(&self, line: &str, tree: &str, message: &str) -> Result<Snapshot> {
        let at = now();
        let parent = self.head(line)?;
        let commit = self.commit_tree(tree, parent.as_deref(), message, at)?;
        self.git().run(&["update-ref", &Self::ref_name(line), &commit])?;
        Ok(Snapshot { commit, tree: tree.to_string(), parent, at })
    }

    /// Write a commit object. Author and committer are pinned so Sheep works on
    /// a machine with no `user.email`, and the date is passed in so a rewritten
    /// history keeps the times the turns actually happened.
    fn commit_tree(&self, tree: &str, parent: Option<&str>, message: &str, at: u64) -> Result<String> {
        let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
        if let Some(p) = parent {
            args.push("-p".into());
            args.push(p.to_string());
        }
        args.push("-m".into());
        args.push(message.into());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();

        let date = format!("{at} +0000");
        self.git()
            .with_env("GIT_AUTHOR_DATE", &date)
            .with_env("GIT_COMMITTER_DATE", &date)
            .run(&argv)
    }

    /// Rebuild a timeline from `turns` as a fresh parent chain and point the ref
    /// at it, returning the new commit id for each turn in order.
    ///
    /// This is how history is actually shortened. Dropping entries from the turn
    /// log alone frees nothing, because every old commit stays reachable through
    /// the chain; the oldest kept turn has to become a root before anything
    /// earlier can be collected. The trees are reused untouched, so every kept
    /// turn restores to exactly the same bytes as before — only the commit ids
    /// change, which is why the caller has to rewrite the log with them.
    pub fn rechain(&self, line: &str, turns: &[(String, String, u64)]) -> Result<Vec<String>> {
        let mut parent: Option<String> = None;
        let mut written = Vec::with_capacity(turns.len());
        for (tree, message, at) in turns {
            let commit = self.commit_tree(tree, parent.as_deref(), message, *at)?;
            parent = Some(commit.clone());
            written.push(commit);
        }
        match parent {
            Some(head) => self.git().run(&["update-ref", &Self::ref_name(line), &head])?,
            None => self.git().run(&["update-ref", "-d", &Self::ref_name(line)])?,
        };
        Ok(written)
    }

    /// Drop everything no ref reaches any more.
    ///
    /// Only ever run against Sheep's own shadow repository — the git dir here is
    /// never the user's. Borrowed objects live in the user's store and are not
    /// touched by this.
    pub fn collect(&self) -> Result<()> {
        let git = Git::bare(&self.git_dir);
        git.run(&["reflog", "expire", "--expire=now", "--all"])?;
        git.run(&["gc", "--prune=now", "--quiet"])?;
        Ok(())
    }

    /// Bytes the shadow repository occupies.
    pub fn size_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
            entries
                .flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => walk(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.git_dir)
    }

    /// The ref a timeline records onto.
    ///
    /// Goes through the same slug as the turn log, because a herdr pane id
    /// contains a colon and git refuses it outright — and because the two must
    /// never disagree about which timeline a pane owns.
    fn ref_name(line: &str) -> String {
        format!("refs/sheep/{}", crate::store::slug(line))
    }

    pub fn head(&self, line: &str) -> Result<Option<String>> {
        let out = self.git().output(&["rev-parse", "--verify", "--quiet", &Self::ref_name(line)])?;
        if !out.status.success() {
            return Ok(None);
        }
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if oid.is_empty() { None } else { Some(oid) })
    }

    /// Resolve a user-supplied reference: a full or short commit id, a line
    /// head, or `<line>~N` / `<line>@{N}` style ancestry.
    pub fn resolve(&self, line: &str, spec: &str) -> Result<String> {
        let candidates = [spec.to_string(), Self::ref_name(spec), format!("{}~{spec}", Self::ref_name(line))];
        for candidate in candidates.iter() {
            let out = self.git().output(&["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")])?;
            if out.status.success() {
                let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !oid.is_empty() {
                    return Ok(oid);
                }
            }
        }
        bail!("no snapshot matches `{spec}`");
    }

    pub fn tree_of(&self, commit: &str) -> Result<String> {
        self.git().run(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// Confirm every blob in `tree` is actually readable.
    ///
    /// Because snapshots borrow objects through `alternates`, an aggressive
    /// `git gc --prune=now` in the user's repository can in principle remove an
    /// object a snapshot still references.
    pub fn verify(&self, tree: &str) -> Result<Vec<String>> {
        let entries = self.git().run_z(&["ls-tree", "-r", "-z", tree])?;
        let mut probes: Vec<String> = Vec::with_capacity(entries.len());
        for entry in &entries {
            // "<mode> <type> <oid>\t<path>"
            let head = entry.split('\t').next().unwrap_or_default();
            let mut fields = head.split_whitespace();
            let (_mode, kind, oid) = (fields.next(), fields.next(), fields.next());
            if kind == Some("blob") {
                if let Some(oid) = oid {
                    probes.push(oid.to_string());
                }
            }
        }
        self.batch_check(&probes)
    }

    /// Confirm only the blobs a restore would actually read.
    ///
    /// `checkout-index` touches nothing outside the plan, so verifying the
    /// whole tree would mean tens of thousands of object lookups to protect
    /// three files. `<tree>:<path>` is resolved by `cat-file` directly, which
    /// keeps this proportional to the size of the restore rather than the size
    /// of the repository.
    pub fn verify_paths(&self, tree: &str, paths: &[String]) -> Result<Vec<String>> {
        let probes: Vec<String> = paths.iter().map(|p| format!("{tree}:{p}")).collect();
        self.batch_check(&probes)
    }

    /// Ask git which of `probes` it cannot resolve. One process for all of them.
    fn batch_check(&self, probes: &[String]) -> Result<Vec<String>> {
        if probes.is_empty() {
            return Ok(Vec::new());
        }
        let input = format!("{}\n", probes.join("\n"));
        let out = self.git().run_stdin(&["cat-file", "--batch-check"], input.as_bytes())?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(stdout
            .lines()
            .filter(|line| line.ends_with(" missing"))
            .map(|line| line.trim_end_matches(" missing").to_string())
            .collect())
    }

    /// Work out exactly which paths a restore to `target_tree` would write and
    /// which it would remove. Nothing on disk is touched.
    pub fn plan(&self, target_tree: &str) -> Result<RestorePlan> {
        let current_tree = self.write_tree("plan")?;
        let records = self.git().run_z(&[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-status",
            &current_tree,
            target_tree,
        ])?;

        let mut plan = RestorePlan {
            target_tree: target_tree.to_string(),
            current_tree,
            ..Default::default()
        };
        // `-z --name-status` alternates: status, path, status, path...
        let mut iter = records.into_iter();
        while let (Some(status), Some(path)) = (iter.next(), iter.next()) {
            match status.chars().next() {
                // Deleted going current -> target: it exists now, not in the target.
                Some('D') => plan.remove.push(path),
                _ => plan.write.push(path),
            }
        }
        plan.write.sort();
        plan.remove.sort();
        Ok(plan)
    }

    /// Apply a plan. Removals happen first so that a path changing between a
    /// file and a directory resolves cleanly in both directions.
    ///
    /// Only the paths in the plan are touched. Restoring a three-file turn
    /// rewrites three files, not the whole checkout.
    pub fn apply(&self, plan: &RestorePlan) -> Result<()> {
        if let Some(missing) = self.verify_paths(&plan.target_tree, &plan.write)?.first() {
            bail!(
                "snapshot is incomplete: object {missing} is unreachable.\nThis happens if the repository was garbage-collected with --prune. Sheep will not restore a partial tree."
            );
        }

        let root = &self.worktree.root;

        // A removal is always a single file: `diff-tree -r` reports leaves, and
        // an emptied directory is pruned afterwards. A removal that is a
        // directory on disk therefore means one of two things, and neither may
        // be deleted.
        //
        // The dangerous one is a nested git repository. `git add -A` records
        // any repository inside the worktree as one gitlink entry — a commit
        // pointer, nothing else — so restoring past the point it appeared
        // produces a one-line plan whose contents no snapshot holds. Deleting
        // it would take that repository's own history, its uncommitted work and
        // its ignored files with it, and the checkpoint taken beforehand could
        // not bring any of it back. That is invariants 4, 5 and 8 at once.
        //
        // The other is a path that turned into a directory between the plan and
        // the write, which is the stale-tree case wearing a different hat.
        let directories: Vec<&String> = plan
            .remove
            .iter()
            .filter(|rel| std::fs::symlink_metadata(root.join(rel)).is_ok_and(|m| m.is_dir()))
            .collect();
        if let Some(first) = directories.first() {
            bail!(
                "refusing to restore: `{first}` is a directory whose contents Sheep never captured{}.\nA git repository inside your worktree is recorded only as a pointer, so removing it here would delete files no snapshot holds — including anything ignored inside it. Move or delete it yourself if that is what you want.",
                if directories.len() > 1 {
                    format!(" (and {} more)", directories.len() - 1)
                } else {
                    String::new()
                }
            );
        }

        for rel in &plan.remove {
            let path = root.join(rel);
            // A path already gone is not a problem: the goal is that it is not
            // there afterwards, not that we were the one to remove it.
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e).with_context(|| format!("cannot remove {}", path.display()));
                }
            }
        }
        prune_empty_dirs(root, &plan.remove)?;

        if !plan.write.is_empty() {
            let index = self.scratch_index("apply");
            let _ = std::fs::remove_file(&index);
            let git = self.git().with_index(&index);
            git.run(&["read-tree", &plan.target_tree])?;
            let input = plan
                .write
                .iter()
                .map(|p| format!("{p}\0"))
                .collect::<String>();
            let out = git.run_stdin(&["checkout-index", "-f", "-u", "--stdin", "-z"], input.as_bytes())?;
            if !out.status.success() {
                bail!(
                    "restore failed while writing files: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let _ = std::fs::remove_file(&index);
        }
        Ok(())
    }

    /// Every snapshot on `line`, newest first.
    pub fn log(&self, line: &str, limit: usize) -> Result<Vec<(String, u64, String)>> {
        if self.head(line)?.is_none() {
            return Ok(Vec::new());
        }
        let raw = self.git().run(&[
            "log",
            "--format=%H%x1f%at%x1f%s",
            &format!("-n{limit}"),
            &Self::ref_name(line),
        ])?;
        Ok(raw
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| {
                let mut parts = l.split('\x1f');
                Some((
                    parts.next()?.to_string(),
                    parts.next()?.parse().ok()?,
                    parts.next().unwrap_or_default().to_string(),
                ))
            })
            .collect())
    }

    /// Paths a tree records as gitlinks — repositories nested inside the
    /// worktree, stored as a commit pointer and nothing more.
    pub fn gitlinks(&self, tree: &str) -> Result<Vec<String>> {
        Ok(self
            .git()
            .run_z(&["ls-tree", "-r", "-z", "-t", tree])?
            .into_iter()
            .filter(|entry| entry.starts_with("160000"))
            .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_string()))
            .collect())
    }

    /// How many blobs a tree holds. Used to describe the very first turn on a
    /// timeline, which has no predecessor to diff against.
    pub fn tree_size(&self, tree: &str) -> Result<usize> {
        Ok(self.git().run_z(&["ls-tree", "-r", "-z", tree])?.len())
    }

    /// `files changed, insertions, deletions` between two trees.
    pub fn diffstat(&self, from: &str, to: &str) -> Result<(usize, u64, u64)> {
        let raw = self.git().run(&["diff-tree", "-r", "--numstat", "--no-renames", from, to])?;
        let mut files = 0usize;
        let (mut adds, mut dels) = (0u64, 0u64);
        for line in raw.lines().filter(|l| !l.is_empty()) {
            files += 1;
            let mut cols = line.split('\t');
            adds += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
            dels += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        }
        Ok((files, adds, dels))
    }
}

/// After removing files, drop directories the removal emptied — but never the
/// worktree root, and never a directory that still holds anything (including
/// ignored files such as a build cache).
fn prune_empty_dirs(root: &Path, removed: &[String]) -> Result<()> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in removed {
        let mut cur = PathBuf::from(rel);
        while let Some(parent) = cur.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            dirs.insert(parent.to_path_buf());
            cur = parent.to_path_buf();
        }
    }
    // Deepest first, so a nested chain collapses in one pass.
    for rel in dirs.into_iter().rev() {
        let path = root.join(&rel);
        if path.starts_with(root) && path != *root {
            let _ = std::fs::remove_dir(&path); // fails harmlessly when non-empty
        }
    }
    Ok(())
}
