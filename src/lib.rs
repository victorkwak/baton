//! baton: coordinate AI coding agents on several machines through git.
//! Design: `docs/design.md`. Build plan and module ownership: `docs/plan.md`.

pub mod cmd;
pub mod config;
pub mod error;
pub mod git;
pub mod identity;
pub mod output;

// Board (plan: agent A).
pub mod board;
pub mod record;
pub mod secrets;
pub mod store;

// Notes sync (plan: agent B).
pub mod notes;

// Claude Code integration (plan: agent C).
pub mod hooks;

/// Shared state for one command run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
    Json,
}
