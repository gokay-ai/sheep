//! The reconnect policy.
//!
//! Kept apart from the loop it drives, because the interesting cases are the
//! ones that are tedious to reach for real: a herdr that accepts a
//! subscription and drops it immediately, a socket that vanishes during a live
//! handoff and comes back, a session that has genuinely ended. All of those are
//! decisions about two pieces of state, and none of them should need a minute
//! of wall-clock to test.

use std::time::{Duration, Instant};

/// First retry delay, and what a healthy connection resets to.
pub const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Slowest we retry. A herdr restart takes seconds; there is no point
/// hammering the socket in between.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a connection has to last before it counts as having worked.
///
/// This is the whole defence against a reconnect spin. A subscription that is
/// acknowledged and then closed immediately still *opens*, so treating "opened"
/// as success resets the backoff every time and the supervisor retries twice a
/// second for ever — each attempt running a full re-sync against a server that
/// is plainly unwell.
pub const MIN_HEALTHY: Duration = Duration::from_secs(30);

/// How long the socket has to stay missing before we call the session dead.
pub const GONE_AFTER: Duration = Duration::from_secs(60);

/// What to do after one connection attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Wait this long, then try again.
    Retry(Duration),
    /// The session really has gone. Exit, and cleanly.
    Stop,
}

#[derive(Debug)]
pub struct Supervisor {
    backoff: Duration,
    /// When the socket was first noticed missing, while it still is.
    missing_since: Option<Instant>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self { backoff: INITIAL_BACKOFF, missing_since: None }
    }

    /// Record how one attempt went and say what to do next.
    ///
    /// `uptime` is how long the connection lasted — zero if it never opened —
    /// and `socket_present` is whether the socket file is on disk right now.
    pub fn after(&mut self, now: Instant, uptime: Duration, socket_present: bool) -> Next {
        if uptime >= MIN_HEALTHY {
            self.backoff = INITIAL_BACKOFF;
        } else {
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        }

        // Presence of the socket is the only thing that moves this. A
        // connection that opened and died says nothing about whether the
        // session is still there, so it must not clear the clock.
        if socket_present {
            self.missing_since = None;
            return Next::Retry(self.backoff);
        }
        let since = *self.missing_since.get_or_insert(now);
        match now.duration_since(since) >= GONE_AFTER {
            true => Next::Stop,
            false => Next::Retry(self.backoff),
        }
    }

    /// The delay the next retry would use. For the log.
    pub fn backoff(&self) -> Duration {
        self.backoff
    }
}
