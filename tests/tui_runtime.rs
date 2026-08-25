//! The event loop's own guarantees.
//!
//! Two of the interface's promises live only in `runtime::run` and nowhere else:
//! that keys typed during a restore cannot act on what the restore leaves
//! behind, and that nothing — a quit, or a terminal that stopped working —
//! exits on top of a write in progress. Neither is reachable from the `App`
//! alone, so both used to rest on code no test touched.
//!
//! The loop's three edges are traits, so here they are a scripted keyboard, a
//! screen that fails on demand, and a worker that answers when it is told to.

use sheep::store::{Turn, TurnKind};
use sheep::tui::app::{App, Key, Level, PlanState};
use sheep::tui::engine::{Action, Job, Notice, Outcome, PlanView, Reply};
use sheep::tui::runtime::{self, Input, Jobs, Pumped, Screen, FINISH_GRACE_SECS};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

// ------------------------------------------------------------------- doubles

/// A terminal's input buffer. `wait` takes one event per frame the way the real
/// loop does; `drain` throws away everything still queued, which is the whole
/// point of the thing under test.
struct Keyboard {
    buffer: VecDeque<Key>,
    /// Ends the session once the buffer runs dry, so a test cannot hang.
    stop_when_empty: bool,
    drained: usize,
    drains: usize,
    waits: usize,
    fail_wait_at: Option<usize>,
    clock: Rc<Cell<u64>>,
}

impl Keyboard {
    fn typing(keys: &str, clock: &Rc<Cell<u64>>) -> Self {
        Self {
            buffer: keys.chars().map(Key::Char).collect(),
            stop_when_empty: true,
            drained: 0,
            drains: 0,
            waits: 0,
            fail_wait_at: None,
            clock: Rc::clone(clock),
        }
    }
}

impl Input for Keyboard {
    fn wait(&mut self, _timeout: Duration) -> anyhow::Result<Option<Key>> {
        self.waits += 1;
        // Time passes while a frame waits, which is what lets the grace period
        // be reached without a test sleeping for a minute.
        self.clock.set(self.clock.get() + 1);
        if self.fail_wait_at == Some(self.waits) {
            anyhow::bail!("the pty went away");
        }
        match self.buffer.pop_front() {
            Some(key) => Ok(Some(key)),
            None if self.stop_when_empty => Ok(Some(Key::Char('q'))),
            None => Ok(None),
        }
    }

    fn drain(&mut self) -> anyhow::Result<usize> {
        self.drains += 1;
        self.drained += self.buffer.len();
        self.buffer.clear();
        Ok(self.drained)
    }

    fn pause(&mut self, _timeout: Duration) {
        self.clock.set(self.clock.get() + 1);
    }
}

struct Display {
    frames: usize,
    fail_at: Option<usize>,
}

impl Display {
    fn working() -> Self {
        Self { frames: 0, fail_at: None }
    }
    fn breaking_at(frame: usize) -> Self {
        Self { frames: 0, fail_at: Some(frame) }
    }
}

impl Screen for Display {
    fn render(&mut self, _app: &App) -> anyhow::Result<()> {
        self.frames += 1;
        if self.fail_at == Some(self.frames) {
            anyhow::bail!("failed to draw: the terminal is gone");
        }
        Ok(())
    }
}

/// What the fake worker answers a restore with. The request id is filled in
/// from the job it is answering, exactly as the real worker does — guessing it
/// in the script would make a test that silently stops meaning anything the
/// moment the app queues one more job on the way in.
enum Answer {
    Stale(PlanView),
    Restored(Outcome),
    Failed { message: String, tree_moved: bool },
}

/// A worker that records what it was asked to do and answers on cue.
#[derive(Default)]
struct Backend {
    inner: RefCell<BackendState>,
}

#[derive(Default)]
struct BackendState {
    sent: Vec<Job>,
    polls: usize,
    restore_req: Option<u64>,
    /// `(deliver once this many polls have happened, answer)`.
    script: VecDeque<(usize, Answer)>,
    /// The poll at which the worker thread ends without answering.
    dies_at: Option<usize>,
}

impl Backend {
    fn answering(script: Vec<(usize, Answer)>) -> Self {
        Self { inner: RefCell::new(BackendState { script: script.into(), ..Default::default() }) }
    }

    /// A worker whose thread ends without answering — a panic inside `execute`,
    /// which drops the sender.
    fn dying_at(poll: usize) -> Self {
        Self { inner: RefCell::new(BackendState { dies_at: Some(poll), ..Default::default() }) }
    }

    fn polls(&self) -> usize {
        self.inner.borrow().polls
    }

