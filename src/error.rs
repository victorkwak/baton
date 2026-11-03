//! Errors that end a command, each with its documented exit code
//! (`docs/design.md`, "Exit codes"). Code that can fail in several ways
//! returns `Result<T>`; `main` prints the message to stderr and exits with
//! [`Error::code`].

use std::fmt;

/// What went wrong, as the caller must act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Usage or internal error (1).
    Internal,
    /// `wait` timed out (3).
    Timeout,
    /// A board rule refused the action (10).
    Rule,
    /// A secret pattern was found in a post (11).
    Secret,
    /// No message with that id (12).
    UnknownId,
    /// `sync`: an uncommitted edit or untracked file collides with an incoming change (20).
    SyncCollision,
    /// `sync`: commit conflict while integrating (21).
    SyncConflict,
    /// `sync`: the configured check command failed (22).
    CheckFailed,
    /// No network (30).
    Offline,
    /// SSH authentication failed (31).
    Auth,
    /// Remote error, or the push lost the race too often (32).
    Remote,
    /// Repository state needs the human (40).
    State,
}

impl Kind {
    pub fn code(self) -> u8 {
        match self {
            Kind::Internal => 1,
            Kind::Timeout => 3,
            Kind::Rule => 10,
            Kind::Secret => 11,
            Kind::UnknownId => 12,
            Kind::SyncCollision => 20,
            Kind::SyncConflict => 21,
            Kind::CheckFailed => 22,
            Kind::Offline => 30,
            Kind::Auth => 31,
            Kind::Remote => 32,
            Kind::State => 40,
        }
    }
}

/// An error with its exit code and a message for stderr.
#[derive(Debug)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
}

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn code(&self) -> u8 {
        self.kind.code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Internal errors from libraries (I/O, parsing) become exit 1.
impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        // Keep a typed error that travelled through anyhow.
        match e.downcast::<Error>() {
            Ok(typed) => typed,
            Err(e) => Error::new(Kind::Internal, format!("{e:#}")),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(Kind::Internal, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `return Err(...)` with a kind and a formatted message.
#[macro_export]
macro_rules! fail {
    ($kind:ident, $($arg:tt)*) => {
        return Err($crate::error::Error::new($crate::error::Kind::$kind, format!($($arg)*)))
    };
}
