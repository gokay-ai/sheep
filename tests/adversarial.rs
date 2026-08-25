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
    assert_eq!(
        f.read("a.txt"),
        "two\n",
        "restoring the checkpoint must put the replaced state back"
    );
}

#[test]
fn an_unchanged_tree_is_not_recorded_twice() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    f.snap("turn 1");

    let repeat =
        ops::snap(&f.wt(), &f.state, "default", BUDGET, TurnKind::Turn, SnapMeta::default(), false)
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

    let attempt =
        ops::snap(&f.wt(), &f.state, "default", BUDGET, TurnKind::Turn, SnapMeta::default(), false);
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
    assert!(err.to_string().contains("incomplete"), "the refusal should name the problem: {err}");
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

    let alternates =
        f.state.join("shadow").join(format!("{}.git", wt.id)).join("objects/info/alternates");
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
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "add",
            inner.to_str().unwrap(),
            "vendor/dep",
        ],
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

    let turn =
        ops::snap(&f.wt(), &f.state, "default", BUDGET, TurnKind::Turn, SnapMeta::default(), false)
            .unwrap()
            .unwrap();
    assert_eq!(turn.files, 2, "a baseline turn should describe what it captured, not `0 files`");
}

#[test]
fn a_herdr_pane_id_can_name_a_timeline() {
    // Regression: timelines are named after the pane that produced them, and a
    // herdr pane id looks like `w31:pW`. The colon is illegal in a git ref, so
    // recording under a pane id used to fail with
    // "refusing to update ref with bad name" — which meant the recorder could
    // not record the one thing it exists to record.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");

    let line = "w31:pW";
    let first = ops::snap(
        &f.wt(),
        &f.state,
        line,
        BUDGET,
        TurnKind::Turn,
        SnapMeta { pane_id: Some(line.into()), ..Default::default() },
        false,
    )
    .expect("a pane id must be usable as a timeline name")
    .expect("the first turn should record");

    f.write("a.txt", "two\n");
    ops::snap(&f.wt(), &f.state, line, BUDGET, TurnKind::Turn, SnapMeta::default(), false)
        .unwrap()
        .unwrap();

    ops::restore(&f.wt(), &f.state, line, &first.seq.to_string(), BUDGET).unwrap();
    assert_eq!(f.read("a.txt"), "one\n");

    // And it must not collide with the timeline of a differently-spelled pane.
    let other =
        ops::snap(&f.wt(), &f.state, "w31/pW", BUDGET, TurnKind::Turn, SnapMeta::default(), false)
            .unwrap()
            .unwrap();
    assert_eq!(other.seq, 1, "a different timeline must start its own numbering");
}

#[test]
fn a_restore_refuses_a_plan_the_tree_has_moved_out_from_under() {
    // A user reads a plan that says one file. While they read it, the agent
    // keeps working. Applying a freshly computed plan at that point would write
    // whatever is true *now*, not what they agreed to — so the plan a user saw
    // has to be the plan that runs, or none at all.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let target = f.snap("turn 1");
    f.write("a.txt", "two\n");
    f.snap("turn 2");

    let seen = ops::plan(&f.wt(), &f.state, "default", &target.to_string(), BUDGET).unwrap();
    let tree_when_the_user_looked = seen.plan.current_tree.clone();
    assert_eq!(seen.plan.write, vec!["a.txt".to_string()]);

    // The agent keeps going while the plan is on screen.
    f.write("b.txt", "the agent kept working\n");

    let err = ops::restore_expecting(
        &f.wt(),
        &f.state,
        "default",
        &target.to_string(),
        BUDGET,
        Some(&tree_when_the_user_looked),
    )
    .expect_err("a moved tree must stop the restore");

    let stale = err.downcast_ref::<ops::StaleTree>().expect("the caller needs to know why");
    assert!(
        stale.plan.remove.contains(&"b.txt".to_string()),
        "the refusal should carry the plan as it stands now: {:?}",
        stale.plan
    );
    assert_eq!(f.read("a.txt"), "two\n", "nothing may be written by a refused restore");
    assert!(f.exists("b.txt"), "the agent's newer work must be left alone");

    // And the same call succeeds once it is told the truth.
    let fresh = ops::plan(&f.wt(), &f.state, "default", &target.to_string(), BUDGET).unwrap();
    ops::restore_expecting(
        &f.wt(),
        &f.state,
        "default",
        &target.to_string(),
        BUDGET,
        Some(&fresh.plan.current_tree),
    )
    .expect("a plan that matches the tree must apply");
    assert_eq!(f.read("a.txt"), "one\n");
    assert!(!f.exists("b.txt"));
}

