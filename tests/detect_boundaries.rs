//! The turn detector, driven by synthetic event sequences.
//!
//! No socket, no clock of its own, no sleeping: the state machine takes
//! sightings and an explicit `Instant`, so every one of these cases runs in
//! microseconds and means the same thing on every machine.
//!
//! The cases that matter most are the ones where the detector has to say *no*.
//! Herdr can flip a pane to `done` while the agent is still mid-turn, and a
//! turn Sheep invents is worse than a turn it misses.

use sheep::herdr::detect::{Detector, Sighting, Signal, Status, Tuning, Verdict, Withdrawn};
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(3_000);
const PATIENCE: Duration = Duration::from_secs(120);

fn detector() -> Detector {
    Detector::new(Tuning { settle: SETTLE, patience: PATIENCE })
}

fn seen(pane: &str, status: Status, revision: u64) -> Sighting {
    Sighting {
        pane_id: pane.to_string(),
        agent: Some("claude".to_string()),
        cwd: Some("/tmp/work".to_string()),
        status,
        revision,
    }
}

fn is_candidate(signal: &Signal) -> bool {
    matches!(signal, Signal::Candidate { .. })
}

fn withdrawn(signals: &[Signal]) -> Option<Withdrawn> {
    signals.iter().find_map(|s| match s {
        Signal::Withdrawn { why, .. } => Some(*why),
        _ => None,
    })
}

fn seen_in(pane: &str, cwd: &str, status: Status, revision: u64) -> Sighting {
    Sighting { cwd: Some(cwd.to_string()), ..seen(pane, status, revision) }
}

fn ripe_cwd(signals: &[Signal]) -> Option<String> {
    signals.iter().find_map(|s| match s {
        Signal::Ripe { cwd, .. } => cwd.clone(),
        _ => None,
    })
}

fn ripe(signals: &[Signal]) -> Option<(String, bool)> {
    signals.iter().find_map(|s| match s {
        Signal::Ripe { pane_id, noisy, .. } => Some((pane_id.clone(), *noisy)),
        _ => None,
    })
}

#[test]
fn a_finished_turn_ripens_once_the_pane_has_been_quiet() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    let opened = d.observe(t0 + Duration::from_secs(5), &seen("w1:p1", Status::Idle, 11));
    assert!(opened.iter().any(is_candidate), "working -> idle opens a candidate: {opened:?}");

    // Nothing fires while the window is still open.
    assert!(d.tick(t0 + Duration::from_secs(6)).is_empty(), "the window is not up yet");

    let fired = d.tick(t0 + Duration::from_secs(9));
    let (pane, noisy) = ripe(&fired).expect("the boundary should ripen");
    assert_eq!(pane, "w1:p1");
    assert!(!noisy, "a pane that went quiet is not noisy");
}

#[test]
fn a_pane_that_was_never_working_is_never_a_boundary() {
    let mut d = detector();
    let t0 = Instant::now();

    // Sheep starting up next to an agent that is already sitting at idle must
    // not read that as a turn it just finished.
    d.observe(t0, &seen("w1:p1", Status::Idle, 4));
    let signals = d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Done, 5));

    assert!(signals.is_empty(), "idle -> done is not a turn: {signals:?}");
    assert!(d.tick(t0 + Duration::from_secs(60)).is_empty(), "and nothing ripens later");
}

#[test]
fn a_false_done_is_withdrawn_when_the_pane_goes_back_to_working() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    let opened = d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Done, 11));
    assert!(opened.iter().any(is_candidate));

    // Herdr was wrong: the agent is still going. This is the whole reason the
    // detector waits instead of recording on the edge.
    let back = d.observe(t0 + Duration::from_millis(1_500), &seen("w1:p1", Status::Working, 12));
    assert_eq!(withdrawn(&back), Some(Withdrawn::StillWorking));

    assert!(
        d.tick(t0 + Duration::from_secs(60)).is_empty(),
        "a withdrawn candidate must never ripen"
    );

    // The real end of the turn still records.
    d.observe(t0 + Duration::from_secs(30), &seen("w1:p1", Status::Done, 40));
    assert!(ripe(&d.tick(t0 + Duration::from_secs(34))).is_some(), "the real boundary lands");
}

