//! `~/.config/baton/config.toml` and the paths `baton` uses.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Kind, Result};

/// The config file. See `docs/design.md`, "Configuration".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// This machine's name.
    pub machine: String,
    /// Every machine's name, this one included.
    pub machines: Vec<String>,
    /// The human's name (an address like a machine).
    pub human: String,
    /// The board repo's remote URL.
    pub board: String,
    /// Where the board is cloned (default: `~/.local/share/baton/board`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_clone: Option<String>,
    /// Optional notes repo shared through `baton sync`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Notes>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notes {
    /// A git checkout on branch `main`.
    pub repo: String,
    /// Command run on the merged tree (cwd = that tree); non-zero stops the sync.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check: Vec<String>,
}

/// A machine, the human, or a message address.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 16
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `~` and `$HOME`.
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::new(Kind::Internal, "HOME is not set"))
}

/// Expand a leading `~/`.
pub fn expand(path: &str) -> Result<PathBuf> {
    Ok(match path.strip_prefix("~/") {
        Some(rest) => home()?.join(rest),
        None if path == "~" => home()?,
        None => PathBuf::from(path),
    })
}

fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(p) if p.is_absolute() => Ok(p),
        _ => Ok(home()?.join(fallback)),
    }
}

/// `$XDG_CONFIG_HOME/baton` (or `~/.config/baton`). `BATON_CONFIG_DIR` overrides (tests).
pub fn config_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("BATON_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join("baton"))
}

/// `$XDG_DATA_HOME/baton` (or `~/.local/share/baton`). `BATON_DATA_DIR` overrides (tests).
pub fn data_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("BATON_DATA_DIR") {
        return Ok(PathBuf::from(p));
    }
    Ok(xdg("XDG_DATA_HOME", ".local/share")?.join("baton"))
}

/// `$XDG_STATE_HOME/baton` (or `~/.local/state/baton`). `BATON_STATE_DIR` overrides (tests).
pub fn state_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("BATON_STATE_DIR") {
        return Ok(PathBuf::from(p));
    }
    Ok(xdg("XDG_STATE_HOME", ".local/state")?.join("baton"))
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    /// Load and validate the config.
    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::new(
                Kind::Internal,
                format!("cannot read {}: {e} (run `baton init`)", path.display()),
            )
        })?;
        Self::parse(&text, &path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let config: Config = toml::from_str(text)
            .map_err(|e| Error::new(Kind::Internal, format!("{}: {e}", path.display())))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for name in self.machines.iter().chain([&self.machine, &self.human]) {
            if !valid_name(name) || name == "any" {
                return Err(Error::new(
                    Kind::Internal,
                    format!("invalid name {name:?}: use [a-z][a-z0-9-]{{0,15}}, not \"any\""),
                ));
            }
        }
        if !self.machines.contains(&self.machine) {
            return Err(Error::new(
                Kind::Internal,
                format!("machine {:?} is not in machines", self.machine),
            ));
        }
        if self.machines.contains(&self.human) {
            return Err(Error::new(
                Kind::Internal,
                format!("human {:?} is also a machine name", self.human),
            ));
        }
        Ok(())
    }

    /// Where the board is cloned.
    pub fn board_clone(&self) -> Result<PathBuf> {
        match &self.board_clone {
            Some(p) => expand(p),
            None => Ok(data_dir()?.join("board")),
        }
    }

    /// The notes repo, if configured.
    pub fn notes_repo(&self) -> Result<Option<PathBuf>> {
        self.notes.as_ref().map(|n| expand(&n.repo)).transpose()
    }

    /// Is `name` a valid message address (`to`)?
    pub fn is_address(&self, name: &str) -> bool {
        name == "any" || name == self.human || self.machines.iter().any(|m| m == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
machine = "ws"
machines = ["ws", "laptop"]
human = "victor"
board = "git@example.com:me/board.git"
"#;

    #[test]
    fn parses_and_validates() {
        let c = Config::parse(GOOD, Path::new("t")).unwrap();
        assert!(c.is_address("laptop") && c.is_address("victor") && c.is_address("any"));
        assert!(!c.is_address("nobody"));
        let bad = GOOD.replace("machine = \"ws\"", "machine = \"other\"");
        assert!(Config::parse(&bad, Path::new("t")).is_err());
        let bad = GOOD.replace("human = \"victor\"", "human = \"ws\"");
        assert!(Config::parse(&bad, Path::new("t")).is_err());
        assert!(!valid_name("Ws") && !valid_name("1ws") && !valid_name("ws_1"));
    }
}
