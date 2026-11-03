//! Notes sync (`docs/design.md`, "Notes sync"): commit the caller's paths,
//! integrate with `origin/main` out of sight, apply, and push.
//!
//! Invariants, whatever fails:
//! - The notes working tree never gets conflict markers: integration runs in
//!   a private worktree (`.git/baton/wt`), and the main worktree only ever
//!   moves with `git reset --keep`.
//! - No uncommitted edit is lost: collisions are computed (and refused)
//!   before the reset; a snapshot is taken first; the local commit stays.
//! - Only the caller's paths are committed (`git commit --only`).
//! - Push is fast-forward only; never forced.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Kind, Result};
use crate::fail;
use crate::git::{Git, classify};

/// How long `sync` waits for another `baton` holding the notes lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(120);
/// Backups older than this are pruned.
pub const BACKUP_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// Push retries after a lost race (so 1 + 3 attempts).
pub const PUSH_RETRIES: u32 = 3;
/// Default idle time before the sweeper commits.
pub const SWEEP_IDLE: Duration = Duration::from_secs(300);

/// Hooks are disabled for git calls in the private worktree.
const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// What to commit.
#[derive(Debug, Clone)]
pub enum Select {
    /// Changed files under these repo-relative paths.
    Paths(Vec<String>),
    /// Every changed file.
    All,
}

/// One `sync` run.
#[derive(Debug, Clone)]
pub struct Options {
    pub select: Select,
    pub message: Option<String>,
    /// `Baton-Machine:` trailer.
    pub machine: String,
    /// `Baton-Session:` trailer.
    pub session: String,
    /// Command run on the integrated tree; empty = none.
    pub check: Vec<String>,
}

/// One line of output: what a step did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `recovered`, `committed`, `integrated`, `pushed`, `unstaged`, `left-out`.
    pub step: &'static str,
    /// The commit the step produced, or empty.
    pub sha: String,
    pub paths: Vec<String>,
}

/// Everything a run did, printed even when it fails part way.
#[derive(Debug, Default)]
pub struct Report {
    pub items: Vec<Item>,
}

impl Report {
    fn add(&mut self, step: &'static str, sha: &str, paths: Vec<String>) {
        self.items.push(Item {
            step,
            sha: sha.to_owned(),
            paths,
        });
    }

    fn committed(&self) -> Option<&str> {
        self.items
            .iter()
            .rev()
            .find(|i| i.step == "committed")
            .map(|i| i.sha.as_str())
    }
}

/// The apply journal (`.git/baton/journal.json`): written before
/// `reset --keep`, removed after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    /// HEAD before the apply.
    old: String,
    /// The integrated commit being applied.
    new: String,
    /// The snapshot ref taken by this run, if any.
    backup: Option<String>,
    at: String,
}

/// A changed path from `git status`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    path: String,
    untracked: bool,
}

/// Test-only fault injection: `BATON_FAULT=<point>` ends the process at that
/// point, as a crash or kill would, so the journal recovery can be tested
/// against real git.
fn fault(point: &str) {
    if std::env::var("BATON_FAULT").is_ok_and(|v| v == point) {
        eprintln!("baton: BATON_FAULT={point}: stopping");
        std::process::exit(99);
    }
}

/// The notes repo, checked by the preflight.
#[derive(Debug)]
pub struct Repo {
    root: PathBuf,
    git_dir: PathBuf,
    git: Git,
}

/// A held `.git/baton.lock`; released on drop (or process exit).
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