    fn restores(&self) -> usize {
        self.inner.borrow().sent.iter().filter(|j| matches!(j, Job::Restore { .. })).count()
    }
    fn patches(&self) -> Vec<String> {
        self.inner
            .borrow()
            .sent
            .iter()
            .filter_map(|j| match j {
                Job::Patch { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Jobs for Backend {
    fn send(&self, job: Job) {
        let mut state = self.inner.borrow_mut();
        if let Job::Restore { req, .. } = &job {
            state.restore_req = Some(*req);
        }
        state.sent.push(job);
    }
    fn poll(&self) -> Pumped {
        let mut state = self.inner.borrow_mut();
        state.polls += 1;
        if state.dies_at.is_some_and(|at| state.polls >= at) {
            return Pumped::Gone;
        }
        if state.script.front().is_none_or(|(at, _)| *at > state.polls) {
            return Pumped::Idle;
        }
        let req = state.restore_req.expect("nothing asked for a restore yet");
        match state.script.pop_front().map(|(_, answer)| answer) {
            Some(Answer::Stale(plan)) => Pumped::Reply(Reply::Stale { req, plan }),
            Some(Answer::Restored(outcome)) => Pumped::Reply(Reply::Restored { req, outcome }),
            Some(Answer::Failed { message, tree_moved }) => {
                Pumped::Reply(Reply::RestoreFailed { req, message, tree_moved })
            }
            None => Pumped::Idle,
        }
    }
}

// ------------------------------------------------------------------- fixtures

fn turn(seq: u64) -> Turn {
    Turn {
        seq,
        kind: TurnKind::Turn,
        commit: format!("{seq}0ffee1234567890abcdef"),
        tree: "t".into(),
        parent: None,
        at: 0,
        files: 2,
        insertions: 4,
        deletions: 1,
        pane_id: Some("w1:p1".into()),
        agent: Some("claude".into()),
        prompt: None,
        note: None,
    }
}

fn plan(files: &[(Action, &str)]) -> PlanView {
    PlanView {
        seq: 1,
        commit: "c0ffee".repeat(6),
        target_tree: "target".into(),
        current_tree: "current".into(),
        written: files.iter().filter(|(a, _)| *a == Action::Write).count(),
        removed: files.iter().filter(|(a, _)| *a == Action::Remove).count(),
        files: files.iter().map(|(a, p)| (*a, (*p).to_string())).collect(),
    }
}

/// An app already showing a plan for turn #1, the way the overlay would.
fn ready() -> App {
    let mut app = App::new("demo", "/tmp/demo", "default");
    app.inside_herdr = true;
    app.apply(Reply::Loaded {
        turns: vec![turn(1)],
        blockers: vec![],
        warnings: vec![],
        others: vec![],
    });
    app.on_key(Key::Enter);
    let jobs = app.take_jobs();
    let Some(Job::Plan { req, .. }) = jobs.first().cloned() else { panic!("no plan job") };
    app.apply(Reply::Planned {
        req,
        plan: plan(&[(Action::Write, "src/a.ts"), (Action::Write, "src/b.ts")]),
    });
    app
}

fn restored() -> Answer {
    Answer::Restored(Outcome {
        seq: 1,
        commit: "c0ffee".repeat(6),
        written: 2,
        removed: 0,
        checkpoint: Some(3),
        notice: Notice::Off,
    })
}

/// The refusal: the agent wrote a third file while the plan sat on screen.
fn stale() -> Answer {
    Answer::Stale(plan(&[
        (Action::Write, "src/a.ts"),
        (Action::Write, "src/b.ts"),
        (Action::Remove, "src/agent-just-wrote-this.ts"),
    ]))
}

fn tick(clock: &Rc<Cell<u64>>) -> impl Fn() -> u64 + '_ {
    move || clock.get()
}

// ---------------------------------------------------------------------- tests

/// The one the app cannot protect on its own. `shift+R` starts a restore; the
/// five presses behind it sit in the terminal's buffer. When the restore comes
/// back refused, a *different* plan is on screen — and those presses must not
/// be able to confirm it.
#[test]
fn keys_buffered_during_a_restore_never_reach_the_plan_that_replaces_it() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("RRRRRR", &clock);
    let mut screen = Display::working();
    let backend = Backend::answering(vec![(3, stale())]);

    runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap();

    assert_eq!(
        backend.restores(),
        1,
        "six presses, one restore: the rest were either ignored while it ran or dropped when it ended"
    );
    assert_eq!(keys.drains, 1, "the buffer is drained exactly once, as the restore ends");
    assert!(keys.drained > 0, "there were still buffered presses to drop");
    match &app.plan {
        PlanState::Ready(plan) => {
            assert_eq!(plan.touched(), 3, "the refusal's plan is what ended up on screen")
        }
        other => panic!("expected the new plan, got {other:?}"),
    }
    assert_eq!(app.status.as_ref().unwrap().level, Level::Bad);
}

/// Same script, with the drain removed from the loop: this is what the test
/// above is actually holding at bay. Driving the app by hand with the keys the
/// terminal would have delivered produces the second write.
#[test]
fn without_the_drain_those_same_keys_would_confirm_the_new_plan() {
    let mut app = ready();
    app.on_key(Key::Char('R'));
    let jobs = app.take_jobs();
    let Some(Job::Restore { req, .. }) =
        jobs.into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };
    let Answer::Stale(plan) = stale() else { unreachable!() };
    app.apply(Reply::Stale { req, plan });
    assert!(!app.restoring);

    // The presses the terminal buffered, delivered after the refusal.
    app.on_key(Key::Char('R'));
    assert_eq!(
        app.take_jobs().iter().filter(|j| matches!(j, Job::Restore { .. })).count(),
        1,
        "nothing in the app stops a delivered key from confirming a plan nobody read — \\
         which is why the loop must not deliver it"
    );
}

#[test]
fn quitting_waits_for_the_write_and_then_leaves() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("Rq", &clock);
    let mut screen = Display::working();
    // Four polls of silence before the worker answers: the loop has to keep
    // going through all of them despite having been told to quit.
    let backend = Backend::answering(vec![(6, restored())]);

    runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap();

    assert!(app.quit);
    assert!(!app.restoring, "the loop only left once the write was done");
    assert!(
        screen.frames > 3,
        "quitting mid-restore should keep drawing, not exit on the next frame (drew {})",
        screen.frames
    );
    let said = app.status.as_ref().expect("the restore reported itself").lines.join(" | ");
    assert!(said.contains("restored to #1"), "{said}");
}

/// A terminal that stops accepting frames is a reason to leave. It is not a
/// reason to abandon a write half-done — and before this loop existed, the `?`
/// on `terminal.draw` did exactly that.
#[test]
fn a_dead_terminal_still_finishes_the_write_before_reporting_itself() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("R", &clock);
    // Frame 1 draws the plan, the R lands, frame 2 draws the restore, then the
    // pty dies with the write in flight.
    let mut screen = Display::breaking_at(3);
    let backend = Backend::answering(vec![(7, restored())]);

