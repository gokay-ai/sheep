//! Two writers in one state directory.
//!
//! Sheep does not merely tolerate a second writer, it guarantees one: `sheep
//! watch` stays alive for the whole session, so every `sheep gc` and every
//! `sheep restore` a user runs happens beside a recorder that may append at any
//! moment. The dangerous shape is `ops::collect`, which reads the turn log,
//! spends seconds rebuilding the kept turns, renames the rebuilt file over the
//! live one, and then prunes every object no ref reaches. A turn appended
//! anywhere inside that window is in neither the rebuilt log nor any ref, and
//! reported itself as recorded on the way past.
//!
//! Both tests here run a real collection in a thread and a real writer beside
//! it, and both fail without the lock in `sheep::lock`.

use sheep::ops::{self, SnapMeta};
use sheep::repo::Worktree;
use sheep::shadow::Shadow;
use sheep::store::{Store, Turn, TurnKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BUDGET: usize = 60_000;
const LINE: &str = "default";

/// Enough turns that rebuilding the kept ones is measured in tenths of a
/// second — which is what makes the window wide enough for a writer to land in
/// the middle of it, and is also the size of a real timeline after a day.
const TURNS: usize = 80;
/// Kept, and so rebuilt one `commit-tree` at a time. The cost of a collection
/// is proportional to this, not to what it drops.
const KEEP: usize = 60;
/// How far into the collection the second writer starts.
const OFFSET: Duration = Duration::from_millis(120);

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    state: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let base = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let (repo, state) = (base.join("repo"), base.join("state"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        Self { _dir: dir, repo, state }
    }

    fn wt(&self) -> Worktree {
        Worktree::discover(&self.repo).expect("worktree discovery")
    }

    fn write(&self, rel: &str, body: &str) {
        std::fs::write(self.repo.join(rel), body).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.repo.join(rel)).unwrap()
    }

    fn snap(&self, note: &str) -> Turn {
        ops::snap(
            &self.wt(),
            &self.state,
            LINE,
            BUDGET,
            TurnKind::Turn,
            SnapMeta { note: Some(note.into()), ..Default::default() },
            false,
        )
        .unwrap_or_else(|e| panic!("snap `{note}` should succeed: {e:#}"))
        .unwrap_or_else(|| panic!("snap `{note}` should record a turn"))
    }

    fn turns(&self) -> Vec<Turn> {
        Store::open(&self.state, &self.wt().id, LINE).unwrap().all().unwrap()
    }

    /// A history long enough to make a collection take real time.
    fn history(&self) {
        for i in 0..TURNS {
            self.write("a.txt", &format!("revision {i}\n"));
            self.write(&format!("only-in-{i}.txt"), "marker\n");
            self.snap(&format!("turn {i}"));
        }
    }

    /// Start a collection on another thread and hand back a handle that reports
    /// how long it took.
    fn collection(&self) -> std::thread::JoinHandle<Duration> {
        let (wt, state) = (self.wt(), self.state.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let report = ops::collect(
                &wt,
                &state,
                LINE,
                ops::Retention { keep: KEEP, max_age_days: None },
                true,
            )
            .expect("the collection itself should succeed");
            assert!(report.dropped > 0, "the collection must actually drop something");
            started.elapsed()
        })
    }

    /// Everything a surviving turn has to still be: in the log, with the tree
    /// it was recorded with, and with every object of that tree readable.
    fn assert_intact(&self, seq: u64, tree: &str, what: &str) {
        let turn = self
            .turns()
            .into_iter()
            .find(|t| t.seq == seq)
            .unwrap_or_else(|| panic!("{what}: #{seq} is not in the log any more"));
        assert_eq!(turn.tree, tree, "{what}: #{seq} must still hold the tree it was recorded with");

        let shadow = Shadow::ensure(self.wt(), &self.state).unwrap();
        let missing = shadow.verify(&turn.tree).expect("verify should run");
        assert!(missing.is_empty(), "{what}: #{seq} lost {} object(s): {missing:?}", missing.len());
        // A commit id survives a collection only if the log was rewritten with
        // the new one, so this is also the check that the two travelled
        // together.
        assert_eq!(
            shadow.tree_of(&turn.commit).expect("the recorded commit must still resolve"),
            turn.tree,
            "{what}: #{seq}'s commit must point at its tree"
        );
    }
}

#[test]
fn a_turn_recorded_during_a_collection_survives_it() {
    // The plain shape of the loss: `sheep snap`, or the recorder filing an
    // agent turn, while `sheep gc --yes` is between reading the log and
    // renaming its replacement over it. The turn printed a commit id and was
    // gone by the time gc finished.
    let f = Fixture::new();
    f.history();

    let collecting = f.collection();
    std::thread::sleep(OFFSET);

    f.write("a.txt", "written beside a collection\n");
    f.write("late.txt", "and this too\n");
    let late = f.snap("recorded while gc was running");

    let took = collecting.join().expect("the collecting thread should not panic");
    assert!(
        took > OFFSET,
        "the collection finished in {took:?}, before the writer even started — nothing was tested"
    );

    f.assert_intact(late.seq, &late.tree, "a turn recorded during a collection");

    // And it restores, which is the only thing the user ever asked of it.
    f.write("a.txt", "something else entirely\n");
    std::fs::remove_file(f.repo.join("late.txt")).unwrap();
    ops::restore(&f.wt(), &f.state, LINE, &late.seq.to_string(), BUDGET).expect("restore");
    assert_eq!(f.read("a.txt"), "written beside a collection\n");
    assert_eq!(f.read("late.txt"), "and this too\n");
}

