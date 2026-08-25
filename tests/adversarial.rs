//! Adversarial tests for the half of Sheep that can destroy someone's work.
//!
//! The product thesis is "undo you can trust". One credible report of Sheep
//! eating uncommitted work ends the project, so these tests are written from
//! the attacker's side: every case here is a way a real repository could be
//! shaped when a restore lands on it.

use sheep::ops::{self, SnapMeta};
use sheep::repo::{self, Blocker, Warning, Worktree};
use sheep::store::TurnKind;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const BUDGET: usize = 60_000;

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    state: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        // macOS puts temp dirs behind a symlink; canonicalize so the paths the
        // tests compare match the ones Sheep resolves.
        let base = fs::canonicalize(dir.path()).expect("canonicalize");
        let repo = base.join("repo");
        let state = base.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&state).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        Self { _dir: dir, repo, state }
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.repo.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.repo.join(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.repo.join(rel).exists()
    }

    fn commit_all(&self, message: &str) {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "--quiet", "-m", message]);
    }

    fn wt(&self) -> Worktree {
        Worktree::discover(&self.repo).expect("worktree discovery")
    }

    fn snap(&self, note: &str) -> u64 {
        self.snap_in(&self.wt(), note)
    }

    fn snap_in(&self, wt: &Worktree, note: &str) -> u64 {
        ops::snap(
            wt,
            &self.state,
            "default",
            BUDGET,
            TurnKind::Turn,
            SnapMeta { note: Some(note.into()), ..Default::default() },
            false,
        )
        .expect("snap should succeed")
        .expect("snap should record a turn")
        .seq
    }

    fn restore(&self, seq: u64) -> ops::Restored {
        ops::restore(&self.wt(), &self.state, "default", &seq.to_string(), BUDGET)
            .expect("restore should succeed")
    }
}

/// Content fingerprint of every file under a directory. Used to prove Sheep
/// leaves the user's `.git` exactly as it found it.
fn fingerprint(root: &Path) -> BTreeMap<String, (u64, u64)> {
    fn walk(dir: &Path, base: &Path, acc: &mut BTreeMap<String, (u64, u64)>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                walk(&path, base, acc);
            } else {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                let body = fs::read(&path).unwrap_or_default();
                let mut sum: u64 = 1469598103934665603;
                for byte in &body {
                    sum ^= *byte as u64;
                    sum = sum.wrapping_mul(1099511628211);
                }
                acc.insert(rel, (body.len() as u64, sum));
            }
        }
    }
    let mut acc = BTreeMap::new();
    walk(root, root, &mut acc);
    acc
}

// ---------------------------------------------------------------------------
// The happy path, stated precisely.
// ---------------------------------------------------------------------------

#[test]
fn restores_a_file_an_agent_rewrote() {
    let f = Fixture::new();
    f.write("src/auth.rs", "fn login() { good() }\n");
    f.commit_all("base");

    let good = f.snap("turn 1");
    f.write("src/auth.rs", "fn login() { WRONG }\n");
    f.write("src/extra.rs", "// the agent also added this\n");
    f.snap("turn 2");

    let done = f.restore(good);
    assert_eq!(f.read("src/auth.rs"), "fn login() { good() }\n");
    assert!(!f.exists("src/extra.rs"), "a file added after the target turn must be removed");
    assert_eq!(done.plan.write, vec!["src/auth.rs".to_string()]);
    assert_eq!(done.plan.remove, vec!["src/extra.rs".to_string()]);
}

#[test]
fn a_restore_is_itself_undoable() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let first = f.snap("turn 1");
    f.write("a.txt", "two\n");
    f.snap("turn 2");

    let done = f.restore(first);
    assert_eq!(f.read("a.txt"), "one\n");

    let checkpoint = done.checkpoint.expect("a checkpoint must be recorded before restoring");
    f.restore(checkpoint.seq);
    assert_eq!(f.read("a.txt"), "two\n", "restoring the checkpoint must put the replaced state back");
}