    let err = runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock))
        .expect_err("a broken terminal has to be reported");

    assert!(format!("{err:#}").contains("the terminal is gone"), "{err:#}");
    assert!(!app.restoring, "the write must have completed before the loop returned");
    assert_eq!(backend.restores(), 1);
    let said = app.status.as_ref().expect("the restore was applied").lines.join(" | ");
    assert!(said.contains("restored to #1"), "{said}");
}

#[test]
fn a_dead_terminal_with_nothing_in_flight_leaves_at_once() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("jjj", &clock);
    let mut screen = Display::breaking_at(2);
    let backend = Backend::answering(vec![]);

    let err = runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap_err();
    assert!(format!("{err:#}").contains("the terminal is gone"), "{err:#}");
    assert!(screen.frames <= 2, "nothing was in flight, so there was nothing to wait for");
}

/// The one path where the loop may still walk away from a write: a worker that
/// never answers. It says so rather than hanging forever.
#[test]
fn a_worker_that_never_answers_is_abandoned_with_an_explanation() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("Rq", &clock);
    let mut screen = Display::working();
    let backend = Backend::answering(vec![]); // never replies

    let err = runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap_err();

    let text = format!("{err:#}");
    assert!(text.contains("may be half-restored"), "{text}");
    assert!(text.contains("sheep log"), "the user needs to be told what to check: {text}");
    assert!(
        clock.get() - 1_000 > FINISH_GRACE_SECS,
        "it should have waited out the grace period, not given up at once"
    );
}

#[test]
fn an_ordinary_quit_returns_cleanly() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("jkq", &clock);
    let mut screen = Display::working();
    let backend = Backend::answering(vec![]);

    runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap();
    assert!(app.quit);
    assert_eq!(backend.restores(), 0);
    assert_eq!(keys.drains, 0, "nothing was restored, so there was no backlog to drop");
}

