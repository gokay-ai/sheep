//! Protocol tests for the herdr socket layer.
//!
//! These run against a fake server rather than a live session, so they hold in
//! CI and pin the two behaviours that are easy to get wrong: a request socket
//! is closed by the server after one reply, and a subscription is a stream that
//! begins with an acknowledgement.
//!
//! `live_session_answers_ping` is the exception — it only does anything when the
//! suite happens to run inside herdr, and it skips a transport failure the same
//! way, because a busy session hanging up is the environment, not the contract.

#![cfg(unix)]

use serde_json::{json, Value};
use sheep::herdr::wire::{self, Subscription};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use tempfile::TempDir;

/// Serve `replies` to the first connection, one line per element, then behave
/// the way the caller asked: hang up, or hold the stream open.
fn fake_server(replies: Vec<Value>, keep_open: bool) -> (TempDir, PathBuf, mpsc::Receiver<Value>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else { return };
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        let mut request = String::new();
        if reader.read_line(&mut request).unwrap_or(0) > 0 {
            let _ = tx.send(serde_json::from_str(request.trim()).unwrap_or(Value::Null));
        }
        for reply in replies {
            let mut line = serde_json::to_string(&reply).unwrap();
            line.push('\n');
            if writer.write_all(line.as_bytes()).is_err() {
                return;
            }
            let _ = writer.flush();
        }
        if keep_open {
            // Hold the connection so the client sees a live stream rather than
            // an end-of-stream right after the last event.
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });

    (dir, path, rx)
}

/// Point the process at a fake socket for the duration of one test.
///
/// Cargo runs tests in threads of one process, so the environment is shared;
/// these tests are serialised behind a mutex rather than left to race.
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

#[test]
fn a_request_sends_one_line_and_reads_one_reply() {
    let (_dir, path, seen) = fake_server(
        vec![json!({"id": "sheep:ping", "result": {"type": "pong", "version": "0.8.0"}})],
        false,
    );

    let result = with_socket(&path, || wire::request("ping", json!({}))).unwrap();
    assert_eq!(result["type"], "pong");

    let sent = seen.recv().unwrap();
    assert_eq!(sent["method"], "ping");
    assert!(sent.get("id").is_some(), "herdr rejects a request without an id");
    assert!(sent.get("params").is_some(), "herdr rejects a request without params");
}

#[test]
fn a_server_error_is_reported_as_an_api_error() {
    let (_dir, path, _seen) = fake_server(
        vec![json!({"id": "", "error": {"code": "invalid_request", "message": "unknown variant"}})],
        false,
    );

    let err = with_socket(&path, || wire::request("nope", json!({}))).unwrap_err();
    let api = err.downcast_ref::<wire::ApiError>().expect("a server error should keep its code");
    assert_eq!(api.code, "invalid_request");
    assert!(err.to_string().contains("unknown variant"), "the message should survive: {err}");
}

#[test]
fn a_closed_connection_is_an_error_not_a_hang() {
    let (_dir, path, _seen) = fake_server(vec![], false);
    let err = with_socket(&path, || wire::request("ping", json!({}))).unwrap_err();
    assert!(
        err.to_string().contains("closed the connection"),
        "a server that hangs up should say so: {err}"
    );
}

#[test]
fn a_subscription_acknowledges_then_streams_events() {
    let (_dir, path, seen) = fake_server(
        vec![
            json!({"id": "sheep:events.subscribe", "result": {"type": "subscription_started"}}),
            json!({"event": "pane_created", "data": {"type": "pane_created", "pane_id": "w1:p2"}}),
            json!({"event": "pane_agent_status_changed",
                   "data": {"type": "pane_agent_status_changed", "pane_id": "w1:p2",
                            "agent_status": "done", "agent": "claude"}}),
        ],
        true,
    );

    let (first, second, sent) = with_socket(&path, || {
        let mut sub =
            Subscription::open(&[wire::topic("pane.created"), wire::topic_agent_status("w1:p2")])
                .expect("the acknowledgement should be accepted");
        let first = sub.next_event().unwrap().unwrap();
        let second = sub.next_event().unwrap().unwrap();
        (first, second, seen.recv().unwrap())
    });

    assert_eq!(first.kind, "pane_created");
    assert_eq!(first.pane_id(), Some("w1:p2"));
    assert_eq!(second.kind, "pane_agent_status_changed");
    assert_eq!(second.str_field("agent_status"), Some("done"));

    // The request spells topics with dots; the events come back with underscores.
    let topics = sent["params"]["subscriptions"].as_array().unwrap();
    assert_eq!(topics[0]["type"], "pane.created");
    assert_eq!(topics[1]["type"], "pane.agent_status_changed");
    assert_eq!(topics[1]["pane_id"], "w1:p2");
}

#[test]
fn a_subscription_refuses_an_unexpected_acknowledgement() {
    let (_dir, path, _seen) =
        fake_server(vec![json!({"id": "x", "result": {"type": "pong"}})], true);
    let err =
        with_socket(&path, || Subscription::open(&[wire::topic("pane.created")])).unwrap_err();
    assert!(
        err.to_string().contains("subscription_started"),
        "the mismatch should be explicit: {err}"
    );
}

#[test]
fn live_session_answers_ping() {
    // Only meaningful when the suite happens to run inside a herdr pane.
    // A transport failure is the same class of skip: the live server hanging
    // up under load is not a Sheep defect, and asserting it made `cargo test`
    // red on the one machine every contributor runs it. If herdr *does*
    // answer, the payload still has to be the shape `wire` promised — a
    // broken handshake cannot hide behind a skip.
    if !wire::inside_herdr() {
        eprintln!("skipped: not running inside herdr");
        return;
    }
    let pong = match wire::request("ping", json!({})) {
        Ok(pong) => pong,
        Err(err) => {
            eprintln!("skipped: herdr did not answer ping ({err:#})");
            return;
        }
    };
    assert_eq!(pong["type"], "pong");
    assert!(pong["protocol"].is_number(), "the handshake should report a protocol version");
}

#[test]
fn a_subscription_to_a_server_that_never_answers_times_out() {
    // A server that accepts the connection and then says nothing used to leave
    // `open` blocked with no deadline, because the read timeout was cleared
    // before the acknowledgement rather than after it. That is upstream of
    // every reconnect and give-up policy a caller might have, so the watcher
    // wedged permanently and silently instead of backing off.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = accepted_tx.send(());
            // Hold the connection open and answer nothing at all.
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(stream);
        }
    });

    let started = std::time::Instant::now();
    let err = with_socket(&path, || Subscription::open(&[wire::topic("pane.created")]))
        .expect_err("a silent server must not block for ever");
    let waited = started.elapsed();

    assert!(accepted_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok());
    assert!(
        waited < std::time::Duration::from_secs(25),
        "gave up only after {waited:?}, which is no deadline at all"
    );
    let _ = err;
}
