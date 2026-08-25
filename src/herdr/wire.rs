//! The herdr socket protocol.
//!
//! Newline-delimited JSON over a unix socket at `$HERDR_SOCKET_PATH`, with two
//! shapes that behave very differently:
//!
//! * **A request** is one line in, one line out, and then the server closes the
//!   connection. There is no connection reuse — a second request on the same
//!   socket gets a broken pipe. [`request`] therefore connects per call, which
//!   is cheap for a unix socket and removes a whole class of state bugs.
//! * **A subscription** starts with `{"result":{"type":"subscription_started"}}`
//!   and then streams event lines for as long as the connection is held open.
//!
//! Verified against herdr 0.8.0 (protocol 19).
//!
//! One asymmetry worth knowing: subscription *requests* name events with dots
//! (`pane.agent_status_changed`), while the events that come back name
//! themselves with underscores (`pane_agent_status_changed`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// How long a single request may take before we give up on the server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Path to the running session's API socket, if we are inside one.
pub fn socket_path() -> Option<PathBuf> {
    match std::env::var("HERDR_SOCKET_PATH") {
        Ok(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => None,
    }
}

/// Whether this process is running inside a herdr pane.
pub fn inside_herdr() -> bool {
    std::env::var("HERDR_ENV").as_deref() == Ok("1") && socket_path().is_some()
}

/// Path to the herdr binary herdr itself told us to use. Preferred over `herdr`
/// on `PATH`, which may be a different version.
pub fn herdr_bin() -> PathBuf {
    match std::env::var("HERDR_BIN_PATH") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from("herdr"),
    }
}

/// An error the server itself reported, as opposed to a transport failure.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr api {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(unix)]
fn connect() -> Result<UnixStream> {
    let path = socket_path()
        .ok_or_else(|| anyhow!("HERDR_SOCKET_PATH is not set: Sheep is not running inside herdr"))?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("cannot reach the herdr session at {}", path.display()))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(stream)
}

#[cfg(not(unix))]
fn connect() -> Result<std::net::TcpStream> {
    bail!("the herdr socket API is not wired up on this platform yet")
}

/// Read one line and parse it as JSON. `None` at end of stream.
fn read_json(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(Some(Value::Null));
    }
    Ok(Some(serde_json::from_str(trimmed).with_context(|| {
        format!("herdr sent something that is not JSON: {}", &trimmed[..trimmed.len().min(200)])
    })?))
}

/// Turn an envelope into either its `result` payload or a typed error.
fn unwrap_envelope(envelope: Value) -> Result<Value> {
    if let Some(error) = envelope.get("error") {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let message =
            error.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
        return Err(ApiError { code, message }.into());
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("herdr replied without a result: {envelope}"))
}

/// Send one request and return its `result`.
///
/// Connects, writes, reads one line, and drops the connection — which is what
/// the server expects, since it closes the socket after replying.
pub fn request(method: &str, params: Value) -> Result<Value> {
    let mut stream = connect()?;
    let body = json!({ "id": format!("sheep:{method}"), "method": method, "params": params });
    let mut line = serde_json::to_string(&body)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).context("cannot send a request to herdr")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    match read_json(&mut reader)? {
        Some(envelope) => unwrap_envelope(envelope),
        None => bail!("herdr closed the connection without replying to `{method}`"),
    }
}

/// Like [`request`], but `Ok(None)` when we are simply not inside herdr.
///
/// Callers that are useful outside a herdr session — the TUI, for instance,
/// which still works over a plain terminal — use this so that "no session" is a
/// state rather than an error.
pub fn try_request(method: &str, params: Value) -> Result<Option<Value>> {
    if !inside_herdr() {
        return Ok(None);
    }
    request(method, params).map(Some)
}

/// One event pushed by a subscription.
#[derive(Debug, Clone)]
pub struct Event {
    /// Underscore-separated event name, e.g. `pane_agent_status_changed`.
    pub kind: String,
    pub data: Value,
}

impl Event {
    pub fn pane_id(&self) -> Option<&str> {
        self.data.get("pane_id").and_then(Value::as_str)
    }
    pub fn workspace_id(&self) -> Option<&str> {
        self.data.get("workspace_id").and_then(Value::as_str)
    }
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }
}

/// A held-open connection streaming events.
///
/// Dropping it unsubscribes. There is no read timeout: a quiet session is
/// normal, and treating silence as failure would make the recorder restart
/// itself all day.
#[derive(Debug)]
pub struct Subscription {
    #[cfg(unix)]
    reader: BufReader<UnixStream>,
    #[cfg(not(unix))]
    reader: BufReader<std::net::TcpStream>,
}

impl Subscription {
    /// Subscribe to `topics`, each a `Subscription` object from herdr's schema —
    /// `{"type":"pane.created"}` or `{"type":"pane.agent_status_changed","pane_id":"w1:p2"}`.
    ///
    /// Note there is no session-wide agent-status topic: `pane.agent_status_changed`
    /// requires a `pane_id`. That sounds like it forces a watcher to subscribe
    /// per pane and re-subscribe as panes appear — it does not. `pane.updated`
    /// takes no parameters and carries the whole `PaneInfo`, status included,
    /// so one subscription covers panes that do not exist yet, with no
    /// re-subscription churn and no window in which events are missed. Reach
    /// for the per-pane topic only when you want one specific pane and nothing
    /// else.
    pub fn open(topics: &[Value]) -> Result<Self> {
        let stream = connect()?;
        // A subscription is long-lived and mostly silent; a read timeout here
        // would fire on every idle stretch.
        stream.set_read_timeout(None)?;

        let mut writer = stream.try_clone().context("cannot duplicate the herdr socket")?;
        let body = json!({
            "id": "sheep:events.subscribe",
            "method": "events.subscribe",
            "params": { "subscriptions": topics },
        });
        let mut line = serde_json::to_string(&body)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).context("cannot open a herdr subscription")?;
        writer.flush()?;

        let mut reader = BufReader::new(stream);
        let ack = read_json(&mut reader)?
            .ok_or_else(|| anyhow!("herdr closed the connection instead of acknowledging"))?;
        let result = unwrap_envelope(ack)?;
        let kind = result.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind != "subscription_started" {
            bail!("herdr answered a subscription with `{kind}` instead of subscription_started");
        }
        Ok(Self { reader })
    }

    /// Block until the next event. `None` when herdr closed the stream, which
    /// is how a session shutdown or a live handoff shows up.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            let Some(value) = read_json(&mut self.reader)? else { return Ok(None) };
            if value.is_null() {
                continue;
            }
            if let Some(error) = value.get("error") {
                bail!("herdr ended the subscription: {error}");
            }
            let Some(kind) = value.get("event").and_then(Value::as_str) else { continue };
            return Ok(Some(Event {
                kind: kind.to_string(),
                data: value.get("data").cloned().unwrap_or(Value::Null),
            }));
        }
    }
}

/// Build a `pane.agent_status_changed` topic for one pane.
pub fn topic_agent_status(pane_id: &str) -> Value {
    json!({ "type": "pane.agent_status_changed", "pane_id": pane_id })
}

/// Build a topic that carries no parameters.
pub fn topic(kind: &str) -> Value {
    json!({ "type": kind })
}