#[test]
fn new_output_during_the_window_restarts_it() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Done, 11));

    // Still painting: an agent that has finished stops writing to the screen.
    d.observe(t0 + Duration::from_millis(3_000), &seen("w1:p1", Status::Done, 12));

    assert!(
        d.tick(t0 + Duration::from_millis(4_100)).is_empty(),
        "the window should have restarted at the new output"
    );
    assert!(
        ripe(&d.tick(t0 + Duration::from_millis(6_100))).is_some(),
        "and close a full settle after the last paint"
    );
}

#[test]
fn a_repeated_revision_is_not_new_output() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    // A rename or a focus change re-sends the pane with the same revision.
    d.observe(t0 + Duration::from_millis(3_500), &seen("w1:p1", Status::Idle, 11));

    assert!(
        ripe(&d.tick(t0 + Duration::from_millis(4_100))).is_some(),
        "only a higher revision means the pane painted"
    );
}

#[test]
fn patience_stops_the_window_restarting_for_ever() {
    let mut d = Detector::new(Tuning { settle: SETTLE, patience: Duration::from_secs(10) });
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));

    // A pane that will not stop painting — a user typing the next prompt, say.
    for step in 1..20 {
        let at = t0 + Duration::from_secs(1) + Duration::from_millis(step * 500);
        d.observe(at, &seen("w1:p1", Status::Idle, 11 + step));
    }

    let fired = d.tick(t0 + Duration::from_secs(12));
    let (_, noisy) = ripe(&fired).expect("patience runs out and the boundary is decided");
    assert!(noisy, "and it is flagged as never having gone quiet");
}

#[test]
fn blocked_withdraws_a_candidate() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Done, 11));
    let signals = d.observe(t0 + Duration::from_millis(1_200), &seen("w1:p1", Status::Blocked, 12));

    assert_eq!(withdrawn(&signals), Some(Withdrawn::Blocked));
    assert!(
        d.tick(t0 + Duration::from_secs(30)).is_empty(),
        "an agent waiting on the user has not finished a turn"
    );
}

#[test]
fn unknown_withdraws_and_forgets_that_the_pane_worked() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    let signals = d.observe(t0 + Duration::from_millis(1_200), &seen("w1:p1", Status::Unknown, 12));
    assert_eq!(withdrawn(&signals), Some(Withdrawn::LostAgent));

    // Coming back to rest without working again proves nothing.
    let after = d.observe(t0 + Duration::from_secs(5), &seen("w1:p1", Status::Idle, 13));
    assert!(after.is_empty(), "herdr lost the thread; do not invent a turn: {after:?}");
    assert!(d.tick(t0 + Duration::from_secs(30)).is_empty());
}

#[test]
fn a_pane_that_goes_away_takes_its_candidate_with_it() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));

    assert_eq!(withdrawn(&d.forget("w1:p1")), Some(Withdrawn::PaneGone));
    assert!(d.tick(t0 + Duration::from_secs(30)).is_empty());
    assert_eq!(d.tracked(), 0);
}

#[test]
fn waiting_re_arms_the_window_instead_of_dropping_the_turn() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    assert!(ripe(&d.tick(t0 + Duration::from_secs(5))).is_some());

    // Corroboration disagreed — the agent is still spawning processes.
    let _ = d.resolve(t0 + Duration::from_secs(5), "w1:p1", Verdict::Wait);
    assert!(d.is_pending("w1:p1"), "the candidate is still on the books");
    assert!(d.tick(t0 + Duration::from_secs(6)).is_empty(), "and not asked about again at once");
    assert!(ripe(&d.tick(t0 + Duration::from_secs(9))).is_some(), "it is asked about again later");
}

#[test]
fn a_recorded_turn_clears_the_pane_until_it_works_again() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    assert!(ripe(&d.tick(t0 + Duration::from_secs(5))).is_some());
    let _ = d.resolve(t0 + Duration::from_secs(5), "w1:p1", Verdict::Settled);

    // herdr often follows `idle` with `done`, or the other way round. Neither
    // is a second turn.
    let after = d.observe(t0 + Duration::from_secs(6), &seen("w1:p1", Status::Done, 12));
    assert!(after.is_empty(), "the same rest state must not record twice: {after:?}");
    assert!(d.tick(t0 + Duration::from_secs(30)).is_empty());
}

