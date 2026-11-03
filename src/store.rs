//! The board clone, through [`git::Git`](crate::git::Git) only.
//!
//! Reads: the records of `refs/remotes/origin/main` in first-parent order,
//! plus plain messages queued offline on `refs/heads/main`.
//! Writes: a temporary index and `commit-tree` (the working tree is never
//! used; the clone has none), then `git push origin <sha>:refs/heads/main`
//! as a compare-and-swap: on rejection, fetch, re-check the rules against
//! the fetched tip, rebuild, retry. Never force.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::board::{Located, Rejected, State};
use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::git::{Git, classify};
use crate::record::{Record, random_u64};

/// Retries after a lost push race (so up to `1 + RETRIES` attempts).
pub const RETRIES: usize = 3;

const MAIN: &str = "refs/heads/main";
const ORIGIN_MAIN: &str = "refs/remotes/origin/main";

/// The board clone.
#[derive(Debug, Clone)]
pub struct Store {
    git: Git,
    path: PathBuf,
    human: String,
    machine: String,
    session: String,
}

/// An exclusive lock on `<clone>/.git/baton.lock`, released on drop.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// A commit on `main` not yet on the remote (a plain message posted offline).
#[derive(Debug, Clone)]
struct Queued {
    files: Vec<(String, Vec<u8>)>,
    message: String,
}

/// What a read found.
#[derive(Debug)]
pub struct Snapshot {
    pub state: State,
    /// `refs/remotes/origin/main`, if the remote has a `main`.
    pub origin: Option<String>,
    /// Commits queued offline, not yet pushed.
    pub queued: usize,
    /// `State::len` of the records that are on the remote (a cursor that
    /// never moves backwards; queued records come after it).
    pub remote_len: usize,
    queued_commits: Vec<Queued>,
}

/// Result of a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The new tip of `main`.
    pub commit: String,
    /// Committed locally only: the remote was unreachable.
    pub queued: bool,
}

enum Push {
    Pushed,
    Rejected,
}

