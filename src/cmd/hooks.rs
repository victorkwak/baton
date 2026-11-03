//! Claude Code hooks: session-start, wait, guard. Owner: agent C (`docs/plan.md`).

use clap::Subcommand;

use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// Stop (asyncRewake): wait for an actionable message; exit 2 to wake.
    Wait,
    /// PreToolUse: guard git and file writes.
    Guard,
}

pub fn hook(_cmd: HookCommand) -> Result<()> {
    crate::fail!(Internal, "hook: not implemented yet")
}
