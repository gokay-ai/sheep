//! What the interface actually puts on screen.
//!
//! Every case here renders a real frame through `ratatui`'s `TestBackend` and
//! asserts on the words in it. "It did not panic" is not a passing bar for a
//! surface whose entire job is to tell someone what is about to happen to their
//! files.

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use sheep::store::{Turn, TurnKind};
use sheep::tui::app::{App, Fatal, Key, PatchState};
use sheep::tui::engine::{Action, Job, PlanView, Reply};
use sheep::tui::render;

const NOW: u64 = 1_780_000_000;

fn turn(seq: u64, kind: TurnKind, minutes_ago: u64) -> Turn {
    Turn {
        seq,
        kind,
        commit: format!("{seq:0>4}c0ffee1234567890abcdef1234567890abcd"),
        tree: format!("{seq:0>4}tree"),
        parent: if seq > 1 { Some("parent".into()) } else { None },
        at: NOW - minutes_ago * 60,
        files: 5,
        insertions: 214,
        deletions: 38,
        pane_id: Some("w3K:p2".into()),
        agent: Some("claude".into()),
        prompt: None,
        note: None,
    }
}

fn app_with(turns: Vec<Turn>) -> App {
    let mut app = App::new("checkout-service", "/tmp/checkout-service", "default");
    app.inside_herdr = true;
    app.now = NOW;
    app.apply(Reply::Loaded { turns, blockers: vec![], warnings: vec![] });
    app
}

fn demo() -> App {
    let mut a = turn(3, TurnKind::Turn, 2);
    a.prompt = Some("tidy this up, make it terser".into());
    a.insertions = 4;
    a.deletions = 24;
    let mut b = turn(2, TurnKind::Turn, 11);
    b.prompt = Some("add a discount code path and wire the cart component to it".into());
    b.files = 2;
    b.insertions = 12;
    b.deletions = 1;
    let mut c = turn(1, TurnKind::Checkpoint, 45);
    c.note = Some("before restore to 20f4c759dbb7".into());
    c.files = 1;
    app_with(vec![a, b, c])
}