impl Store {
    /// Open the board clone, cloning it when missing, and check that its
    /// `origin` is the configured board.
    pub fn open(config: &Config, session: &str) -> Result<Store> {
        let path = config.board_clone()?;
        if !path.join(".git").exists() {
            clone(&config.board, &path)?;
        }
        let store = Store {
            git: Git::new(&path),
            path,
            human: config.human.clone(),
            machine: config.machine.clone(),
            session: session.to_owned(),
        };
        store.preflight(&config.board)?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    fn preflight(&self, url: &str) -> Result<()> {
        if self.path.join(".git/shallow").exists() {
            crate::fail!(
                State,
                "board clone {} is shallow; re-clone it in full",
                self.path.display()
            );
        }
        let out = self.git.output(["config", "--get", "remote.origin.url"])?;
        let have = out.line();
        if have != url {
            crate::fail!(
                State,
                "board clone {} has origin {have:?}, but the config says {url:?}",
                self.path.display()
            );
        }
        Ok(())
    }

    /// Take the clone's exclusive lock (blocks while another `baton` holds it).
    pub fn lock(&self) -> Result<Lock> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.join(".git/baton.lock"))?;
        file.lock()?;
        Ok(Lock { _file: file })
    }

    /// Fetch the remote.
    pub fn fetch(&self) -> Result<()> {
        self.git.run(["fetch", "--quiet", "--prune", "origin"])?;
        Ok(())
    }

    /// Fetch; when the remote is unreachable (exit 30), warn and carry on
    /// with the local copy. Other errors stop.
    pub fn fetch_or_warn(&self) -> Result<bool> {
        match self.fetch() {
            Ok(()) => Ok(true),
            Err(e) if e.kind == Kind::Offline => {
                eprintln!("baton: {e}; showing the local copy of the board");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// The remote's `main` without fetching (`ls-remote`): cheap polling.
    pub fn remote_tip(&self) -> Result<Option<String>> {
        let out = self.git.run(["ls-remote", "origin", MAIN])?;
        Ok(out
            .stdout_str()
            .split_whitespace()
            .next()
            .map(str::to_owned))
    }

    fn rev(&self, r: &str) -> Result<Option<String>> {
        let out = self
            .git
            .output(["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")])?;
        Ok(out.ok().then(|| out.line()))
    }

    /// `refs/remotes/origin/main` (as of the last fetch).
    pub fn origin_tip(&self) -> Result<Option<String>> {
        self.rev(ORIGIN_MAIN)
    }

    /// Read the board: the remote's records, then the queued ones.
    pub fn read(&self) -> Result<Snapshot> {
        let origin = self.rev(ORIGIN_MAIN)?;
        let mut main = self.rev(MAIN)?;
        // Fast-forward `main` when it is only behind.
        if let (Some(o), Some(m)) = (&origin, &main)
            && o != m
            && self.is_ancestor(m, o)?
        {
            self.git.run(["update-ref", MAIN, o])?;
            main = Some(o.clone());
        }
        if main.is_none()
            && let Some(o) = &origin
        {
            self.git.run(["update-ref", MAIN, o])?;
            main = Some(o.clone());
        }
        let mut commits = match &origin {
            Some(o) => self.commits(std::slice::from_ref(o))?,
            None => Vec::new(),
        };
        let remote_commits = commits.len();
        let queued_range = match (&origin, &main) {
            (_, None) => None,
            (Some(o), Some(m)) if o == m => None,
            (Some(o), Some(m)) => Some(vec![format!("{o}..{m}")]),
            (None, Some(m)) => Some(vec![m.clone()]),
        };
        if let Some(range) = queued_range {
            commits.extend(self.commits(&range)?);
        }

        let mut located = Vec::new();
        let mut bad = Vec::new();
        let mut remote_len = 0;
        let mut queued_commits = Vec::new();
        for (n, c) in commits.iter().enumerate() {
            for (path, bytes) in &c.files {
                let parsed = std::str::from_utf8(bytes)
                    .map_err(|_| Error::new(Kind::Internal, format!("{path}: not UTF-8")))
                    .and_then(|text| Record::parse(path, text));
                match parsed {
                    Ok(record) => located.push(Located {
                        record,
                        commit: c.sha.clone(),
                        seq: located.len(),
                    }),
                    Err(e) => bad.push(Rejected {
                        id: path.clone(),
                        commit: c.sha.clone(),
                        reason: e.message,
                    }),
                }
            }
            if n + 1 == remote_commits {
                remote_len = located.len();
            }
            if n >= remote_commits {
                queued_commits.push(Queued {
                    files: c.files.clone(),
                    message: c.message.clone(),
                });
            }
        }
        let mut state = State::fold(&self.human, &located);
        state.rejected.extend(bad);
        Ok(Snapshot {
            state,
            origin,
            queued: queued_commits.len(),
            remote_len,
            queued_commits,
        })
    }

    fn is_ancestor(&self, a: &str, b: &str) -> Result<bool> {
        let out = self.git.output(["merge-base", "--is-ancestor", a, b])?;
        match out.status {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(classify("merge-base --is-ancestor", &out.stderr)),
        }
    }

    /// Commits of `range` in first-parent order, oldest first, with the
    /// board files each one added (sorted by id). Modified or deleted paths
    /// are ignored: a record is what its file held when it was added.
    fn commits(&self, range: &[String]) -> Result<Vec<CommitFiles>> {
        let mut args: Vec<String> = [
            "log",
            "--first-parent",
            "--reverse",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            "--format=%x01%H%x02%B%x03",
        ]
        .map(String::from)
        .to_vec();
        args.extend(range.iter().cloned());
        args.push("--".into());
        let out = self.git.run(&args)?;
        let text = out.stdout_str();
        let mut commits: Vec<CommitFiles> = Vec::new();
        let mut blobs: Vec<String> = Vec::new();
        for chunk in text.split('\u{1}').filter(|c| !c.is_empty()) {
            let (sha, rest) = chunk.split_once('\u{2}').unwrap_or((chunk, ""));
            let (message, raw) = rest.split_once('\u{3}').unwrap_or((rest, ""));
            let mut files = Vec::new();
            for line in raw.lines().filter(|l| l.starts_with(':')) {
                // :old_mode new_mode old_sha new_sha status\tpath
                let Some((meta, path)) = line.split_once('\t') else {
                    continue;
                };
                let f: Vec<&str> = meta.split_whitespace().collect();
                if f.len() == 5 && f[4] == "A" && is_record_path(path) {
                    files.push((path.to_owned(), f[3].to_owned()));
                }
            }
            files.sort_by(|a, b| file_id(&a.0).cmp(file_id(&b.0)));
            blobs.extend(files.iter().map(|(_, b)| b.clone()));
            commits.push(CommitFiles {
                sha: sha.trim().to_owned(),
                message: message.trim_end().to_owned(),
                files: files
                    .into_iter()
                    .map(|(p, b)| (p, b.into_bytes()))
                    .collect(),
            });
        }
        // Replace blob ids by contents, in one cat-file call.
        if !blobs.is_empty() {
            let input: String = blobs.iter().map(|b| format!("{b}\n")).collect();
            let out = self
                .git
                .output_with_input(["cat-file", "--batch"], Some(input.as_bytes()))?;
            if !out.ok() {
                return Err(classify("cat-file --batch", &out.stderr));
            }
            let mut contents = parse_batch(&out.stdout, blobs.len())?.into_iter();
            for c in &mut commits {
                for f in &mut c.files {
                    f.1 = contents.next().expect("one content per blob");
                }
            }
        }
        Ok(commits)
    }

    fn commit_env(&self) -> Git {
        let name = format!("baton {}", self.machine);
        let email = format!("{}@baton.invalid", self.machine);
        self.git
            .clone()
            .with_env("GIT_AUTHOR_NAME", name.clone())
            .with_env("GIT_AUTHOR_EMAIL", email.clone())
            .with_env("GIT_COMMITTER_NAME", name)
            .with_env("GIT_COMMITTER_EMAIL", email)
    }

    /// Commit `files` on top of `parent` through a temporary index. A path
    /// already in the parent with the same content is skipped (a queued
    /// commit that reached the remote before); with other content it is
    /// refused (append-only). Returns the parent when nothing is left.
    fn commit_files(
        &self,
        files: &[(String, Vec<u8>)],
        parent: Option<&str>,
        message: &str,
    ) -> Result<Option<String>> {
        let index = self.path.join(format!(
            ".git/baton-index-{}-{:x}",
            std::process::id(),
            random_u64()
        ));
        let result = self.commit_in(&index, files, parent, message);
        let _ = std::fs::remove_file(&index);
        result
    }

    fn commit_in(
        &self,
        index: &Path,
        files: &[(String, Vec<u8>)],
        parent: Option<&str>,
        message: &str,
    ) -> Result<Option<String>> {
        let git = self
            .commit_env()
            .with_env("GIT_INDEX_FILE", index.to_string_lossy().into_owned());
        match parent {
            Some(p) => git.run(["read-tree", p])?,
            None => git.run(["read-tree", "--empty"])?,
        };
        let mut added = 0;
        for (path, bytes) in files {
            let blob = git.output_with_input(["hash-object", "-w", "--stdin"], Some(bytes))?;
            if !blob.ok() {
                return Err(classify("hash-object", &blob.stderr));
            }
            let blob = blob.line();
            let existing = git.run(["ls-files", "--stage", "--", path])?.line();
            if !existing.is_empty() {
                if existing.split_whitespace().nth(1) == Some(blob.as_str()) {
                    continue;
                }
                crate::fail!(
                    Rule,
                    "rule append-only: {path} already exists on the board with other content"
                );
            }
            git.run([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ])?;
            added += 1;
        }
        if added == 0 {
            return Ok(parent.map(str::to_owned));
        }
        let tree = git.run(["write-tree"])?.line();
        let mut args = vec!["commit-tree".to_owned(), tree];
        if let Some(p) = parent {
            args.push("-p".into());
            args.push(p.to_owned());
        }
        args.push("-m".into());
        args.push(message.to_owned());
        Ok(Some(git.run(&args)?.line()))
    }

    fn commit_message(&self, summary: &str) -> String {
        let mut m = format!("{summary}\n\nBaton-Machine: {}", self.machine);
        if !self.session.is_empty() {
            m.push_str(&format!("\nBaton-Session: {}", self.session));
        }
        m
    }

    /// Commit records on `parent`; returns the new sha.
    pub fn commit(
        &self,
        records: &[Record],
        parent: Option<&str>,
        summary: &str,
    ) -> Result<String> {
        let files: Vec<(String, Vec<u8>)> = records
            .iter()
            .map(|r| (r.path(), r.render().into_bytes()))
            .collect();
        self.commit_files(&files, parent, &self.commit_message(summary))?
            .ok_or_else(|| Error::new(Kind::Internal, "nothing to commit"))
    }

    fn push(&self, sha: &str) -> Result<Push> {
        let out = self
            .git
            .output(["push", "--porcelain", "origin", &format!("{sha}:{MAIN}")])?;
        if out.ok() {
            return Ok(Push::Pushed);
        }
        let all = format!("{}\n{}", out.stdout_str(), out.stderr);
        let lower = all.to_ascii_lowercase();
        let raced = lower.contains("[rejected]")
            || lower.contains("non-fast-forward")
            || lower.contains("fetch first")
            || (lower.contains("[remote rejected]")
                && (lower.contains("failed to update ref")
                    || lower.contains("cannot lock ref")
                    || lower.contains("incorrect old value")
                    || lower.contains("failed to lock")));
        if raced {
            return Ok(Push::Rejected);
        }
        Err(classify(&format!("push origin {sha}:{MAIN}"), &all))
    }

    /// The compare-and-swap write (`docs/design.md`, D3).
    ///
    /// Fetch, read the state, call `build` (which re-checks the rules and
    /// returns the records to add; it is called again after every lost
    /// race), commit on the fetched tip (after any queued offline commits),
    /// and push. Retries [`RETRIES`] times with jittered backoff, then
    /// fails with `Kind::Remote`.
    ///
    /// Offline (fetch or push classified as offline): when every record is
    /// a plain message, the commit is queued on local `main` and pushed by
    /// the next write; otherwise `Kind::Offline` and nothing is committed.
    /// An empty `build` result only pushes the queue.
    pub fn push_cas<F>(&self, summary: &str, mut build: F) -> Result<Written>
    where
        F: FnMut(&State) -> Result<Vec<Record>>,
    {
        for attempt in 0..=RETRIES {
            let online = match self.fetch() {
                Ok(()) => true,
                Err(e) if e.kind == Kind::Offline => false,
                Err(e) => return Err(e),
            };
            let snap = self.read()?;
            let mut records = build(&snap.state)?;
            records.sort_by(|a, b| a.id().cmp(b.id()));
            let rule_bearing = records.iter().any(Record::rule_bearing);
            if !online && rule_bearing {
                return self.queue_offline(&records, rule_bearing, summary, "fetch");
            }
            snap.state.check_commit(&records)?;
            if !online {
                return self.queue_offline(&records, rule_bearing, summary, "fetch");
            }
            // Rebuild queued commits on the fetched tip, then add ours.
            let mut tip = snap.origin.clone();
            for q in &snap.queued_commits {
                tip = self.commit_files(&q.files, tip.as_deref(), &q.message)?;
            }
            if !records.is_empty() {
                tip = Some(self.commit(&records, tip.as_deref(), summary)?);
            }
            let Some(sha) = tip else {
                return Err(Error::new(Kind::Internal, "nothing to write"));
            };
            if snap.origin.as_deref() == Some(sha.as_str()) {
                return Ok(Written {
                    commit: sha,
                    queued: false,
                });
            }
            match self.push(&sha) {
                Ok(Push::Pushed) => {
                    self.git.run(["update-ref", MAIN, &sha])?;
                    self.git.run(["update-ref", ORIGIN_MAIN, &sha])?;
                    return Ok(Written {
                        commit: sha,
                        queued: false,
                    });
                }
                Ok(Push::Rejected) => {
                    if attempt == RETRIES {
                        break;
                    }
                    let base = 50u64 << attempt;
                    let jitter = random_u64() % (100u64 << attempt);
                    std::thread::sleep(Duration::from_millis(base + jitter));
                }
                Err(e) if e.kind == Kind::Offline => {
                    return self.queue_offline(&records, rule_bearing, summary, "push");
                }
                Err(e) => return Err(e),
            }
        }
        crate::fail!(
            Remote,
            "remote: the push lost the race {} times; nothing was written (try again)",
            RETRIES + 1
        )
    }

    /// Offline: commit plain messages on local `main`; refuse anything else.
    fn queue_offline(
        &self,
        records: &[Record],
        rule_bearing: bool,
        summary: &str,
        step: &str,
    ) -> Result<Written> {
        if rule_bearing {
            crate::fail!(
                Offline,
                "offline: the board remote is unreachable ({step}); {summary} needs the network, nothing was committed"
            );
        }
        let main = self.rev(MAIN)?;
        if records.is_empty() {
            crate::fail!(Offline, "offline: the board remote is unreachable ({step})");
        }
        let sha = self.commit(records, main.as_deref(), summary)?;
        self.git.run(["update-ref", MAIN, &sha])?;
        eprintln!("baton: offline ({step}): committed locally; the next online write pushes it");
        Ok(Written {
            commit: sha,
            queued: true,
        })
    }
}

#[derive(Debug)]
struct CommitFiles {
    sha: String,
    message: String,
    files: Vec<(String, Vec<u8>)>,
}

fn is_record_path(path: &str) -> bool {
    (path.starts_with("messages/") && path.ends_with(".md"))
        || (path.starts_with("events/") && path.ends_with(".json"))
}

/// The id part of a record path (the sort key within one commit).
fn file_id(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().unwrap_or(name)
}

/// Parse `git cat-file --batch` output: `<sha> <type> <size>\n<bytes>\n`.
fn parse_batch(out: &[u8], n: usize) -> Result<Vec<Vec<u8>>> {
    let err = || Error::new(Kind::Internal, "unexpected git cat-file --batch output");
    let mut res = Vec::with_capacity(n);
    let mut i = 0;
    while res.len() < n {
        let nl = out[i..].iter().position(|&b| b == b'\n').ok_or_else(err)? + i;
        let header = std::str::from_utf8(&out[i..nl]).map_err(|_| err())?;
        let size: usize = header
            .split(' ')
            .nth(2)
            .and_then(|s| s.parse().ok())
            .ok_or_else(err)?;
        let start = nl + 1;
        let end = start + size;
        if end > out.len() {
            return Err(err());
        }
        res.push(out[start..end].to_vec());
        i = end + 1;
    }
    Ok(res)
}

fn clone(url: &str, path: &Path) -> Result<()> {
    if path.exists() && path.read_dir()?.next().is_some() {
        crate::fail!(
            State,
            "{} exists and is not a git clone; move it away or set board_clone",
            path.display()
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(Kind::Internal, "board clone path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    Git::new(parent).run([
        "clone".as_ref(),
        "--quiet".as_ref(),
        "--no-checkout".as_ref(),
        url.as_ref(),
        path.as_os_str(),
    ])?;
    Ok(())
}
