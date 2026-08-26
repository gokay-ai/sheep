//! The state directory's advisory lock.
//!
//! Sheep guarantees a second writer: `sheep watch` stays alive for a whole
//! session, so every `sheep snap`, `sheep restore` and `sheep gc` a user runs
//! lands beside a recorder that may append at any moment. Without a lock
//! `ops::collect` is an unsynchronised read-modify-write of an append-only log
//! — read the turns, spend seconds rebuilding them, rename the rebuilt file
//! over the live one, then prune every object no ref reaches — and anything
//! appended inside that window is dropped from the log and its objects
//! collected. A restore that reported "previous state kept as turn #501" loses
//! #501 the same way.
//!
//! ## Why one lock per worktree, and not per timeline
//!
//! A timeline owns a file in the turn log and a ref in the shadow repository,
//! so a lock per `(worktree, timeline)` looks like the natural grain. It is not
//! enough. All of a worktree's timelines share **one** shadow repository, and
//! the last thing `collect` does is `gc --prune=now` on it, which deletes every
//! object no ref reaches — including the tree another timeline wrote a
//! millisecond ago and has not yet pointed a commit at. Snapshotting is
//! `write-tree` → `diffstat` → `commit-tree` → `update-ref`, and until that last
//! step the new objects are reachable from nothing at all. So the unit that has
//! to be exclusive is the unit `gc` operates on: the worktree.
//!
//! The cost is that two agents recording into one checkout serialise their
//! snapshots. A snapshot is a few hundred milliseconds, herdr's model is a
//! worktree per agent, and the alternative is losing turns, so this is the
//! trade we want.
//!
//! ## Shape
//!
//! `create_new` on a file under `<state>/locks/`, which is atomic on every
//! filesystem Sheep targets and needs no C dependency. The holder writes a
//! token identifying itself and re-stamps the file every [`BEAT`]; a lock whose
//! file has not been stamped within [`STALE_AFTER`] — in *either* direction, so
//! that a clock which ran backwards cannot leave a file no heartbeat will ever
//! catch up with — is debris left by a killed process, and is broken by whoever
//! notices, atomically, with a rename. A killed recorder therefore wedges
//! nothing for longer than half a minute, and a live holder cannot have its
//! lock taken while it is still beating — on clocks that agree to within that
//! window, which two processes on one machine sharing one state directory do.
//!
//! Nothing here ever blocks for ever, and that is a property of the loop rather
//! than of the happy path: every way round it reaches the deadline check, so a
//! rename that cannot succeed ends in [`Busy`] like any other contention rather
//! than in a spinning core.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a lock file may sit untouched before it counts as debris.
///
/// The holder re-stamps it six times inside this window, so reaching it means
/// the process is gone, stopped, or the machine slept. Short enough that a
/// killed `sheep watch` does not cost a user a minute of turns.
pub const STALE_AFTER: Duration = Duration::from_secs(30);

/// How often a holder re-stamps its lock file.
const BEAT: Duration = Duration::from_secs(5);

/// How often a waiter re-tries. Small enough that handing the lock over is not
/// noticeable next to a snapshot, large enough not to spin.
const POLL: Duration = Duration::from_millis(15);