#[test]
fn an_unchanged_tree_is_not_recorded_twice() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    f.snap("turn 1");

    let repeat = ops::snap(
        &f.wt(),
        &f.state,
        "default",
        BUDGET,
        TurnKind::Turn,
        SnapMeta::default(),
        false,
    )
    .expect("snap should succeed");
    assert!(repeat.is_none(), "an agent turn that changed nothing must not create a turn");
}

#[test]
fn works_in_a_repository_with_no_commits() {
    let f = Fixture::new();
    f.write("first.txt", "hello\n");
    let turn = f.snap("before any commit exists");

    f.write("first.txt", "goodbye\n");
    f.snap("turn 2");
    f.restore(turn);
    assert_eq!(f.read("first.txt"), "hello\n");
}

// ---------------------------------------------------------------------------
// The promise: Sheep does not touch the user's repository.
// ---------------------------------------------------------------------------

#[test]
fn never_writes_into_the_users_git_directory() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let first = f.snap("turn 1");
    f.write("a.txt", "two\n");
    f.write("b.txt", "new\n");
    f.snap("turn 2");

    let before = fingerprint(&f.repo.join(".git"));
    f.restore(first);
    let after = fingerprint(&f.repo.join(".git"));

    assert_eq!(
        before, after,
        "a full snapshot and restore cycle must leave the user's .git byte-identical"
    );
}

#[test]
fn gitignored_files_are_never_captured_and_never_removed() {
    let f = Fixture::new();
    f.write(".gitignore", "node_modules/\n.env\n");
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let first = f.snap("turn 1");

    // Exactly the things a user would be furious to lose.
    f.write("node_modules/left-pad/index.js", "module.exports = 1\n");
    f.write(".env", "SECRET=hunter2\n");
    f.write("a.txt", "two\n");
    f.snap("turn 2");

    let done = f.restore(first);
    assert_eq!(f.read("a.txt"), "one\n");
    assert!(f.exists(".env"), "an ignored file must survive a restore");
    assert_eq!(f.read(".env"), "SECRET=hunter2\n");
    assert!(f.exists("node_modules/left-pad/index.js"), "ignored trees must survive a restore");
    assert!(
        !done.plan.remove.iter().any(|p| p.contains("node_modules") || p.contains(".env")),
        "ignored paths must never appear in a restore plan: {:?}",
        done.plan.remove
    );
}

#[test]
fn a_restore_touches_only_the_paths_in_its_plan() {
    let f = Fixture::new();
    f.write("touched.txt", "one\n");
    f.write("untouched.txt", "stable\n");
    f.commit_all("base");
    let first = f.snap("turn 1");
    f.write("touched.txt", "two\n");
    f.snap("turn 2");

    // Backdate the bystander so any rewrite is unmistakable.
    let bystander = f.repo.join("untouched.txt");
    Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&bystander)
        .status()
        .expect("touch should run");
    let before = fs::metadata(&bystander).unwrap().modified().unwrap();

    f.restore(first);

    let after = fs::metadata(&bystander).unwrap().modified().unwrap();
    assert_eq!(before, after, "a file identical in both trees must not be rewritten");
    assert_eq!(f.read("untouched.txt"), "stable\n");
}

// ---------------------------------------------------------------------------
// Refusals. Ambiguous state is a stop, not a guess.
// ---------------------------------------------------------------------------