#[test]
fn dropping_a_candidate_does_not_leave_the_pane_armed() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    assert!(ripe(&d.tick(t0 + Duration::from_secs(5))).is_some());
    let _ = d.resolve(t0 + Duration::from_secs(5), "w1:p1", Verdict::Drop);

    assert!(!d.is_pending("w1:p1"));
    let after = d.observe(t0 + Duration::from_secs(6), &seen("w1:p1", Status::Done, 12));
    assert!(after.is_empty(), "a dropped candidate must not come back: {after:?}");
}

#[test]
fn the_prompt_is_held_from_the_start_of_a_turn_until_it_is_recorded() {
    let mut d = detector();
    let t0 = Instant::now();

    let started = d.observe(t0, &seen("w1:p1", Status::Working, 10));
    assert!(
        started.iter().any(|s| matches!(s, Signal::Started { .. })),
        "a turn starting is when the prompt is still on the screen: {started:?}"
    );

    d.set_prompt("w1:p1", Some("fix the flaky test".into()));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    assert_eq!(d.prompt("w1:p1"), Some("fix the flaky test"));

    d.tick(t0 + Duration::from_secs(5));
    let _ = d.resolve(t0 + Duration::from_secs(5), "w1:p1", Verdict::Settled);
    assert_eq!(d.prompt("w1:p1"), None, "one prompt belongs to exactly one turn");
}

#[test]
fn a_new_turn_starting_re_reads_the_prompt() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Idle, 10));
    let signals = d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Working, 11));
    assert!(signals.iter().any(|s| matches!(s, Signal::Started { .. })));

    // Still working is not a new turn, and must not re-read the pane.
    let same = d.observe(t0 + Duration::from_secs(2), &seen("w1:p1", Status::Working, 12));
    assert!(same.is_empty(), "only the edge into working starts a turn: {same:?}");
}

#[test]
fn each_pane_keeps_its_own_window() {
    let mut d = detector();
    let t0 = Instant::now();

    for pane in ["w1:p1", "w1:p2", "w2:p1", "w2:p2"] {
        d.observe(t0, &seen(pane, Status::Working, 10));
    }
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    d.observe(t0 + Duration::from_secs(4), &seen("w2:p2", Status::Done, 11));

    assert_eq!(d.next_deadline(), Some(t0 + Duration::from_secs(1) + SETTLE));

    let first = d.tick(t0 + Duration::from_secs(5));
    assert_eq!(ripe(&first).map(|(pane, _)| pane).as_deref(), Some("w1:p1"));
    assert!(d.tick(t0 + Duration::from_secs(5)).is_empty(), "a fired candidate does not repeat");

    let second = d.tick(t0 + Duration::from_secs(8));
    assert_eq!(ripe(&second).map(|(pane, _)| pane).as_deref(), Some("w2:p2"));
    assert_eq!(d.tracked(), 4);
}

#[test]
fn a_status_string_herdr_adds_later_reads_as_unknown() {
    // Forward compatibility: an unrecognised status must never look like rest.
    assert_eq!(Status::parse("compacting"), Status::Unknown);
    assert!(!Status::parse("compacting").is_rest());
    assert!(Status::parse("idle").is_rest() && Status::parse("done").is_rest());
    assert!(!Status::parse("working").is_rest() && !Status::parse("blocked").is_rest());
}

#[test]
fn a_turn_belongs_to_the_directory_it_happened_in() {
    // Herdr re-sends a pane when it changes directory, and the settle window is
    // ten seconds wide. Reading the *current* directory at the far end of it
    // files the turn against whatever repository the pane has wandered into.
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen_in("w1:p1", "/repo/a", Status::Working, 10));
    let opened =
        d.observe(t0 + Duration::from_secs(1), &seen_in("w1:p1", "/repo/a", Status::Idle, 11));
    assert!(opened.iter().any(is_candidate));

    let moved =
        d.observe(t0 + Duration::from_secs(2), &seen_in("w1:p1", "/repo/b", Status::Idle, 12));
    assert_eq!(
        withdrawn(&moved),
        Some(Withdrawn::MovedDirectory),
        "a pane that moved is not describing the tree the turn happened in"
    );
    assert!(
        d.tick(t0 + Duration::from_secs(30)).is_empty(),
        "and nothing may be filed against either repository"
    );
}

