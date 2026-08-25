//! The recorder, end to end, against a scripted herdr and real git worktrees.
//!
//! The detector is tested on its own in `detect_boundaries.rs`; what is under
//! test here is everything the detector deliberately does not know — the
//! corroboration against the live session, the pane-to-worktree mapping, the
//! timeline naming, and the promise that one bad worktree cannot stop the rest
//! of the session being recorded.
//!
//! No socket is involved. [`Session`] and [`Source`] are the two seams, and
//! both are scripted here, so these run in CI with no herdr anywhere.

use serde_json::json;
use sheep::herdr::detect::{Sighting, Status, Tuning};
use sheep::herdr::log::Log;
use sheep::herdr::recorder::{Config, Ended, LineBy, Pump, Recorder, Source};
use sheep::herdr::session::{Processes, Session};
use sheep::herdr::wire::Event;
use sheep::repo::Worktree;
use sheep::store::{Store, Turn, TurnKind};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

const SETTLE_MS: u64 = 120;
/// Short, so the tests that exercise giving up do not take a minute each.
const PATIENCE_MS: u64 = 500;
const RECONCILE_MS: u64 = 40;
const BUDGET: usize = 60_000;

// ---------------------------------------------------------------- fixtures --

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(args)
        .output()
        .expect("git should run");
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
}

struct Ground {
    _dir: TempDir,
    base: PathBuf,
    state: PathBuf,
}

impl Ground {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        // macOS hides temp dirs behind a symlink; Sheep canonicalises, so the
        // paths handed to the recorder have to be canonical too.
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let state = base.join("state");
        std::fs::create_dir_all(&state).unwrap();
        Self { _dir: dir, base, state }
    }

    /// A real repository with one commit in it.
    fn repo(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "--quiet", "-b", "main"]);
        std::fs::write(path.join("src.rs"), "fn main() {}\n").unwrap();
        git(&path, &["add", "-A"]);
        git(&path, &["commit", "--quiet", "-m", "start"]);
        path
    }

    /// A directory that is deliberately not a repository.
    fn plain(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Everything on the timeline, baseline included.
    fn turns(&self, repo: &Path, line: &str) -> Vec<Turn> {
        let id = Worktree::discover(repo).expect("worktree").id;
        Store::open(&self.state, &id, line).unwrap().all().unwrap()
    }

    /// Only what the recorder claims an agent actually finished.
    fn recorded(&self, repo: &Path, line: &str) -> Vec<Turn> {
        self.turns(repo, line).into_iter().filter(|t| t.kind == TurnKind::Turn).collect()
    }

    /// The baseline a timeline gets before its first turn, if it has one.
    fn baseline(&self, repo: &Path, line: &str) -> Option<Turn> {
        self.turns(repo, line).into_iter().find(|t| t.kind == TurnKind::Checkpoint)
    }
}

// ------------------------------------------------------------ scripted herdr --

#[derive(Default)]
struct Facts {
    /// What herdr answers when the recorder asks directly. This is the
    /// authority the settle step corroborates against.
    panes: HashMap<String, Sighting>,
    /// Successive answers to `pane.process_info`; the last one repeats.
    processes: HashMap<String, VecDeque<Processes>>,
    screens: HashMap<String, String>,
    reported: Vec<(String, u64)>,
    missing: Vec<String>,
    /// Whether `processes` should fail with a server error that is *not* a
    /// not-found. Herdr answers `invalid_request` for a method it does not
    /// have and for bad params, and neither may read as "the pane is gone".
    broken_processes: bool,
    /// Panes that have vanished from `agent.list` without an event saying so —
    /// what a reconnect gap looks like.
    unlisted: Vec<String>,
    /// Panes herdr answers about without attributing an agent. Before the
    /// second review this shape reached the recorder as `Ok(None)`, which it
    /// reads as "the pane is gone", so a single flap cost a turn.
    unattributed: Vec<String>,
    /// Panes herdr answers *nothing* for, while they are otherwise alive —
    /// what a renamed payload key produced. A fake that cannot enter this state
    /// is why the state survived a review.
    silent: Vec<String>,
}

#[derive(Clone, Default)]
struct Herdr(Arc<Mutex<Facts>>);

impl Herdr {
    fn pane(&self, pane_id: &str, agent: &str, cwd: &Path, status: Status) -> &Self {
        self.0.lock().unwrap().panes.insert(
            pane_id.to_string(),
            Sighting {
                pane_id: pane_id.to_string(),
                agent: Some(agent.to_string()),
                cwd: Some(cwd.display().to_string()),
                status,
                revision: 0,
            },
        );
        self
    }

    fn processes(&self, pane_id: &str, sequence: Vec<Processes>) -> &Self {
        self.0.lock().unwrap().processes.insert(pane_id.to_string(), sequence.into());
        self
    }

    fn screen(&self, pane_id: &str, text: &str) -> &Self {
        self.0.lock().unwrap().screens.insert(pane_id.to_string(), text.to_string());
        self
    }

