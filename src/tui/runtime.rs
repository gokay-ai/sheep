//! The event loop, with its three edges behind traits.
//!
//! Draw, read a key, pump the worker. Nothing here decides anything — that is
//! [`crate::tui::app`] — but two of the guarantees the interface makes live
//! *only* in this loop and nowhere else:
//!
//! * whatever was typed while a restore was running is thrown away when it
//!   ends, because those keys were aimed at a screen that no longer exists;
//! * the process does not leave while a restore is in flight, however it came
//!   to be leaving.
//!
//! Both of those used to sit inline against crossterm and a live terminal,
//! which meant the only way to check them was by hand. The traits exist so a
//! test can drive the loop with a scripted keyboard, a screen that fails on
//! demand and a worker that answers when it is told to.

use crate::tui::app::{App, Key};
use crate::tui::engine::{Job, Reply, Worker};
use anyhow::Result;
use std::time::Duration;

/// How long a frame waits for a key before it redraws anyway. Also the
/// spinner's clock, so it has to stay well under a second.
pub const FRAME: Duration = Duration::from_millis(120);

/// How long the loop will wait for an in-flight restore before leaving without
/// it. A restore is a second of `git`; a minute means something is wrong, and
/// trapping someone in a window forever is not the answer to that.
pub const FINISH_GRACE_SECS: u64 = 60;

/// Where keystrokes come from.
pub trait Input {
    /// Wait up to `timeout` for a key. `Ok(None)` when nothing arrived, or when
    /// what arrived was not a key the interface has a meaning for.
    fn wait(&mut self, timeout: Duration) -> Result<Option<Key>>;
    /// Discard everything already buffered, without blocking. Returns how many
    /// events were dropped.
    fn drain(&mut self) -> Result<usize>;
    /// Let `timeout` pass without reading anything. Used while leaving, when
    /// keys are no longer wanted but the loop still has to be paced.
    fn pause(&mut self, timeout: Duration);
}

/// Where frames go.
pub trait Screen {
    fn render(&mut self, app: &App) -> Result<()>;
}

/// What one poll of the worker found.
///
/// `Gone` is why this is not an `Option<Reply>`. A worker whose thread has
/// ended and a worker that is simply busy look identical through a
/// `try_recv().ok()`, and the difference matters: a restore that will never be
/// answered would otherwise leave the interface drawing "restoring…" for ever,
/// and quitting would cost the full grace period and then report a worktree
/// that may be half-restored — a false alarm if the thread died in `plan`,
/// before the checkpoint and before a byte was written.
pub enum Pumped {
    Reply(Reply),
    /// Nothing yet. The worker is alive.
    Idle,
    /// The worker ended. Nothing will ever answer.
    Gone,
}

/// The worker, or whatever a test puts in its place.
pub trait Jobs {
    fn send(&self, job: Job);
    fn poll(&self) -> Pumped;
}

/// A worker is optional: the interface still runs, and still quits cleanly,
/// when there is no worktree to give it one. That case is `Idle` rather than
/// `Gone` — nothing was ever expected to answer, so nothing is missing.
impl Jobs for Option<Worker> {
    fn send(&self, job: Job) {
        if let Some(worker) = self {
            worker.send(job);
        }
    }
    fn poll(&self) -> Pumped {
        let Some(worker) = self else { return Pumped::Idle };
        match worker.replies.try_recv() {
            Ok(reply) => Pumped::Reply(reply),
            Err(std::sync::mpsc::TryRecvError::Empty) => Pumped::Idle,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Pumped::Gone,
        }
    }
}

/// Run until the user quits, or until the terminal stops working — and in
/// either case, not until any restore in flight has finished.
///
/// `clock` is unix seconds; it is the same one the app ticks on, and it is a
/// parameter so the grace period is reachable from a test without waiting a
/// minute.
pub fn run(
    app: &mut App,
    screen: &mut impl Screen,
    input: &mut impl Input,
    jobs: &impl Jobs,
    clock: &dyn Fn() -> u64,
) -> Result<()> {
    // A terminal that stops accepting frames is a reason to leave. It is not a
    // reason to abandon a write half-done, so the error is carried to the exit
    // rather than returned from the middle of one.
    let mut failure: Option<anyhow::Error> = None;
    // A worker that ended is a different kind of trouble: the screen still
    // works, so the reason stays up and the user leaves when they have read it.
    let mut lost: Option<anyhow::Error> = None;
    let mut leaving_since: Option<u64> = None;

    loop {
        if failure.is_none() {
            if let Err(e) = screen.render(app) {
                failure = Some(e);
            }
        }

        if app.quit || failure.is_some() {
            // Once we are on the way out, stop reading keys entirely: the only
            // thing left to do is let the worker finish.
            input.pause(FRAME);
        } else {
            match input.wait(FRAME) {
                Ok(Some(key)) => app.on_key(key),
                Ok(None) => {}
                Err(e) => failure = Some(e),
            }
        }

        let was_restoring = app.restoring;
        loop {
            match jobs.poll() {
                Pumped::Reply(reply) => app.apply(reply),
                Pumped::Idle => break,
                Pumped::Gone => {
                    if lost.is_none() {
                        lost = Some(anyhow::anyhow!(app.worker_lost()));
                    }
                    break;
                }
            }
        }
        if lost.is_none() {
            for job in app.take_jobs() {
                jobs.send(job);
            }
        } else {
            app.take_jobs();
        }
        // A restore just ended. Anything typed while it ran is sitting in the
        // terminal's buffer aimed at a screen that no longer exists — a refusal
        // puts a *different* plan up, and a queued `shift+R` would confirm a
        // plan nobody read.
        if was_restoring && !app.restoring {
            if let Err(e) = input.drain() {
                failure.get_or_insert(e);
            }
        }

        app.tick(clock());

        if app.quit || failure.is_some() {
            if !app.restoring {
                return match failure.or(lost) {
                    Some(e) => Err(e),
                    None => Ok(()),
                };
            }
            let since = *leaving_since.get_or_insert_with(clock);
            if clock().saturating_sub(since) > FINISH_GRACE_SECS {
                let note = format!(
                    "left with a restore still running after {FINISH_GRACE_SECS}s; the worktree may be half-restored. Run `sheep log` and `sheep diff` before trusting it."
                );
                return Err(match failure.or(lost) {
                    Some(e) => e.context(note),
                    None => anyhow::anyhow!(note),
                });
            }
        }
    }
}
