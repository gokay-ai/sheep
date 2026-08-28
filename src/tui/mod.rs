//! The terminal interface Sheep shows inside a herdr pane.
//!
//! Two surfaces, both driven by [`crate::store`] and [`crate::ops`]:
//!
//! * the **dock** — a timeline of turns, docked beside the agent, that answers
//!   "what has this agent done to my files, and when";
//! * the **rewind overlay** — pick a turn, see exactly which files a restore
//!   would write and remove, confirm, and go back.
//!
//! The overlay never calls a restore without showing the plan first: the dry
//! run is the product, not a safety afterthought. That is a property of the
//! state machine rather than of the drawing code — see [`app::App::open_rewind`]
//! and the `Confirm` path in [`app`].
//!
//! The split is deliberate and worth keeping:
//!
//! | module | may block | knows about a terminal |
//! |---|---|---|
//! | [`app`] | no | no |
//! | [`render`] | no | draws, reads nothing |
//! | [`engine`] | yes — it is the only place that does | no |
//! | [`runtime`] | no | only through [`runtime::Screen`] / [`runtime::Input`] |
//! | [`cli`] | no | owns it |
//!
//! Which is why the whole interface, restore included, is testable with
//! `ratatui`'s `TestBackend` and no pseudo-terminal.

pub mod app;
pub mod cli;
pub mod engine;
pub mod render;
pub mod runtime;
pub mod text;
pub mod theme;

pub use app::{App, Key, Mode};
pub use cli::{leave_line, UiArgs};