/// Render one frame and flatten it to text, one line per row.
fn frame(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render::draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[track_caller]
fn shows(screen: &str, needle: &str) {
    assert!(screen.contains(needle), "expected `{needle}` on screen:\n{screen}");
}

#[track_caller]
fn hides(screen: &str, needle: &str) {
    assert!(!screen.contains(needle), "did not expect `{needle}` on screen:\n{screen}");
}

fn plan_view() -> PlanView {
    PlanView {
        seq: 2,
        commit: "2faeca7e387e0000000000000000000000000000".into(),
        target_tree: "tt".into(),
        current_tree: "ct".into(),
        written: 2,
        removed: 1,
        files: vec![
            (Action::Write, "src/api/cart.ts".into()),
            (Action::Write, "src/ui/Cart.tsx".into()),
            (Action::Remove, "src/api/checkout.ts".into()),
        ],
    }
}

/// Open the rewind view the way a keystroke does, and answer the plan job it
/// asks for. Going through the real request id is the point: a reply the app
/// did not ask for must not be able to put a plan on screen.
fn open_rewind(app: &mut App, plan: PlanView) {
    app.on_key(Key::Enter);
    let jobs = app.take_jobs();
    let Some(Job::Plan { req, .. }) = jobs.first() else {
        panic!("opening the rewind view should ask for a plan, got {jobs:?}");
    };
    app.apply(Reply::Planned { req: *req, plan });
}

// ------------------------------------------------------------------- the dock

#[test]
fn the_timeline_says_what_each_turn_did() {
    let screen = frame(&demo(), 92, 24);

    // identity, kind, agent, age
    shows(&screen, "#3");
    shows(&screen, "turn");
    shows(&screen, "checkpoint");
    shows(&screen, "claude");
    shows(&screen, "2m ago");
    shows(&screen, "45m ago");

    // the shape of the change
    shows(&screen, "5 files");
    shows(&screen, "+4");
    shows(&screen, "−24");
    shows(&screen, "+12 −1");

    // the captured prompt, and the fact that it is only a reading of a screen
    shows(&screen, "“tidy this up, make it terser”");
    shows(&screen, "read off the pane — not authoritative");

    // the checkpoint's note stands in for a prompt it never had
    shows(&screen, "before restore to 20f4c759dbb7");

    // where you are in the timeline, and the snapshot behind the selected row
    shows(&screen, "1/3");
    shows(&screen, "0003c0ffee12");
    shows(&screen, "checkout-service");
    shows(&screen, "timeline default");
}

#[test]
fn a_first_turn_is_not_described_as_an_empty_diff() {
    let mut first = turn(1, TurnKind::Turn, 3);
    first.insertions = 0;
    first.deletions = 0;
    first.files = 5;
    let screen = frame(&app_with(vec![first]), 80, 14);
    shows(&screen, "5 files captured — the starting point");
    hides(&screen, "+0 −0");
}

#[test]
fn a_forty_column_dock_keeps_every_turn_readable() {
    let screen = frame(&demo(), 40, 26);
    for needle in ["#3", "#2", "#1", "turn", "claude", "5 files", "+4 −24"] {
        shows(&screen, needle);
    }
    // A dock this narrow drops detail rather than overflowing.
    for line in screen.lines() {
        assert!(line.chars().count() <= 40, "line is wider than the dock: {line:?}");
    }
}

#[test]
fn the_empty_state_says_what_to_run() {
    let screen = frame(&app_with(vec![]), 64, 16);
    shows(&screen, "nothing recorded on `default` yet");
    shows(&screen, "sheep snap");
    shows(&screen, "sheep watch");
    shows(&screen, "record every agent turn");
    hides(&screen, "1/0");
}

#[test]
fn a_blocked_worktree_is_named_above_the_timeline() {
    let mut app = demo();
    app.apply(Reply::Loaded {
        turns: app.turns.clone(),
        blockers: vec!["a rebase is in progress. Sheep will not touch a tree mid-operation.".into()],
        warnings: vec![],
    });
    let screen = frame(&app, 76, 18);
    shows(&screen, "blocked");
    shows(&screen, "cannot record or restore");
    shows(&screen, "a rebase is in progress");
}

#[test]
fn a_worktree_sheep_cannot_use_explains_itself_instead_of_showing_a_box() {
    let app = App::new("plain", "/tmp/plain", "default").dead(Fatal {
        headline: "not a git worktree".into(),
        detail: "/tmp/plain is not inside a git worktree.".into(),
        remedy: vec!["cd into a checkout, or run `git init` here.".into()],
    });
    let screen = frame(&app, 70, 14);
    shows(&screen, "not a git worktree");
    shows(&screen, "is not inside a git worktree");
    shows(&screen, "git init");
    shows(&screen, "q quit");
    hides(&screen, "timeline");
}

// ----------------------------------------------------------- the rewind plan

#[test]
fn the_plan_preview_shows_every_path_before_anything_is_written() {
    let mut app = demo();
    app.on_key(Key::Down); // select #2, the turn we want back
    open_rewind(&mut app, plan_view());
    let screen = frame(&app, 92, 26);

    shows(&screen, "rewind to #2");
    shows(&screen, "back to #2");
    shows(&screen, "“add a discount code path and wire the cart component to it”");
    shows(&screen, "3 paths change");
    shows(&screen, "2 written");
    shows(&screen, "1 removed");

    shows(&screen, "will be written (2)");
    shows(&screen, "+ src/api/cart.ts");
    shows(&screen, "+ src/ui/Cart.tsx");
    shows(&screen, "will be removed (1)");
    shows(&screen, "− src/api/checkout.ts");

    // the footer has to state the consequence, the undo, and the write-back
    shows(&screen, "restoring rewrites 2 files and deletes 1 file");
    shows(&screen, "snapshotted first as a new turn, so this is undoable");
    shows(&screen, "the agent in pane w3K:p2 will be told what was taken back");
    shows(&screen, "shift+R");
    shows(&screen, "dry run — nothing is written yet");
}

#[test]
fn the_plan_preview_survives_a_forty_column_dock() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    let screen = frame(&app, 40, 24);
    shows(&screen, "rewind to #2");
    shows(&screen, "3 paths");
    shows(&screen, "+ src/api/cart.ts");
    shows(&screen, "− src/api/checkout.ts");
    shows(&screen, "shift+R");
    for line in screen.lines() {
        assert!(line.chars().count() <= 40, "line is wider than the dock: {line:?}");
    }
}

