//! Who is running `baton`: an agent on this machine, or the human.
//!
//! A convenience, not security (`docs/design.md`, D6): any process running as
//! the user can fake these signals.

use std::io::IsTerminal as _;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    /// A Claude Code agent (`CLAUDECODE` is set). `session` may be empty.
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
        if std::env::var_os("CLAUDECODE").is_some() {
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
