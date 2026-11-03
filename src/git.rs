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

/// Longest directory for the control socket: `<dir>/bcm-` plus 40 hex
/// characters (`%C`) must stay under the ~104-byte Unix socket limit.
const MAX_CONTROL_DIR: usize = 56;

/// The `GIT_SSH_COMMAND` every call in `dir` uses: the SSH command git would
/// use there (so a repo's `core.sshCommand` key choice still applies), plus
/// baton's options. ssh keeps the first value of each option, so the user's
/// own options win.
///
/// Connection reuse only with a private directory: with
/// `ControlMaster=auto`, ssh uses any socket already at the path, so a
/// directory another user can write to (like `/tmp`) would let them sit in
/// the middle of the connection.
pub fn ssh_command(dir: &Path) -> String {
    let base = configured_ssh(dir);
    let reuse = match control_dir() {
        Some(cdir) => format!(
            "-o ControlMaster=auto -o ControlPersist=60 -o 'ControlPath={}/bcm-%C'",
            cdir.display()
        ),
        None => "-o ControlMaster=no".to_owned(),
    };
    format!("{base} -o BatchMode=yes -o ConnectTimeout=10 {reuse}")
}

/// The SSH command git itself would run for `dir`: `$GIT_SSH_COMMAND`, else
/// `core.sshCommand` (repo, global or system config), else `$GIT_SSH`, else
/// `ssh`.
fn configured_ssh(dir: &Path) -> String {
    let nonempty = |s: String| (!s.trim().is_empty()).then_some(s);
    if let Some(cmd) = std::env::var("GIT_SSH_COMMAND").ok().and_then(nonempty) {
        return cmd;
    }
    let mut git = Command::new("git");
    if dir.is_dir() {
        git.arg("-C").arg(dir);
    }
    let configured = git
        .args(["config", "--get", "core.sshCommand"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| nonempty(String::from_utf8_lossy(&o.stdout).trim().to_owned()));
    if let Some(cmd) = configured {
        return cmd;
    }
    // GIT_SSH names a program without arguments.
    match std::env::var("GIT_SSH").ok().and_then(nonempty) {
        Some(prog) if !prog.contains('\'') => format!("'{prog}'"),
        _ => "ssh".to_owned(),
    }
}

/// `$XDG_RUNTIME_DIR`, else `~/.ssh`: the first that is private (owned by
/// the user who owns `$HOME`, no group or other permissions), short enough,
/// and safe to quote.
fn control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let owner = std::fs::metadata(&home).ok()?.uid();
    let candidates = [
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        Some(home.join(".ssh")),
    ];
    candidates.into_iter().flatten().find(|dir| {
        let text = dir.to_string_lossy();
        std::fs::symlink_metadata(dir)
            .is_ok_and(|m| m.is_dir() && m.uid() == owner && m.mode() & 0o077 == 0)
            && dir.is_absolute()
            && text.len() <= MAX_CONTROL_DIR
            && !text.contains(['\'', '"', ' ', '\\', '%'])
    })
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
            .env("GIT_SSH_COMMAND", ssh_command(&self.dir))
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
        // Each reader signals when its pipe closes, which happens when git
        // exits; waiting on that (with the timeout) costs no polling delay.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let out_done = done_tx.clone();
        let out_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b);
            let _ = out_done.send(());
            b
        });
        let err_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b);
            let _ = done_tx.send(());
            b
        });
        let deadline = Instant::now() + self.timeout;
        let mut closed = 0;
        let status = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if closed < 2 {
                if done_rx.recv_timeout(left).is_ok() {
                    closed += 1;
                    continue;
                }
            } else if let Some(status) = child.try_wait()? {
                break status;
            } else if !left.is_zero() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
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
        || refused_by_permissions(&s)
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
        || s.contains("[remote rejected]")
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

/// The remote refused for lack of permission (the key works, but may not
/// write here). Checked before the offline patterns: hosts follow it with
/// "Could not read from remote repository". `s` is lowercase.
fn refused_by_permissions(s: &str) -> bool {
    s.contains(" denied to ")
        || s.contains("permission to ")
        || s.contains("access denied")
        || s.contains("not allowed to push")
        || s.contains("marked as read only")
        || s.contains("[remote rejected]")
            && [
                "permission",
                "denied",
                "not allowed",
                "forbidden",
                "read only",
                "read-only",
            ]
            .iter()
            .any(|w| s.contains(w))
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
    fn permission_refusals_are_auth_not_offline() {
        for stderr in [
            // GitHub, SSH key without write access.
            "ERROR: Permission to me/board.git denied to other-user.\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.",
            // GitHub, read-only deploy key.
            "ERROR: The key you are authenticating with has been marked as read only.\nfatal: Could not read from remote repository.",
            // GitLab, protected branch.
            "remote: GitLab: You are not allowed to push code to protected branches on this project.\nTo example.com:me/board.git\n ! [remote rejected] main -> main (pre-receive hook declined)",
            // HTTPS.
            "remote: Access denied\nfatal: unable to access 'https://example.com/board.git/': The requested URL returned error: 403",
            " ! [remote rejected] main -> main (permission denied)",
        ] {
            assert_eq!(classify("push", stderr).kind, Kind::Auth, "{stderr}");
        }
        // A race is still a race, and a plain remote error stays remote.
        assert_eq!(
            classify(
                "push",
                " ! [remote rejected] main -> main (pre-receive hook declined)"
            )
            .kind,
            Kind::Remote
        );
    }

    #[test]
    fn keeps_the_repos_own_ssh_command_first() {
        // A repo that pins its key (like `core.sshCommand "ssh -i key"`)
        // must keep it: baton's options are appended, never replacing it.
        if std::env::var_os("GIT_SSH_COMMAND").is_some() {
            return; // the environment would win, as it does for git itself
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&[
            "config",
            "core.sshCommand",
            "ssh -i /path/to/repo_key -o IdentitiesOnly=yes",
        ]);
        let cmd = ssh_command(dir);
        assert!(
            cmd.starts_with("ssh -i /path/to/repo_key -o IdentitiesOnly=yes "),
            "{cmd}"
        );
        assert!(cmd.contains("BatchMode=yes"), "{cmd}");
    }

    #[test]
    fn control_path_is_short_and_private_or_off() {
        let cmd = ssh_command(Path::new("/nonexistent-dir"));
        assert!(cmd.contains("BatchMode=yes"), "{cmd}");
        match cmd.split("ControlPath=").nth(1) {
            Some(rest) => {
                let path = rest.trim_end_matches('\'');
                // %C expands to 40 hex characters; the socket limit is about 104 bytes.
                assert!(path.len() - 2 + 40 < 104, "{path}");
                assert!(!path.starts_with("/tmp"), "{path}");
                let dir = std::path::Path::new(path).parent().unwrap();
                use std::os::unix::fs::MetadataExt as _;
                assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o077, 0, "{path}");
            }
            None => assert!(cmd.contains("ControlMaster=no"), "{cmd}"),
        }
    }
}