#[test]
fn turning_the_write_back_off_is_visible_in_the_confirmation() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.on_key(Key::Char('n'));
    let screen = frame(&app, 88, 26);
    shows(&screen, "the agent will NOT be told what changed");
}

#[test]
fn a_timeline_with_no_pane_says_nobody_will_be_told() {
    let mut a = turn(2, TurnKind::Turn, 4);
    a.pane_id = None;
    let mut b = turn(1, TurnKind::Turn, 40);
    b.pane_id = None;
    let mut app = app_with(vec![a, b]);
    open_rewind(&mut app, plan_view());
    let screen = frame(&app, 88, 24);
    shows(&screen, "no agent pane recorded on this timeline");
}

#[test]
fn a_plan_that_cannot_be_made_shows_the_reason_not_an_empty_list() {
    let mut app = demo();
    app.on_key(Key::Enter);
    let jobs = app.take_jobs();
    let Some(Job::Plan { req, seq }) = jobs.first().cloned() else { panic!("no plan job") };
    app.apply(Reply::PlanFailed {
        req,
        seq,
        message: "snapshot is incomplete: object 4b825dc is unreachable.".into(),
    });
    let screen = frame(&app, 80, 20);
    shows(&screen, "this turn cannot be restored");
    shows(&screen, "object 4b825dc is unreachable");
    hides(&screen, "shift+R");
}

#[test]
fn a_turn_already_on_disk_offers_nothing_to_restore() {
    let mut app = demo();
    let mut plan = plan_view();
    plan.files.clear();
    plan.written = 0;
    plan.removed = 0;
    open_rewind(&mut app, plan);
    let screen = frame(&app, 80, 20);
    shows(&screen, "the working tree already matches this turn");
    shows(&screen, "there is nothing for a restore to do");
    hides(&screen, "shift+R");
}

#[test]
fn the_patch_pane_shows_the_hunks_a_restore_would_apply() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.on_key(Key::Char('d'));
    let jobs = app.take_jobs();
    let Some(Job::Patch { req, path, .. }) = jobs.first().cloned() else {
        panic!("asking for a diff should queue a patch job, got {jobs:?}")
    };
    assert_eq!(path, "src/api/cart.ts");
    app.apply(Reply::Patched {
        req,
        path,
        body: "@@ -1 +1,4 @@\n-export const total = (i) => 0;\n+export function total() {\n+  return 1;\n+}"
            .into(),
    });
    assert!(matches!(app.patch, PatchState::Ready { .. }));

    let wide = frame(&app, 100, 24);
    shows(&wide, "src/api/cart.ts");
    shows(&wide, "@@ -1 +1,4 @@");
    shows(&wide, "+export function total() {");
    shows(&wide, "-export const total = (i) => 0;");
    // wide enough for both: the file list stays visible beside the patch
    shows(&wide, "will be written (2)");

    // narrow: the patch takes the panel rather than being squeezed into nothing
    let narrow = frame(&app, 56, 24);
    shows(&narrow, "@@ -1 +1,4 @@");
}