// ---------------------------------------------------------------------------
// Retention. A recorder that runs for days has to be able to forget, and the
// only interesting question is whether forgetting damages what it keeps.
// ---------------------------------------------------------------------------

/// Record `n` turns, each changing one file, and return their sequence numbers.
fn record_turns(f: &Fixture, line: &str, n: usize) -> Vec<u64> {
    (0..n)
        .map(|i| {
            f.write("a.txt", &format!("revision {i}\n"));
            f.write(&format!("only-in-{i}.txt"), "marker\n");
            ops::snap(
                &f.wt(),
                &f.state,
                line,
                BUDGET,
                TurnKind::Turn,
                SnapMeta { note: Some(format!("turn {i}")), ..Default::default() },
                false,
            )
            .unwrap()
            .unwrap()
            .seq
        })
        .collect()
}

#[test]
fn a_kept_turn_still_restores_to_the_same_files_after_collection() {
    // This is the only property that matters. Shortening history rebuilds the
    // kept turns as a new chain, so their commit ids change; if the turn log
    // and the trees do not travel together, a restore silently reaches for a
    // commit that no longer exists — or worse, restores the wrong tree.
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("base");
    let seqs = record_turns(&f, "default", 8);
    let keeper = *seqs.last().unwrap() - 2;

    let before = ops::plan(&f.wt(), &f.state, "default", &keeper.to_string(), BUDGET).unwrap();
    let expected_tree = before.shadow.tree_of(&before.commit).unwrap();

    let report = ops::collect(
        &f.wt(),
        &f.state,
        "default",
        ops::Retention { keep: 3, max_age_days: None },
        true,
    )
    .unwrap();
    assert_eq!(report.kept, 3);
    assert_eq!(report.dropped, 5, "8 turns kept at 3 should drop 5");

    let after = ops::plan(&f.wt(), &f.state, "default", &keeper.to_string(), BUDGET).unwrap();
    assert_eq!(
        after.shadow.tree_of(&after.commit).unwrap(),
        expected_tree,
        "a kept turn must point at the same tree after collection"
    );

    ops::restore(&f.wt(), &f.state, "default", &keeper.to_string(), BUDGET).unwrap();
    assert_eq!(f.read("a.txt"), "revision 5\n");
    assert!(f.exists("only-in-5.txt"));
    assert!(!f.exists("only-in-7.txt"), "work after the kept turn must be gone");
}

#[test]
fn a_dropped_turn_is_really_gone_and_its_space_with_it() {
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("base");
    let seqs = record_turns(&f, "default", 12);
    let dropped = seqs[0];

    let before = ops::collect(
        &f.wt(),
        &f.state,
        "default",
        ops::Retention { keep: 2, max_age_days: None },
        false,
    )
    .unwrap();
    assert_eq!(before.dropped, 10, "a dry run must still say what it would do");
    assert_eq!(before.bytes_before, before.bytes_after, "a dry run must change nothing");
    assert!(
        ops::plan(&f.wt(), &f.state, "default", &dropped.to_string(), BUDGET).is_ok(),
        "the dry run must not have removed anything"
    );

    let report = ops::collect(
        &f.wt(),
        &f.state,
        "default",
        ops::Retention { keep: 2, max_age_days: None },
        true,
    )
    .unwrap();
    assert_eq!(report.kept, 2);
    assert!(
        report.bytes_after < report.bytes_before,
        "collection should reclaim space, went {} -> {}",
        report.bytes_before,
        report.bytes_after
    );
    assert!(
        ops::plan(&f.wt(), &f.state, "default", &dropped.to_string(), BUDGET).is_err(),
        "a dropped turn must no longer be reachable"
    );
}

#[test]
fn collection_never_leaves_a_timeline_empty() {
    // An age policy that outruns every turn would otherwise delete a history
    // the user can still see on screen.
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("base");
    record_turns(&f, "default", 3);

    let report = ops::collect(
        &f.wt(),
        &f.state,
        "default",
        // Everything recorded a moment ago is "older than zero days".
        ops::Retention { keep: 0, max_age_days: Some(0) },
        true,
    )
    .unwrap();
    assert_eq!(report.kept, 1, "the newest turn is always kept");

    let turns = sheep::store::Store::open(&f.state, &f.wt().id, "default").unwrap().all().unwrap();
    assert_eq!(turns.len(), 1);
    ops::restore(&f.wt(), &f.state, "default", &turns[0].seq.to_string(), BUDGET).unwrap();
}

