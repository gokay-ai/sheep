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

pub mod cli;
pub mod wire;

pub use cli::WatchArgs;
pub use wire::{inside_herdr, request, try_request, ApiError, Event, Subscription};
