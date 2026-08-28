//! `sheep ui` has to say why it cannot run, rather than hanging or dying inside
//! raw mode, when it does not have a terminal.

use std::process::{Command, Stdio};

fn sheep() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sheep"))
}

#[test]
fn ui_without_a_terminal_says_so() {
    let state = tempfile::tempdir().expect("temp state dir");
    let output = sheep()
        .arg("ui")
        .env("SHEEP_STATE_DIR", state.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sheep ui");
    assert!(!output.status.success(), "ui must not enter raw mode without a tty");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("interactive terminal"),
        "a raw-mode error is not a reason a person can act on; stderr was: {err}"
    );
    assert!(err.contains("sheep log") || err.contains("--snapshot"), "{err}");
}