fn split_z(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Do two repo-relative paths overlap (equal, or one is a directory of the other)?
fn overlaps(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b
        || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn list(paths: &[String]) -> String {
    const SHOW: usize = 10;
    let mut s = paths
        .iter()
        .take(SHOW)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOW {
        s.push_str(&format!(" and {} more", paths.len() - SHOW));
    }
    s
}

/// Turn a user path into a repo-relative path with `/` separators.
/// Absolute paths must be inside the repo; `..` and `.git` are refused.
pub fn normalize_path(root: &Path, path: &Path) -> Result<String> {
    let rel: PathBuf = if path.is_absolute() {
        let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        // Canonicalize the deepest existing ancestor, so symlinked roots
        // (`/tmp` on macOS) still match; keep the missing tail as given.
        let mut existing = path.to_path_buf();
        let mut tail = Vec::new();
        while !existing.exists() {
            match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_owned());
                    existing = parent.to_path_buf();
                }
                _ => break,
            }
        }
        let mut full = fs::canonicalize(&existing).unwrap_or(existing);
        full.extend(tail.iter().rev());
        match full
            .strip_prefix(&canon_root)
            .or_else(|_| path.strip_prefix(root))
        {
            Ok(r) => r.to_path_buf(),
            Err(_) => fail!(
                Internal,
                "path {} is outside the notes repo {}",
                path.display(),
                root.display()
            ),
        }
    } else {
        path.to_path_buf()
    };
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(s) => {
                let s = s.to_str().ok_or_else(|| {
                    Error::new(
                        Kind::Internal,
                        format!("path {} is not UTF-8", path.display()),
                    )
                })?;
                parts.push(s.to_owned());
            }
            _ => fail!(
                Internal,
                "path {} must stay inside the notes repo (no `..`)",
                path.display()
            ),
        }
    }
    if parts.is_empty() {
        fail!(
            Internal,
            "path {} is the whole notes repo: name the files you changed (the human uses --all)",
            path.display()
        );
    }
    if parts[0] == ".git" {
        fail!(Internal, "path {} is inside .git", path.display());
    }
    Ok(parts.join("/"))
}

impl Repo {
    /// Preflight: the main worktree of a non-bare, non-shallow repo, on
    /// `main`, with no rebase, merge or other git operation in progress.
    pub fn open(path: &Path) -> Result<Repo> {
        if !path.is_dir() {
            fail!(State, "notes repo {} does not exist", path.display());
        }
        let git = Git::new(path)
            .with_env("GIT_LITERAL_PATHSPECS", "1")
            .with_env("GIT_EDITOR", ":");
        let out = git.output([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
            "--is-shallow-repository",
        ])?;
        if !out.ok() {
            fail!(
                State,
                "notes repo {} is not a git checkout: {}",
                path.display(),
                first_line(&out.stderr)
            );
        }
        let text = out.stdout_str();
        let lines: Vec<&str> = text.lines().collect();
        let [top, git_dir, common, shallow] = lines[..] else {
            fail!(Internal, "unexpected `git rev-parse` output: {text:?}");
        };
        let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let root = PathBuf::from(top);
        if canon(&root) != canon(path) {
            fail!(
                State,
                "notes.repo {} is not the top of its git checkout ({})",
                path.display(),
                root.display()
            );
        }
        let git_dir = PathBuf::from(git_dir);
        if canon(&git_dir) != canon(Path::new(common)) {
            fail!(
                State,
                "notes repo {} is a linked worktree; point notes.repo at the main worktree",
                path.display()
            );
        }
        if shallow == "true" {
            fail!(
                State,
                "notes repo is a shallow clone; run `git fetch --unshallow` in {}",
                root.display()
            );
        }
        let repo = Repo {
            git: git.clone(),
            root,
            git_dir,
        };
        repo.check_state()?;
        Ok(repo)
    }