/// The screen the status line now tells people to read. Its file list and its
/// diff pane have to describe the same plan; they used to describe two.
#[test]
fn the_refusal_screen_does_not_leave_the_previous_plans_diff_beside_the_new_one() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());

    // Open the diff and answer the patch it asks for.
    app.on_key(Key::Char('d'));
    let jobs = app.take_jobs();
    let Some(Job::Patch { req, path, .. }) = jobs.first().cloned() else {
        panic!("opening the diff should ask for a patch, got {jobs:?}")
    };
    assert_eq!(path, "src/api/cart.ts");
    app.apply(Reply::Patched {
        req,
        path,
        body: "@@ -1 +1 @@\n-the patch for the plan that was on screen".into(),
    });
    shows(&frame(&app, 100, 26), "the patch for the plan that was on screen");

    // Press the key; the worker refuses because the agent kept working.
    app.on_key(Key::Char('R'));
    let jobs = app.take_jobs();
    let Some(Job::Restore { req, .. }) =
        jobs.into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };
    let fresher = PlanView {
        seq: 2,
        commit: "2faeca7e387e0000000000000000000000000000".into(),
        target_tree: "tt".into(),
        current_tree: "moved".into(),
        written: 1,
        removed: 1,
        files: vec![
            (Action::Write, "src/api/order.ts".into()),
            (Action::Remove, "src/api/agent-wrote-this.ts".into()),
        ],
    };
    app.apply(Reply::Stale { req, plan: fresher });

    let screen = frame(&app, 100, 26);
    shows(&screen, "nothing was restored");
    // the new plan
    shows(&screen, "+ src/api/order.ts");
    shows(&screen, "− src/api/agent-wrote-this.ts");
    // and no trace of the old one, in either pane
    hides(&screen, "the patch for the plan that was on screen");
    hides(&screen, "src/api/cart.ts");
    // the pane is still open, and now points at the new plan's first file
    assert!(app.show_patch);
    let asked: Vec<String> = app
        .take_jobs()
        .into_iter()
        .filter_map(|j| match j {
            Job::Patch { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec!["src/api/order.ts"], "the refusal has to refetch the evidence");
}

/// Open the diff for a path, then get refused with a plan that contains *the
/// same path*. The hunks must be fetched again: they were computed against a
/// tree that no longer describes the working directory, and serving them from
/// the cache would put a stale diff under a fresh file list — the exact defect
/// with a different route in.
#[test]
fn a_refusal_refetches_hunks_for_a_path_the_new_plan_still_contains() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());

    app.on_key(Key::Char('d'));
    let jobs = app.take_jobs();
    let Some(Job::Patch { req, path, .. }) = jobs.first().cloned() else { panic!("no patch job") };
    assert_eq!(path, "src/api/cart.ts");
    app.apply(Reply::Patched { req, path, body: "@@ -1 +1 @@\n-hunks from the old tree".into() });
    shows(&frame(&app, 100, 26), "hunks from the old tree");

    app.on_key(Key::Char('R'));
    let jobs = app.take_jobs();
    let Some(Job::Restore { req, .. }) =
        jobs.into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };
    // The new plan keeps cart.ts and drops the rest.
    app.apply(Reply::Stale {
        req,
        plan: PlanView {
            seq: 2,
            commit: "2faeca7e387e0000000000000000000000000000".into(),
            target_tree: "tt".into(),
            current_tree: "moved".into(),
            written: 1,
            removed: 0,
            files: vec![(Action::Write, "src/api/cart.ts".into())],
        },
    });

    let asked: Vec<String> = app
        .take_jobs()
        .into_iter()
        .filter_map(|j| match j {
            Job::Patch { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec!["src/api/cart.ts"],
        "the hunks must be read again, not served from a cache keyed on a tree that has moved"
    );
    assert!(
        matches!(&app.patch, PatchState::Loading(p) if p == "src/api/cart.ts"),
        "expected a fresh fetch, got {:?}",
        app.patch
    );
    hides(&frame(&app, 100, 26), "hunks from the old tree");
}

/// The worst thing the interface could say. `shadow::apply` deletes before it
/// writes, so a failure in between leaves files already gone — and the old
/// screen answered that with "nothing was written" over a border still
/// promising a dry run.
#[test]
fn a_restore_that_stopped_partway_is_not_reported_as_nothing_written() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.on_key(Key::Char('R'));
    let jobs = app.take_jobs();
    let Some(Job::Restore { req, .. }) =
        jobs.into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };
    app.apply(Reply::RestoreFailed {
        req,
        message: "the restore failed: cannot write src/api/cart.ts: Permission denied (os error 13).                   Your working tree is between two states — `sheep restore #4 --yes` returns it to how it was."
            .into(),
        tree_moved: true,
    });

    let screen = frame(&app, 92, 26);
    hides(&screen, "nothing was written");
    hides(&screen, "dry run — nothing is written yet");
    shows(&screen, "the restore stopped partway — your files are between two states");
    shows(&screen, "Permission denied");
    // The last clause of the message is the way back; a status block that cuts
    // the sentence short throws away the only thing the user can act on.
    shows(&screen, "`sheep restore #4 --yes` returns it to how it was.");
    shows(&screen, "this worktree is between two states");
    // and no key that would write again is offered on a tree in this state
    hides(&screen, "shift+R");

    // Narrow, where the message has to wrap: the last clause is the way back,
    // and a status block that runs out of room throws away the only thing the
    // user can act on.
    let narrow = frame(&app, 58, 30);
    shows(&narrow, "it to how it was.");
    hides(&narrow, "nothing was written");

    // Escaping back to the dock does not make it go away.
    app.on_key(Key::Esc);
    let dock = frame(&app, 92, 26);
    shows(&dock, "this worktree is between two states");
    shows(&dock, "unsafe");
    shows(&dock, "Permission denied");
}

