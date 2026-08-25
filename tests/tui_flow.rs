//! Driving the interface: what a keystroke is allowed to do, and what happens
//! on disk when the one key that writes is pressed.
//!
//! The end-to-end case runs the real thing — a real git repository, real
//! snapshots, `ops::restore` through the interface's own job queue. The only
//! part left out is the terminal, which draws and decides nothing.

use sheep::ops::{self, SnapMeta};
use sheep::repo::{Worktree, DEFAULT_MAX_FILES};
use sheep::store::{Store, TurnKind};
use sheep::tui::app::{self, App, Key, Level, Mode, PlanState};
use sheep::tui::cli::{self, UiArgs};
use sheep::tui::engine::{self, Action, Ctx, Job, Notice, PlanView, Reply};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ------------------------------------------------------------------ fixture

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
        let dir = TempDir::new().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
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

    fn wt(&self) -> Worktree {
        Worktree::discover(&self.repo).unwrap()
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            wt: self.wt(),
            state: self.state.clone(),
            line: "default".into(),
            max_files: DEFAULT_MAX_FILES,
        }
    }

    fn snap(&self, agent: &str, prompt: &str) -> u64 {
        ops::snap(
            &self.wt(),
            &self.state,
            "default",
            DEFAULT_MAX_FILES,
            TurnKind::Turn,
            SnapMeta {
                agent: Some(agent.into()),
                prompt: Some(prompt.into()),
                ..Default::default()
            },
            false,
        )
        .unwrap()
        .unwrap()
        .seq
    }

    fn turns(&self) -> Vec<sheep::store::Turn> {
        Store::open(&self.state, &self.wt().id, "default").unwrap().all().unwrap()
    }
}

/// Run every job the app has queued, on this thread, until it stops asking.
fn pump(ctx: &Ctx, app: &mut App) {
    for _ in 0..8 {
        let jobs = app.take_jobs();
        if jobs.is_empty() {
            return;
        }
        for job in jobs {
            app.apply(engine::execute(ctx, job));
        }
    }
    panic!("the app kept queueing jobs");
}

fn plan_view() -> PlanView {
    PlanView {
        seq: 1,
        commit: "c0ffee".repeat(6),
        target_tree: "target".into(),
        current_tree: "current".into(),
        written: 1,
        removed: 0,
        files: vec![(Action::Write, "src/a.ts".into())],
    }
}

fn app_with_plan() -> App {
    let mut app = App::new("demo", "/tmp/demo", "default");
    app.apply(Reply::Loaded {
        turns: vec![sheep::store::Turn {
            seq: 1,
            kind: TurnKind::Turn,
            commit: "c0ffee".repeat(6),
            tree: "t".into(),
            parent: None,
            at: app.now - 60,
            files: 1,
            insertions: 2,
            deletions: 0,
            pane_id: Some("w1:p1".into()),
            agent: Some("claude".into()),
            prompt: None,
            note: None,
        }],
        blockers: vec![],
        warnings: vec![],
        others: vec![],
    });
    app.on_key(Key::Enter);
    let jobs = app.take_jobs();
    let Some(Job::Plan { req, .. }) = jobs.first().cloned() else { panic!("no plan job") };
    app.apply(Reply::Planned { req, plan: plan_view() });
    app
}

/// The dock has to be able to tell "nothing has happened yet" from "you are
/// reading a timeline nothing writes". That is not a rendering nicety: the two
/// were indistinguishable while the recorder filed turns under `claude` and the
/// plugin's panes opened `w31:pW`, and the second one is a lie.
#[test]
fn a_load_reports_the_other_timelines_recorded_for_this_worktree() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);

    // Two agents recorded here; the pane about to open is pointed elsewhere.
    for line in ["claude", "codex"] {
        ops::snap(
            &fixture.wt(),
            &fixture.state,
            line,
            DEFAULT_MAX_FILES,
            TurnKind::Turn,
            SnapMeta { agent: Some(line.into()), ..Default::default() },
            true,
        )
        .unwrap()
        .unwrap();
    }

    let mut ctx = fixture.ctx();
    ctx.line = "w31:pW".into();
    let Reply::Loaded { turns, others, .. } = engine::execute(&ctx, Job::Reload) else {
        panic!("a reload should load")
    };
    assert!(turns.is_empty(), "nothing was ever recorded under a pane id");
    assert_eq!(others, vec!["claude".to_string(), "codex".to_string()]);

    // And the timeline you are actually on is not offered as somewhere else to
    // look — `Store::lines_for` answers in slugs, so the comparison has to.
    ctx.line = "claude".into();
    let Reply::Loaded { others, .. } = engine::execute(&ctx, Job::Reload) else {
        panic!("a reload should load")
    };
    assert_eq!(others, vec!["codex".to_string()]);
}