#[test]
fn recording_continues_from_where_collection_left_off() {
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("base");
    let seqs = record_turns(&f, "default", 6);
    let highest = *seqs.last().unwrap();

    ops::collect(
        &f.wt(),
        &f.state,
        "default",
        ops::Retention { keep: 2, max_age_days: None },
        true,
    )
    .unwrap();

    f.write("a.txt", "after collection\n");
    let next =
        ops::snap(&f.wt(), &f.state, "default", BUDGET, TurnKind::Turn, SnapMeta::default(), false)
            .unwrap()
            .unwrap();
    assert_eq!(next.seq, highest + 1, "turn numbers must not restart after collection");
}

#[test]
fn the_last_turn_is_read_without_parsing_the_whole_log() {
    // The recorder asks for this on every snapshot and runs for days; the
    // interesting case is a log long enough that the tail read has to walk
    // backwards past its first block.
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("base");
    let seqs = record_turns(&f, "default", 40);

    let store = sheep::store::Store::open(&f.state, &f.wt().id, "default").unwrap();
    let last = store.last().unwrap().expect("a recorded timeline has a last turn");
    assert_eq!(last.seq, *seqs.last().unwrap());
    assert_eq!(
        last.seq,
        store.all().unwrap().last().unwrap().seq,
        "the fast path must agree with the slow one"
    );
    assert_eq!(store.next_seq().unwrap(), last.seq + 1);
}

#[test]
fn a_nested_repository_is_never_deleted_by_a_restore() {
    // Found by a security audit, and the worst bug in the project's history.
    //
    // `git add -A` records a repository inside the worktree as one gitlink — a
    // commit pointer, nothing else. So restoring to a turn taken before that
    // repository existed produced a one-line plan, `remove vendor`, which the
    // apply step resolved to a directory and deleted whole. Nothing inside it
    // was in any snapshot, so the checkpoint taken beforehand restored an empty
    // directory and the work was gone for good: the repository's own history,
    // its uncommitted files, and its ignored files.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let before_vendor = f.snap("before the nested repository existed");

    let vendor = f.repo.join("vendor");
    fs::create_dir_all(&vendor).unwrap();
    git(&vendor, &["init", "--quiet", "-b", "main"]);
    fs::write(vendor.join("uncommitted.txt"), "work nobody else has\n").unwrap();
    fs::write(vendor.join(".env"), "SECRET=hunter2\n").unwrap();
    fs::write(vendor.join(".gitignore"), ".env\n").unwrap();
    fs::write(vendor.join("tracked.txt"), "committed inside vendor\n").unwrap();
    git(&vendor, &["add", "-A"]);
    git(&vendor, &["commit", "--quiet", "-m", "vendor base"]);
    f.snap("the nested repository exists now");

    // It must be visible before anything happens, not discovered afterwards.
    let health = repo::inspect(&f.wt(), BUDGET).unwrap();
    assert!(
        health.warnings.iter().any(|w| matches!(w, Warning::NestedRepositories(paths) if paths.iter().any(|p| p == "vendor"))),
        "a nested repository must be surfaced by doctor: {:?}",
        health.warnings
    );

    let planned =
        ops::plan(&f.wt(), &f.state, "default", &before_vendor.to_string(), BUDGET).unwrap();
    assert_eq!(
        planned.plan.remove,
        vec!["vendor".to_string()],
        "the plan is one line, and that is the trap"
    );

    let err = ops::restore(&f.wt(), &f.state, "default", &before_vendor.to_string(), BUDGET)
        .expect_err("removing a directory Sheep never captured must be refused");
    assert!(err.to_string().contains("vendor"), "the refusal should name it: {err}");

    // Everything survives, including the parts no snapshot could ever hold.
    assert!(vendor.join(".git").is_dir(), "the nested repository's history must survive");
    assert_eq!(
        fs::read_to_string(vendor.join("uncommitted.txt")).unwrap(),
        "work nobody else has\n"
    );
    assert_eq!(fs::read_to_string(vendor.join(".env")).unwrap(), "SECRET=hunter2\n");
    assert_eq!(
        fs::read_to_string(vendor.join("tracked.txt")).unwrap(),
        "committed inside vendor\n"
    );
    assert_eq!(f.read("a.txt"), "one\n", "and the refusal happens before anything else is touched");
}