    fn gone(&self, pane_id: &str) -> &Self {
        self.0.lock().unwrap().missing.push(pane_id.to_string());
        self
    }

    fn break_processes(&self, broken: bool) {
        self.0.lock().unwrap().broken_processes = broken;
    }

    fn pane_status(&self, pane_id: &str, status: Status) {
        if let Some(pane) = self.0.lock().unwrap().panes.get_mut(pane_id) {
            pane.status = status;
            pane.revision += 1;
        }
    }

    fn relist(&self, pane_id: &str) {
        self.0.lock().unwrap().unlisted.retain(|p| p != pane_id);
    }

    /// Answer about the pane, but without an agent on it.
    fn forget_agent(&self, pane_id: &str, forgotten: bool) {
        let mut facts = self.0.lock().unwrap();
        match forgotten {
            true => facts.unattributed.push(pane_id.to_string()),
            false => facts.unattributed.retain(|p| p != pane_id),
        }
    }

    /// Answer nothing at all for a pane that is nonetheless there.
    fn answer_nothing_for(&self, pane_id: &str) {
        self.0.lock().unwrap().silent.push(pane_id.to_string());
    }

    /// Drop a pane out of `agent.list` while leaving it answerable, the way a
    /// released agent looks to a recorder that missed the event.
    fn unlist(&self, pane_id: &str) {
        self.0.lock().unwrap().unlisted.push(pane_id.to_string());
    }

    fn reported(&self) -> Vec<(String, u64)> {
        self.0.lock().unwrap().reported.clone()
    }
}

impl Session for Herdr {
    fn agents(&self) -> anyhow::Result<Vec<Sighting>> {
        let facts = self.0.lock().unwrap();
        Ok(facts.panes.values().filter(|s| !facts.unlisted.contains(&s.pane_id)).cloned().collect())
    }

    fn pane(&self, pane_id: &str) -> anyhow::Result<Option<Sighting>> {
        let facts = self.0.lock().unwrap();
        if facts.missing.iter().any(|p| p == pane_id) || facts.silent.iter().any(|p| p == pane_id) {
            return Ok(None);
        }
        let mut pane = facts.panes.get(pane_id).cloned();
        if facts.unattributed.iter().any(|p| p == pane_id) {
            if let Some(pane) = pane.as_mut() {
                pane.agent = None;
            }
        }
        Ok(pane)
    }

    fn processes(&self, pane_id: &str) -> anyhow::Result<Option<Processes>> {
        let mut facts = self.0.lock().unwrap();
        if facts.broken_processes {
            anyhow::bail!("herdr api invalid_request: unknown variant `pane.process_info`");
        }
        if facts.missing.iter().any(|p| p == pane_id) {
            return Ok(None);
        }
        let Some(queue) = facts.processes.get_mut(pane_id) else { return Ok(None) };
        if queue.len() > 1 {
            Ok(queue.pop_front())
        } else {
            Ok(queue.front().cloned())
        }
    }

    fn screen(&self, pane_id: &str, _lines: u32) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().unwrap().screens.get(pane_id).cloned())
    }

    fn report_turn(&self, pane_id: &str, seq: u64, _ttl: Duration) -> anyhow::Result<()> {
        self.0.lock().unwrap().reported.push((pane_id.to_string(), seq));
        Ok(())
    }
}

/// An agent process holding the pane's foreground group, with `extra` other
/// pids under it — the MCP servers and tool children a real agent carries.
fn agent_running(extra: &[u32]) -> Processes {
    let mut running = vec![(4_100, "claude.exe".to_string())];
    running.extend(extra.iter().map(|pid| (*pid, "node".to_string())));
    Processes { shell_pid: 4_000, leader: 4_100, running }
}

/// The same agent after a restart: a new leader pid under the same shell.
fn restarted_agent() -> Processes {
    Processes {
        shell_pid: 4_000,
        leader: 5_100,
        running: vec![(5_100, "claude.exe".to_string()), (5_200, "node".to_string())],
    }
}

/// The pane has dropped back to its shell: whatever herdr last said about the
/// agent describes a program that has exited.
fn shell_only() -> Processes {
    Processes { shell_pid: 4_000, leader: 4_000, running: vec![(4_000, "zsh".to_string())] }
}

// ------------------------------------------------------------ scripted stream --

enum Step {
    Ev(Event),
    /// Let real time pass so a settle window can close.
    Rest(u64),
    /// Change the working tree, or the scripted session, between turns.
    Do(Box<dyn Fn() + Send>),
    /// The subscription itself failed.
    Fail(String),
}

struct Script(VecDeque<Step>);

impl Script {
    fn new(steps: Vec<Step>) -> Self {
        Self(steps.into())
    }
}

