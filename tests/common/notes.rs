//! Test world for notes sync: a bare repo standing in for the remote, and
//! machines that each have their own clone and `baton` directories.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// The check script used by tests: fails when any file contains `BAD`.
pub const CHECK_SCRIPT: &str = r#"#!/bin/sh
if grep -rIl --exclude-dir=.git BAD . >/dev/null 2>&1; then
  echo "check: BAD found in $(grep -rIl --exclude-dir=.git BAD . | head -1)" >&2
  exit 1
fi
exit 0
"#;

/// Five lines, so tests can edit adjacent lines.
pub const SEED_NOTE: &str = "one\ntwo\nthree\nfour\nfive\n";

pub struct World {
    pub tmp: TempDir,
    pub bare: PathBuf,
}

pub struct Machine {
    pub name: String,
    pub dir: PathBuf,
    pub repo: PathBuf,
}

/// Run git with a clean environment (no user or system config).
pub fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdin(Stdio::null());
    c
}

/// Run git and return trimmed stdout; panic on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl World {
    /// A bare remote seeded with `README.md` and `note.md`.
    pub fn new() -> World {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        let seed = tmp.path().join("seed");
        git(
            tmp.path(),
            &["init", "-q", "-b", "main", seed.to_str().unwrap()],
        );
        fs::write(seed.join("README.md"), "notes\n").unwrap();
        fs::write(seed.join("note.md"), SEED_NOTE).unwrap();
        fs::write(seed.join(".gitignore"), "*.tmp\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "seed"]);
        git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&seed, &["push", "-q", "origin", "main"]);
        fs::write(tmp.path().join("check.sh"), CHECK_SCRIPT).unwrap();
        World { tmp, bare }
    }

    pub fn check_script(&self) -> PathBuf {
        self.tmp.path().join("check.sh")
    }

    /// A machine with its own clone; `check` adds the BAD-file check.
    pub fn machine(&self, name: &str, check: bool) -> Machine {
        let dir = self.tmp.path().join(name);
        let repo = dir.join("notes");
        for d in ["config", "data", "state"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        git(
            &dir,
            &[
                "clone",
                "-q",
                self.bare.to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
        );
        git(&repo, &["config", "user.name", name]);
        git(
            &repo,
            &["config", "user.email", &format!("{name}@example.com")],
        );
        let check_line = if check {
            format!(
                "check = [\"sh\", {:?}]\n",
                self.check_script().to_str().unwrap()
            )
        } else {
            String::new()
        };
        let config = format!(
            "machine = {name:?}\nmachines = [\"ws\", \"laptop\"]\nhuman = \"victor\"\nboard = \"unused\"\n\n[notes]\nrepo = {:?}\n{check_line}",
            repo.to_str().unwrap()
        );
        fs::write(dir.join("config/config.toml"), config).unwrap();
        Machine {
            name: name.to_owned(),
            dir,
            repo,
        }
    }
}

impl Machine {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_baton"));
        c.args(args)
            .env("BATON_CONFIG_DIR", self.dir.join("config"))
            .env("BATON_DATA_DIR", self.dir.join("data"))
            .env("BATON_STATE_DIR", self.dir.join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("BATON_FAULT")
            .env_remove("BATON_SWEEP_IDLE_SECS")
            .stdin(Stdio::null());
        c
    }

    /// Run `baton` as a script (no agent, no TTY).
    pub fn baton(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run `baton` with extra environment variables.
    pub fn baton_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.command(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// Run `baton` as a Claude Code agent with session `s1`.
    pub fn agent(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("CLAUDECODE", "1")
            .env("CLAUDE_CODE_SESSION_ID", "s1")
            .output()
            .unwrap()
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.repo.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, text).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.repo.join(rel)).unwrap()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.repo.join(rel).exists()
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.repo, args)
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// `git status --porcelain` lines.
    pub fn status(&self) -> Vec<String> {
        let out = git_cmd(&self.repo)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Files changed by the commit `rev`.
    pub fn files_in(&self, rev: &str) -> Vec<String> {
        let out = self.git(&["show", "--name-only", "--format=", rev]);
        out.lines().map(str::to_owned).collect()
    }

    pub fn git_dir(&self) -> PathBuf {
        self.repo.join(".git")
    }

    /// Fetch and return `origin/main`.
    pub fn remote_main(&self) -> String {
        self.git(&["fetch", "-q", "origin"]);
        self.git(&["rev-parse", "origin/main"])
    }
}

/// Exit code of a finished run.
pub fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Assert a run succeeded, showing its output otherwise.
pub fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "exit {}\nstdout: {}\nstderr: {}",
        code(out),
        stdout(out),
        stderr(out)
    );
}

/// No file under `dir` (outside `.git`) contains a conflict marker.
pub fn assert_no_markers(dir: &Path) {
    fn walk(dir: &Path) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            if p.is_dir() {
                walk(&p);
            } else if let Ok(text) = fs::read_to_string(&p) {
                assert!(
                    !text.contains("<<<<<<<") && !text.contains(">>>>>>>"),
                    "conflict markers in {}",
                    p.display()
                );
            }
        }
    }
    walk(dir);
}

pub fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perm = fs::metadata(path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).unwrap();
}