    /// Branch and in-progress operations; run again under the lock.
    fn check_state(&self) -> Result<()> {
        let head = self.git.output(["symbolic-ref", "-q", "HEAD"])?;
        let head = head.line();
        if head != "refs/heads/main" {
            fail!(
                State,
                "notes repo {} is not on branch main ({}); switch back before syncing",
                self.root.display(),
                if head.is_empty() {
                    "detached HEAD"
                } else {
                    &head
                }
            );
        }
        for (name, what) in [
            ("rebase-merge", "a rebase"),
            ("rebase-apply", "a rebase or am"),
            ("MERGE_HEAD", "a merge"),
            ("CHERRY_PICK_HEAD", "a cherry-pick"),
            ("REVERT_HEAD", "a revert"),
            ("BISECT_LOG", "a bisect"),
        ] {
            if self.git_dir.join(name).exists() {
                fail!(
                    State,
                    "{what} is in progress in {}; finish or abort it first",
                    self.root.display()
                );
            }
        }
        if self.git_dir.join("index.lock").exists() {
            fail!(
                State,
                "{} exists: another git process is running, or one crashed (remove it if no git runs)",
                self.git_dir.join("index.lock").display()
            );
        }
        if !self
            .git
            .output(["rev-parse", "-q", "--verify", "HEAD^{commit}"])?
            .ok()
        {
            fail!(
                State,
                "notes repo {} has no commits yet; make the first commit by hand",
                self.root.display()
            );
        }
        if !self.git.output(["remote", "get-url", "origin"])?.ok() {
            fail!(
                State,
                "notes repo {} has no remote `origin`",
                self.root.display()
            );
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn baton_dir(&self) -> PathBuf {
        self.git_dir.join("baton")
    }

    fn lock_path(&self) -> PathBuf {
        self.git_dir.join("baton.lock")
    }

    fn journal_path(&self) -> PathBuf {
        self.baton_dir().join("journal.json")
    }

    fn wt_path(&self) -> PathBuf {
        self.baton_dir().join("wt")
    }

    fn open_lock_file(&self) -> Result<File> {
        Ok(File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?)
    }

    /// Take `.git/baton.lock`, waiting up to `wait`.
    pub fn lock(&self, wait: Duration) -> Result<Lock> {
        let file = self.open_lock_file()?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= wait {
                        fail!(
                            State,
                            "another baton has held {} for {}s",
                            self.lock_path().display(),
                            wait.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// Take the lock only if it is free.
    pub fn try_lock(&self) -> Result<Option<Lock>> {
        let file = self.open_lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn rev(&self, rev: &str) -> Result<String> {
        Ok(self
            .git
            .run(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?
            .line())
    }

    fn head(&self) -> Result<String> {
        self.rev("HEAD")
    }

    /// Changed files (tracked changes, staged or not, and untracked files),
    /// optionally limited to `pathspec`.
    fn status(&self, pathspec: Option<&[String]>) -> Result<Vec<Change>> {
        let mut args = vec![
            "status".to_owned(),
            "--porcelain=v1".into(),
            "-z".into(),
            "--untracked-files=all".into(),
            "--no-renames".into(),
            "--ignore-submodules=all".into(),
        ];
        if let Some(p) = pathspec {
            args.push("--".into());
            args.extend(p.iter().cloned());
        }
        let out = self.git.run(&args)?;
        Ok(split_z(&out.stdout)
            .into_iter()
            .filter(|e| e.len() > 3)
            .map(|e| Change {
                untracked: e.starts_with("??"),
                path: e[3..].to_owned(),
            })
            .collect())
    }

    /// Ignored files and directories (collapsed), which `reset --keep`
    /// would overwrite without asking.
    fn ignored(&self) -> Result<Vec<String>> {
        let out = self.git.run([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ])?;
        Ok(split_z(&out.stdout))
    }

    fn staged(&self) -> Result<BTreeSet<String>> {
        let out = self
            .git
            .run(["diff", "--cached", "--name-only", "-z", "--no-renames"])?;
        Ok(split_z(&out.stdout).into_iter().collect())
    }

    fn diff_names(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let out = self
            .git
            .run(["diff", "--name-only", "-z", "--no-renames", a, b, "--"])?;
        Ok(split_z(&out.stdout))
    }

    // ---- recovery -------------------------------------------------------

    /// Finish (or report) an apply that a previous run did not finish.
    pub fn recover(&self, report: &mut Report) -> Result<()> {
        let path = self.journal_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let journal: Journal = serde_json::from_str(&text).map_err(|e| {
            Error::new(
                Kind::State,
                format!(
                    "unreadable journal {}: {e}; check the working tree, then delete it",
                    path.display()
                ),
            )
        })?;
        let head = self.head()?;
        if head == journal.new {
            // The reset finished; only the journal was left.
            fs::remove_file(&path)?;
            report.add("recovered", &journal.new, Vec::new());
            return Ok(());
        }
        if head != journal.old {
            fail!(
                State,
                "journal {} expects HEAD {} or {}, but HEAD is {head}; check the notes repo (snapshot: {}), then delete the journal",
                path.display(),
                journal.old,
                journal.new,
                journal.backup.as_deref().unwrap_or("none")
            );
        }
        // HEAD is still `old`. An interrupted reset may have written some of
        // `new`'s files already: they now look like local edits. Stage the
        // ones that match `new` exactly, so `reset --keep` accepts them.
        let incoming = self.diff_names(&journal.old, &journal.new)?;
        let mut ready = BTreeSet::new();
        for p in &incoming {
            let new_blob = self.git.output([
                "rev-parse",
                "-q",
                "--verify",
                &format!("{}:{p}", journal.new),
            ])?;
            let file = self.root.join(p);
            let on_disk = if file.is_file() {
                Some(self.git.run(["hash-object", "--", p])?.line())
            } else {
                None
            };
            match (new_blob.ok().then(|| new_blob.line()), on_disk) {
                (Some(blob), Some(disk)) if blob == disk => {
                    let mode = self
                        .git
                        .run(["ls-tree", "-z", &journal.new, "--", p])?
                        .stdout_str();
                    let mode = mode.split(' ').next().unwrap_or("100644").to_owned();
                    self.git.run([
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("{mode},{blob},{p}"),
                    ])?;
                    ready.insert(p.clone());
                }
                (None, None) => {
                    self.git.run(["update-index", "--force-remove", "--", p])?;
                    ready.insert(p.clone());
                }
                _ => {}
            }
        }
        self.reset_keep(&journal.new, &incoming, &ready)?;
        fs::remove_file(&path)?;
        report.add("recovered", &journal.new, incoming);
        Ok(())
    }

    // ---- snapshot -------------------------------------------------------

    /// Save uncommitted work: `git stash create` under
    /// `refs/baton/backup/<ts>`, untracked files copied to
    /// `.git/baton/backup/<ts>/`. Skipped when nothing changed since the
    /// last snapshot. Returns the ref, if one was made.
    fn snapshot(&self) -> Result<Option<String>> {
        let untracked: Vec<String> = self
            .status(None)?
            .into_iter()
            .filter(|c| c.untracked)
            .map(|c| c.path)
            .collect();
        let stash = self.git.run(["stash", "create"])?.line();
        // Fingerprint: the stash tree plus each untracked file's blob id.
        let mut fingerprint = if stash.is_empty() {
            String::new()
        } else {
            self.git
                .run(["rev-parse", &format!("{stash}^{{tree}}")])?
                .line()
        };
        if !untracked.is_empty() {
            let mut args = vec!["hash-object".to_owned(), "--".to_owned()];
            args.extend(untracked.iter().cloned());
            let blobs = self.git.run(&args)?.stdout_str();
            for (p, b) in untracked.iter().zip(blobs.lines()) {
                fingerprint.push_str(&format!("\n{p}\t{b}"));
            }
        }
        if fingerprint.is_empty() {
            return Ok(None);
        }
        let dir = self.baton_dir();
        fs::create_dir_all(&dir)?;
        let last = dir.join("last-snapshot");
        if fs::read_to_string(&last).is_ok_and(|f| f == fingerprint) {
            return Ok(None);
        }
        let ts = format!("{}-{}", now_secs(), std::process::id());
        let name = format!("refs/baton/backup/{ts}");
        if !stash.is_empty() {
            self.git.run(["update-ref", &name, &stash])?;
        }
        if !untracked.is_empty() {
            let to = dir.join("backup").join(&ts);
            for p in &untracked {
                let src = self.root.join(p);
                let dst = to.join(p);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                if src.is_file() {
                    fs::copy(&src, &dst)?;
                }
            }
        }
        fs::write(&last, fingerprint)?;
        Ok(Some(name))
    }

    /// Delete snapshots older than [`BACKUP_KEEP`]. Best effort.
    fn prune_backups(&self) {
        let cutoff = now_secs().saturating_sub(BACKUP_KEEP.as_secs());
        let old = |name: &str| {
            name.split('-')
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|t| t < cutoff)
        };
        if let Ok(out) = self
            .git
            .run(["for-each-ref", "--format=%(refname)", "refs/baton/backup/"])
        {
            for r in out.stdout_str().lines() {
                if old(r.rsplit('/').next().unwrap_or("")) {
                    let _ = self.git.run(["update-ref", "-d", r]);
                }
            }
        }
        if let Ok(entries) = fs::read_dir(self.baton_dir().join("backup")) {
            for e in entries.flatten() {
                if old(&e.file_name().to_string_lossy()) {
                    let _ = fs::remove_dir_all(e.path());
                }
            }
        }
    }

    // ---- commit ---------------------------------------------------------

    /// Commit exactly `changes` with `commit --only`; other staged and
    /// unstaged work stays as it is. The repo's pre-commit hook runs.
    fn commit(&self, changes: &[Change], opts: &Options, sweep: bool) -> Result<String> {
        let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let untracked: Vec<String> = changes
            .iter()
            .filter(|c| c.untracked)
            .map(|c| c.path.clone())
            .collect();
        if !untracked.is_empty() {
            let mut args = vec!["add".to_owned(), "-N".into(), "--".into()];
            args.extend(untracked.iter().cloned());
            self.git.run(&args)?;
        }
        let message = match &opts.message {
            Some(m) if !m.trim().is_empty() => m.clone(),
            _ => format!(
                "{} {}: {}",
                if sweep { "baton sweep" } else { "baton sync" },
                opts.machine,
                list(&paths)
            ),
        };
        let mut args = vec![
            "commit".to_owned(),
            "--quiet".into(),
            "--only".into(),
            "--no-edit".into(),
            "-m".into(),
            message,
            "--trailer".into(),
            format!("Baton-Machine: {}", opts.machine),
            "--trailer".into(),
            format!("Baton-Session: {}", opts.session),
            "--".into(),
        ];
        args.extend(paths.iter().cloned());
        let out = self.git.output(&args)?;
        if out.ok() {
            return self.head();
        }
        // Undo the intent-to-add entries, so the files are untracked again.
        if !untracked.is_empty() {
            let mut args = vec!["reset".to_owned(), "-q".into(), "--".into()];
            args.extend(untracked);
            let _ = self.git.output(&args);
        }
        let hook = self
            .git
            .output(["rev-parse", "--git-path", "hooks/pre-commit"])?
            .line();
        let hook = if Path::new(&hook).is_absolute() {
            PathBuf::from(hook)
        } else {
            self.root.join(hook)
        };
        if hook.is_file() {
            fail!(
                CheckFailed,
                "commit refused (pre-commit hook): {}; nothing committed",
                first_line(&out.stderr)
            );
        }
        Err(classify("commit", &out.stderr))
    }

    // ---- fetch and integrate --------------------------------------------

    /// Fetch `main`; returns `origin/main`, or `None` for an empty remote.
    fn fetch(&self) -> Result<Option<String>> {
        let out = self.git.output([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ])?;
        if !out.ok() {
            if out.stderr.contains("couldn't find remote ref") {
                return Ok(None);
            }
            return Err(classify("fetch origin main", &out.stderr));
        }
        Ok(Some(self.rev("refs/remotes/origin/main")?))
    }

    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.git.output(["merge-base", a, b])?;
        Ok(out.ok().then(|| out.line()))
    }

    /// Make the private worktree a clean checkout of `sha`.
    fn worktree(&self, sha: &str) -> Result<Git> {
        let path = self.wt_path();
        let wt = Git::new(&path)
            .with_env("GIT_EDITOR", ":")
            .with_env("GIT_LITERAL_PATHSPECS", "1");
        let usable = path.join(".git").is_file()
            && wt
                .output(["rev-parse", "--is-inside-work-tree"])
                .is_ok_and(|o| o.ok());
        if usable {
            let rebase_dir = wt
                .output([
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "rebase-merge",
                ])?
                .line();
            if Path::new(&rebase_dir).exists() {
                let _ = wt.output(NO_HOOKS.iter().copied().chain(["rebase", "--abort"]));
            }
            wt.run(
                NO_HOOKS
                    .iter()
                    .copied()
                    .chain(["checkout", "--quiet", "--detach", "--force", sha]),
            )?;
            wt.run(["clean", "-q", "-ffd"])?;
        } else {
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(self.baton_dir())?;
            self.git.run(["worktree", "prune"])?;
            let p = path.to_string_lossy().into_owned();
            self.git.run(
                NO_HOOKS
                    .iter()
                    .copied()
                    .chain(["worktree", "add", "--quiet", "--detach", "--force", &p, sha]),
            )?;
        }
        Ok(wt)
    }

    /// Rebase the local commits (`head` not in `upstream`) onto `upstream`
    /// in the private worktree. A conflict aborts with exit 21.
    fn rebase(&self, head: &str, upstream: &str) -> Result<String> {
        let wt = self.worktree(head)?;
        let out = wt.output(NO_HOOKS.iter().copied().chain([
            "-c",
            "rebase.autoStash=false",
            "-c",
            "rebase.updateRefs=false",
            "-c",
            "rebase.autoSquash=false",
            "rebase",
            "--quiet",
            upstream,
        ]))?;
        if out.ok() {
            return Ok(wt.run(["rev-parse", "HEAD"])?.line());
        }
        let conflicted = wt
            .output(["diff", "--name-only", "-z", "--diff-filter=U"])
            .map(|o| split_z(&o.stdout))
            .unwrap_or_default();
        let _ = wt.output(NO_HOOKS.iter().copied().chain(["rebase", "--abort"]));
        if conflicted.is_empty() {
            fail!(
                Internal,
                "rebase onto origin/main failed: {}",
                first_line(&out.stderr)
            );
        }
        fail!(
            SyncConflict,
            "local commits conflict with origin/main in {}; nothing applied, local commit kept (merge by hand: git pull --rebase)",
            list(&conflicted)
        )
    }

    /// Run the check command in the private worktree at `sha`.
    fn run_check(&self, check: &[String], sha: &str) -> Result<()> {
        let wt = self.worktree(sha)?;
        self.run_check_in(check, wt.dir())
    }

    fn run_check_in(&self, check: &[String], dir: &Path) -> Result<()> {
        let Some((program, args)) = check.split_first() else {
            return Ok(());
        };
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .env("BATON_NOTES_REPO", &self.root)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| {
                Error::new(
                    Kind::CheckFailed,
                    format!("cannot run check {program}: {e}"),
                )
            })?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let why = if stderr.trim().is_empty() {
            first_line(&stdout)
        } else {
            first_line(&stderr)
        };
        fail!(CheckFailed, "check `{}` failed: {why}", check.join(" "))
    }

    // ---- apply ----------------------------------------------------------

    /// `git reset --keep new`, refusing first (exit 20, files named) when an
    /// uncommitted edit, untracked or ignored file collides with `incoming`.
    /// `ready` are paths already staged at their `new` content (recovery).
    fn reset_keep(&self, new: &str, incoming: &[String], ready: &BTreeSet<String>) -> Result<()> {
        let mut dirty: Vec<String> = self.status(None)?.into_iter().map(|c| c.path).collect();
        dirty.extend(self.ignored()?);
        let mut collide: Vec<String> = dirty
            .iter()
            .filter(|d| !ready.contains(*d) && incoming.iter().any(|i| overlaps(d, i)))
            .cloned()
            .collect();
        collide.sort();
        collide.dedup();
        if !collide.is_empty() {
            fail!(
                SyncCollision,
                "uncommitted or untracked files collide with incoming changes: {}; nothing applied (commit or move them, then sync again)",
                list(&collide)
            );
        }
        let out = self.git.output(["reset", "--quiet", "--keep", new])?;
        if !out.ok() {
            let named: Vec<String> = out
                .stderr
                .lines()
                .filter_map(|l| l.split('\'').nth(1).map(str::to_owned))
                .collect();
            if named.is_empty() {
                fail!(
                    State,
                    "git reset --keep refused: {}; nothing applied",
                    first_line(&out.stderr)
                );
            }
            fail!(
                SyncCollision,
                "uncommitted files collide with incoming changes: {}; nothing applied",
                list(&named)
            );
        }
        Ok(())
    }

    /// Move the main worktree from `old` to `new`, under the journal.
    fn apply(&self, old: &str, new: &str, backup: Option<&str>, report: &mut Report) -> Result<()> {
        if self.head()? != old {
            fail!(
                State,
                "HEAD moved during sync (expected {old}); nothing applied, run baton sync again"
            );
        }
        let incoming = self.diff_names(old, new)?;
        let staged_before = self.staged()?;
        let journal = Journal {
            version: 1,
            old: old.to_owned(),
            new: new.to_owned(),
            backup: backup.map(str::to_owned),
            at: jiff::Timestamp::now().to_string(),
        };
        let path = self.journal_path();
        fs::create_dir_all(self.baton_dir())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&journal).map_err(anyhow::Error::from)?,
        )?;
        fs::rename(&tmp, &path)?;
        fault("after-journal");
        if let Err(e) = self.reset_keep(new, &incoming, &BTreeSet::new()) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        fault("after-reset");
        fs::remove_file(&path)?;
        report.add("integrated", new, incoming);
        let staged_after = self.staged()?;
        let unstaged: Vec<String> = staged_before.difference(&staged_after).cloned().collect();
        if !unstaged.is_empty() {
            eprintln!(
                "baton: unstaged (edits kept in the working tree): {}",
                list(&unstaged)
            );
            report.add("unstaged", "", unstaged);
        }
        Ok(())
    }

    // ---- push -----------------------------------------------------------

    /// Fast-forward push; `Ok(false)` when another machine won the race.
    fn push(&self, sha: &str) -> Result<bool> {
        let out = self.git.output([
            "push",
            "--quiet",
            "--porcelain",
            "--no-follow-tags",
            "origin",
            &format!("{sha}:refs/heads/main"),
        ])?;
        if out.ok() {
            return Ok(true);
        }
        let all = format!("{}\n{}", out.stdout_str(), out.stderr);
        let race = [
            "(fetch first)",
            "(non-fast-forward)",
            "incorrect old value provided",
            "cannot lock ref",
            "failed to update ref",
        ];
        if race.iter().any(|m| all.contains(m)) {
            return Ok(false);
        }
        Err(classify("push origin main", &out.stderr))
    }

    // ---- the whole run --------------------------------------------------

    /// Fetch, integrate, check, apply and push, retrying a lost race.
    fn integrate_and_push(
        &self,
        check: &[String],
        backup: Option<&str>,
        report: &mut Report,
    ) -> Result<()> {
        let mut races = 0;
        loop {
            let head = self.head()?;
            let upstream = self.fetch()?;
            let (new, publish) = match upstream.as_deref() {
                None => (head.clone(), true),
                Some(up) if up == head => (head.clone(), false),
                Some(up) => match self.merge_base(&head, up)? {
                    None => fail!(
                        State,
                        "notes repo and origin/main have unrelated histories; merge them by hand"
                    ),
                    Some(base) if base == up => (head.clone(), true),
                    Some(base) if base == head => (up.to_owned(), false),
                    Some(_) => (self.rebase(&head, up)?, true),
                },
            };
            if publish && !check.is_empty() {
                self.run_check(check, &new).map_err(|e| {
                    Error::new(
                        e.kind,
                        format!("{e}; nothing applied or pushed, local commit kept"),
                    )
                })?;
            }
            if new != head {
                self.apply(&head, &new, backup, report)?;
            }
            if !publish {
                return Ok(());
            }
            if self.push(&new)? {
                report.add("pushed", &new, Vec::new());
                return Ok(());
            }
            races += 1;
            if races > PUSH_RETRIES {
                fail!(
                    Remote,
                    "remote: push lost the race {} times; local commit kept, run baton sync again",
                    races
                );
            }
            std::thread::sleep(jitter(races));
        }
    }

    /// One sync under the lock.
    pub fn sync_locked(&self, opts: &Options, report: &mut Report) -> Result<()> {
        self.check_state()?;
        self.recover(report)?;
        let changes = match &opts.select {
            Select::Paths(p) => self.status(Some(p))?,
            Select::All => self.status(None)?,
        };
        self.commit_and_integrate(&changes, opts, false, report)
    }

    fn commit_and_integrate(
        &self,
        changes: &[Change],
        opts: &Options,
        sweep: bool,
        report: &mut Report,
    ) -> Result<()> {
        let backup = self.snapshot()?;
        if !changes.is_empty() {
            let sha = self.commit(changes, opts, sweep)?;
            report.add(
                "committed",
                &sha,
                changes.iter().map(|c| c.path.clone()).collect(),
            );
        }
        self.prune_backups();
        self.integrate_and_push(&opts.check, backup.as_deref(), report)
            .map_err(|e| match report.committed() {
                Some(sha) if !e.message.contains("local commit kept") => Error::new(
                    e.kind,
                    format!("{e} (local commit {} kept)", &sha[..sha.len().min(12)]),
                ),
                _ => e,
            })
    }

    /// One sweeper pass under the lock: commit every change once the repo
    /// has been idle for `idle`, leaving out files that fail the check.
    /// Returns the files left out.
    pub fn sweep_locked(
        &self,
        opts: &Options,
        idle: Duration,
        report: &mut Report,
    ) -> Result<Vec<String>> {
        self.check_state()?;
        self.recover(report)?;
        let changes = self.status(None)?;
        let now = SystemTime::now();
        let busy = changes.iter().any(|c| {
            fs::symlink_metadata(self.root.join(&c.path))
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() < idle)
        });
        if busy {
            return Ok(Vec::new());
        }
        let mut left_out = Vec::new();
        let mut keep = changes.clone();
        if !changes.is_empty() && !opts.check.is_empty() && !self.trial(&opts.check, &changes)? {
            keep.clear();
            for c in &changes {
                if self.trial(&opts.check, std::slice::from_ref(c))? {
                    keep.push(c.clone());
                } else {
                    left_out.push(c.path.clone());
                }
            }
        }
        if !left_out.is_empty() {
            report.add("left-out", "", left_out.clone());
        }
        self.commit_and_integrate(&keep, opts, true, report)?;
        Ok(left_out)
    }

    /// Does the check pass on HEAD plus the working-tree version of `changes`?
    fn trial(&self, check: &[String], changes: &[Change]) -> Result<bool> {
        let head = self.head()?;
        let wt = self.worktree(&head)?;
        for c in changes {
            let src = self.root.join(&c.path);
            let dst = wt.dir().join(&c.path);
            if src.is_file() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dst)?;
            } else if dst.is_file() {
                fs::remove_file(&dst)?;
            }
        }
        match self.run_check_in(check, wt.dir()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind == Kind::CheckFailed => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Backoff before retry `n`: 100-400 ms times `n`, jittered.
fn jitter(n: u32) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let r = u64::from(nanos ^ std::process::id().wrapping_mul(2_654_435_761)) % 300;
    Duration::from_millis((100 + r) * u64::from(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules() {
        assert!(overlaps("a/b.md", "a/b.md"));
        assert!(overlaps("a/", "a/b.md"));
        assert!(overlaps("a", "a/b.md"));
        assert!(overlaps("a/b.md", "a"));
        assert!(!overlaps("a/b.md", "a/bc.md"));
        assert!(!overlaps("ab", "a"));
    }

    #[test]
    fn normalizes_paths() {
        let root = Path::new("/nonexistent/notes");
        assert_eq!(
            normalize_path(root, Path::new("./a//b.md")).unwrap(),
            "a/b.md"
        );
        assert_eq!(
            normalize_path(root, Path::new("/nonexistent/notes/x/y.md")).unwrap(),
            "x/y.md"
        );
        assert!(normalize_path(root, Path::new("../x.md")).is_err());
        assert!(normalize_path(root, Path::new("/elsewhere/x.md")).is_err());
        assert!(normalize_path(root, Path::new(".git/config")).is_err());
        assert!(normalize_path(root, Path::new(".")).is_err());
    }

    #[test]
    fn list_truncates() {
        let many: Vec<String> = (0..12).map(|i| format!("f{i}")).collect();
        assert!(list(&many).ends_with("and 2 more"));
    }
}
