//! The socket half of the recorder, against a fake herdr.
//!
//! One question decides most of this file: which server answers mean "the thing
//! you asked about is not there", and which mean "something is wrong". Reading
//! the second as the first is how a recorder stops recording for the rest of
//! the day while its log still says everything is fine — the corroboration sees
//! `None`, calls it a pane that has gone, and drops every turn silently.
//!
//! The codes here are not invented. They were read off a live herdr 0.8.0
//! (protocol 19) by asking it about a pane that does not exist, calling a
//! method it does not have, and sending params it will not accept.

#![cfg(unix)]

use serde_json::{json, Value};
use sheep::herdr::recorder::{LiveSource, Pump, Source};
use sheep::herdr::session::{Live, Session};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// A server that answers every connection the same way.
///
/// `wire::request` connects per call, so a fake that serves one connection and
/// stops would make the second question in a corroboration hang rather than
/// fail.
fn always(reply: Value, keep_open: bool) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();

    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let reply = reply.clone();
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 {
                    return;
                }
                let mut line = serde_json::to_string(&reply).unwrap();
                line.push('\n');
                let _ = writer.write_all(line.as_bytes());
                let _ = writer.flush();
                if keep_open {
                    std::thread::sleep(Duration::from_secs(5));
                }
            });
        }
    });

    (dir, path)
}

/// A server that accepts a connection and then says nothing at all.
///
/// This is the shape that used to wedge a watcher for ever: the handshake read
/// had no deadline, so it blocked upstream of every backoff and give-up policy
/// the caller had. It has to surface as an error the supervisor can retry.
fn mute() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(1).filter_map(Result::ok).collect();
        std::thread::sleep(Duration::from_secs(60));
        drop(held);
    });
    (dir, path)
}

/// A server that acknowledges a subscription, streams `events`, then holds the
/// connection open for `linger` before hanging up.
fn streaming_for(events: Vec<Value>, linger: Duration) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();

    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        let _ = reader.read_line(&mut request);

        let mut send = |value: &Value| {
            let mut line = serde_json::to_string(value).unwrap();
            line.push('\n');
            writer.write_all(line.as_bytes()).is_ok() && writer.flush().is_ok()
        };
        send(&json!({"id": "s", "result": {"type": "subscription_started"}}));
        for event in &events {
            if !send(event) {
                return;
            }
        }
        // Held open on purpose. A server that hangs up the instant it has
        // acknowledged is a different case, and one the client cannot even
        // finish setting up the socket for.
        std::thread::sleep(linger);
    });

    (dir, path)
}

fn with_socket<T>(path: &PathBuf, body: impl FnOnce() -> T) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var("HERDR_SOCKET_PATH").ok();
    std::env::set_var("HERDR_SOCKET_PATH", path);
    let out = body();
    match previous {
        Some(value) => std::env::set_var("HERDR_SOCKET_PATH", value),
        None => std::env::remove_var("HERDR_SOCKET_PATH"),
    }
    out
}

fn error(code: &str, message: &str) -> Value {
    json!({ "id": "x", "error": { "code": code, "message": message } })
}

// ------------------------------------------- what absence is, and what is not --

#[test]
fn a_pane_that_does_not_exist_is_absence() {
    let (_dir, path) = always(error("pane_not_found", "pane w9:p9 not found"), false);
    with_socket(&path, || {
        assert!(Live.pane("w9:p9").expect("not-found is an answer, not a fault").is_none());
        assert!(Live.processes("w9:p9").expect("same for process info").is_none());
        assert!(Live.screen("w9:p9", 10).expect("and for reading the screen").is_none());
    });
}

#[test]
fn an_agent_that_does_not_exist_is_absence_too() {
    let (_dir, path) = always(error("agent_not_found", "agent target nope not found"), false);
    with_socket(&path, || {
        assert!(Live.pane("w9:p9").unwrap().is_none());
    });
}

#[test]
fn a_method_herdr_does_not_have_is_a_fault_not_absence() {
    // This is the one that matters. A herdr built without `pane.process_info`,
    // or a params mistake on our side, answers `invalid_request` — and if that
    // reads as "the pane is gone" the recorder drops every turn from then on
    // and says nothing about it.
    let (_dir, path) =
        always(error("invalid_request", "unknown variant `pane.process_info`"), false);
    with_socket(&path, || {
        let err = Live.processes("w1:p1").expect_err("a broken method is not a missing pane");
        assert!(err.to_string().contains("invalid_request"), "and it keeps its code: {err}");

        let err = Live.pane("w1:p1").expect_err("nor is a params mistake");
        assert!(err.to_string().contains("unknown variant"), "and its message: {err}");
    });
}

