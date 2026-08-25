//! What the recorder needs from a herdr session, and the live implementation.
//!
//! Everything the recorder asks of herdr goes through [`Session`]. That is one
//! trait rather than direct [`crate::herdr::wire`] calls so the recorder can be
//! driven end to end in a test — against a real temporary git worktree — with
//! no server anywhere.

use super::detect::{Sighting, Status};
use super::wire;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// The pane's foreground process group, as herdr sees it.
///
/// This is the corroboration that herdr's own status cannot provide: herdr
/// infers `done` from what the pane painted, the kernel knows what is actually
/// running in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processes {
    pub shell_pid: u32,
    pub leader: u32,
    /// `(pid, name)`, in whatever order herdr reported them.
    pub running: Vec<(u32, String)>,
}

/// Shells that being *the* foreground process means no agent is running.
const SHELLS: [&str; 12] =
    ["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "pwsh", "login", "elvish"];

impl Processes {
    /// True when something other than the pane's shell holds the foreground.
    ///
    /// A pane whose foreground process group *is* its shell has no agent in it:
    /// whatever herdr last said about that pane describes a program that has
    /// since exited, and a status like that is not evidence of a finished turn.
    pub fn agent_is_running(&self) -> bool {
        self.leader != self.shell_pid && self.running.iter().any(|(_, name)| !is_shell(name))
    }

    /// The set of pids, for comparing one look at the pane against another.
    pub fn pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self.running.iter().map(|(pid, _)| *pid).collect();
        pids.sort_unstable();
        pids
    }

    /// The name of whatever leads the foreground process group, for the log.
    pub fn leader_name(&self) -> &str {
        self.running
            .iter()
            .find(|(pid, _)| *pid == self.leader)
            .map(|(_, name)| name.as_str())
            .unwrap_or("?")
    }
}

fn is_shell(name: &str) -> bool {
    let stem = name.trim_start_matches('-');
    let stem = stem.strip_suffix(".exe").unwrap_or(stem);
    SHELLS.contains(&stem)
}

/// The half of herdr's API the recorder uses.
pub trait Session {
    /// Every pane that currently has an agent. Used to seed and to re-sync
    /// after a reconnect.
    fn agents(&self) -> Result<Vec<Sighting>>;
    /// One pane, freshly read. `None` when the pane is gone.
    fn pane(&self, pane_id: &str) -> Result<Option<Sighting>>;
    /// The pane's foreground process group. `None` when the pane is gone.
    fn processes(&self, pane_id: &str) -> Result<Option<Processes>>;
    /// The visible screen, ANSI stripped. `None` when the pane is gone.
    fn screen(&self, pane_id: &str, lines: u32) -> Result<Option<String>>;
    /// Tell herdr which turn this pane is on, so it can show it.
    fn report_turn(&self, pane_id: &str, seq: u64, ttl: Duration) -> Result<()>;
}

/// Error codes that genuinely mean "the thing you asked about is not there".
///
/// Verified against herdr 0.8.0 (protocol 19) by asking about a pane that does
/// not exist: `pane.get`, `pane.process_info`, `pane.read` and
/// `pane.report_metadata` all answer `pane_not_found`, and `agent.get` answers
/// `agent_not_found`.
///
/// Nothing else may be read as absence. A herdr build without a method, a
/// params mistake and a transient internal failure all arrive as an
/// `ApiError` too — `invalid_request` in the first two cases — and swallowing
/// those as "the pane is gone" is how a recorder stops recording for the rest
/// of the day while its log still says everything is fine.
const NOT_FOUND: [&str; 2] = ["pane_not_found", "agent_not_found"];

/// The live session on `$HERDR_SOCKET_PATH`.
pub struct Live;

impl Live {
    /// `None` when herdr says the thing does not exist; the error otherwise.
    ///
    /// Panes close while the recorder is mid-question, and that is not a
    /// fault. Every other failure is one, and has to reach the caller so it
    /// can wait and say so rather than quietly deciding there was no turn.
    fn optional(result: Result<Value>) -> Result<Option<Value>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(err) => match err.downcast_ref::<wire::ApiError>() {
                Some(api) if NOT_FOUND.contains(&api.code.as_str()) => Ok(None),
                _ => Err(err),
            },
        }
    }
}

impl Session for Live {
    fn agents(&self) -> Result<Vec<Sighting>> {
        let result = wire::request("agent.list", json!({}))?;
        Ok(result
            .get("agents")
            .and_then(Value::as_array)
            .map(|agents| agents.iter().filter_map(sighting).collect())
            .unwrap_or_default())
    }

    fn pane(&self, pane_id: &str) -> Result<Option<Sighting>> {
        let Some(result) =
            Live::optional(wire::request("pane.get", json!({ "pane_id": pane_id })))?
        else {
            return Ok(None);
        };
        Ok(result.get("pane").and_then(sighting))
    }

    fn processes(&self, pane_id: &str) -> Result<Option<Processes>> {
        let Some(result) =
            Live::optional(wire::request("pane.process_info", json!({ "pane_id": pane_id })))?
        else {
            return Ok(None);
        };
        Ok(result.get("process_info").and_then(processes))
    }

    fn screen(&self, pane_id: &str, lines: u32) -> Result<Option<String>> {
        let params = json!({
            "pane_id": pane_id,
            "source": "visible",
            "lines": lines,
            "strip_ansi": true,
        });
        let Some(result) = Live::optional(wire::request("pane.read", params))? else {
            return Ok(None);
        };
        Ok(result
            .get("read")
            .and_then(|read| read.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    fn report_turn(&self, pane_id: &str, seq: u64, ttl: Duration) -> Result<()> {
        let params = json!({
            "pane_id": pane_id,
            "source": "sheep",
            "tokens": { "turn": format!("#{seq}") },
            "ttl_ms": ttl.as_millis().clamp(1, 86_400_000) as u64,
        });
        wire::request("pane.report_metadata", params).map(|_| ())
    }
}

/// Read a `PaneInfo`-shaped object into a sighting.
///
/// Panes with no agent are not sightings: Sheep records agent turns, and a
/// plain shell pane has none. Skipping them here keeps the detector's map to
/// the panes that can ever produce a boundary.
pub fn sighting(pane: &Value) -> Option<Sighting> {
    let pane_id = pane.get("pane_id").and_then(Value::as_str)?.to_string();
    let agent = pane.get("agent").and_then(Value::as_str).map(str::to_string);
    agent.as_ref()?;
    Some(Sighting {
        pane_id,
        agent,
        cwd: pane
            .get("cwd")
            .and_then(Value::as_str)
            .or_else(|| pane.get("foreground_cwd").and_then(Value::as_str))
            .map(str::to_string),
        status: pane
            .get("agent_status")
            .and_then(Value::as_str)
            .map(Status::parse)
            .unwrap_or(Status::Unknown),
        revision: pane.get("revision").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn processes(info: &Value) -> Option<Processes> {
    let running = info
        .get("foreground_processes")
        .and_then(Value::as_array)
        .map(|procs| {
            procs
                .iter()
                .filter_map(|p| {
                    let pid = p.get("pid").and_then(Value::as_u64)? as u32;
                    let name = p
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| p.get("argv0").and_then(Value::as_str))
                        .unwrap_or("?")
                        .to_string();
                    Some((pid, name))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Processes {
        shell_pid: info.get("shell_pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        leader: info.get("foreground_process_group_id").and_then(Value::as_u64).unwrap_or(0) as u32,
        running,
    })
}
