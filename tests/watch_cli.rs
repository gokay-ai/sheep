//! `sheep watch` has to talk when a human runs it, and refuse when it cannot.
//!
//! The recorder is silent as a daemon (`watchd.sh` redirects stdio). A hand-run
//! that also prints nothing is indistinguishable from a hang, which is how
//! `sheep watch` read in a terminal until the log started echoing on a tty.

use std::process::Command;
use std::time::Duration;

fn sheep() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sheep"))
}

#[test]
fn watch_refuses_outside_a_herdr_session_and_says_why() {
    // HERDR_ENV and HERDR_SOCKET_PATH leak from a developer running the suite
    // inside herdr; without scrubbing them this would subscribe and wait.
    let output = sheep()
        .arg("watch")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .expect("run sheep watch");
    assert!(!output.status.success(), "watch must not start outside herdr");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("inside a herdr session"),
        "a refusal that prints nothing is the blank hang this test exists to stop; stderr was: {err}"
    );
}

#[test]
fn watch_help_says_it_stays_in_the_foreground() {
    let output = sheep().args(["watch", "--help"]).output().expect("run sheep watch --help");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("foreground"), "{text}");
    assert!(text.contains("terminal"), "help must say a hand-run prints; was: {text}");
}

/// Guard against a regression that waits on herdr instead of refusing: if this
/// test hangs, `watch` started when it should have exited.
#[test]
fn watch_outside_herdr_returns_before_a_quiet_window() {
    let mut child = sheep()
        .arg("watch")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sheep watch");
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().expect("wait") {
            Some(status) => {
                assert!(!status.success());
                return;
            }
            None => {
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "`sheep watch` outside herdr must refuse immediately, not sit in the reconnect loop"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
