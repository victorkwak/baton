//! Notes commands: sync, sweep. Owner: agent B (`docs/plan.md`).

use clap::Args;

use crate::Format;
use crate::error::Result;

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Files or directories this caller changed (relative to the notes repo).
    #[arg(long, num_args = 1.., conflicts_with = "all", required_unless_present = "all")]
    pub paths: Vec<std::path::PathBuf>,
    /// Every change in the notes repo (the human and the sweeper only).
    #[arg(long)]
    pub all: bool,
    /// Commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
}

pub fn sync(_format: Format, _args: SyncArgs) -> Result<()> {
    crate::fail!(Internal, "sync: not implemented yet")
}

pub fn sweep(_format: Format) -> Result<()> {
    crate::fail!(Internal, "sweep: not implemented yet")
}