/// The lock is held by someone else and did not come free in time.
///
/// Deliberately its own type: a recorder that must not stall a session treats
/// this as "skip this turn", while `sheep restore` treats it as "wait, then
/// tell the user what is going on". Both need to tell it apart from a real
/// failure.
#[derive(Debug)]
pub struct Busy {
    pub path: PathBuf,
    /// Whatever the holder wrote about itself, for the message.
    pub holder: String,
    pub waited: Duration,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another Sheep process is writing this worktree's history ({}) and did not finish within {:.1}s. Nothing was changed; try again.",
            self.holder,
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for Busy {}

/// An exclusive claim on one worktree's state. Released on drop.
#[derive(Debug)]
pub struct Guard {
    path: PathBuf,
    token: String,
    /// Dropping this stops the heartbeat thread.
    stop: Option<Sender<()>>,
    beat: Option<JoinHandle<()>>,
}

impl Guard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Stop beating before removing the file: a beat that landed afterwards
        // would recreate a lock nobody holds.
        drop(self.stop.take());
        if let Some(beat) = self.beat.take() {
            let _ = beat.join();
        }
        // Only if it is still ours. If our lock was broken as stale and someone
        // else now holds the file, removing it would hand a third process a
        // lock two of them think they have.
        if holder(&self.path).as_deref() == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Take the lock for `worktree_id`, waiting up to `wait` for it.
///
/// Fails with [`Busy`] rather than blocking for ever: every caller has
/// something better to do than hang, and the recorder must never stall a
/// session because a `gc` is running.
pub fn hold(state: &Path, worktree_id: &str, wait: Duration) -> Result<Guard> {
    let dir = state.join("locks");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{worktree_id}.lock"));
    let token = token();
    let started = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(token.as_bytes())
                    .and_then(|()| file.flush())
                    .with_context(|| format!("cannot write {}", path.display()))?;
                drop(file);
                let (stop, stopped) = mpsc::channel();
                let beat = std::thread::spawn({
                    let path = path.clone();
                    let token = token.clone();
                    move || heartbeat(path, token, stopped)
                });
                return Ok(Guard { path, token, stop: Some(stop), beat: Some(beat) });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot create {}", path.display()));
            }
        }

        // Held. Debris from a process that died still looks exactly like this,
        // and the only thing that tells them apart is whether anyone is still
        // stamping the file.
        let freed = is_stale(&path) && break_stale(&path, &token);

        // Every path through this loop reaches here, and this is the reason
        // `break_stale` reports whether it worked. Skipping the deadline to go
        // round again is only safe if something actually changed: a rename that
        // cannot succeed — a read-only state directory, a file somebody made
        // immutable — would otherwise spin a core until the process is killed.
        // That is worse than the stall the whole design gives up a turn to
        // avoid, and the module says plainly that it cannot happen.
        let waited = started.elapsed();
        if waited >= wait {
            return Err(Busy {
                holder: holder(&path).unwrap_or_else(|| "unidentified".into()),
                path,
                waited,
            }
            .into());
        }
        if !freed {
            std::thread::sleep(POLL.min(wait.saturating_sub(waited)));
        }
    }
}

/// Re-stamp the lock file until the guard goes away, so a live holder is never
/// mistaken for debris however long its work takes.
fn heartbeat(path: PathBuf, token: String, stopped: Receiver<()>) {
    loop {
        match stopped.recv_timeout(BEAT) {
            Err(RecvTimeoutError::Timeout) => {}
            // Either the guard was dropped or it went away with its thread.
            _ => return,
        }
        // If the file is no longer ours — broken as stale while this process
        // was frozen — stamping it would extend somebody else's lease.
        if holder(&path).as_deref() != Some(token.as_str()) {
            return;
        }
        let _ = OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| f.write_all(token.as_bytes()).and_then(|()| f.flush()));
    }
}

/// Remove a lock file nobody is stamping any more. Reports whether it went.
///
/// Through a rename, because two waiters can notice the same debris at the same
/// moment: the rename picks exactly one winner, and the loser simply goes round
/// again and finds the lock free — rather than both deleting, both creating,
/// and both believing they hold it.
///
/// The answer matters to the caller. `false` means nothing changed, and going
/// round again without waiting would be a spin.
fn break_stale(path: &Path, token: &str) -> bool {
    let debris = path.with_extension(format!("stale-{token}"));
    if std::fs::rename(path, &debris).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&debris);
    true
}