#[test]
fn a_ripe_boundary_reports_where_the_work_was_done() {
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen_in("w1:p1", "/repo/a", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen_in("w1:p1", "/repo/a", Status::Idle, 11));
    // A sighting with no directory at all is not a move — herdr omits the field
    // rather than reporting a change.
    d.observe(
        t0 + Duration::from_secs(2),
        &Sighting { cwd: None, ..seen("w1:p1", Status::Idle, 12) },
    );

    let fired = d.tick(t0 + Duration::from_secs(6));
    assert_eq!(ripe_cwd(&fired).as_deref(), Some("/repo/a"));
}

#[test]
fn patience_bounds_the_corroboration_loop_too() {
    // `Wait` is what the recorder answers when it cannot corroborate — which is
    // also what it answers when herdr has stopped replying. Each retry costs two
    // blocking requests, so an unbounded retry keeps the loop wedged on one sick
    // pane while every other pane goes unrecorded.
    let mut d = Detector::new(Tuning { settle: SETTLE, patience: Duration::from_secs(30) });
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));

    let mut asked = 0;
    let mut gave_up = None;
    for step in 1..40 {
        let at = t0 + Duration::from_secs(step * 3);
        if ripe(&d.tick(at)).is_none() {
            continue;
        }
        asked += 1;
        let signals = d.resolve(at, "w1:p1", Verdict::Wait);
        if let Some(why) = withdrawn(&signals) {
            gave_up = Some((why, step));
            break;
        }
    }

    assert_eq!(
        gave_up.map(|(why, _)| why),
        Some(Withdrawn::Uncorroborated),
        "after {asked} attempts the candidate should have been given up on"
    );
    assert!(!d.is_pending("w1:p1"), "and it must not still be on the books");
    assert!(d.tick(t0 + Duration::from_secs(600)).is_empty());
}

#[test]
fn giving_up_on_a_candidate_does_not_leave_the_pane_armed() {
    let mut d = Detector::new(Tuning { settle: SETTLE, patience: Duration::from_secs(5) });
    let t0 = Instant::now();

    d.observe(t0, &seen("w1:p1", Status::Working, 10));
    d.observe(t0 + Duration::from_secs(1), &seen("w1:p1", Status::Idle, 11));
    assert!(ripe(&d.tick(t0 + Duration::from_secs(5))).is_some());

    let signals = d.resolve(t0 + Duration::from_secs(20), "w1:p1", Verdict::Wait);
    assert_eq!(withdrawn(&signals), Some(Withdrawn::Uncorroborated));

    // The pane has to earn a new candidate by working again.
    let after = d.observe(t0 + Duration::from_secs(21), &seen("w1:p1", Status::Done, 12));
    assert!(after.is_empty(), "a given-up candidate must not come back: {after:?}");
}

#[test]
fn a_turn_starting_says_where_and_who() {
    // The recorder needs both to take a baseline on the timeline this pane is
    // about to write to, before the agent touches anything.
    let mut d = detector();
    let t0 = Instant::now();

    d.observe(t0, &seen_in("w1:p1", "/repo/a", Status::Idle, 10));
    let signals =
        d.observe(t0 + Duration::from_secs(1), &seen_in("w1:p1", "/repo/a", Status::Working, 11));

    let started = signals.iter().find_map(|s| match s {
        Signal::Started { agent, cwd, .. } => Some((agent.clone(), cwd.clone())),
        _ => None,
    });
    assert_eq!(started, Some((Some("claude".to_string()), Some("/repo/a".to_string()))));
}

#[test]
fn the_detector_can_list_what_it_is_holding() {
    let mut d = detector();
    let t0 = Instant::now();
    for pane in ["w1:p1", "w1:p2"] {
        d.observe(t0, &seen(pane, Status::Working, 10));
    }
    let mut ids = d.pane_ids();
    ids.sort();
    assert_eq!(ids, vec!["w1:p1".to_string(), "w1:p2".to_string()]);
}