// ------------------------------------------------------- the road to a write

#[test]
fn no_plan_on_screen_means_no_restore() {
    let mut app = App::new("demo", "/tmp/demo", "default");
    app.apply(Reply::Loaded { turns: vec![], blockers: vec![], warnings: vec![], others: vec![] });
    // Nothing recorded: enter must not even open the picker, and must say why
    // rather than leaving the user on an empty overlay.
    app.on_key(Key::Enter);
    assert!(app.take_jobs().is_empty());
    assert_eq!(app.mode, Mode::Dock, "there is no plan to show, so there is no overlay to open");
    assert!(app.status.as_ref().unwrap().lines[0].contains("nothing recorded yet"));

    // A worktree Sheep will not touch is the same: refused, with the blocker's
    // own words rather than a plan that would fail a second later.
    let mut app = app_with_plan();
    app.on_key(Key::Esc);
    app.apply(Reply::Loaded {
        turns: app.turns.clone(),
        blockers: vec!["a rebase is in progress.".into()],
        warnings: vec![],
        others: vec![],
    });
    app.on_key(Key::Enter);
    assert!(app.take_jobs().is_empty(), "a blocked worktree must not even be planned against");
    assert_eq!(app.mode, Mode::Dock);
    assert!(app
        .status
        .as_ref()
        .unwrap()
        .lines
        .iter()
        .any(|l| l.contains("a rebase is in progress")));

    // A plan that is still loading is not a plan anyone has read.
    let mut app = app_with_plan();
    app.on_key(Key::Enter); // toggles the diff pane, not a restore
    app.take_jobs();
    app.plan = PlanState::Loading(1);
    app.on_key(Key::Char('R'));
    assert!(
        !app.take_jobs().iter().any(|j| matches!(j, Job::Restore { .. })),
        "a restore must not be reachable while the plan is still being worked out"
    );
}

#[test]
fn only_the_shift_key_restores() {
    for key in [Key::Enter, Key::Char('y'), Key::Char('r'), Key::Right, Key::Char(' ')] {
        let mut app = app_with_plan();
        app.on_key(key);
        assert!(
            !app.take_jobs().iter().any(|j| matches!(j, Job::Restore { .. })),
            "{key:?} must not start a restore"
        );
    }
    let mut app = app_with_plan();
    app.on_key(Key::Char('R'));
    let restores: Vec<_> =
        app.take_jobs().into_iter().filter(|j| matches!(j, Job::Restore { .. })).collect();
    assert_eq!(restores.len(), 1, "shift+R should start exactly one restore");
    match &restores[0] {
        Job::Restore { seq, expect_tree, pane, notify, .. } => {
            assert_eq!(*seq, 1);
            // The tree the plan was computed against travels with the job so the
            // worker can refuse if the working tree moved in the meantime.
            assert_eq!(expect_tree, "current");
            assert_eq!(pane.as_deref(), Some("w1:p1"));
            assert!(notify);
        }
        _ => unreachable!(),
    }
}