#[test]
fn an_internal_failure_is_a_fault_too() {
    // Nothing is on an allow-list by accident: any code we have not verified
    // means "gone" has to propagate.
    let (_dir, path) = always(error("internal", "the server fell over"), false);
    with_socket(&path, || {
        assert!(Live.pane("w1:p1").is_err());
        assert!(Live.processes("w1:p1").is_err());
        assert!(Live.report_turn("w1:p1", 7, Duration::from_secs(60)).is_err());
    });
}

// ------------------------------------------------------- reading the answers --

#[test]
fn a_pane_reply_becomes_a_sighting() {
    let reply = json!({"id": "x", "result": {"type": "pane_info", "pane": {
        "pane_id": "w1:p1", "terminal_id": "t", "workspace_id": "w1", "tab_id": "w1:t1",
        "focused": false, "agent": "claude", "agent_status": "done",
        "cwd": "/repo", "revision": 42 }}});
    let (_dir, path) = always(reply, false);

    let sighting = with_socket(&path, || Live.pane("w1:p1").unwrap()).expect("a pane");
    assert_eq!(sighting.pane_id, "w1:p1");
    assert_eq!(sighting.agent.as_deref(), Some("claude"));
    assert_eq!(sighting.cwd.as_deref(), Some("/repo"));
    assert_eq!(sighting.revision, 42);
    assert!(sighting.status.is_rest());
}

#[test]
fn a_pane_whose_agent_herdr_forgot_is_still_a_pane() {
    // It used to come back as `None`, which the corroboration reads as "this
    // pane no longer exists" and answers by dropping the turn. A pane with no
    // agent is a pane; whether that is interesting is the recorder's call, and
    // it can only make it if the difference survives the parse.
    let reply = json!({"id": "x", "result": {"type": "pane_info", "pane": {
        "pane_id": "w1:p9", "terminal_id": "t", "workspace_id": "w1", "tab_id": "w1:t1",
        "focused": false, "agent_status": "unknown", "cwd": "/repo", "revision": 1 }}});
    let (_dir, path) = always(reply, false);

    let sighting = with_socket(&path, || Live.pane("w1:p9").unwrap())
        .expect("a pane herdr answered about is not a pane that is gone");
    assert_eq!(sighting.pane_id, "w1:p9");
    assert_eq!(sighting.agent, None, "and it says so plainly");
}

// ------------------------------------- a reply that arrived but is not right --

#[test]
fn a_reply_without_the_pane_it_promised_is_a_fault() {
    // The other side of the not-found allow-list. A success envelope missing
    // the key it is supposed to carry — a renamed field on a protocol bump —
    // must not read as absence, or every turn is dropped for ever while the log
    // says only that the pane is gone.
    let (_dir, path) = always(json!({"id": "x", "result": {"type": "pane_info"}}), false);
    with_socket(&path, || {
        let err = Live.pane("w1:p1").expect_err("a reply with no pane in it is not an absent pane");
        assert!(err.to_string().contains("without `pane`"), "and it says what was missing: {err}");
    });
}

#[test]
fn a_reply_without_the_process_info_it_promised_is_a_fault() {
    let (_dir, path) = always(json!({"id": "x", "result": {"type": "pane_process_info"}}), false);
    with_socket(&path, || {
        let err = Live.processes("w1:p1").expect_err("a shape we do not understand is a fault");
        assert!(err.to_string().contains("without `process_info`"), "{err}");
    });
}

#[test]
fn a_reply_without_the_screen_it_promised_is_a_fault() {
    let (_dir, path) = always(json!({"id": "x", "result": {"type": "pane_read"}}), false);
    with_socket(&path, || {
        assert!(Live.screen("w1:p1", 10).is_err(), "prompt capture must not paper over a bump");
    });
}

#[test]
fn an_agent_list_that_lists_nothing_at_all_is_a_fault() {
    // The most dangerous of the four. `reconcile` prunes against this list, so
    // reading a malformed reply as "no agents anywhere" forgets every pane in
    // the session at once — and every one of them loses the turn in flight.
    let (_dir, path) = always(json!({"id": "x", "result": {"type": "agent_list"}}), false);
    with_socket(&path, || {
        let err = Live.agents().expect_err("no list is not an empty list");
        assert!(err.to_string().contains("without `agents`"), "{err}");
    });
}