impl Source for Script {
    fn poll(&mut self, _timeout: Duration) -> Pump {
        match self.0.pop_front() {
            Some(Step::Ev(event)) => Pump::Event(event),
            Some(Step::Rest(ms)) => {
                std::thread::sleep(Duration::from_millis(ms));
                Pump::Idle
            }
            Some(Step::Do(action)) => {
                action();
                Pump::Idle
            }
            Some(Step::Fail(why)) => Pump::Failed(why),
            None => Pump::Closed,
        }
    }
}

fn status(pane_id: &str, agent: &str, cwd: &Path, status: &str, revision: u64) -> Step {
    Step::Ev(Event {
        kind: "pane_updated".into(),
        data: json!({
            "type": "pane_updated",
            "pane": {
                "pane_id": pane_id,
                "terminal_id": "term_1",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "focused": false,
                "agent": agent,
                "agent_status": status,
                "cwd": cwd.display().to_string(),
                "revision": revision,
            }
        }),
    })
}

/// A `pane_updated` that carries no agent — herdr momentarily not calling this
/// pane an agent pane, while it is plainly still painting.
fn unattributed_paint(pane_id: &str, cwd: &Path, revision: u64) -> Step {
    Step::Ev(Event {
        kind: "pane_updated".into(),
        data: json!({
            "type": "pane_updated",
            "pane": {
                "pane_id": pane_id,
                "terminal_id": "term_1",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "focused": false,
                "agent_status": "idle",
                "cwd": cwd.display().to_string(),
                "revision": revision,
            }
        }),
    })
}

fn edit(path: PathBuf, body: &'static str) -> Step {
    Step::Do(Box::new(move || std::fs::write(&path, body).unwrap()))
}

/// A full turn in one pane: work starts, files change, work stops, the pane
/// goes quiet long enough for the boundary to be believed.
fn one_turn(pane: &str, agent: &str, repo: &Path, rev: u64, body: &'static str) -> Vec<Step> {
    vec![
        status(pane, agent, repo, "working", rev),
        edit(repo.join("src.rs"), body),
        status(pane, agent, repo, "idle", rev + 1),
        Step::Rest(SETTLE_MS * 2),
    ]
}

fn config(state: &Path, dry_run: bool) -> Config {
    Config {
        dry_run,
        tuning: Tuning {
            settle: Duration::from_millis(SETTLE_MS),
            patience: Duration::from_millis(PATIENCE_MS),
        },
        line_by: LineBy::Agent,
        file_budget: BUDGET,
        state: state.to_path_buf(),
        // Long enough that the periodic re-sync never fires mid-test.
        reconcile_every: Duration::from_secs(3_600),
    }
}

fn run(herdr: &Herdr, state: &Path, dry_run: bool, steps: Vec<Step>) -> Recorder<Herdr> {
    let mut recorder = Recorder::new(herdr.clone(), config(state, dry_run), Log::to_stdout());
    let _ = recorder.pump(&mut Script::new(steps));
    recorder
}

fn tweak(herdr: &Herdr, change: impl Fn(&Herdr) + Send + 'static) -> Step {
    let handle = herdr.clone();
    Step::Do(Box::new(move || change(&handle)))
}

// ------------------------------------------------------------------- tests --

#[test]
fn a_finished_turn_is_recorded_against_its_pane_and_agent() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ tidy up the parser\n\n⏺ Reading src.rs\n");

    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n"));

    let baseline =
        ground.baseline(&repo, "claude").expect("a timeline needs something to rewind to");
    assert_eq!(baseline.note.as_deref(), Some("baseline, before the first recorded turn"));

    let turns = ground.recorded(&repo, "claude");
    assert_eq!(turns.len(), 1, "one finished turn, one entry: {turns:?}");
    let turn = &turns[0];
    assert_eq!(turn.kind, TurnKind::Turn);
    assert_eq!(
        turn.parent.as_deref(),
        Some(baseline.commit.as_str()),
        "measured against the baseline"
    );
    assert_eq!(turn.insertions, 1, "and the diffstat is real, not the whole tree");
    assert_eq!(turn.agent.as_deref(), Some("claude"));
    assert_eq!(turn.pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(turn.prompt.as_deref(), Some("tidy up the parser"));
    assert_eq!(
        herdr.reported(),
        vec![("w1:p1".to_string(), turn.seq)],
        "herdr is told the turn number"
    );
}

#[test]
fn herdr_changing_its_mind_stops_the_turn_being_recorded() {
    // The false-`done` case, caught by the corroboration rather than the clock:
    // the event stream said the pane went to rest, but herdr's own answer when
    // asked directly is that the agent is still working.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Working)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ keep going\n");

    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n"));

    assert!(ground.recorded(&repo, "claude").is_empty(), "a done herdr took back is not a turn");
    assert!(herdr.reported().is_empty());
}