#[test]
fn a_plan_that_went_stale_is_reshown_rather_than_applied() {
    let mut app = app_with_plan();
    app.on_key(Key::Char('R'));
    let Some(Job::Restore { req, .. }) =
        app.take_jobs().into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };

    let mut fresher = plan_view();
    fresher.files.push((Action::Remove, "src/gone.ts".into()));
    fresher.removed = 1;
    fresher.current_tree = "moved".into();
    app.apply(Reply::Stale { req, plan: fresher });

    assert!(!app.restoring);
    let status = app.status.as_ref().expect("a stale plan has to be reported");
    assert_eq!(status.level, Level::Bad);
    assert!(status.lines.iter().any(|l| l.contains("nothing was restored")), "{status:?}");
    match &app.plan {
        PlanState::Ready(plan) => {
            assert_eq!(plan.touched(), 2, "the new plan replaces the old one")
        }
        other => panic!("expected the new plan on screen, got {other:?}"),
    }
}

#[test]
fn the_message_to_the_agent_names_the_turn_the_damage_and_the_way_back() {
    let text = engine::rewind_message(7, &"a1b2c3d4e5f6".repeat(3), 6, 2, Some(13));
    assert!(text.starts_with("[sheep]"), "{text}");
    assert!(text.contains("turn #7"), "{text}");
    assert!(text.contains("a1b2c3d4e5f6"), "{text}");
    assert!(text.contains("8 path(s) changed on disk: 6 rewritten, 2 deleted"), "{text}");
    assert!(text.contains("re-read any file before you edit it"), "{text}");
    assert!(text.contains("`sheep restore #13 --yes`"), "{text}");

    // With no checkpoint there is no way back to promise, so it is not promised.
    let text = engine::rewind_message(7, "abc", 1, 0, None);
    assert!(!text.contains("puts it back"), "{text}");
}

// ------------------------------------------------------------- end to end