#[test]
fn a_restore_beside_a_collection_keeps_the_checkpoint_it_promised() {
    // The worst version, because Sheep says it out loud: a restore prints
    // "previous state kept as turn #501 — `sheep restore #501` puts it back",
    // and a collection running at the same time drops #501 from the log and
    // prunes the objects behind it. The user is told their work is one command
    // away when it is nowhere at all.
    let f = Fixture::new();
    f.history();

    let target = f.turns().last().expect("a history").seq - 3;
    // Uncommitted work, of the kind the checkpoint exists to hold.
    f.write("a.txt", "work the restore is about to take back\n");
    f.write("unsaved.txt", "never snapshotted by anyone\n");

    let collecting = f.collection();
    std::thread::sleep(OFFSET);

    let done = ops::restore(&f.wt(), &f.state, LINE, &target.to_string(), BUDGET)
        .expect("the restore should succeed");
    let checkpoint = done.checkpoint.expect("a restore must record a checkpoint before writing");

    let took = collecting.join().expect("the collecting thread should not panic");
    assert!(
        took > OFFSET,
        "the collection finished in {took:?}, before the restore even started — nothing was tested"
    );

    f.assert_intact(
        checkpoint.seq,
        &checkpoint.tree,
        "the checkpoint a restore promised by number",
    );

    // The promise, taken at face value.
    ops::restore(&f.wt(), &f.state, LINE, &checkpoint.seq.to_string(), BUDGET)
        .expect("`sheep restore #<checkpoint>` must put it back");
    assert_eq!(f.read("a.txt"), "work the restore is about to take back\n");
    assert_eq!(f.read("unsaved.txt"), "never snapshotted by anyone\n");
}

#[test]
fn a_writer_that_cannot_have_the_lock_is_told_so_rather_than_left_waiting() {
    // What contention costs, stated. A recorder must not stall a session, so
    // the wait is bounded and the answer is a `lock::Busy` — its own error type
    // precisely so that "somebody else is writing, try again" can be told apart
    // from "this worktree is not safe to record". Nothing is written on the way
    // past.
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.snap("the first turn");
    let before = f.turns().len();

    let held = sheep::lock::hold(&f.state, &f.wt().id, Duration::from_millis(50))
        .expect("the lock should be free");

    f.write("a.txt", "two\n");
    let started = Instant::now();
    let err =
        ops::snap(&f.wt(), &f.state, LINE, BUDGET, TurnKind::Turn, SnapMeta::default(), false)
            .expect_err("a snapshot cannot record while someone else holds the lock");

    assert!(ops::is_busy(&err), "contention must be its own error, not a general failure: {err:#}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the wait has to be bounded: a recorder that blocks for ever stalls the session"
    );
    assert_eq!(f.turns().len(), before, "and nothing may be half-written on the way out");

    drop(held);
    f.snap("and now it records");
    assert_eq!(f.turns().len(), before + 1);
}

#[test]
fn a_collection_and_a_snapshot_do_not_interleave_their_refs() {
    // Whichever order the two end up in, the timeline is one chain afterwards:
    // every turn in the log is reachable from the ref, and the ref's tip is the
    // newest turn. A ref written without a compare-and-swap can lose one side's
    // work outright, and the log then names commits nothing points at.
    let f = Fixture::new();
    f.history();

    let collecting = f.collection();
    std::thread::sleep(OFFSET);
    f.write("a.txt", "beside the collection\n");
    let late = f.snap("beside the collection");
    collecting.join().expect("the collecting thread should not panic");

    let shadow = Shadow::ensure(f.wt(), &f.state).unwrap();
    let turns = f.turns();
    let head = shadow.head(LINE).unwrap().expect("the timeline must still have a head");
    let reachable: Vec<String> =
        shadow.log(LINE, 10_000).unwrap().into_iter().map(|(commit, _, _)| commit).collect();

    assert_eq!(
        turns.last().map(|t| t.seq),
        Some(late.seq),
        "the newest turn in the log must be the one recorded last"
    );
    assert_eq!(
        reachable.first().map(String::as_str),
        turns.last().map(|t| t.commit.as_str()),
        "the ref's tip must be the newest turn in the log"
    );
    assert!(reachable.contains(&head), "the head must be on its own timeline");
    for turn in &turns {
        assert!(
            reachable.contains(&turn.commit),
            "#{} is in the log but nothing reaches its commit {}",
            turn.seq,
            ops::short(&turn.commit)
        );
    }
}

/// A collection that finds the ref somewhere other than where it read it
/// refuses, rather than pointing the ref at a chain that does not contain
/// whatever moved it.
///
/// This is the compare-and-swap on its own. Under the lock it cannot fire; it
/// is here for the case where the lock is not genuinely held, which is also the
/// only case where losing the turn would be silent.
#[test]
fn a_rebuilt_timeline_refuses_a_ref_that_moved_under_it() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    let first = f.snap("turn 1");

    let shadow = Shadow::ensure(f.wt(), &f.state).unwrap();
    let read_at = shadow.head(LINE).unwrap();

    // Somebody records after that read.
    f.write("a.txt", "two\n");
    let second = f.snap("turn 2");

    let chain = vec![(first.tree.clone(), first.subject(), first.at)];
    let err = shadow
        .rechain(LINE, &chain, read_at.as_deref())
        .expect_err("a ref that moved must not be swapped away");
    assert!(
        format!("{err:#}").contains("moved while Sheep was writing to it"),
        "the refusal must say what happened: {err:#}"
    );
    assert_eq!(
        shadow.head(LINE).unwrap().as_deref(),
        Some(second.commit.as_str()),
        "and the ref must be left exactly where the other writer put it"
    );
}
