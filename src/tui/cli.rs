//! `sheep ui` — the terminal interface's command-line surface.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct UiArgs {
    /// Open straight into the rewind picker rather than the timeline dock.
    #[arg(long)]
    pub rewind: bool,
}

pub fn run(_args: &UiArgs, _repo: Option<&std::path::Path>, _line: &str) -> Result<()> {
    anyhow::bail!("`sheep ui` is not implemented yet")
}
