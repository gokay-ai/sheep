//! The reconnect policy.
//!
//! `sheep watch` is meant to be started once and left for a day, so the states
//! that matter are the ones that only show up after hours: a herdr that
//! acknowledges a subscription and drops it immediately, a socket that
//! disappears during a live handoff and comes back, a session that has really
//! ended. Driving those for real would mean minutes of wall-clock per case, so
//! the policy is a value that takes the clock as an argument.

use sheep::herdr::supervise::{
    Next, Supervisor, GONE_AFTER, INITIAL_BACKOFF, MAX_BACKOFF, MIN_HEALTHY,
};
use std::time::{Duration, Instant};

fn retry_after(next: Next) -> Duration {
    match next {
        Next::Retry(delay) => delay,
        Next::Stop => panic!("expected a retry, got a stop"),
    }
}

#[test]
fn a_connection_that_never_worked_backs_off() {
    let mut supervisor = Supervisor::new();
    let now = Instant::now();

    let first = retry_after(supervisor.after(now, Duration::ZERO, true));
    let second = retry_after(supervisor.after(now, Duration::ZERO, true));
    let third = retry_after(supervisor.after(now, Duration::ZERO, true));

    assert!(first < second && second < third, "{first:?} {second:?} {third:?}");
    assert_eq!(first, INITIAL_BACKOFF * 2);
}

#[test]
fn a_subscription_that_dies_at_once_does_not_become_a_spin() {
    // The failure this exists for: a subscription that is acknowledged and then
    // closed still *opens*, so treating "opened" as success resets the backoff
    // every time. The supervisor would then retry twice a second for ever, each
    // attempt running a full re-sync against a server that is plainly unwell.
    let mut supervisor = Supervisor::new();
    let now = Instant::now();

    let mut delay = Duration::ZERO;
    for _ in 0..12 {
        // Opened, lived for a moment, died. The socket is still there.
        delay = retry_after(supervisor.after(now, Duration::from_millis(20), true));
    }

    assert_eq!(delay, MAX_BACKOFF, "twelve instant deaths must have reached the ceiling");
}

#[test]
fn a_connection_that_lasted_resets_the_backoff() {
    let mut supervisor = Supervisor::new();
    let now = Instant::now();

    for _ in 0..6 {
        let _ = supervisor.after(now, Duration::ZERO, true);
    }
    assert!(supervisor.backoff() > INITIAL_BACKOFF);

    let delay = retry_after(supervisor.after(now, MIN_HEALTHY, true));
    assert_eq!(delay, INITIAL_BACKOFF, "a connection that worked earns a fast retry again");
}

#[test]
fn a_connection_just_under_the_bar_does_not_reset_it() {
    let mut supervisor = Supervisor::new();
    let now = Instant::now();

    let first = retry_after(supervisor.after(now, MIN_HEALTHY - Duration::from_millis(1), true));
    let second = retry_after(supervisor.after(now, MIN_HEALTHY - Duration::from_millis(1), true));
    assert!(second > first, "the bar is a bar, not a suggestion: {first:?} then {second:?}");
}

#[test]
fn a_socket_that_comes_back_is_a_handoff_not_a_death() {
    // A live handoff replaces the socket. Somebody watching a session through
    // one should not lose their recorder.
    let mut supervisor = Supervisor::new();
    let start = Instant::now();

    assert!(matches!(supervisor.after(start, Duration::ZERO, false), Next::Retry(_)));
    assert!(matches!(
        supervisor.after(start + GONE_AFTER / 2, Duration::ZERO, false),
        Next::Retry(_)
    ));
    // Back again, well inside the window.
    assert!(matches!(
        supervisor.after(start + GONE_AFTER / 2, Duration::ZERO, true),
        Next::Retry(_)
    ));
    // And the clock started over, so the old absence cannot add up to a death.
    assert!(matches!(
        supervisor.after(start + GONE_AFTER * 2, Duration::ZERO, false),
        Next::Retry(_)
    ));
}

#[test]
fn a_socket_that_stays_missing_ends_the_watch() {
    let mut supervisor = Supervisor::new();
    let start = Instant::now();

    assert!(matches!(supervisor.after(start, Duration::ZERO, false), Next::Retry(_)));
    assert_eq!(
        supervisor.after(start + GONE_AFTER, Duration::ZERO, false),
        Next::Stop,
        "a session that really has gone should stop the recorder, cleanly"
    );
}

#[test]
fn a_present_socket_never_ends_the_watch() {
    // A herdr that is up but refusing is worth waiting for, however long it
    // takes: it is what a restart looks like from here.
    let mut supervisor = Supervisor::new();
    let start = Instant::now();
    for minute in 0..120 {
        let at = start + Duration::from_secs(minute * 60);
        assert!(matches!(supervisor.after(at, Duration::ZERO, true), Next::Retry(_)));
    }
}
