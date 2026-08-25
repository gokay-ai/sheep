//! Talking to the herdr session Sheep is running inside.
//!
//! Sheep records a turn when an agent finishes one. Herdr already knows that:
//! it tracks a status per pane (`idle` / `working` / `blocked` / `done` /
//! `unknown`) and publishes state changes over its local socket. This module
//! turns that stream into turn boundaries the recorder can act on.
//!
//! Nothing here decides *what* to record — that is [`crate::ops`]. This layer
//! only answers "did an agent just finish a turn, in which pane, and which
//! working directory does that pane sit in".
//!
//! The split is deliberate: [`wire`] speaks the protocol, [`detect`] is a pure
//! state machine over sightings and time, [`session`] is the narrow slice of
//! herdr's API the recorder needs, and [`recorder`] is the loop that joins them
//! to [`crate::ops::snap`]. Only [`session::Live`] and [`recorder::LiveSource`]
//! ever touch a socket, so everything above them is testable without a server.

pub mod cli;
pub mod detect;
pub mod log;
pub mod prompt;
pub mod recorder;
pub mod session;
pub mod wire;

pub use cli::WatchArgs;
pub use detect::{Detector, Sighting, Signal, Status, Tuning, Verdict, Withdrawn};
pub use recorder::{Config, LineBy, Pump, Recorder, Source};
pub use session::{Processes, Session};
pub use wire::{inside_herdr, request, try_request, ApiError, Event, Subscription};