/// The mapping the loop depends on, at the one place it is made. Everything
/// else here uses a fake worker, which cannot tell a dropped sender from a busy
/// one — so without this, `Disconnected` could go back to reading as `Idle` and
/// nothing would notice.
#[test]
fn a_worker_whose_thread_ended_polls_as_gone_not_idle() {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
    let worker = Some(sheep::tui::engine::Worker::from_channels(job_tx, reply_rx));

    assert!(matches!(worker.poll(), Pumped::Idle), "alive with nothing queued");
    reply_tx.send(Reply::Turns(Vec::new())).unwrap();
    assert!(matches!(worker.poll(), Pumped::Reply(_)));
    assert!(matches!(worker.poll(), Pumped::Idle), "drained again");

    drop(reply_tx); // the worker thread ended
    assert!(
        matches!(worker.poll(), Pumped::Gone),
        "a worker that will never answer must not read as one that is merely busy"
    );

    // No worker at all is not a dead worker: nothing was ever expected.
    assert!(matches!(None::<sheep::tui::engine::Worker>.poll(), Pumped::Idle));
    drop(job_rx);
}

/// A worker whose thread ends is not a worker that is being slow. Before the
/// loop could tell the difference, a restore it would never answer left the
/// interface drawing "restoring…" for ever, and quitting cost the full grace
/// period and then reported a worktree that may be half-restored — a guess,
/// and often a false one.
#[test]
fn a_worker_that_dies_is_reported_at_once_rather_than_waited_out() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("R", &clock);
    let mut screen = Display::working();
    let backend = Backend::dying_at(3);

    let err = runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock))
        .expect_err("a worker that vanished has to be reported");

    let text = format!("{err:#}");
    assert!(text.contains("worker stopped while a restore was running"), "{text}");
    assert!(text.contains("sheep log"), "the user has to be told what to check: {text}");
    assert!(
        !text.contains("may be half-restored"),
        "that is the grace-period guess, not this: {text}"
    );

    assert!(!app.restoring, "the loop must stop waiting for a reply that is not coming");
    assert!(
        app.uncertain.is_some(),
        "a worker that died mid-write leaves a tree nobody can vouch for"
    );
    assert!(
        clock.get() - 1_000 < FINISH_GRACE_SECS,
        "it should not have waited out the grace period (waited {})",
        clock.get() - 1_000
    );
    assert!(backend.polls() < 20, "nor should it have spun: {} polls", backend.polls());
}

/// The same death with nothing in flight: the session is over, but there is
/// nothing to be uncertain about.
#[test]
fn a_worker_that_dies_while_idle_says_so_without_alarming_anyone() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("jj", &clock);
    let mut screen = Display::working();
    let backend = Backend::dying_at(2);

    let err = runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("worker stopped"), "{text}");
    assert!(!text.contains("Whether anything was written"), "nothing was in flight: {text}");
    assert!(app.uncertain.is_none(), "no write was running, so the tree is fine");
}

/// A restore that fails between `shadow::apply`'s deletions and its writes.
/// `ops` puts the tree back and says so; the interface has to pass that on
/// rather than assert its own version of events.
#[test]
fn a_failure_that_was_recovered_from_leaves_nothing_uncertain() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    let mut keys = Keyboard::typing("R", &clock);
    let mut screen = Display::working();
    let backend = Backend::answering(vec![(
        3,
        Answer::Failed {
            message:
                "the restore failed: cannot write src/a.ts. Your files were put back as they were."
                    .into(),
            tree_moved: false,
        },
    )]);

    runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap();

    assert!(!app.restoring);
    assert!(app.uncertain.is_none(), "the tree was put back, so nothing is uncertain");
    let said = app.status.as_ref().unwrap().lines.join(" | ");
    assert!(said.contains("your files are as they were"), "{said}");
    assert!(said.contains("put back as they were"), "the operation's own words: {said}");
}

/// Finding 1, through the loop: the refusal screen the status line points at
/// has to refresh the evidence beside the plan, not leave the previous file's
/// diff sitting there under a new file list.
#[test]
fn a_refusal_reloads_the_diff_pane_for_the_plan_it_puts_up() {
    let clock = Rc::new(Cell::new(1_000));
    let mut app = ready();
    // open the diff, move to the second file, then press the key
    let mut keys = Keyboard::typing("djR", &clock);
    let mut screen = Display::working();
    let backend = Backend::answering(vec![(5, stale())]);

    runtime::run(&mut app, &mut screen, &mut keys, &backend, &tick(&clock)).unwrap();

    let asked = backend.patches();
    assert_eq!(
        asked,
        vec!["src/a.ts", "src/b.ts", "src/a.ts"],
        "the refusal has to ask for the first file of the plan it just put up"
    );
    assert!(app.show_patch, "the diff pane is still open, so it must not be showing stale content");
}