#[test]
fn a_pane_still_spawning_processes_is_not_finished() {
    // The other half of the false-`done` defence. Herdr's status agrees the
    // pane is at rest, but its foreground process group churned across the
    // window: the agent is running tools, whatever the screen looked like.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .screen("w1:p1", "❯ run the suite\n")
        // Fingerprint at the candidate, then a tool child that has gone by the
        // time the window closes, then a stable group.
        .processes(
            "w1:p1",
            vec![agent_running(&[4_200, 4_300]), agent_running(&[4_200]), agent_running(&[4_200])],
        );

    let mut steps = one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n");
    // Enough quiet for the re-armed window to close as well.
    steps.push(Step::Rest(SETTLE_MS * 3));

    run(&herdr, &ground.state, false, steps);

    let turns = ground.recorded(&repo, "claude");
    assert_eq!(turns.len(), 1, "the turn lands once the process group settles: {turns:?}");
}

#[test]
fn a_pane_whose_agent_has_exited_records_nothing() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Done)
        .processes("w1:p1", vec![shell_only()])
        .screen("w1:p1", "❯ finish up\n");

    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n"));

    assert!(
        ground.recorded(&repo, "claude").is_empty(),
        "a pane back at its shell has no agent to have finished a turn"
    );
}

#[test]
fn a_pane_that_is_not_a_worktree_is_skipped_without_complaint() {
    let ground = Ground::new();
    let plain = ground.plain("notes");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &plain, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[])])
        .screen("w1:p1", "❯ read the notes\n");

    let recorder =
        run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &plain, 10, "x\n"));

    assert_eq!(recorder.recorded(), 0);
    assert!(!ground.state.join("turns").exists(), "nothing was filed for a non-repository");
    assert!(herdr.reported().is_empty());
}

#[test]
fn four_agents_in_four_worktrees_get_four_timelines() {
    let ground = Ground::new();
    let repos: Vec<PathBuf> = (1..=4).map(|n| ground.repo(&format!("agent{n}"))).collect();
    let herdr = Herdr::default();

    let mut steps = Vec::new();
    for (index, repo) in repos.iter().enumerate() {
        let pane = format!("w1:p{}", index + 1);
        herdr
            .pane(&pane, "claude", repo, Status::Idle)
            .processes(&pane, vec![agent_running(&[4_200])])
            .screen(&pane, "❯ do the thing\n");
        steps.push(status(&pane, "claude", repo, "working", 10));
        steps.push(edit(repo.join("src.rs"), "fn main() { /* changed */ }\n"));
    }
    // Every pane finishes before any window closes: four boundaries in flight.
    for (index, repo) in repos.iter().enumerate() {
        steps.push(status(&format!("w1:p{}", index + 1), "claude", repo, "idle", 11));
    }
    steps.push(Step::Rest(SETTLE_MS * 3));

    let recorder = run(&herdr, &ground.state, false, steps);
    assert_eq!(recorder.recorded(), 4);

    for repo in &repos {
        let turns = ground.recorded(repo, "claude");
        assert_eq!(turns.len(), 1, "{} should hold exactly its own turn", repo.display());
        assert_eq!(turns[0].seq, 2, "after the baseline this timeline holds one turn");
    }

    // Each pane must be told *its own* number. Counting four reports would pass
    // just as well if all four named the same pane.
    let mut reported = herdr.reported();
    reported.sort();
    assert_eq!(
        reported,
        vec![
            ("w1:p1".to_string(), 2),
            ("w1:p2".to_string(), 2),
            ("w1:p3".to_string(), 2),
            ("w1:p4".to_string(), 2),
        ]
    );
}

#[test]
fn one_unrecordable_worktree_does_not_stop_the_others() {
    let ground = Ground::new();
    let good = ground.repo("good");
    let stuck = ground.repo("stuck");
    // Sheep refuses a tree mid-operation. The recorder has to log that and
    // carry on, not fall over.
    std::fs::write(stuck.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();

    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &stuck, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[])])
        .screen("w1:p1", "❯ merge things\n");
    herdr
        .pane("w1:p2", "codex", &good, Status::Idle)
        .processes("w1:p2", vec![agent_running(&[])])
        .screen("w1:p2", "❯ tidy up\n");

    let mut steps = one_turn("w1:p1", "claude", &stuck, 10, "fn main() { /* a */ }\n");
    steps.extend(one_turn("w1:p2", "codex", &good, 10, "fn main() { /* b */ }\n"));

    let recorder = run(&herdr, &ground.state, false, steps);

    assert!(ground.turns(&stuck, "claude").is_empty(), "the stuck worktree records nothing");
    assert_eq!(ground.recorded(&good, "codex").len(), 1, "and the healthy one is unaffected");
    assert_eq!(recorder.recorded(), 1);
}

#[test]
fn a_dry_run_leaves_nothing_behind() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ change one line\n");

    let recorder =
        run(&herdr, &ground.state, true, one_turn("w1:p1", "claude", &repo, 10, "fn main() {}\n"));

    assert_eq!(recorder.recorded(), 0);
    assert!(!ground.state.join("turns").exists(), "no turn log");
    assert!(!ground.state.join("shadow").exists(), "no shadow repository");
    assert!(herdr.reported().is_empty(), "and herdr is not told anything either");
}

