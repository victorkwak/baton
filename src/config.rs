//! `~/.config/baton/config.toml` and the paths `baton` uses.

use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// How long a session keeps listening after its last turn: a
    /// duration (`90m`, `4h`) or `never` (default: [`DEFAULT_IDLE`]).
    /// `baton on --idle` overrides it for one session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    /// Optional notes repo shared through `baton sync`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Notes>,
}

/// How long a session listens after its last turn, unless the config or
/// `baton on --idle` says otherwise: one hour.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(3600);

/// `9m`, `90s`, `1h`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| {
        Error::new(
            Kind::Internal,
            format!("bad duration {s:?}: use e.g. 9m, 90s, 1h"),
        )
    })?;
    Ok(Duration::from_secs(n * mult))
}

/// An idle limit: a positive duration, or `never` (`None`).
pub fn parse_idle(s: &str) -> Result<Option<Duration>> {
    if s.trim() == "never" {
        return Ok(None);
    }
    match parse_duration(s) {
        Ok(d) if d.is_zero() => Err(Error::new(
            Kind::Internal,
            format!("bad idle limit {s:?}: use a duration above zero (e.g. 90m, 4h) or never"),
        )),
        other => other.map(Some),
    }
}

/// `1h`, `90m`, `45s` or `never`: the shortest exact form.
pub fn format_idle(idle: Option<Duration>) -> String {
    match idle.map(|d| d.as_secs()) {
        None => "never".to_owned(),
        Some(s) if s % 3600 == 0 => format!("{}h", s / 3600),
        Some(s) if s % 60 == 0 => format!("{}m", s / 60),
        Some(s) => format!("{s}s"),
    }
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
        self.idle()?;
        Ok(())
    }

    /// This machine's idle limit (`None`: never).
    pub fn idle(&self) -> Result<Option<Duration>> {
        match &self.idle {
            None => Ok(Some(DEFAULT_IDLE)),
            Some(s) => parse_idle(s)
                .map_err(|e| Error::new(Kind::Internal, format!("config idle: {}", e.message))),
        }
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

    #[test]
    fn idle_limit() {
        let c = Config::parse(GOOD, Path::new("t")).unwrap();
        assert_eq!(c.idle().unwrap(), Some(DEFAULT_IDLE));
        let c = Config::parse(&format!("{GOOD}idle = \"4h\"\n"), Path::new("t")).unwrap();
        assert_eq!(c.idle().unwrap(), Some(Duration::from_secs(4 * 3600)));
        let c = Config::parse(&format!("{GOOD}idle = \"never\"\n"), Path::new("t")).unwrap();
        assert_eq!(c.idle().unwrap(), None);
        for bad in ["0", "0m", "1d", "soon"] {
            let text = format!("{GOOD}idle = \"{bad}\"\n");
            assert!(Config::parse(&text, Path::new("t")).is_err(), "{bad}");
        }
        assert_eq!(format_idle(Some(DEFAULT_IDLE)), "1h");
        assert_eq!(format_idle(Some(Duration::from_secs(5400))), "90m");
        assert_eq!(format_idle(Some(Duration::from_secs(45))), "45s");
        assert_eq!(format_idle(None), "never");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("9m").unwrap(), Duration::from_secs(540));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("1d").is_err());
    }
}
