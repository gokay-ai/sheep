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
//! run is the product, not a safety afterthought.

pub mod cli;

pub use cli::UiArgs;
