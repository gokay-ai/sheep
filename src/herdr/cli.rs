//! `sheep watch` — the recorder's command-line surface.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    /// Print detected turn boundaries instead of recording them.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(_args: &WatchArgs) -> Result<()> {
    anyhow::bail!("`sheep watch` is not implemented yet")
}