#[test]
fn a_restore_driven_from_the_overlay_puts_the_right_files_on_disk() {
    let fixture = Fixture::new();
    fixture.write("src/cart.ts", "export const total = (i) => sum(i);\n");
    fixture.write("src/discount.ts", "export const apply = () => 1;\n");
    fixture.write("README.md", "# demo\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    let good = fixture.snap("claude", "add a discount path");

    // The turn that went wrong: two files rewritten, one deleted, one invented.
    fixture.write("src/cart.ts", "export const total = (i) => 0;\n");
    fixture.write("README.md", "# demo\nrewritten\n");
    fs::remove_file(fixture.repo.join("src/discount.ts")).unwrap();
    fixture.write("src/checkout.ts", "export const checkout = () => {};\n");
    let bad = fixture.snap("claude", "tidy this up");
    assert_eq!((good, bad), (1, 2));

    let ctx = fixture.ctx();
    let mut app = App::new("repo", fixture.repo.display().to_string(), "default");
    app.reload();
    pump(&ctx, &mut app);
    assert_eq!(app.turns.iter().map(|t| t.seq).collect::<Vec<_>>(), vec![2, 1]);

    // Walk down to the good turn and open its plan, as a user would.
    app.on_key(Key::Down);
    app.on_key(Key::Enter);
    pump(&ctx, &mut app);
    let PlanState::Ready(plan) = &app.plan else { panic!("no plan: {:?}", app.plan) };
    assert_eq!(plan.seq, 1);
    let mut written: Vec<&str> =
        plan.files.iter().filter(|(a, _)| *a == Action::Write).map(|(_, p)| p.as_str()).collect();
    written.sort();
    assert_eq!(written, vec!["README.md", "src/cart.ts", "src/discount.ts"]);
    assert_eq!(
        plan.files
            .iter()
            .filter(|(a, _)| *a == Action::Remove)
            .map(|(_, p)| p.as_str())
            .collect::<Vec<_>>(),
        vec!["src/checkout.ts"]
    );

    // Nothing has been touched yet: the preview is a dry run.
    assert!(fixture.repo.join("src/checkout.ts").exists());

    app.on_key(Key::Char('R'));
    pump(&ctx, &mut app);

    // The files are back.
    assert_eq!(
        fs::read_to_string(fixture.repo.join("src/cart.ts")).unwrap(),
        "export const total = (i) => sum(i);\n"
    );
    assert_eq!(fs::read_to_string(fixture.repo.join("README.md")).unwrap(), "# demo\n");
    assert!(fixture.repo.join("src/discount.ts").exists(), "the deleted module should be back");
    assert!(!fixture.repo.join("src/checkout.ts").exists(), "the invented file should be gone");

    // Undo is undoable: the state that was replaced is a turn of its own.
    let turns = fixture.turns();
    let checkpoint = turns
        .iter()
        .find(|t| t.kind == TurnKind::Checkpoint)
        .expect("a checkpoint should have been taken before anything was written");
    assert!(checkpoint.seq > bad);
    assert!(checkpoint.note.as_deref().unwrap_or_default().starts_with("before restore to"));

    // And the interface says so, in words, pointing at the way back.
    let status = app.status.as_ref().expect("a restore should report itself");
    assert_eq!(status.level, Level::Good);
    let said = status.lines.join(" | ");
    assert!(said.contains("restored to #1"), "{said}");
    assert!(said.contains("3 files written, 1 removed"), "{said}");
    assert!(said.contains(&format!("turn #{}", checkpoint.seq)), "{said}");
    // No pane was recorded on this timeline, so there is nobody to tell.
    assert!(said.contains("no agent pane recorded"), "{said}");

    // The timeline reloaded itself and now describes what is on disk.
    assert!(app.turns.iter().any(|t| t.seq == checkpoint.seq));
}

#[test]
fn the_agent_write_back_is_a_no_op_outside_herdr() {
    // Serialised because the environment is shared across the tests in this
    // binary; nothing else here reads these variables.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = (std::env::var("HERDR_ENV").ok(), std::env::var("HERDR_SOCKET_PATH").ok());
    std::env::remove_var("HERDR_ENV");
    std::env::remove_var("HERDR_SOCKET_PATH");

    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    fixture.snap("claude", "first");
    fixture.write("a.txt", "two\n");
    fixture.snap("claude", "second");

    let ctx = fixture.ctx();
    let Reply::Planned { plan, .. } = engine::execute(&ctx, Job::Plan { req: 1, seq: 1 }) else {
        panic!("planning should succeed")
    };
    let reply = engine::execute(
        &ctx,
        Job::Restore {
            req: 2,
            seq: 1,
            expect_tree: plan.current_tree.clone(),
            pane: Some("w1:p1".into()),
            notify: true,
        },
    );

    if let (Some(env), Some(sock)) = (&previous.0, &previous.1) {
        std::env::set_var("HERDR_ENV", env);
        std::env::set_var("HERDR_SOCKET_PATH", sock);
    }

    match reply {
        Reply::Restored { outcome, .. } => {
            assert_eq!(outcome.notice, Notice::Skipped("not running inside herdr".into()));
            assert_eq!(fs::read_to_string(fixture.repo.join("a.txt")).unwrap(), "one\n");
        }
        other => panic!("expected a restore, got {other:?}"),
    }
}

#[test]
fn a_working_tree_that_moved_under_the_plan_is_refused() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    fixture.snap("claude", "first");
    fixture.write("a.txt", "two\n");
    fixture.snap("claude", "second");

    let ctx = fixture.ctx();
    // The agent kept working while the plan sat on screen.
    fixture.write("a.txt", "three\n");

    let reply = engine::execute(
        &ctx,
        Job::Restore {
            req: 1,
            seq: 1,
            expect_tree: "a-tree-that-is-no-longer-what-is-on-disk".into(),
            pane: None,
            notify: false,
        },
    );
    match reply {
        Reply::Stale { plan, .. } => {
            assert_eq!(plan.seq, 1);
            // The plan handed back describes the tree as it is *now*, so the
            // user is reading the truth rather than the plan they had.
            assert_ne!(plan.current_tree, "a-tree-that-is-no-longer-what-is-on-disk");
            assert!(!plan.commit.is_empty(), "the refused plan should still name its target");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(fixture.repo.join("a.txt")).unwrap(),
        "three\n",
        "a stale plan must not write anything"
    );
    assert!(
        fixture.turns().iter().all(|t| t.kind != TurnKind::Checkpoint),
        "a refused restore should not even checkpoint"
    );
}

/// The realistic shape of the race: a plan is read, the agent writes, then the
/// key is pressed. Nothing may be applied, and the new plan takes the screen.
#[test]
fn a_tree_that_moves_between_reading_the_plan_and_pressing_the_key_is_refused() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    fixture.write("b.txt", "b\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    fixture.snap("claude", "first");
    fixture.write("a.txt", "two\n");
    fixture.snap("claude", "second");

    let ctx = fixture.ctx();
    let mut app = App::new("repo", fixture.repo.display().to_string(), "default");
    app.reload();
    pump(&ctx, &mut app);
    app.on_key(Key::Down); // turn #1
    app.on_key(Key::Enter);
    pump(&ctx, &mut app);
    let PlanState::Ready(seen) = &app.plan else { panic!("no plan") };
    assert_eq!(seen.touched(), 1, "the plan on screen touches one file");

    // The agent writes a second file while the plan is being read.
    fixture.write("b.txt", "b changed by the agent\n");

    app.on_key(Key::Char('R'));
    pump(&ctx, &mut app);

    assert!(!app.restoring);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("a.txt")).unwrap(),
        "two\n",
        "nothing may be written when the tree moved"
    );
    assert_eq!(fs::read_to_string(fixture.repo.join("b.txt")).unwrap(), "b changed by the agent\n");
    assert!(
        fixture.turns().iter().all(|t| t.kind != TurnKind::Checkpoint),
        "a refused restore should not even checkpoint"
    );
    let status = app.status.as_ref().expect("the refusal has to be reported");
    assert_eq!(status.level, Level::Bad);
    assert!(status.lines.iter().any(|l| l.contains("nothing was restored")), "{status:?}");
    match &app.plan {
        PlanState::Ready(plan) => assert_eq!(
            plan.touched(),
            2,
            "the plan on screen is now the one that reflects what the agent did"
        ),
        other => panic!("expected the new plan, got {other:?}"),
    }
}