#[test]
fn refuses_a_worktree_with_unresolved_conflicts() {
    let f = Fixture::new();
    f.write("conflict.txt", "base\n");
    f.commit_all("base");
    git(&f.repo, &["checkout", "--quiet", "-b", "other"]);
    f.write("conflict.txt", "theirs\n");
    f.commit_all("theirs");
    git(&f.repo, &["checkout", "--quiet", "main"]);
    f.write("conflict.txt", "ours\n");
    f.commit_all("ours");

    let merge = Command::new("git")
        .current_dir(&f.repo)
        .args(["merge", "other"])
        .output()
        .expect("merge should run");
    assert!(!merge.status.success(), "the merge should have conflicted");

    let health = repo::inspect(&f.wt(), BUDGET).expect("inspect should succeed");
    assert!(
        matches!(health.blockers.first(), Some(Blocker::UnmergedPaths(_))),
        "unmerged paths must be a blocker, got {:?}",
        health.blockers
    );

    let attempt = ops::snap(
        &f.wt(),
        &f.state,
        "default",
        BUDGET,
        TurnKind::Turn,
        SnapMeta::default(),
        false,
    );
    assert!(attempt.is_err(), "snapshotting a conflicted tree must fail loudly");
}

#[test]
fn refuses_a_worktree_mid_operation() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    fs::write(f.repo.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();

    let health = repo::inspect(&f.wt(), BUDGET).unwrap();
    assert!(
        matches!(health.blockers.first(), Some(Blocker::OperationInProgress(_))),
        "an in-flight merge must be a blocker, got {:?}",
        health.blockers
    );
}

#[test]
fn refuses_a_directory_that_is_not_a_worktree() {
    let dir = TempDir::new().unwrap();
    let err = Worktree::discover(dir.path()).expect_err("a plain directory must be refused");
    assert!(
        err.to_string().contains("not inside a git worktree"),
        "the error should say why: {err}"
    );
}

#[test]
fn refuses_a_worktree_over_the_file_budget() {
    let f = Fixture::new();
    for i in 0..5 {
        f.write(&format!("f{i}.txt"), "x\n");
    }
    f.commit_all("base");

    let health = repo::inspect(&f.wt(), 2).unwrap();
    assert!(
        matches!(health.blockers.first(), Some(Blocker::TooLarge { files: 5, limit: 2 })),
        "the budget must be enforced, got {:?}",
        health.blockers
    );
    assert!(repo::inspect(&f.wt(), BUDGET).unwrap().is_safe(), "the default budget must allow it");
}

