//! Runs the system `git` for the board clone and the notes repo.
//!
//! Every call: a timeout, SSH that never prompts (`BatchMode`) and reuses
//! connections through a short control path (a long one exceeds the Unix
//! socket limit and every call fails, silently, so the exit status of each
//! call is checked), and stderr classified into offline / auth / remote.

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Kind, Result};

/// Default limit for one git call.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// A git invocation in one repository.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    env: Vec<(String, String)>,
    timeout: Duration,
}

/// Output of a finished git call.
#[derive(Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Trimmed first line of stdout.
    pub fn line(&self) -> String {
        self.stdout_str()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_owned()
    }
}

/// The `GIT_SSH_COMMAND` every call uses.
pub fn ssh_command() -> String {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=10 -o ControlMaster=auto -o ControlPersist=60 -o ControlPath={}/bcm-%C",
        dir.display()
    )
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            env: Vec::new(),
            timeout: TIMEOUT,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Add an environment variable for every call (e.g. `GIT_INDEX_FILE`).
    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run git; return its output whatever the exit status.
    pub fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_input(args, None)
    }

    /// Run git with bytes on stdin.
    pub fn output_with_input<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_SSH_COMMAND", ssh_command())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::new(Kind::Internal, format!("cannot run git: {e}")))?;
        if let Some(bytes) = input {
            // Write stdin from a thread so a large input cannot deadlock on full pipes.
            let mut stdin = child.stdin.take().expect("piped stdin");
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b);
            b
        });
        let err_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b);
            b
        });
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::new(
                    Kind::Remote,
                    format!(
                        "git timed out after {}s in {}",
                        self.timeout.as_secs(),
                        self.dir.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = out_t.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
        Ok(Output {
            status: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    /// Run git; a non-zero exit becomes an error, classified when it is a
    /// network or auth failure.
    pub fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.output(&args)?;
        if out.ok() {
            return Ok(out);
        }
        let what = args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        Err(classify(&what, &out.stderr))
    }
}

/// Turn a failed git call's stderr into a typed error.
pub fn classify(what: &str, stderr: &str) -> Error {
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no message)");
    let s = stderr.to_ascii_lowercase();
    let kind = if s.contains("permission denied (publickey")
        || s.contains("could not open a connection to your authentication agent")
        || s.contains("incorrect passphrase")
        || s.contains("host key verification failed")
        || s.contains("authentication failed")
    {
        Kind::Auth
    } else if s.contains("could not resolve hostname")
        || s.contains("connection timed out")
        || s.contains("network is unreachable")
        || s.contains("connection refused")
        || s.contains("no route to host")
        || s.contains("temporary failure in name resolution")
        || s.contains("could not read from remote repository")
            && !s.contains("does not appear to be a git repository")
            && !s.contains("repository not found")
    {
        Kind::Offline
    } else if s.contains("remote:")
        || s.contains("internal server error")
        || s.contains("[rejected]")
    {
        Kind::Remote
    } else {
        Kind::Internal
    };
    let label = match kind {
        Kind::Auth => "auth",
        Kind::Offline => "offline",
        Kind::Remote => "remote",
        _ => "git",
    };
    Error::new(kind, format!("{label}: git {what}: {first}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_failures() {
        assert_eq!(
            classify("fetch", "git@x: Permission denied (publickey).\nfatal: Could not read from remote repository.").kind,
            Kind::Auth
        );
        assert_eq!(
            classify("fetch", "ssh: Could not resolve hostname x: Name or service not known\nfatal: Could not read from remote repository.").kind,
            Kind::Offline
        );
        assert_eq!(
            classify("push", " ! [rejected]        main -> main (fetch first)").kind,
            Kind::Remote
        );
        assert_eq!(classify("log", "fatal: bad revision").kind, Kind::Internal);
    }

    #[test]
    fn control_path_is_short() {
        let cmd = ssh_command();
        let path = cmd.rsplit("ControlPath=").next().unwrap();
        // %C expands to 40 hex characters; the socket limit is about 104-108 bytes.
        assert!(path.len() - 2 + 40 < 100, "{path}");
    }
}