#[test]
fn the_patch_preview_reads_the_hunks_out_of_the_shadow_repo() {
    let fixture = Fixture::new();
    fixture.write("src/cart.ts", "export const total = () => 1;\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    fixture.snap("claude", "first");
    fixture.write("src/cart.ts", "export const total = () => 2;\n");
    fixture.snap("claude", "second");

    let ctx = fixture.ctx();
    let Reply::Planned { plan, .. } = engine::execute(&ctx, Job::Plan { req: 1, seq: 1 }) else {
        panic!("planning should succeed")
    };
    let reply = engine::execute(
        &ctx,
        Job::Patch {
            req: 2,
            path: "src/cart.ts".into(),
            current_tree: plan.current_tree.clone(),
            target_tree: plan.target_tree.clone(),
        },
    );
    match reply {
        Reply::Patched { body, .. } => {
            assert!(body.contains("@@"), "{body}");
            assert!(body.contains("-export const total = () => 2;"), "{body}");
            assert!(body.contains("+export const total = () => 1;"), "{body}");
            // The noise above the first hunk is stripped: a preview panel is
            // twelve rows and four of them cannot go on `index`/`---`/`+++`.
            assert!(!body.contains("diff --git"), "{body}");
        }
        other => panic!("expected a patch, got {other:?}"),
    }
}

/// The app's half of the protection: while a restore is in flight, nothing a
/// key does may queue work. The other half — that keys buffered by the terminal
/// during the write never get delivered afterwards — is not reachable from here
/// at all, because an `App` has no input buffer. It lives in the event loop and
/// is tested in `tui_runtime.rs`.
#[test]
fn no_key_queues_work_while_a_restore_is_in_flight() {
    let mut app = app_with_plan();
    app.on_key(Key::Char('R'));
    let first: Vec<_> =
        app.take_jobs().into_iter().filter(|j| matches!(j, Job::Restore { .. })).collect();
    assert_eq!(first.len(), 1);
    assert!(app.restoring);

    // Someone leaning on the key for the second the restore takes, plus every
    // other key that normally does something.
    for key in [
        Key::Char('R'),
        Key::Char('R'),
        Key::Enter,
        Key::Char('j'),
        Key::Char('d'),
        Key::Esc,
        Key::Char('n'),
        Key::Char('r'),
    ] {
        app.on_key(key);
    }
    assert!(app.take_jobs().is_empty(), "no keystroke may queue work while a restore is in flight");
    assert!(app.status.as_ref().unwrap().lines[0].contains("keys are ignored"));
    // The keys that would otherwise have moved the screen out from under the
    // write did not.
    assert_eq!(app.mode, Mode::Rewind);
    assert!(!app.show_patch);
    assert!(app.notify, "`n` must not have flipped the write-back mid-restore");

    // And the legitimate path is intact: once the worker answers, a deliberate
    // press on the plan that is now on screen does start a restore.
    let Job::Restore { req, .. } = first[0] else { unreachable!() };
    let mut fresher = plan_view();
    fresher.files.push((Action::Remove, "src/gone.ts".into()));
    fresher.removed = 1;
    app.apply(Reply::Stale { req, plan: fresher });
    assert!(!app.restoring);
    app.on_key(Key::Char('R'));
    assert_eq!(app.take_jobs().iter().filter(|j| matches!(j, Job::Restore { .. })).count(), 1);
}

#[test]
fn quitting_during_a_restore_leaves_the_write_in_flight_for_the_runtime_to_finish() {
    let mut app = app_with_plan();
    app.on_key(Key::Char('R'));
    app.take_jobs();
    assert!(app.restoring);

    app.on_key(Key::Char('q'));
    assert!(app.quit, "quitting must still be possible");
    assert!(app.restoring, "quitting must not clear the flag the runtime waits on before exiting");

    // Only when the worker answers does the interface become quittable.
    let mut done = plan_view();
    done.files.clear();
    app.apply(Reply::Restored {
        req: 2,
        outcome: sheep::tui::engine::Outcome {
            seq: 1,
            commit: "c0ffee".repeat(6),
            written: 1,
            removed: 0,
            checkpoint: Some(2),
            notice: Notice::Off,
        },
    });
    assert!(!app.restoring);
}

/// The mapping from a failed restore to what the interface says. The case that
/// matters — `ops` could not put the tree back — cannot be arranged against
/// real git without changing a directory's permissions between two of its
/// calls, so it is checked here against the error `ops` would hand over.
#[test]
fn a_restore_that_could_not_be_undone_is_reported_as_a_moved_tree() {
    let unrecovered: anyhow::Error = ops::RestoreFailed {
        recovered: false,
        checkpoint_seq: Some(4),
        cause: "cannot write src/a.ts: Permission denied (os error 13)".into(),
        recovery_error: Some("cannot create src/a.ts".into()),
    }
    .into();
    match engine::restore_failure(7, &unrecovered) {
        Reply::RestoreFailed { req, message, tree_moved } => {
            assert_eq!(req, 7);
            assert!(tree_moved, "ops could not put the tree back, so it is between two states");
            // the operation's own words, not a second phrasing that can drift
            assert_eq!(message, unrecovered.to_string());
            assert!(message.contains("`sheep restore #4 --yes`"), "{message}");
        }
        other => panic!("expected a failure reply, got {other:?}"),
    }

    let recovered: anyhow::Error = ops::RestoreFailed {
        recovered: true,
        checkpoint_seq: Some(4),
        cause: "cannot write src/a.ts".into(),
        recovery_error: None,
    }
    .into();
    match engine::restore_failure(7, &recovered) {
        Reply::RestoreFailed { tree_moved, message, .. } => {
            assert!(!tree_moved, "the files were put back, so nothing is uncertain");
            assert!(message.contains("put back as they were"), "{message}");
        }
        other => panic!("expected a failure reply, got {other:?}"),
    }

    // Anything that is not a `RestoreFailed` happened before `apply`, which is
    // the only thing that touches the working tree.
    let early = anyhow::anyhow!("refusing to continue: a rebase is in progress.");
    match engine::restore_failure(7, &early) {
        Reply::RestoreFailed { tree_moved, message, .. } => {
            assert!(!tree_moved);
            assert!(message.contains("a rebase is in progress"), "{message}");
        }
        other => panic!("expected a failure reply, got {other:?}"),
    }
}

/// And the real thing, end to end: a restore whose write cannot land, through
/// `engine::execute` against a real repository.
#[test]
fn a_restore_whose_write_fails_reports_what_ops_did_about_it() {
    let fixture = Fixture::new();
    fixture.write("locked/present.txt", "one\n");
    fixture.write("top.txt", "top\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    let good = fixture.snap("claude", "before");

    // Turn #2 drops the file inside `locked/` and adds one at the root, so a
    // restore to #1 must delete `extra.txt` and then re-create
    // `locked/present.txt` — a creation, which needs write permission on the
    // directory.
    fs::remove_file(fixture.repo.join("locked/present.txt")).unwrap();
    fixture.write("extra.txt", "added by the turn that went wrong\n");
    fixture.snap("claude", "after");

    let locked = fixture.repo.join("locked");
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    let enforced = fs::File::create(locked.join("probe")).is_err();
    if !enforced {
        let _ = fs::remove_file(locked.join("probe"));
        let _ = fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        eprintln!("skipped: this filesystem does not enforce directory permissions");
        return;
    }

    let ctx = fixture.ctx();
    let Reply::Planned { plan, .. } = engine::execute(&ctx, Job::Plan { req: 1, seq: good }) else {
        panic!("planning should succeed")
    };
    let reply = engine::execute(
        &ctx,
        Job::Restore {
            req: 2,
            seq: good,
            expect_tree: plan.current_tree.clone(),
            pane: None,
            notify: false,
        },
    );
    // Put the directory back before any assertion can abort the test and leave
    // a tempdir nobody can delete.
    let _ = fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755));

    match reply {
        Reply::RestoreFailed { message, tree_moved, .. } => {
            assert!(message.starts_with("the restore failed:"), "{message}");
            assert!(
                message.contains("Permission denied") || message.contains("locked"),
                "{message}"
            );
            // `ops` re-applied the checkpoint, so the deletion it had already
            // performed was undone — and the interface must not call that a
            // tree between two states.
            assert!(!tree_moved, "ops recovered, so nothing is uncertain: {message}");
            assert!(message.contains("put back as they were"), "{message}");
            assert!(
                fixture.repo.join("extra.txt").exists(),
                "the file the failed restore deleted should have been put back"
            );
        }
        other => panic!("expected a failure reply, got {other:?}"),
    }
    assert!(
        fixture.turns().iter().any(|t| t.kind == TurnKind::Checkpoint),
        "the checkpoint taken before the attempt is the way back and must be on the timeline"
    );
}