/// The recovered case: `ops` put the files back, so the tree is exactly as it
/// was and nothing on screen needs a warning.
#[test]
fn a_restore_that_was_undone_for_us_says_so_and_leaves_no_warning() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.on_key(Key::Char('R'));
    let jobs = app.take_jobs();
    let Some(Job::Restore { req, .. }) =
        jobs.into_iter().find(|j| matches!(j, Job::Restore { .. }))
    else {
        panic!("no restore job")
    };
    // The wording a real failure produces, verbatim from `ops`.
    app.apply(Reply::RestoreFailed {
        req,
        message: "the restore failed: restore failed while writing files: error: unable to create \
                  file locked/discount.ts: Permission denied. Your files were put back as they were."
            .into(),
        tree_moved: false,
    });

    let screen = frame(&app, 92, 26);
    shows(&screen, "your files are as they were");
    shows(&screen, "put back as they were.");
    hides(&screen, "this worktree is between two states");
    // The plan is still true — the tree is exactly what it was — so the file
    // list stays up and the key stays offered.
    shows(&screen, "will be written (2)");
    shows(&screen, "shift+R");

    // Narrow, where it has to wrap. Here the status block is the only thing
    // carrying the message — the plan is still `Ready`, so the body is the file
    // list — and a block that runs out of room silently drops the clause that
    // says what actually became of the files.
    shows(&frame(&app, 58, 30), "put back as they were.");

    app.on_key(Key::Esc);
    hides(&frame(&app, 92, 26), "unsafe");
}

/// The evidence pane's own failure. Nothing rendered it before, so its wording
/// was whatever the last edit left behind.
#[test]
fn a_patch_that_cannot_be_read_says_so_in_the_pane() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.on_key(Key::Char('d'));
    let jobs = app.take_jobs();
    let Some(Job::Patch { req, path, .. }) = jobs.first().cloned() else { panic!("no patch job") };
    app.apply(Reply::PatchFailed {
        req,
        path,
        message: "git diff-tree failed: fatal: bad object 4b825dc".into(),
    });
    let screen = frame(&app, 96, 24);
    shows(&screen, "src/api/cart.ts");
    shows(&screen, "bad object 4b825dc");
    // the plan itself is untouched by a preview that could not be read
    shows(&screen, "will be written (2)");
    shows(&screen, "shift+R");
}

/// A timeline Sheep cannot read at all. The dock has to say why rather than
/// showing an empty box that looks like "nothing has happened yet".
#[test]
fn a_timeline_that_cannot_be_read_says_why() {
    let mut app = demo();
    app.apply(Reply::Broken("cannot read /state/turns/x.ndjson: Permission denied".into()));
    let screen = frame(&app, 80, 20);
    shows(&screen, "cannot read /state/turns/x.ndjson");
    shows(&screen, "Permission denied");
}