#[test]
fn refuses_to_restore_a_snapshot_with_a_missing_object() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let first = f.snap("turn 1");
    // Content that exists only inside Sheep's own object store.
    f.write("only-in-sheep.txt", "never committed by the user\n");
    f.snap("turn 2");
    f.write("a.txt", "two\n");
    f.snap("turn 3");

    // Simulate the one real hazard of borrowing objects: a pruned blob.
    let wt = f.wt();
    let shadow_dir = f.state.join("shadow").join(format!("{}.git", wt.id));
    let planned = ops::plan(&wt, &f.state, "default", &first.to_string(), BUDGET).unwrap();
    let target = planned.shadow.tree_of(&planned.commit).unwrap();
    let listing = git(&shadow_dir, &["ls-tree", "-r", &target]);
    let oid = listing
        .lines()
        .find(|l| l.ends_with("a.txt"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("a.txt should be in the target tree")
        .to_string();

    // The blob is reachable from the user's own history, so removing it from
    // the shadow store alone is not enough; unlink it from both.
    for objects in [shadow_dir.join("objects"), f.repo.join(".git").join("objects")] {
        let _ = fs::remove_file(objects.join(&oid[..2]).join(&oid[2..]));
        let _ = fs::remove_dir_all(objects.join("pack"));
    }

    let missing = planned.shadow.verify(&target).unwrap();
    assert!(!missing.is_empty(), "verify must notice the pruned object");

    let err = planned
        .shadow
        .apply(&planned.plan)
        .expect_err("restoring an incomplete snapshot must fail");
    assert!(
        err.to_string().contains("incomplete"),
        "the refusal should name the problem: {err}"
    );
    assert_eq!(f.read("a.txt"), "two\n", "a refused restore must not have written anything");
}

// ---------------------------------------------------------------------------
// Shapes a real repository actually takes.
// ---------------------------------------------------------------------------

#[test]
fn handles_a_linked_worktree_and_borrows_the_main_object_database() {
    let f = Fixture::new();
    f.write("a.txt", "main\n");
    f.commit_all("base");

    let linked = f.repo.parent().unwrap().join("agent-1");
    git(&f.repo, &["worktree", "add", "--quiet", "-b", "agent-1", linked.to_str().unwrap()]);

    let wt = Worktree::discover(&linked).expect("a linked worktree must be discoverable");
    assert!(wt.is_linked(), "the linked worktree must be recognised as linked");
    assert_eq!(
        wt.objects_dir(),
        fs::canonicalize(f.repo.join(".git").join("objects")).unwrap(),
        "a linked worktree must borrow the main repository's object database"
    );

    let first = f.snap_in(&wt, "turn 1");
    fs::write(linked.join("a.txt"), "rewritten by the agent\n").unwrap();
    f.snap_in(&wt, "turn 2");

    ops::restore(&wt, &f.state, "default", &first.to_string(), BUDGET).unwrap();
    assert_eq!(fs::read_to_string(linked.join("a.txt")).unwrap(), "main\n");

    let alternates = f
        .state
        .join("shadow")
        .join(format!("{}.git", wt.id))
        .join("objects/info/alternates");
    let body = fs::read_to_string(alternates).unwrap();
    assert!(
        body.contains(wt.objects_dir().to_str().unwrap()),
        "the shadow repo must borrow rather than copy: {body}"
    );
}

#[test]
fn survives_a_file_becoming_a_directory_and_back() {
    let f = Fixture::new();
    f.write("thing", "I am a file\n");
    f.commit_all("base");
    let as_file = f.snap("turn 1");

    fs::remove_file(f.repo.join("thing")).unwrap();
    f.write("thing/inner.txt", "I am a directory now\n");
    let as_dir = f.snap("turn 2");

    f.restore(as_file);
    assert_eq!(f.read("thing"), "I am a file\n");
    assert!(f.repo.join("thing").is_file());

    f.restore(as_dir);
    assert!(f.repo.join("thing").is_dir(), "the directory must come back");
    assert_eq!(f.read("thing/inner.txt"), "I am a directory now\n");
}

#[test]
fn preserves_the_executable_bit_and_symlinks() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        f.write("run.sh", "#!/bin/sh\necho one\n");
        fs::set_permissions(f.repo.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("run.sh", f.repo.join("link")).unwrap();
        f.commit_all("base");
        let first = f.snap("turn 1");

        f.write("run.sh", "#!/bin/sh\necho two\n");
        fs::remove_file(f.repo.join("link")).unwrap();
        f.snap("turn 2");

        f.restore(first);
        let mode = fs::metadata(f.repo.join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bit must survive a restore");
        let link = fs::symlink_metadata(f.repo.join("link")).unwrap();
        assert!(link.file_type().is_symlink(), "the symlink must come back as a symlink");
    }
}

#[test]
fn handles_awkward_filenames_and_binary_content() {
    let f = Fixture::new();
    f.write("a file with spaces.txt", "one\n");
    f.write("türkçe/çalışma dosyası.txt", "iki\n");
    fs::write(f.repo.join("blob.bin"), [0u8, 159, 146, 150, 0, 255, 10]).unwrap();
    f.commit_all("base");
    let first = f.snap("turn 1");

    f.write("a file with spaces.txt", "CHANGED\n");
    f.write("türkçe/çalışma dosyası.txt", "CHANGED\n");
    fs::write(f.repo.join("blob.bin"), [1u8, 2, 3]).unwrap();
    f.snap("turn 2");

    f.restore(first);
    assert_eq!(f.read("a file with spaces.txt"), "one\n");
    assert_eq!(f.read("türkçe/çalışma dosyası.txt"), "iki\n");
    assert_eq!(fs::read(f.repo.join("blob.bin")).unwrap(), vec![0u8, 159, 146, 150, 0, 255, 10]);
}

#[test]
fn reports_submodules_as_a_warning_rather_than_failing() {
    let f = Fixture::new();
    let inner = f.repo.parent().unwrap().join("dep");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "--quiet", "-b", "main"]);
    fs::write(inner.join("dep.txt"), "dep\n").unwrap();
    git(&inner, &["add", "-A"]);
    git(&inner, &["commit", "--quiet", "-m", "dep"]);

    f.write("a.txt", "one\n");
    f.commit_all("base");
    git(
        &f.repo,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", inner.to_str().unwrap(), "vendor/dep"],
    );
    f.commit_all("add submodule");

    let health = repo::inspect(&f.wt(), BUDGET).unwrap();
    assert!(health.is_safe(), "a submodule must not block: {:?}", health.blockers);
    assert!(
        health.warnings.iter().any(|w| matches!(w, Warning::Submodules(_))),
        "a submodule must be surfaced as a warning: {:?}",
        health.warnings
    );

    let first = f.snap("turn 1");
    f.write("a.txt", "two\n");
    f.snap("turn 2");
    f.restore(first);
    assert_eq!(f.read("a.txt"), "one\n");
    assert!(f.exists("vendor/dep/dep.txt"), "the submodule checkout must be left alone");
}