#[test]
fn an_agent_list_that_is_genuinely_empty_is_not() {
    let (_dir, path) =
        always(json!({"id": "x", "result": {"type": "agent_list", "agents": []}}), false);
    assert!(
        with_socket(&path, || Live.agents().expect("an empty session is a fine answer")).is_empty()
    );
}

#[test]
fn a_process_reply_names_the_foreground_group() {
    let reply = json!({"id": "x", "result": {"type": "pane_process_info", "process_info": {
    "pane_id": "w1:p1", "shell_pid": 100, "foreground_process_group_id": 200,
    "foreground_processes": [
        {"pid": 200, "name": "claude.exe", "argv0": "claude"},
        {"pid": 201, "name": "node", "argv0": "node"}
    ]}}});
    let (_dir, path) = always(reply, false);

    let processes = with_socket(&path, || Live.processes("w1:p1").unwrap()).expect("processes");
    assert!(processes.agent_is_running(), "a non-shell leader means an agent is there");
    assert_eq!(processes.pids(), vec![200, 201]);
    assert_eq!(processes.leader_name(), "claude.exe");
}

#[test]
fn a_pane_sitting_at_its_own_shell_has_no_agent_running() {
    let reply = json!({"id": "x", "result": {"type": "pane_process_info", "process_info": {
        "pane_id": "w1:p1", "shell_pid": 100, "foreground_process_group_id": 100,
        "foreground_processes": [{"pid": 100, "name": "zsh", "argv0": "-zsh"}]}}});
    let (_dir, path) = always(reply, false);

    let processes = with_socket(&path, || Live.processes("w1:p1").unwrap()).expect("processes");
    assert!(
        !processes.agent_is_running(),
        "whatever herdr last said, no agent could have finished a turn here"
    );
}

// ------------------------------------------------------------ the event pump --

#[test]
fn the_live_source_hands_events_to_the_loop_and_reports_the_hang_up() {
    // The subscription is read on its own thread so a silent session does not
    // stop a settle window firing on time. This covers that handoff: the
    // acknowledgement, one event across the channel, and the end of the stream
    // arriving as something the supervisor can act on.
    // A short linger: long enough that the client finishes setting the socket
    // up, short enough that the hang-up lands inside the second poll.
    let (_dir, path) = streaming_for(
        vec![json!({
        "event": "pane_updated",
        "data": {"type": "pane_updated", "pane": {
            "pane_id": "w1:p1", "terminal_id": "t", "workspace_id": "w1", "tab_id": "w1:t1",
            "focused": false, "agent": "claude", "agent_status": "working",
            "cwd": "/repo", "revision": 7 }}
        })],
        Duration::from_secs(1),
    );

    with_socket(&path, || {
        let mut source = LiveSource::open().expect("the acknowledgement should be accepted");

        match source.poll(Duration::from_secs(5)) {
            Pump::Event(event) => {
                assert_eq!(event.kind, "pane_updated");
                assert_eq!(event.data["pane"]["agent_status"], "working");
            }
            _ => panic!("the first poll should deliver the event"),
        }

        // The server hangs up after its script. That has to arrive as a closed
        // stream rather than a hang, because it is what a live handoff and a
        // session shutdown both look like.
        assert!(
            matches!(source.poll(Duration::from_secs(5)), Pump::Closed),
            "the end of the stream is a disconnection the supervisor retries"
        );
    });
}

#[test]
fn a_quiet_stream_times_out_rather_than_blocking_the_loop() {
    // A session where nothing is happening is the normal case, and the loop
    // still has settle windows to fire on time.
    let (_dir, path) = streaming_for(Vec::new(), Duration::from_secs(5));
    with_socket(&path, || {
        let mut source = LiveSource::open().expect("acknowledged");
        assert!(matches!(source.poll(Duration::from_millis(200)), Pump::Idle));
    });
}

#[test]
fn an_open_that_is_never_answered_is_a_fault_not_a_wedge() {
    // Slow on purpose: the handshake deadline is what is under test, so this
    // waits for it. Before it existed, a herdr that accepted the connection and
    // then went silent blocked this read with no deadline at all — upstream of
    // the supervisor, so the watcher wedged instead of reconnecting.
    let (_dir, path) = mute();
    let began = std::time::Instant::now();
    with_socket(&path, || {
        assert!(
            LiveSource::open().is_err(),
            "a server that never acknowledges has to surface as an error the supervisor retries"
        );
    });
    assert!(began.elapsed() < Duration::from_secs(30), "and it has to give up, not hang");
}