#[test]
fn a_turn_that_changed_nothing_does_not_reach_the_timeline() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[])])
        .screen("w1:p1", "❯ what does this file do\n");

    let mut steps = one_turn("w1:p1", "claude", &repo, 10, "fn main() { /* edited */ }\n");
    // A second turn that answers a question without touching a file.
    steps.extend(vec![
        status("w1:p1", "claude", &repo, "working", 20),
        status("w1:p1", "claude", &repo, "idle", 21),
        Step::Rest(SETTLE_MS * 2),
    ]);

    run(&herdr, &ground.state, false, steps);

    let turns = ground.recorded(&repo, "claude");
    assert_eq!(turns.len(), 1, "an answer with no edit is not a checkpoint worth keeping");
}

#[test]
fn a_pane_that_appears_after_start_up_is_picked_up() {
    let ground = Ground::new();
    let repo = ground.repo("late");
    // Deliberately not in `agents()`: the recorder only learns about this pane
    // from the stream, the way a pane split at lunchtime arrives.
    let herdr = Herdr::default();
    herdr.processes("w9:p9", vec![agent_running(&[])]).screen("w9:p9", "❯ start something new\n");
    herdr.pane("w9:p9", "claude", &repo, Status::Idle);
    herdr.0.lock().unwrap().panes.clear();

    let mut steps = vec![Step::Ev(Event {
        kind: "pane_created".into(),
        data: json!({ "type": "pane_created", "pane": {
            "pane_id": "w9:p9", "terminal_id": "t", "workspace_id": "w9", "tab_id": "w9:t1",
            "focused": true, "agent": "claude", "agent_status": "working",
            "cwd": repo.display().to_string(), "revision": 1 }}),
    })];
    steps.push(edit(repo.join("src.rs"), "fn main() { /* late */ }\n"));
    steps.push(status("w9:p9", "claude", &repo, "idle", 2));
    steps.push(Step::Rest(SETTLE_MS * 2));

    // The pane has to be answerable when the recorder corroborates.
    herdr.pane("w9:p9", "claude", &repo, Status::Idle);

    run(&herdr, &ground.state, false, steps);
    assert_eq!(ground.recorded(&repo, "claude").len(), 1);
}

#[test]
fn a_pane_that_closed_mid_window_records_nothing() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[])])
        .screen("w1:p1", "❯ close me\n")
        .gone("w1:p1");

    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n"));

    assert!(
        ground.recorded(&repo, "claude").is_empty(),
        "a pane that is gone cannot finish a turn"
    );
}

#[test]
fn timelines_can_be_named_by_pane_instead() {
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[])])
        .screen("w1:p1", "❯ separate me\n");

    let mut recorder = Recorder::new(
        herdr.clone(),
        Config { line_by: LineBy::Pane, ..config(&ground.state, false) },
        Log::to_stdout(),
    );
    recorder.pump(&mut Script::new(one_turn("w1:p1", "claude", &repo, 10, "fn main() { }\n")));

    assert!(ground.turns(&repo, "claude").is_empty());
    // The raw pane id is the timeline name. `store::slug` is what makes it safe
    // as a file name and a git ref, and both the turn log and the shadow
    // repository go through it — the recorder must not clean it a second time
    // and disagree with them about which timeline this pane owns.
    assert_eq!(ground.recorded(&repo, "w1:p1").len(), 1);
}

// ------------------------------------------- the ways a turn can be invented --

#[test]
fn a_pane_that_moves_mid_window_files_nothing_anywhere() {
    // Herdr re-sends a pane when it changes directory, and the settle window is
    // ten seconds wide by default. Reading the pane's *current* directory when
    // the window closes snapshots whatever repository it has wandered into —
    // filing a turn against a tree nobody touched, while the one the agent
    // actually edited records nothing at all.
    let ground = Ground::new();
    let worked_in = ground.repo("worked-in");
    let wandered_to = ground.repo("wandered-to");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &worked_in, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ edit the parser\n");

    let steps = vec![
        status("w1:p1", "claude", &worked_in, "working", 10),
        edit(worked_in.join("src.rs"), "fn main() { /* real work */ }\n"),
        status("w1:p1", "claude", &worked_in, "idle", 11),
        // The pane moves before the window closes.
        status("w1:p1", "claude", &wandered_to, "idle", 12),
        Step::Rest(SETTLE_MS * 3),
    ];
    let recorder = run(&herdr, &ground.state, false, steps);

    assert!(
        ground.recorded(&wandered_to, "claude").is_empty(),
        "nothing may be filed against a repository the agent never touched"
    );
    assert!(
        ground.recorded(&worked_in, "claude").is_empty(),
        "and a boundary we can no longer vouch for is withdrawn, not guessed at"
    );
    assert_eq!(recorder.recorded(), 0);
    assert!(herdr.reported().is_empty());
}