/// Whether the lock file has gone long enough without a stamp to be debris.
///
/// Both directions count. A stamp far in the *future* is not evidence of a live
/// holder — a heartbeat cannot reach it, so the file would never age out and
/// the worktree could never be recorded into again. A backwards clock step from
/// NTP, a resumed VM, or a state directory on a filesystem whose clock runs
/// ahead all produce exactly that, and only a human deleting the file by hand
/// would recover it. So a lead of more than [`STALE_AFTER`] is treated the same
/// as a lag of more than [`STALE_AFTER`].
///
/// A small lead is left alone: two clocks a second or two apart is ordinary,
/// and a holder stamping every [`BEAT`] under such a clock is alive.
fn is_stale(path: &Path) -> bool {
    let Some(modified) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) else {
        // Gone between the failed create and now. Not debris — just go round.
        return false;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age > STALE_AFTER,
        Err(ahead) => ahead.duration() > STALE_AFTER,
    }
}

fn holder(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// Identifies one holder. The pid is what a user wants to see in a message; the
/// counter and the clock are what keep two holders in one process apart.
fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("pid {} · {}·{}", std::process::id(), nanos, NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn a_second_holder_is_refused_while_the_first_is_alive() {
        let state = dir();
        let held = hold(state.path(), "w", Duration::from_millis(50)).expect("first holder");
        let err = hold(state.path(), "w", Duration::from_millis(80))
            .expect_err("the lock must not be handed out twice");
        assert!(err.downcast_ref::<Busy>().is_some(), "contention must be its own error: {err:#}");
        drop(held);
        hold(state.path(), "w", Duration::from_millis(50)).expect("released on drop");
    }

    #[test]
    fn two_worktrees_do_not_block_each_other() {
        let state = dir();
        let _a = hold(state.path(), "a", Duration::from_millis(50)).expect("a");
        let _b = hold(state.path(), "b", Duration::from_millis(50)).expect("b");
    }

    #[test]
    fn a_waiter_gets_the_lock_as_soon_as_it_is_released() {
        let state = dir();
        let held = hold(state.path(), "w", Duration::from_millis(50)).expect("first holder");
        let path = state.path().to_path_buf();
        let waiter = std::thread::spawn(move || hold(&path, "w", Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(60));
        drop(held);
        waiter.join().expect("waiter thread").expect("the lock must come free");
    }

    #[test]
    fn debris_from_a_killed_process_does_not_wedge_the_lock_for_ever() {
        // A lock file nobody is stamping is exactly what a killed `sheep watch`
        // leaves behind. Forged here by writing the file directly, which is
        // what that process left: a file with no heartbeat behind it.
        let state = dir();
        let locks = state.path().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join("w.lock");
        std::fs::write(&path, "pid 999999 · long gone").unwrap();
        let old = SystemTime::now() - (STALE_AFTER + Duration::from_secs(5));
        filetime(&path, old);

        let taken = hold(state.path(), "w", Duration::from_millis(50))
            .expect("a lock nobody holds must be breakable");
        assert_eq!(taken.path(), path);
    }

    #[test]
    fn a_lock_that_is_still_being_stamped_is_not_stale() {
        let state = dir();
        let _held = hold(state.path(), "w", Duration::from_millis(50)).expect("first holder");
        // Backdate it past the threshold, then let one heartbeat land.
        let path = state.path().join("locks").join("w.lock");
        filetime(&path, SystemTime::now() - (STALE_AFTER + Duration::from_secs(5)));
        std::thread::sleep(BEAT + Duration::from_millis(500));
        assert!(!is_stale(&path), "a live holder must keep its lock fresh");
        hold(state.path(), "w", Duration::from_millis(50))
            .expect_err("a lock being stamped must not be broken");
    }

    #[test]
    fn a_lock_stamped_in_the_future_is_still_breakable() {
        // A backwards clock step — an NTP correction, a resumed VM — leaves
        // debris whose timestamp no heartbeat can ever reach. Reading that as
        // "somebody is holding it" wedges the worktree for good: nothing
        // records into it again until a human works out which file to delete,
        // and nothing tells them that is what happened.
        let state = dir();
        let locks = state.path().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join("w.lock");
        std::fs::write(&path, "pid 999999 · from a clock that ran ahead").unwrap();
        filetime(&path, SystemTime::now() + Duration::from_secs(3600));

        let taken = hold(state.path(), "w", Duration::from_millis(300))
            .expect("a lock stamped in the future must not wedge the worktree for ever");
        assert_eq!(taken.path(), path);
    }

    #[test]
    fn a_clock_a_second_or_two_ahead_is_not_a_dead_holder() {
        // The other side of it. Two clocks rarely agree exactly, and a holder
        // stamping every `BEAT` under a slightly fast one is alive — breaking
        // its lock would put two writers in the state directory, which is the
        // thing this module exists to prevent.
        let state = dir();
        let locks = state.path().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join("w.lock");
        std::fs::write(&path, "pid 4242 · alive, on a slightly fast clock").unwrap();
        filetime(&path, SystemTime::now() + Duration::from_secs(2));

        assert!(!is_stale(&path), "a small lead is a live holder, not debris");
        hold(state.path(), "w", Duration::from_millis(80))
            .expect_err("a live holder's lock must not be taken");
    }

    #[test]
    #[cfg(unix)]
    fn a_lock_that_cannot_be_broken_gives_up_instead_of_spinning() {
        // Debris that is there but cannot be removed: a read-only state
        // directory, a file someone made immutable, a filesystem that has gone
        // read-only under us. Going round again without honouring the deadline
        // turns "wait five seconds and skip the turn" into a core spinning
        // until the process is killed — worse than the stall the whole design
        // gives up a turn to avoid.
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;

        let state = dir();
        let locks = state.path().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join("w.lock");
        std::fs::write(&path, "pid 999999 · long gone").unwrap();
        filetime(&path, SystemTime::now() - (STALE_AFTER + Duration::from_secs(5)));

        // A directory nothing may be renamed within.
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o555)).unwrap();
        let restore = |mode| {
            let _ = std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(mode));
        };
        if std::fs::rename(&path, locks.join("probe")).is_ok() {
            // Running as root, where permissions do not bind. Nothing to test.
            let _ = std::fs::rename(locks.join("probe"), &path);
            restore(0o755);
            return;
        }

        // On another thread, so that a `hold` which never returns fails this
        // test with a message rather than hanging the suite.
        let (tx, rx) = mpsc::channel();
        let dir_path = state.path().to_path_buf();
        std::thread::spawn(move || {
            let outcome = hold(&dir_path, "w", Duration::from_millis(200));
            let _ = tx.send(outcome.err().map(|e| e.downcast::<Busy>().is_ok()));
        });

        let answer = rx.recv_timeout(Duration::from_secs(5));
        // Before any assertion: a stuck thread is still spinning on this
        // directory, and restoring it lets it finish and the tempdir clean up.
        restore(0o755);
        match answer {
            Ok(Some(true)) => {}
            Ok(Some(false)) => panic!("contention must be reported as `Busy`"),
            Ok(None) => panic!("the lock must not have been handed out"),
            Err(_) => panic!(
                "hold() never returned: it is spinning on a rename it cannot do instead of honouring its deadline"
            ),
        }
    }

    /// Backdate a file's modification time. No `filetime` crate and no C
    /// toolchain: `touch -t` is on every platform's path, and this is test-only.
    fn filetime(path: &Path, when: SystemTime) {
        let secs = when.duration_since(UNIX_EPOCH).unwrap().as_secs();
        // `touch -d @<epoch>` is GNU; `-t` is portable but wants local time, so
        // go through the one tool that speaks epoch seconds everywhere.
        let stamp = std::process::Command::new("date")
            .args(["-r", &secs.to_string(), "+%Y%m%d%H%M.%S"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| {
                let out = std::process::Command::new("date")
                    .args(["-d", &format!("@{secs}"), "+%Y%m%d%H%M.%S"])
                    .output()
                    .expect("date should run");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            });
        let out = std::process::Command::new("touch")
            .args(["-t", &stamp])
            .arg(path)
            .output()
            .expect("touch should run");
        assert!(out.status.success(), "touch -t {stamp}: {:?}", out);
    }
}
