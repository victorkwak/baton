//! Helpers for the board's end-to-end tests: a local bare repo as the board
//! remote and simulated machines, each with its own config, data and state
//! directories. Every call runs the real `baton` binary against real git.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

/// An unresolvable SSH remote: `git` fails fast with "Could not resolve
/// hostname", which `baton` classifies as offline (exit 30).
pub const OFFLINE_URL: &str = "ssh://git@nonexistent.invalid/board.git";

pub const HUMAN: &str = "victor";

/// The shared board remote.
pub struct Net {
    pub dir: TempDir,
    pub bare: PathBuf,
}

impl Net {
    pub fn new() -> Net {
        let dir = TempDir::new().unwrap();
        let bare = dir.path().join("board.git");
        git(
            dir.path(),
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                bare.to_str().unwrap(),
            ],
        );
        Net { dir, bare }
    }

    pub fn url(&self) -> String {
        self.bare.to_str().unwrap().to_owned()
    }

    /// A machine named `name` among `machines`.
    pub fn machine(&self, name: &str, machines: &[&str]) -> Machine {
        let root = self.dir.path().join(format!("m-{name}"));
        let m = Machine {
            name: name.to_owned(),
            root,
            agent: true,
            env: Vec::new(),
        };
        std::fs::create_dir_all(m.config_dir()).unwrap();
        m.write_config(&self.url(), machines);
        m
    }

    /// Commits on the remote's `main`, oldest first: `(sha, parent count)`.
    pub fn history(&self) -> Vec<(String, usize)> {
        let has_main = Command::new("git")
            .arg("-C")
            .arg(&self.bare)
            .args(["rev-parse", "--verify", "-q", "refs/heads/main"])
            .output()
            .unwrap()
            .status
            .success();
        if !has_main {
            return Vec::new();
        }
        let out = git(&self.bare, &["rev-list", "--reverse", "--parents", "main"]);
        out.lines()
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f[0].to_owned(), f.len() - 1)
            })
            .collect()
    }

    /// Paths on the remote's `main`.
    pub fn files(&self) -> Vec<String> {
        git(&self.bare, &["ls-tree", "-r", "--name-only", "main"])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Run git; panic on failure; return stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// One finished `baton` run.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// Assert exit 0; return stdout.
    pub fn ok(self) -> String {
        assert_eq!(self.code, 0, "expected success: {self:?}");
        self.stdout
    }

    /// Assert the exit code; return stderr.
    pub fn code(self, code: i32) -> String {
        assert_eq!(self.code, code, "expected exit {code}: {self:?}");
        self.stderr
    }

    pub fn json(&self) -> Value {
        let v: Value = serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("bad JSON ({e}): {self:?}"));
        assert_eq!(v["schema_version"], 1);
        v
    }
}

/// A simulated machine.
#[derive(Debug, Clone)]
pub struct Machine {
    pub name: String,
    pub root: PathBuf,
    /// Run as a Claude Code agent (`CLAUDECODE=1`).
    pub agent: bool,
    /// Extra environment for every command.
    pub env: Vec<(String, String)>,
}

impl Machine {
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn clone_dir(&self) -> PathBuf {
        self.root.join("data/board")
    }

    pub fn write_config(&self, url: &str, machines: &[&str]) {
        let list = machines
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            self.config_dir().join("config.toml"),
            format!(
                "machine = {:?}\nmachines = [{list}]\nhuman = {HUMAN:?}\nboard = {url:?}\n",
                self.name
            ),
        )
        .unwrap();
    }

    /// Point the config and the clone's `origin` at `url`.
    pub fn set_board(&self, url: &str, machines: &[&str]) {
        self.write_config(url, machines);
        git(&self.clone_dir(), &["remote", "set-url", "origin", url]);
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_baton"));
        cmd.args(args)
            .env("HOME", &self.root)
            .env("BATON_CONFIG_DIR", self.config_dir())
            .env("BATON_DATA_DIR", self.root.join("data"))
            .env("BATON_STATE_DIR", self.root.join("state"))
            .env("BATON_POLL_SECS", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            // Not inherited from a Claude Code session running the tests.
            .env_remove("CLAUDE_CODE_CHILD_SESSION")
            .env_remove("CLAUDE_PID")
            .stdin(Stdio::null());
        if self.agent {
            cmd.env("CLAUDECODE", "1")
                .env("CLAUDE_CODE_SESSION_ID", format!("{}-session", self.name));
        } else {
            cmd.env_remove("CLAUDECODE")
                .env_remove("CLAUDE_CODE_SESSION_ID");
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    /// `post --to <to> -m <body> <subject> <extra...>`; returns the id.
    pub fn post(&self, to: &str, subject: &str, extra: &[&str]) -> String {
        let mut args = vec!["post", "--to", to, "-m", "body text"];
        args.extend_from_slice(extra);
        args.push(subject);
        self.run(&args).ok().trim().to_owned()
    }

    /// The `inbox` ids (text output).
    pub fn inbox_ids(&self, all: bool) -> Vec<String> {
        let mut args = vec!["inbox"];
        if all {
            args.push("--all");
        }
        rows(&self.run(&args).ok())
            .into_iter()
            .map(|r| r[0].clone())
            .collect()
    }
}

/// Tab-separated rows after the `# ...` header.
pub fn rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| l.split('\t').map(str::to_owned).collect())
        .collect()
}