#[test]
fn removing_a_file_empties_its_directory_without_taking_the_neighbours() {
    let f = Fixture::new();
    f.write("keep/stay.txt", "stay\n");
    f.commit_all("base");
    let first = f.snap("turn 1");

    f.write("added/deep/new.txt", "added by the agent\n");
    f.write("keep/also-new.txt", "added by the agent\n");
    f.snap("turn 2");

    f.restore(first);
    assert!(!f.repo.join("added").exists(), "a directory the agent created must be pruned");
    assert!(f.exists("keep/stay.txt"), "a directory with survivors must be kept");
    assert!(!f.exists("keep/also-new.txt"));
}

// ---------------------------------------------------------------------------
// Scale. Small fixtures hide the failures that only appear at real repository
// sizes, so this case exists specifically to keep one of them from coming back.
// ---------------------------------------------------------------------------

#[test]
fn survives_a_repository_large_enough_to_fill_a_pipe_buffer() {
    // Regression: verification feeds one line per object to
    // `git cat-file --batch-check`. Writing all of it before reading any of the
    // reply deadlocked the moment the child's stdout buffer filled, which a
    // few thousand files is more than enough to do. It hung rather than failed,
    // which is the worst way for a restore to break.
    let f = Fixture::new();
    for i in 0..5_000 {
        f.write(&format!("pkg/m{:02}/f{i}.ts", i % 50), "export const v = 1;\n");
    }
    f.commit_all("base");
    let baseline = f.snap("turn 1");

    f.write("pkg/m00/f0.ts", "BROKEN\n");
    f.write("pkg/m01/f1.ts", "BROKEN\n");
    f.write("pkg/m00/new.ts", "added by the agent\n");
    f.snap("turn 2");

    let done = f.restore(baseline);
    assert_eq!(done.plan.write.len(), 2, "only the modified files should be written");
    assert_eq!(done.plan.remove, vec!["pkg/m00/new.ts".to_string()]);
    assert_eq!(f.read("pkg/m00/f0.ts"), "export const v = 1;\n");
    assert!(!f.exists("pkg/m00/new.ts"));
}

#[test]
fn the_first_turn_reports_the_size_of_the_tree() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.write("b/c.txt", "two\n");
    f.commit_all("base");

    let turn = ops::snap(
        &f.wt(),
        &f.state,
        "default",
        BUDGET,
        TurnKind::Turn,
        SnapMeta::default(),
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(turn.files, 2, "a baseline turn should describe what it captured, not `0 files`");
}