#[test]
fn a_single_file_turn_is_not_described_in_the_plural() {
    let mut one = turn(1, TurnKind::Turn, 4);
    one.files = 1;
    one.parent = Some("p".into());
    let screen = frame(&app_with(vec![one]), 80, 12);
    shows(&screen, "1 file ");
    hides(&screen, "1 files");
}

#[test]
fn the_help_overlay_documents_the_restore_key() {
    let mut app = demo();
    app.on_key(Key::Char('?'));
    let screen = frame(&app, 76, 22);
    shows(&screen, "keys");
    shows(&screen, "shift+R");
    shows(&screen, "restore — only from a plan on screen");
    shows(&screen, "scroll the patch");
}

#[test]
fn a_short_commit_id_does_not_take_the_interface_down() {
    // `ops::short` bounds its own slice; a second unbounded one on top of it
    // turns a truncated turn log into a panic on every frame wide enough to
    // show the column.
    let mut stub = turn(2, TurnKind::Turn, 3);
    stub.commit = "abc".into();
    let mut empty = turn(1, TurnKind::Turn, 9);
    empty.commit = String::new();
    let app = app_with(vec![stub, empty]);

    for width in [40u16, 62, 63, 92, 160] {
        let screen = frame(&app, width, 18);
        shows(&screen, "#2");
        shows(&screen, "#1");
    }
    // Wide enough for the id column: the short id is shown as far as it goes.
    shows(&frame(&app, 92, 18), "abc · 3m ago");
}

#[test]
fn the_border_does_not_promise_a_dry_run_while_it_is_writing() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    let before = frame(&app, 88, 24);
    shows(&before, "dry run — nothing is written yet");

    app.restoring = true;
    let during = frame(&app, 88, 24);
    shows(&during, "checkpointing the tree you have, then restoring");
    shows(&during, "writing — do not interrupt");
    hides(&during, "dry run — nothing is written yet");
    // and the key that starts a write is no longer offered
    hides(&during, "shift+R");
}

#[test]
fn quitting_during_a_restore_says_why_the_window_is_still_open() {
    let mut app = demo();
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    app.restoring = true;
    shows(&frame(&app, 88, 24), "keys are ignored until it finishes");

    app.on_key(Key::Char('q'));
    assert!(app.quit && app.restoring, "quitting must not clear the in-flight restore");
    let screen = frame(&app, 88, 24);
    shows(&screen, "finishing this before quitting");
    shows(&screen, "killing the window now would");
    shows(&screen, "neither state.");
}

#[test]
fn outside_herdr_the_footer_does_not_promise_a_write_back() {
    // A timeline can carry a pane id from an earlier herdr session while this
    // process has no socket to reach it through.
    let mut app = demo();
    app.inside_herdr = false;
    app.on_key(Key::Down);
    open_rewind(&mut app, plan_view());
    let screen = frame(&app, 88, 26);
    assert!(
        app.agent_pane().is_some(),
        "the fixture has to carry a pane id for this to mean anything"
    );
    hides(&screen, "will be told what was taken back");
    shows(&screen, "not running inside herdr — there is no agent to tell.");
    shows(&screen, "standalone");
}

#[test]
fn nothing_ever_draws_past_the_edge_of_the_terminal() {
    let mut plan = plan_view();
    plan.files
        .push((Action::Write, "a/very/deeply/nested/path/that/keeps/going/on/file.tsx".into()));
    plan.written += 1;
    for (w, h) in [(40u16, 12u16), (52, 20), (76, 18), (120, 40), (200, 60)] {
        for setup in 0..3 {
            let mut app = demo();
            match setup {
                1 => open_rewind(&mut app, plan.clone()),
                2 => app.on_key(Key::Char('?')),
                _ => {}
            }
            let screen = frame(&app, w, h);
            assert_eq!(screen.lines().count(), h as usize, "wrong row count at {w}x{h}");
            for line in screen.lines() {
                assert!(
                    line.chars().count() <= w as usize,
                    "{w}x{h} setup {setup} overflowed: {line:?}"
                );
            }
        }
    }
}