/// `--keys` runs before a single frame is drawn. `R` is the only key that
/// writes, so it is the one key `--keys` must not carry — otherwise
/// `sheep ui --rewind --select 1 --keys R --snapshot 80x20` rewrites a worktree
/// with nothing ever shown to anyone, which is the whole premise inverted.
#[test]
fn scripted_keys_cannot_reach_a_write() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    fixture.write("b.txt", "keep me\n");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "init"]);
    fixture.snap("claude", "first");
    fixture.write("a.txt", "two\n");
    fs::remove_file(fixture.repo.join("b.txt")).unwrap();
    fixture.snap("claude", "second");

    let ctx = fixture.ctx();
    let mut app = App::new("repo", fixture.repo.display().to_string(), "default");
    // Everything a caller could hand the scripted driver, aimed squarely at a
    // restore: open the plan for turn #1, then press the key.
    let args = UiArgs {
        rewind: true,
        no_notify: false,
        select: Some("1".into()),
        keys: Some("RRR".into()),
        snapshot: Some("80x20".into()),
    };
    cli::settle(&ctx, &mut app, &args);

    // The plan is on screen, which is the point of the driver.
    match &app.plan {
        PlanState::Ready(plan) => assert_eq!(plan.seq, 1),
        other => panic!("expected the plan for #1, got {other:?}"),
    }
    // And nothing happened to the worktree.
    assert_eq!(fs::read_to_string(fixture.repo.join("a.txt")).unwrap(), "two\n");
    assert!(!fixture.repo.join("b.txt").exists(), "restoring #1 would have brought this back");
    assert!(!app.restoring);
    assert!(
        fixture.turns().iter().all(|t| t.kind != TurnKind::Checkpoint),
        "a scripted key must not have got as far as checkpointing"
    );
    assert_eq!(fixture.turns().len(), 2, "no turn was appended");
}