#[test]
fn a_move_the_stream_missed_is_still_caught_when_herdr_is_asked() {
    // The detector withdraws candidates for moves it saw. A reconnect loses
    // events, so the corroboration asks outright as well.
    let ground = Ground::new();
    let worked_in = ground.repo("worked-in");
    let wandered_to = ground.repo("wandered-to");
    let herdr = Herdr::default();
    // What herdr answers when asked directly: the pane has already moved.
    herdr
        .pane("w1:p1", "claude", &wandered_to, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ edit the parser\n");

    // What the stream says: the whole turn happened in the other repository.
    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &worked_in, 10, "fn x() {}\n"));

    assert!(ground.recorded(&wandered_to, "claude").is_empty());
    assert!(ground.recorded(&worked_in, "claude").is_empty());
}

#[test]
fn a_boundary_with_no_change_leaves_a_baseline_and_no_turn() {
    // `ops::snap` can only tell that nothing changed by comparing against the
    // previous turn, so on an empty timeline the first boundary would always
    // record — `1 file(s) +0 -0` — whether or not anything happened. That is
    // the mechanism by which a phantom boundary becomes a turn on disk.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ what does this file do\n");

    // A whole turn that touches nothing.
    let steps = vec![
        status("w1:p1", "claude", &repo, "working", 10),
        status("w1:p1", "claude", &repo, "idle", 11),
        Step::Rest(SETTLE_MS * 2),
    ];
    let recorder = run(&herdr, &ground.state, false, steps);

    assert!(
        ground.recorded(&repo, "claude").is_empty(),
        "a turn that changed nothing is not a turn, even as the first entry"
    );
    let baseline =
        ground.baseline(&repo, "claude").expect("but there is still somewhere to rewind to");
    assert_eq!(baseline.kind, TurnKind::Checkpoint);
    assert_eq!(baseline.seq, 1);
    assert_eq!(recorder.recorded(), 0);
    assert!(herdr.reported().is_empty(), "and herdr is not told about a turn that did not happen");
}

#[test]
fn a_server_error_makes_the_recorder_wait_and_then_give_up() {
    // `invalid_request` — a herdr without the method, or a params mistake — is
    // not "there is no pane", so the boundary is held rather than dropped. But
    // holding it cannot be for ever: each retry costs two blocking requests, so
    // a candidate that can never be corroborated has to be given up on inside
    // `patience` instead of wedging the loop while every other pane goes
    // unrecorded.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ first\n");

    let mut steps = vec![tweak(&herdr, |h| h.break_processes(true))];
    steps.extend(one_turn("w1:p1", "claude", &repo, 10, "fn main() { /* one */ }\n"));
    steps.push(Step::Rest(PATIENCE_MS * 2));
    // Herdr recovers, and the pane is still sitting at rest. The turn that was
    // given up on must stay given up on: it was never corroborated.
    steps.push(tweak(&herdr, |h| h.break_processes(false)));
    steps.push(status("w1:p1", "claude", &repo, "idle", 12));
    steps.push(Step::Rest(SETTLE_MS * 3));

    let recorder = run(&herdr, &ground.state, false, steps);

    assert!(
        ground.recorded(&repo, "claude").is_empty(),
        "an uncorroborated boundary does not become a turn once the server feels better"
    );
    assert_eq!(recorder.recorded(), 0);
}

#[test]
fn a_server_error_does_not_stop_the_recorder_recording() {
    // The other half: refusing one turn must not be the end of recording. A
    // herdr whose `pane.process_info` fails once has to leave the next turn
    // recordable.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ first\n");

    let mut steps = vec![tweak(&herdr, |h| h.break_processes(true))];
    steps.extend(one_turn("w1:p1", "claude", &repo, 10, "fn main() { /* one */ }\n"));
    steps.push(Step::Rest(PATIENCE_MS * 2));
    steps.push(tweak(&herdr, |h| h.break_processes(false)));
    steps.extend(one_turn("w1:p1", "claude", &repo, 20, "fn main() { /* two */ }\n"));
    steps.push(Step::Rest(SETTLE_MS * 2));

    let recorder = run(&herdr, &ground.state, false, steps);

    let turns = ground.recorded(&repo, "claude");
    assert_eq!(turns.len(), 1, "the healthy turn still lands: {turns:?}");
    assert_eq!(recorder.recorded(), 1);
}