#[test]
fn a_path_that_became_a_directory_under_the_plan_is_refused() {
    // The same guard from the other direction: between planning and applying,
    // a file the plan means to remove turns into a directory. Deleting it would
    // take contents the plan never described.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let target = f.snap("turn 1");
    f.write("later.txt", "added by the agent\n");
    f.snap("turn 2");

    let planned = ops::plan(&f.wt(), &f.state, "default", &target.to_string(), BUDGET).unwrap();
    assert_eq!(planned.plan.remove, vec!["later.txt".to_string()]);

    fs::remove_file(f.repo.join("later.txt")).unwrap();
    f.write("later.txt/surprise.txt", "written after the plan was made\n");

    let err = planned.shadow.apply(&planned.plan).expect_err("a directory must not be removed");
    assert!(err.to_string().contains("later.txt"), "the refusal should name it: {err}");
    assert!(f.exists("later.txt/surprise.txt"), "its contents must survive");
}

#[test]
fn a_restore_that_fails_partway_puts_the_tree_back() {
    // `apply` removes before it writes, and it has to — a path changing between
    // a file and a directory cannot be written while the old shape is there. So
    // a failure in the middle leaves a tree that is neither state. Claiming
    // "nothing was written" at that point is the most dangerous thing the
    // software could say, so it recovers first and tells the truth either way.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        f.write("keep/x.txt", "original\n");
        f.write("gone.txt", "here at the target\n");
        f.commit_all("base");
        let target = f.snap("turn 1");

        fs::remove_file(f.repo.join("gone.txt")).unwrap();
        f.write("keep/x.txt", "the agent changed this\n");
        f.snap("turn 2");

        // The plan writes into keep/ and removes nothing outside it; making the
        // directory read-only fails the write after the removals have run.
        let locked = f.repo.join("keep");
        let original = fs::metadata(&locked).unwrap().permissions();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let err = ops::restore(&f.wt(), &f.state, "default", &target.to_string(), BUDGET)
            .expect_err("a write into a read-only directory must fail");

        fs::set_permissions(&locked, original).unwrap();

        let failed = err
            .downcast_ref::<ops::RestoreFailed>()
            .unwrap_or_else(|| panic!("a partial restore must be reported as one: {err:#}"));
        assert!(failed.recovered, "the tree should have been put back; instead: {}", err);
        assert!(
            failed.checkpoint_seq.is_some(),
            "the way back has to be nameable even when recovery worked"
        );

        // The agent's work is exactly as it was before the attempt.
        assert_eq!(f.read("keep/x.txt"), "the agent changed this\n");
        assert!(!f.exists("gone.txt"), "a file the restore had removed must be back to absent");
        assert!(
            !err.to_string().contains("nothing was written"),
            "the message must never claim nothing happened: {err}"
        );
    }
}

