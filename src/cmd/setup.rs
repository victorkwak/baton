//! Setup commands: init, doctor. Owner: agent C (`docs/plan.md`).

use clap::Args;

use crate::Format;
use crate::error::Result;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// This machine's name.
    #[arg(long)]
    pub machine: String,
    /// Every machine's name (comma-separated), this one included.
    #[arg(long, value_delimiter = ',')]
    pub machines: Vec<String>,
    /// The human's name.
    #[arg(long)]
    pub human: String,
    /// The board repo's remote URL.
    #[arg(long)]
    pub board: String,
    /// Optional notes repo to share with `baton sync`.
    #[arg(long)]
    pub notes: Option<String>,
    /// Optional check command for the notes repo (repeat for each argument).
    #[arg(long)]
    pub notes_check: Vec<String>,
}

pub fn init(_args: InitArgs) -> Result<()> {
    crate::fail!(Internal, "init: not implemented yet")
}

pub fn doctor(_format: Format) -> Result<()> {
    crate::fail!(Internal, "doctor: not implemented yet")
}