#[test]
fn the_only_scripted_key_refused_is_the_one_that_writes() {
    assert_eq!(cli::scripted_key('R'), None);
    assert_eq!(cli::scripted_key('r'), Some(Key::Char('r')));
    assert_eq!(cli::scripted_key('d'), Some(Key::Char('d')));
    assert_eq!(cli::scripted_key('\r'), Some(Key::Enter));
    assert_eq!(cli::scripted_key('\x1b'), Some(Key::Esc));
}

/// What a restore reports, in each of the four things that can happen to the
/// write-back. This is the plugin's headline behaviour and the sentence a user
/// reads to find out whether their agent knows what happened; only the
/// "no pane" case was covered anywhere.
#[test]
fn a_restore_reports_the_way_back_and_what_the_agent_was_told() {
    let outcome = |notice: Notice| sheep::tui::engine::Outcome {
        seq: 7,
        commit: "a1b2c3d4e5f6".repeat(3),
        written: 6,
        removed: 2,
        checkpoint: Some(13),
        notice,
    };

    let told = app::restored_lines(&outcome(Notice::Sent("w3K:p2".into()))).join(" | ");
    assert!(told.contains("restored to #7 · 6 files written, 2 removed"), "{told}");
    assert!(told.contains("turn #13"), "the way back has to be named: {told}");
    assert!(told.contains("`sheep restore #13 --yes`"), "{told}");
    assert!(told.contains("the agent in pane w3K:p2 was told what was taken back"), "{told}");

    let off = app::restored_lines(&outcome(Notice::Off)).join(" | ");
    assert!(off.contains("the agent was not told"), "{off}");
    assert!(off.contains("press n"), "and how to turn it back on: {off}");

    let skipped = app::restored_lines(&outcome(Notice::Skipped("not running inside herdr".into())))
        .join(" | ");
    assert!(skipped.contains("not told: not running inside herdr"), "{skipped}");

    let failed =
        app::restored_lines(&outcome(Notice::Failed("herdr api not_found: no such pane".into())))
            .join(" | ");
    assert!(failed.contains("could not tell the agent"), "{failed}");
    assert!(failed.contains("no such pane"), "the reason has to survive: {failed}");

    // A restore with nothing to checkpoint must not promise a way back.
    let mut nothing = outcome(Notice::Off);
    nothing.checkpoint = None;
    let bare = app::restored_lines(&nothing).join(" | ");
    assert!(!bare.contains("sheep restore #"), "{bare}");
    assert!(bare.contains("nothing needed checkpointing"), "{bare}");
}

#[test]
fn the_cursor_follows_the_turn_rather_than_the_row() {
    let mut app = App::new("demo", "/tmp/demo", "default");
    let make = |seq: u64| sheep::store::Turn {
        seq,
        kind: TurnKind::Turn,
        commit: format!("{seq}"),
        tree: "t".into(),
        parent: None,
        at: 0,
        files: 1,
        insertions: 0,
        deletions: 0,
        pane_id: None,
        agent: None,
        prompt: None,
        note: None,
    };
    app.apply(Reply::Turns(vec![make(3), make(2), make(1)]));
    app.on_key(Key::Down);
    assert_eq!(app.selected().unwrap().seq, 2);

    // The agent records another turn while the dock is open.
    app.apply(Reply::Turns(vec![make(4), make(3), make(2), make(1)]));
    assert_eq!(
        app.selected().unwrap().seq,
        2,
        "a new turn arriving must not slide the cursor onto a different snapshot"
    );
}