#[test]
fn each_turn_is_corroborated_against_its_own_process_group() {
    // A fingerprint is only meaningful for the candidate that took it. Leaving
    // turn N's process group in the map means turn N+1 is measured against it,
    // and a changed leader between two turns then throws away a real turn.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ first\n");

    let mut steps = one_turn("w1:p1", "claude", &repo, 10, "fn main() { /* one */ }\n");
    // The agent restarts: a new leader pid, and the fingerprint read for the
    // second candidate fails, so there is nothing legitimate to compare to.
    steps.push(tweak(&herdr, |h| {
        h.processes("w1:p1", vec![restarted_agent()]);
        h.break_processes(true);
    }));
    steps.push(status("w1:p1", "claude", &repo, "working", 20));
    steps.push(edit(repo.join("src.rs"), "fn main() { /* two */ }\n"));
    steps.push(status("w1:p1", "claude", &repo, "idle", 21));
    steps.push(tweak(&herdr, |h| h.break_processes(false)));
    steps.push(Step::Rest(SETTLE_MS * 3));

    run(&herdr, &ground.state, false, steps);

    let turns = ground.recorded(&repo, "claude");
    assert_eq!(
        turns.len(),
        2,
        "the second turn must not be judged against the first one's processes: {turns:?}"
    );
}

// ------------------------------------------- the ways it can stop recording --

#[test]
fn reconcile_forgets_a_pane_that_dropped_off_the_agent_list() {
    // `pane.closed`, `pane.exited` and agent-released are exactly the events a
    // reconnect loses. Without pruning, a pane's `worked` flag stays true for
    // the rest of the day and the next time it looks idle it files a turn for
    // work nobody can vouch for.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Working)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ do the thing\n");

    let steps = vec![
        // The agent goes away without an event saying so.
        tweak(&herdr, |h| {
            h.unlist("w1:p1");
        }),
        Step::Rest(RECONCILE_MS * 3),
        // ...and comes back looking idle, having done nothing we witnessed.
        tweak(&herdr, |h| {
            h.relist("w1:p1");
            h.pane_status("w1:p1", Status::Idle);
        }),
        edit(repo.join("src.rs"), "fn main() { /* whoever did this */ }\n"),
        status("w1:p1", "claude", &repo, "idle", 30),
        Step::Rest(SETTLE_MS * 3),
    ];

    let mut recorder = Recorder::new(
        herdr.clone(),
        Config {
            reconcile_every: Duration::from_millis(RECONCILE_MS),
            ..config(&ground.state, false)
        },
        Log::to_stdout(),
    );
    let _ = recorder.pump(&mut Script::new(steps));

    assert!(
        ground.recorded(&repo, "claude").is_empty(),
        "a pane herdr stopped listing is not evidence of a finished turn"
    );
}

#[test]
fn reconcile_picks_up_a_turn_the_stream_never_mentioned() {
    // The other direction: everything about this turn arrives through
    // `agent.list`, the way it does after a reconnect swallowed the events.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ quietly\n");

    let steps = vec![
        tweak(&herdr, |h| h.pane_status("w1:p1", Status::Working)),
        Step::Rest(RECONCILE_MS * 3),
        edit(repo.join("src.rs"), "fn main() { /* done offstage */ }\n"),
        tweak(&herdr, |h| h.pane_status("w1:p1", Status::Idle)),
        Step::Rest(RECONCILE_MS * 3),
        Step::Rest(SETTLE_MS * 3),
    ];

    let mut recorder = Recorder::new(
        herdr.clone(),
        Config {
            reconcile_every: Duration::from_millis(RECONCILE_MS),
            ..config(&ground.state, false)
        },
        Log::to_stdout(),
    );
    let _ = recorder.pump(&mut Script::new(steps));

    assert_eq!(
        ground.recorded(&repo, "claude").len(),
        1,
        "a re-sync has to be able to see a whole turn on its own"
    );
}

#[test]
fn the_pump_says_how_the_stream_ended() {
    let ground = Ground::new();
    let herdr = Herdr::default();

    let mut recorder = Recorder::new(herdr.clone(), config(&ground.state, false), Log::to_stdout());
    assert!(
        matches!(recorder.pump(&mut Script::new(vec![])), Ended::Disconnected),
        "an exhausted stream is a disconnection, which the supervisor retries"
    );

    let ended = recorder.pump(&mut Script::new(vec![Step::Fail("herdr ended it".into())]));
    match ended {
        Ended::Failed(why) => assert!(why.contains("herdr ended it"), "the reason survives: {why}"),
        other => panic!("expected a reported failure, got {other:?}"),
    }
}

