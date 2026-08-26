//! sheep — undo for AI coding agents.
//!
//! The crate is split so that the half which touches a user's files can be
//! exercised with no herdr, no terminal and no agent in the picture. Everything
//! in [`ops`] is callable from a test, and the CLI in `main.rs` is a thin shell
//! over it.

pub mod git;
pub mod herdr;
pub mod lock;
pub mod ops;
pub mod repo;
pub mod shadow;
pub mod store;
pub mod tui;
