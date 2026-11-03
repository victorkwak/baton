//! Who is running `baton`: an agent on this machine, or the human.
//!
//! A convenience, not security (`docs/design.md`, D6): any process running as
//! the user can fake these signals.

use std::io::IsTerminal as _;

use crate::config::Config;

/// Set by Claude Code only for the processes its own tools start (Bash,
/// hooks). An IDE terminal opened from Claude Code inherits `CLAUDECODE`
/// but not this one.
pub const CHILD_SESSION_ENV: &str = "CLAUDE_CODE_CHILD_SESSION";

/// Is this process run by a Claude Code agent? `CLAUDE_CODE_CHILD_SESSION`
/// says so. Without it, `CLAUDECODE` counts only when stdin is not a
/// terminal: a human typing in an IDE terminal that Claude Code started
/// has `CLAUDECODE` and a TTY. The one check every command uses.
pub fn is_agent() -> bool {
    if std::env::var_os(CHILD_SESSION_ENV).is_some() {
        return true;
    }
    std::env::var_os("CLAUDECODE").is_some() && !std::io::stdin().is_terminal()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    /// A Claude Code agent ([`is_agent`]). `session` may be empty.
    Agent { machine: String, session: String },
    /// The human at a terminal.
    Human { machine: String, name: String },
    /// Neither (a script or service without a TTY): acts as this machine.
    Machine { machine: String },
}

impl Who {
    /// Decide from the environment. `session` is the hook's stdin
    /// `session_id` when a hook knows it.
    pub fn detect(config: &Config, session: Option<&str>) -> Who {
        let machine = config.machine.clone();
        if is_agent() {
            let session = session
                .map(str::to_owned)
                .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
                .unwrap_or_else(|| format!("{}-{}", config.machine, std::process::id()));
            return Who::Agent { machine, session };
        }
        if std::io::stdin().is_terminal() {
            return Who::Human {
                machine,
                name: config.human.clone(),
            };
        }
        Who::Machine { machine }
    }

    /// The `from` written on records.
    pub fn from(&self) -> &str {
        match self {
            Who::Agent { machine, .. } | Who::Machine { machine } => machine,
            Who::Human { name, .. } => name,
        }
    }

    pub fn machine(&self) -> &str {
        match self {
            Who::Agent { machine, .. } | Who::Human { machine, .. } | Who::Machine { machine } => {
                machine
            }
        }
    }

    pub fn session(&self) -> &str {
        match self {
            Who::Agent { session, .. } => session,
            _ => "",
        }
    }

    pub fn is_human(&self) -> bool {
        matches!(self, Who::Human { .. })
    }
}