#[test]
fn a_cd_while_the_agent_is_still_working_files_nothing_anywhere() {
    // The auditor's reproduction, end to end. The agent works in one worktree;
    // the pane reports the other while still `working`; it goes idle. Every
    // check that reads the directory after that point compares the new one
    // against itself and agrees, so the turn used to land in a repository the
    // agent never edited — as a whole-tree first entry, with herdr told `#1`
    // for a turn that did not happen.
    let ground = Ground::new();
    let worked_in = ground.repo("worked-in");
    let wandered_to = ground.repo("wandered-to");
    let herdr = Herdr::default();
    // Herdr's own answer agrees with the move, which is what made asking it
    // outright no defence at all.
    herdr
        .pane("w1:p1", "claude", &wandered_to, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ edit the parser\n");

    let steps = vec![
        status("w1:p1", "claude", &worked_in, "working", 10),
        edit(worked_in.join("src.rs"), "fn main() { /* the real work */ }\n"),
        // The pane moves while the agent is still working.
        status("w1:p1", "claude", &wandered_to, "working", 11),
        status("w1:p1", "claude", &wandered_to, "idle", 12),
        Step::Rest(SETTLE_MS * 3),
    ];
    let recorder = run(&herdr, &ground.state, false, steps);

    assert!(
        ground.turns(&wandered_to, "claude").is_empty(),
        "nothing at all may be filed against a repository the agent never touched"
    );
    assert!(
        ground.recorded(&worked_in, "claude").is_empty(),
        "and a turn we can no longer vouch for is abandoned, not guessed at"
    );
    assert!(
        ground.baseline(&worked_in, "claude").is_some(),
        "the baseline still went where the turn began — that is the discrepancy that must never pass"
    );
    assert_eq!(recorder.recorded(), 0);
    assert!(herdr.reported().is_empty(), "and herdr is not told a turn number");
}

#[test]
fn a_pane_herdr_stops_attributing_waits_instead_of_losing_the_turn() {
    // "This pane exists but I am not calling it an agent right now" used to be
    // indistinguishable from "this pane is gone", and cost the turn outright.
    // A release is real and will keep saying so until patience runs out; a flap
    // for one reply must not throw away work.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ keep my turn\n");

    let mut steps = vec![tweak(&herdr, |h| h.forget_agent("w1:p1", true))];
    steps.extend(one_turn("w1:p1", "claude", &repo, 10, "fn main() { /* kept */ }\n"));
    // Herdr remembers what it is looking at again, well inside patience.
    steps.push(tweak(&herdr, |h| h.forget_agent("w1:p1", false)));
    steps.push(Step::Rest(SETTLE_MS * 3));

    let recorder = run(&herdr, &ground.state, false, steps);

    assert_eq!(
        ground.recorded(&repo, "claude").len(),
        1,
        "the turn survives a flap in agent attribution"
    );
    assert_eq!(recorder.recorded(), 1);
}

#[test]
fn a_pane_herdr_has_no_answer_for_is_gone_and_only_that() {
    // The fake can now say "no answer for a pane that is otherwise alive",
    // which is what a renamed payload key produced. `Ok(None)` has exactly one
    // meaning left — herdr said the pane is not there — so this is a drop, and
    // anything that is *not* that has to arrive as an error instead. The shapes
    // themselves are pinned in `recorder_session.rs`.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ vanish\n");
    herdr.answer_nothing_for("w1:p1");

    run(&herdr, &ground.state, false, one_turn("w1:p1", "claude", &repo, 10, "fn x() {}\n"));

    assert!(ground.recorded(&repo, "claude").is_empty());
}

#[test]
fn a_pane_still_painting_is_watched_even_while_herdr_forgets_its_agent() {
    // Panes with no agent are ignored, which is right — but only for panes we
    // are not already following. A pane mid-turn whose attribution flaps must
    // keep being watched, or its quiet window closes while it is still
    // painting and the boundary is believed too early.
    let ground = Ground::new();
    let repo = ground.repo("work");
    let herdr = Herdr::default();
    herdr
        .pane("w1:p1", "claude", &repo, Status::Idle)
        .processes("w1:p1", vec![agent_running(&[4_200])])
        .screen("w1:p1", "❯ keep watching\n");

    let seen_early = Arc::new(AtomicUsize::new(usize::MAX));
    let probe = {
        let seen = Arc::clone(&seen_early);
        let state = ground.state.clone();
        let repo = repo.clone();
        Step::Do(Box::new(move || {
            let id = Worktree::discover(&repo).expect("worktree").id;
            let turns = Store::open(&state, &id, "claude").unwrap().all().unwrap();
            seen.store(turns.iter().filter(|t| t.kind == TurnKind::Turn).count(), Ordering::SeqCst);
        }))
    };

    let mut steps = vec![
        status("w1:p1", "claude", &repo, "working", 10),
        edit(repo.join("src.rs"), "fn main() { /* still going */ }\n"),
        status("w1:p1", "claude", &repo, "idle", 11),
    ];
    // Painting for longer than a whole window, with no agent on any of it.
    for step in 0..6 {
        steps.push(Step::Rest(SETTLE_MS / 2));
        steps.push(unattributed_paint("w1:p1", &repo, 12 + step));
    }
    steps.push(probe);
    steps.push(Step::Rest(SETTLE_MS * 3));

    run(&herdr, &ground.state, false, steps);

    assert_eq!(
        seen_early.load(Ordering::SeqCst),
        0,
        "the window must have been restarted by paints herdr did not attribute"
    );
    assert_eq!(
        ground.recorded(&repo, "claude").len(),
        1,
        "and the turn still lands once the pane finally goes quiet"
    );
}