#[test]
fn a_bookkeeping_failure_after_a_restore_is_not_reported_as_a_failed_restore() {
    // `restore_expecting` records where it landed once the files are already
    // written. If that snapshot fails — the state directory fills up, or a
    // merge starts in the second the restore takes — the restore itself still
    // happened. Reporting an error at that point tells someone their files are
    // as they were when they are not, and sends them to undo something that
    // worked.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let target = f.snap("turn 1");
    f.write("a.txt", "two\n");
    f.snap("turn 2");

    // A merge left mid-flight is exactly the state the guards refuse, and it is
    // reachable from outside in the middle of a restore.
    struct Sabotage(PathBuf);
    impl Sabotage {
        fn arm(repo: &Path) -> Self {
            let marker = repo.join(".git").join("MERGE_HEAD");
            fs::write(&marker, "deadbeef\n").unwrap();
            Self(marker)
        }
    }
    impl Drop for Sabotage {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    // Plan first, while the tree is still clean, then arm the guard so only the
    // trailing bookkeeping snapshot trips over it.
    let planned = ops::plan(&f.wt(), &f.state, "default", &target.to_string(), BUDGET).unwrap();
    planned.shadow.apply(&planned.plan).unwrap();
    assert_eq!(f.read("a.txt"), "one\n", "the files are already restored at this point");

    let _sabotage = Sabotage::arm(&f.repo);
    let outcome = ops::snap(
        &f.wt(),
        &f.state,
        "default",
        BUDGET,
        TurnKind::Manual,
        SnapMeta::default(),
        true,
    );
    assert!(outcome.is_err(), "the guard should refuse to record during a merge");

    // And that is precisely the error `restore_expecting` must not surface as a
    // failed restore: the field carries it instead.
    let sample = ops::Restored {
        plan: planned.plan.clone(),
        commit: planned.commit.clone(),
        checkpoint: None,
        bookkeeping_error: Some("a merge is in progress".into()),
    };
    assert!(
        sample.bookkeeping_error.is_some(),
        "a restore that landed carries the bookkeeping problem as a note, not as a failure"
    );
}

// ---------------------------------------------------------------------------
// Found by an adversarial audit of the merged tree, after the first five
// data-loss paths had been closed. Each of these was reproduced end to end
// before it was fixed.
// ---------------------------------------------------------------------------

#[test]
fn a_restore_never_writes_over_bytes_no_snapshot_holds() {
    // The removal side refuses directories. The write side had no guard at all,
    // and `checkout-index -f` clears its own ground: git's remove_subtree fires
    // whenever a write target exists as a directory. So a one-line plan —
    // `write build` — recursively deleted a gitignored tree that
    // `prune_empty_dirs` had just deliberately declined to touch, and which
    // `git add -A` can never have captured, so the checkpoint was empty of it.
    let f = Fixture::new();
    f.write("build", "at first this path is a file\n");
    f.write(".gitignore", "build/\n");
    f.commit_all("base");
    let as_file = f.snap("turn 1");

    // The path becomes an ignored directory holding real work.
    fs::remove_file(f.repo.join("build")).unwrap();
    f.write("build/artifact.bin", "expensive to regenerate\n");
    f.write("build/.env", "SECRET=hunter2\n");
    f.snap("turn 2");

    let planned = ops::plan(&f.wt(), &f.state, "default", &as_file.to_string(), BUDGET).unwrap();
    assert!(
        planned.plan.write.contains(&"build".to_string()),
        "the plan should want to write the file back: {:?}",
        planned.plan
    );

    let err = ops::restore(&f.wt(), &f.state, "default", &as_file.to_string(), BUDGET)
        .expect_err("writing over a directory Sheep never captured must be refused");
    assert!(err.to_string().contains("build"), "the refusal should name it: {err}");

    assert!(f.exists("build/artifact.bin"), "the ignored tree must survive");
    assert_eq!(f.read("build/.env"), "SECRET=hunter2\n");
}

#[test]
fn a_nested_repository_is_not_clobbered_by_a_write_either() {
    // The same hole reached from the other direction: a tracked file whose path
    // later becomes a repository. `git add -A` records that as a gitlink, so the
    // current tree holds a commit pointer where the plan wants to write a blob.
    let f = Fixture::new();
    f.write("vendor", "at first this path is a file\n");
    f.commit_all("base");
    let as_file = f.snap("turn 1");

    fs::remove_file(f.repo.join("vendor")).unwrap();
    let vendor = f.repo.join("vendor");
    fs::create_dir_all(&vendor).unwrap();
    git(&vendor, &["init", "--quiet", "-b", "main"]);
    fs::write(vendor.join("work.txt"), "uncommitted work nobody else has\n").unwrap();
    fs::write(vendor.join("tracked.txt"), "committed inside vendor\n").unwrap();
    git(&vendor, &["add", "-A"]);
    git(&vendor, &["commit", "--quiet", "-m", "vendor base"]);
    f.snap("turn 2");

    let err = ops::restore(&f.wt(), &f.state, "default", &as_file.to_string(), BUDGET)
        .expect_err("writing over a nested repository must be refused");
    assert!(err.to_string().contains("vendor"), "the refusal should name it: {err}");
    assert!(vendor.join(".git").is_dir(), "the nested repository's history must survive");
    assert_eq!(
        fs::read_to_string(vendor.join("work.txt")).unwrap(),
        "uncommitted work nobody else has\n"
    );
}

#[test]
fn a_tracked_file_an_ignore_rule_matches_is_still_captured() {
    // The scratch index starts empty, so every path in it looked untracked, and
    // git applies ignore rules only to untracked paths. Real git never does
    // this, because its index already knows what is tracked. The result was
    // that a file the repository *tracks* was in no snapshot at all, while
    // `doctor` counted it and reported `status ready`.
    let f = Fixture::new();
    f.write("config/app.env", "PORT=8080\n");
    f.write("normal.txt", "one\n");
    f.commit_all("base");
    // The rule arrives after the file is already tracked — an everyday event
    // when an agent tidies a .gitignore mid-session.
    f.write(".gitignore", "*.env\n");
    f.commit_all("ignore env files");

    let captured = f.snap("turn 1");
    f.write("config/app.env", "PORT=9999\n");
    f.write("normal.txt", "two\n");
    f.snap("turn 2");

    let planned = ops::plan(&f.wt(), &f.state, "default", &captured.to_string(), BUDGET).unwrap();
    assert!(
        planned.plan.write.contains(&"config/app.env".to_string()),
        "a tracked file must be in the plan even when an ignore rule matches it: {:?}",
        planned.plan
    );

    ops::restore(&f.wt(), &f.state, "default", &captured.to_string(), BUDGET).unwrap();
    assert_eq!(f.read("config/app.env"), "PORT=8080\n", "a tracked file must round-trip");
    assert_eq!(f.read("normal.txt"), "one\n");
}

#[test]
fn a_genuinely_untracked_ignored_file_is_still_left_alone() {
    // The companion to the test above: staging tracked paths explicitly must not
    // start capturing things invariant 8 promises never to touch.
    let f = Fixture::new();
    f.write(".gitignore", ".env\nnode_modules/\n");
    f.write("a.txt", "one\n");
    f.commit_all("base");
    let first = f.snap("turn 1");

    f.write(".env", "SECRET=hunter2\n");
    f.write("node_modules/left-pad/index.js", "module.exports = 1\n");
    f.write("a.txt", "two\n");
    f.snap("turn 2");

    let planned = ops::plan(&f.wt(), &f.state, "default", &first.to_string(), BUDGET).unwrap();
    assert!(
        !planned.plan.remove.iter().any(|p| p.contains(".env") || p.contains("node_modules")),
        "an untracked ignored path must never enter a plan: {:?}",
        planned.plan
    );
    ops::restore(&f.wt(), &f.state, "default", &first.to_string(), BUDGET).unwrap();
    assert_eq!(f.read(".env"), "SECRET=hunter2\n");
    assert!(f.exists("node_modules/left-pad/index.js"));
}

#[test]
fn a_path_sheep_cannot_name_is_refused_rather_than_silently_skipped() {
    let mut plan = sheep::shadow::RestorePlan::default();
    plan.remove.push("caf\u{FFFD}.txt".to_string());
    plan.target_tree = "0".repeat(40);
    plan.current_tree = "0".repeat(40);

    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("base");
    f.snap("turn 1");
    let shadow = sheep::shadow::Shadow::ensure(f.wt(), &f.state).unwrap();

    let err = shadow.apply(&plan).expect_err("a path we cannot address must not be acted on");
    assert!(
        err.to_string().contains("not valid UTF-8"),
        "a removal that would silently no-op must be a refusal instead: {err}"
    );
}

#[test]
fn line_endings_survive_a_hostile_global_gitconfig() {
    // The shadow reads repo-local config from its own bare git dir, never the
    // user's — but it inherited the *machine's* global config, where
    // `core.autocrlf = input` is routine advice on macOS and Linux. A CRLF file
    // was recorded as LF and written back as LF; the checkpoint was normalised
    // on the way in too, so the undo did not restore the original bytes either;
    // and `sheep snap` afterwards said "nothing changed since the last turn",
    // so Sheep could not see the damage it had done.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let f = Fixture::new();
    let hostile = f.repo.parent().unwrap().join("hostile.gitconfig");
    fs::write(&hostile, "[core]\n\tautocrlf = input\n").unwrap();

    let previous = std::env::var("GIT_CONFIG_GLOBAL").ok();
    std::env::set_var("GIT_CONFIG_GLOBAL", &hostile);

    let crlf = b"line one\r\nline two\r\n";
    fs::write(f.repo.join("windows.bat"), crlf).unwrap();
    // Commit with the file's bytes as they are, so the repository itself is not
    // the thing that normalised them.
    git(&f.repo, &["-c", "core.autocrlf=false", "add", "-A"]);
    git(&f.repo, &["-c", "core.autocrlf=false", "commit", "--quiet", "-m", "base"]);

    let intact = f.snap("turn 1");
    fs::write(f.repo.join("windows.bat"), b"CHANGED\r\n").unwrap();
    f.snap("turn 2");
    ops::restore(&f.wt(), &f.state, "default", &intact.to_string(), BUDGET).unwrap();

    let after = fs::read(f.repo.join("windows.bat")).unwrap();

    match previous {
        Some(value) => std::env::set_var("GIT_CONFIG_GLOBAL", value),
        None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
    }

    assert_eq!(
        after, crlf,
        "the bytes written back must be the bytes recorded, whatever the machine's gitconfig says"
    );
}
