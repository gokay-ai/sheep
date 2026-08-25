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
use sheep::herdr::recorder::{Config, LineBy, Pump, Recorder, Source};
use sheep::herdr::session::{Processes, Session};
use sheep::herdr::wire::Event;
use sheep::repo::Worktree;
use sheep::store::{Store, Turn, TurnKind};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

const SETTLE_MS: u64 = 120;
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

    fn turns(&self, repo: &Path, line: &str) -> Vec<Turn> {
        let id = Worktree::discover(repo).expect("worktree").id;
        Store::open(&self.state, &id, line).unwrap().all().unwrap()
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

    fn reported(&self) -> Vec<(String, u64)> {
        self.0.lock().unwrap().reported.clone()
    }
}

impl Session for Herdr {
    fn agents(&self) -> anyhow::Result<Vec<Sighting>> {
        Ok(self.0.lock().unwrap().panes.values().cloned().collect())
    }

    fn pane(&self, pane_id: &str) -> anyhow::Result<Option<Sighting>> {
        let facts = self.0.lock().unwrap();
        if facts.missing.iter().any(|p| p == pane_id) {
            return Ok(None);
        }
        Ok(facts.panes.get(pane_id).cloned())
    }

    fn processes(&self, pane_id: &str) -> anyhow::Result<Option<Processes>> {
        let mut facts = self.0.lock().unwrap();
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
    /// Change the working tree between turns.
    Do(Box<dyn Fn() + Send>),
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
            patience: Duration::from_secs(5),
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
    recorder.pump(&mut Script::new(steps));
    recorder
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

    let turns = ground.turns(&repo, "claude");
    assert_eq!(turns.len(), 1, "one finished turn, one entry: {turns:?}");
    let turn = &turns[0];
    assert_eq!(turn.kind, TurnKind::Turn);
    assert_eq!(turn.agent.as_deref(), Some("claude"));
    assert_eq!(turn.pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(turn.prompt.as_deref(), Some("tidy up the parser"));
    assert_eq!(herdr.reported(), vec![("w1:p1".to_string(), 1)], "herdr is told the turn number");
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

    assert!(ground.turns(&repo, "claude").is_empty(), "a done herdr took back is not a turn");
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

    let turns = ground.turns(&repo, "claude");
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
        ground.turns(&repo, "claude").is_empty(),
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
        let turns = ground.turns(repo, "claude");
        assert_eq!(turns.len(), 1, "{} should hold exactly its own turn", repo.display());
        assert_eq!(turns[0].seq, 1, "each timeline numbers from one");
    }

    let mut reported = herdr.reported();
    reported.sort();
    assert_eq!(reported.len(), 4, "each pane is told its own turn number: {reported:?}");
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
    assert_eq!(ground.turns(&good, "codex").len(), 1, "and the healthy one is unaffected");
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

    let turns = ground.turns(&repo, "claude");
    assert_eq!(turns.len(), 1, "an answer with no edit is not a checkpoint worth keeping");
}

#[test]
fn a_pane_that_appears_after_start_up_is_picked_up() {
    let ground = Ground::new();
    let repo = ground.repo("late");
    // Deliberately not in `agents()`: the recorder only learns about this pane
    // from the stream, the way a pane split at lunchtime arrives.
    let herdr = Herdr::default();
    herdr
        .processes("w9:p9", vec![agent_running(&[])])
        .screen("w9:p9", "❯ start something new\n");
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
    assert_eq!(ground.turns(&repo, "claude").len(), 1);
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

    assert!(ground.turns(&repo, "claude").is_empty(), "a pane that is gone cannot finish a turn");
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
    // A pane id has a colon in it, and a colon is legal in neither a file name
    // nor a git ref; the timeline name is cleaned before either sees it.
    assert_eq!(ground.turns(&repo, "w1-p1").len(), 1);
    assert_eq!(ground.turns(&repo, "w1:p1").len(), 1, "and `Store` cleans it the same way");
}
