//! Setup commands: init, doctor. Owner: agent C (`docs/plan.md`).
//!
//! `init` writes only inside baton's own directories (config, data, state);
//! it prints what to add to Claude Code's settings and how to install the
//! sweeper, and installs nothing itself. `doctor` only reports.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Args, ValueEnum};
use serde_json::{Value, json};

use crate::Format;
use crate::config::{Config, Notes, config_dir, home};
use crate::error::{Error, Kind, Result};
use crate::git::Git;
use crate::hooks::sessions_dir;
use crate::out;
use crate::output::envelope;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Os {
    Linux,
    Macos,
}

impl Os {
    fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// This machine's name.
    #[arg(long)]
    pub machine: String,
    /// Every machine's name (comma-separated), this one included.
    #[arg(long, value_delimiter = ',')]
    pub machines: Vec<String>,
    /// The human's name.
    #[arg(long)]
    pub human: String,
    /// The board repo's remote URL.
    #[arg(long)]
    pub board: String,
    /// Optional notes repo to share with `baton sync`.
    #[arg(long)]
    pub notes: Option<String>,
    /// Optional check command for the notes repo (repeat for each argument).
    #[arg(long)]
    pub notes_check: Vec<String>,
    /// Which sweeper unit to write (default: this system's).
    #[arg(long, value_enum)]
    pub os: Option<Os>,
}

/// Quote for a POSIX shell when needed.
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn exe() -> Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(p.canonicalize().unwrap_or(p))
}

/// The settings to merge into `~/.claude/settings.json`.
/// Agents change the board only through `baton`: file edits in the board
/// clone are denied.
pub fn claude_settings(exe: &Path, board: &Path) -> Value {
    let bin = sh_quote(&exe.to_string_lossy());
    let board = board.to_string_lossy();
    let board = board.trim_start_matches('/');
    json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("{bin} hook wait"),
                    "asyncRewake": true,
                    "timeout": 3600
                }]
            }]
        },
        "permissions": {
            "allow": [
                "Bash(baton inbox:*)",
                "Bash(baton show:*)",
                "Bash(baton tasks:*)",
                "Bash(baton wait:*)",
                "Bash(baton doctor:*)",
                "Bash(baton post:*)",
                "Bash(baton take:*)",
                "Bash(baton close:*)",
                "Bash(baton release:*)"
            ],
            "deny": [format!("Edit(//{board}/**)")]
        }
    })
}

/// `BATON_*_DIR` overrides in effect, for the sweeper's environment.
fn dir_overrides() -> Vec<(&'static str, String)> {
    ["BATON_CONFIG_DIR", "BATON_DATA_DIR", "BATON_STATE_DIR"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k, v)))
        .collect()
}

fn systemd_units(exe: &Path) -> (String, String) {
    let mut env = String::new();
    for (k, v) in dir_overrides() {
        env.push_str(&format!("Environment=\"{k}={v}\"\n"));
    }
    let service = format!(
        "[Unit]\n\
         Description=baton notes sweeper (one pass)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} sweep\n\
         {env}\
         # An SSH remote needs your agent; for example:\n\
         #Environment=SSH_AUTH_SOCK=%t/ssh-agent.socket\n",
        sh_quote(&exe.to_string_lossy())
    );
    let timer = "[Unit]\n\
                 Description=Run baton sweep every minute\n\
                 \n\
                 [Timer]\n\
                 OnBootSec=1min\n\
                 OnUnitActiveSec=1min\n\
                 AccuracySec=5s\n\
                 \n\
                 [Install]\n\
                 WantedBy=timers.target\n"
        .to_owned();
    (service, timer)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub const LAUNCHD_LABEL: &str = "dev.baton.sweep";

fn launchd_plist(exe: &Path, home: &Path) -> String {
    let mut env = String::from(
        "    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>\n",
    );
    for (k, v) in dir_overrides() {
        env.push_str(&format!("    <key>{k}</key><string>{}</string>\n", xml(&v)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>sweep</string>
  </array>
  <key>StartInterval</key><integer>60</integer>
  <key>RunAtLoad</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(&exe.to_string_lossy()),
        xml(&home.join("Library/Logs/baton-sweep.log").to_string_lossy()),
    )
}

pub fn init(args: InitArgs) -> Result<()> {
    let path = Config::path()?;
    if path.exists() {
        crate::fail!(
            Internal,
            "{} exists; `baton init` never overwrites it. Edit it, or move it away and run init again",
            path.display()
        );
    }
    if args.notes.is_none() && !args.notes_check.is_empty() {
        crate::fail!(Internal, "--notes-check needs --notes");
    }
    let config = Config {
        machine: args.machine.clone(),
        machines: args.machines.clone(),
        human: args.human.clone(),
        board: args.board.clone(),
        board_clone: None,
        notes: args.notes.as_ref().map(|repo| Notes {
            repo: repo.clone(),
            check: args.notes_check.clone(),
        }),
    };
    config.validate()?;
    if let Some(repo) = config.notes_repo()?
        && !repo.join(".git").exists()
    {
        crate::fail!(
            Internal,
            "notes repo {} is not a git checkout",
            repo.display()
        );
    }
    // Clone first: a failed clone leaves no config behind.
    let store = Store::open(&config, "")?;
    let text = toml::to_string(&config).map_err(anyhow::Error::from)?;
    std::fs::create_dir_all(config_dir()?)?;
    {
        use std::io::Write as _;
        let mut f = File::create_new(&path)
            .map_err(|e| Error::new(Kind::Internal, format!("{}: {e}", path.display())))?;
        f.write_all(text.as_bytes())?;
    }
    let exe = exe()?;
    out!("baton: wrote {}", path.display());
    out!("baton: board cloned in {}", store.path().display());
    out!();
    out!("1. Merge into ~/.claude/settings.json (keep the hooks you already have):");
    out!();
    out!(
        "{}",
        serde_json::to_string_pretty(&claude_settings(&exe, store.path()))
            .map_err(anyhow::Error::from)?
    );
    out!();
    out!("2. Install the /baton skill (the plugin in baton's repo, `plugin/`):");
    out!("   claude plugin marketplace add <path or URL of the baton repo>");
    out!("   claude plugin install baton@baton");

    if config.notes.is_some() {
        let units = config_dir()?.join("units");
        std::fs::create_dir_all(&units)?;
        out!();
        out!("3. The notes sweeper runs `baton sweep` every minute. Files written:");
        match args.os.unwrap_or_else(Os::current) {
            Os::Linux => {
                let (service, timer) = systemd_units(&exe);
                let s = units.join("baton-sweep.service");
                let t = units.join("baton-sweep.timer");
                std::fs::write(&s, service)?;
                std::fs::write(&t, timer)?;
                out!("   {}", s.display());
                out!("   {}", t.display());
                out!("   Install them with:");
                out!("   mkdir -p ~/.config/systemd/user");
                out!(
                    "   cp {} {} ~/.config/systemd/user/",
                    sh_quote(&s.to_string_lossy()),
                    sh_quote(&t.to_string_lossy())
                );
                out!("   systemctl --user daemon-reload");
                out!("   systemctl --user enable --now baton-sweep.timer");
                out!(
                    "   For an SSH remote, the service needs SSH_AUTH_SOCK (see the comment in the service file)."
                );
            }
            Os::Macos => {
                let p = units.join(format!("{LAUNCHD_LABEL}.plist"));
                std::fs::write(&p, launchd_plist(&exe, &home()?))?;
                out!("   {}", p.display());
                out!("   Install it with:");
                out!("   mkdir -p ~/Library/LaunchAgents");
                out!(
                    "   cp {} ~/Library/LaunchAgents/",
                    sh_quote(&p.to_string_lossy())
                );
                out!(
                    "   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{LAUNCHD_LABEL}.plist"
                );
            }
        }
    }
    out!();
    out!("Then check the setup with `baton doctor`.");
    Ok(())
}

// ---------------------------------------------------------------------------
// doctor

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

struct Report {
    checks: Vec<(Status, &'static str, String)>,
}

impl Report {
    fn add(&mut self, s: Status, name: &'static str, detail: impl Into<String>) {
        self.checks.push((s, name, detail.into()));
    }
}

/// Is `url` reached over SSH?
fn ssh_url(url: &str) -> bool {
    if url.starts_with("ssh://") || url.starts_with("git+ssh://") {
        return true;
    }
    if url.contains("://") || url.starts_with('/') || url.starts_with('.') {
        return false;
    }
    // scp-like `host:path` (a colon before any slash).
    match (url.find(':'), url.find('/')) {
        (Some(c), Some(s)) => c < s,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Is the lock file at `path` held by another process? `None` when absent.
fn lock_held(path: &Path) -> Option<bool> {
    let f = File::options().write(true).open(path).ok()?;
    match f.try_lock() {
        Ok(()) => Some(false),
        Err(std::fs::TryLockError::WouldBlock) => Some(true),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
fn claude_settings_path() -> Result<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Ok(PathBuf::from(d).join("settings.json")),
        _ => Ok(home()?.join(".claude/settings.json")),
    }
}

/// Hook entries for `event` as `(matcher, handler)` pairs.
fn hook_entries<'a>(settings: &'a Value, event: &str) -> Vec<(&'a str, &'a Value)> {
    let mut out = Vec::new();
    if let Some(groups) = settings["hooks"][event].as_array() {
        for g in groups {
            let matcher = g["matcher"].as_str().unwrap_or("");
            if let Some(hs) = g["hooks"].as_array() {
                for h in hs {
                    out.push((matcher, h));
                }
            }
        }
    }
    out
}

fn is_baton(h: &Value, sub: &str) -> bool {
    h["command"]
        .as_str()
        .is_some_and(|c| c.contains("baton") && c.contains(sub))
}

/// The program of a hook command, when it is an absolute path that is missing.
fn missing_program(h: &Value) -> Option<String> {
    let cmd = h["command"].as_str()?;
    let first = if let Some(rest) = cmd.strip_prefix('\'') {
        rest.split('\'').next()?.to_owned()
    } else {
        cmd.split_whitespace().next()?.to_owned()
    };
    (first.starts_with('/') && !Path::new(&first).exists()).then_some(first)
}

fn check_hooks(r: &mut Report) {
    let path = match claude_settings_path() {
        Ok(p) => p,
        Err(e) => return r.add(Status::Warn, "hooks", e.message),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return r.add(
                Status::Warn,
                "hooks",
                format!(
                    "{} not found; `baton init` prints the hooks to add",
                    path.display()
                ),
            );
        }
    };
    let settings: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return r.add(Status::Fail, "hooks", format!("{}: {e}", path.display())),
    };
    let stop = hook_entries(&settings, "Stop");
    match stop.iter().find(|(_, h)| is_baton(h, "hook wait")) {
        None => r.add(
            Status::Warn,
            "hook-wait",
            "no Stop hook runs `baton hook wait`: sessions are not woken for replies",
        ),
        Some((_, h)) => {
            if let Some(p) = missing_program(h) {
                r.add(Status::Fail, "hook-wait", format!("{p} does not exist"));
            } else if h["asyncRewake"] != json!(true) {
                r.add(
                    Status::Fail,
                    "hook-wait",
                    "the Stop hook needs \"asyncRewake\": true (without it the session blocks)",
                );
            } else if h["timeout"].as_u64() != Some(3600) {
                r.add(
                    Status::Warn,
                    "hook-wait",
                    "the Stop hook's timeout is not 3600; the waiter stops one minute before an hour",
                );
            } else {
                r.add(Status::Ok, "hook-wait", "Stop hook with asyncRewake");
            }
        }
    }
    let pre = hook_entries(&settings, "PreToolUse");
    if pre.iter().any(|(_, h)| is_baton(h, "hook guard")) {
        r.add(
            Status::Warn,
            "hook-guard",
            "the PreToolUse `baton hook guard` hook is obsolete (a no-op); remove it",
        );
    }
}

/// The agent is only one way to hold the SSH key: a key file without a
/// passphrase, or the macOS Keychain, works too. So an agent without keys
/// warns only when the board remote did not answer.
fn check_ssh(r: &mut Report, url: &str, reachable: bool) {
    if !ssh_url(url) {
        r.add(
            Status::Ok,
            "ssh-agent",
            "not needed (the board is not an SSH remote)",
        );
        return;
    }
    match agent_state() {
        Ok(detail) => r.add(Status::Ok, "ssh-agent", detail),
        Err(problem) if reachable => r.add(
            Status::Ok,
            "ssh-agent",
            format!("{problem}, but SSH reaches the board without it"),
        ),
        Err(problem) => r.add(
            Status::Warn,
            "ssh-agent",
            format!("{problem}: load a key with `ssh-add`, or use a key file without a passphrase"),
        ),
    }
}

/// The keys the SSH agent holds, or why it holds none.
fn agent_state() -> std::result::Result<String, String> {
    if std::env::var_os("SSH_AUTH_SOCK").is_none_or(|s| s.is_empty()) {
        return Err("SSH_AUTH_SOCK is not set".into());
    }
    let out = Command::new("ssh-add")
        .arg("-l")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let n = String::from_utf8_lossy(&o.stdout).lines().count();
            Ok(format!("{n} key(s) loaded"))
        }
        Ok(o) if o.status.code() == Some(1) => Err("the agent holds no keys".into()),
        Ok(_) => Err("cannot reach the agent at SSH_AUTH_SOCK".into()),
        Err(e) => Err(format!("cannot run ssh-add: {e}")),
    }
}

fn check_board(r: &mut Report, config: &Config) -> bool {
    let clone = match config.board_clone() {
        Ok(p) => p,
        Err(e) => {
            r.add(Status::Fail, "board-clone", e.message);
            return false;
        }
    };
    if !clone.join(".git").exists() {
        r.add(
            Status::Fail,
            "board-clone",
            format!(
                "{} is missing; any board command (or `baton init`) clones it",
                clone.display()
            ),
        );
        return false;
    }
    let git = Git::new(&clone);
    let url = git
        .output(["config", "--get", "remote.origin.url"])
        .map(|o| o.line())
        .unwrap_or_default();
    if url != config.board {
        r.add(
            Status::Fail,
            "board-clone",
            format!(
                "{} has origin {url:?}, the config says {:?}",
                clone.display(),
                config.board
            ),
        );
    } else if clone.join(".git/shallow").exists() {
        r.add(
            Status::Fail,
            "board-clone",
            format!("{} is shallow; re-clone it", clone.display()),
        );
    } else {
        r.add(Status::Ok, "board-clone", clone.display().to_string());
    }
    let reachable = match git.run(["ls-remote", "origin", "refs/heads/main"]) {
        Ok(o) => {
            match o.stdout_str().split_whitespace().next() {
                Some(sha) => r.add(
                    Status::Ok,
                    "board-remote",
                    format!("reachable, main at {}", &sha[..sha.len().min(12)]),
                ),
                None => r.add(Status::Ok, "board-remote", "reachable, no messages yet"),
            }
            true
        }
        Err(e) => {
            r.add(Status::Fail, "board-remote", e.message);
            false
        }
    };
    check_queue(r, config);
    // Locks and leftovers.
    let mut notes = Vec::new();
    if lock_held(&clone.join(".git/baton.lock")) == Some(true) {
        notes.push("the board lock is held by another baton process".to_owned());
    }
    let leftovers = std::fs::read_dir(clone.join(".git"))
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("baton-index-"))
                .count()
        })
        .unwrap_or(0);
    if leftovers > 0 {
        notes.push(format!(
            "{leftovers} leftover temporary index file(s) in {}/.git (baton-index-*; safe to delete when no baton runs)",
            clone.display()
        ));
    }
    if notes.is_empty() {
        r.add(Status::Ok, "board-locks", "free");
    } else {
        r.add(Status::Warn, "board-locks", notes.join("; "));
    }
    reachable
}

fn check_notes(r: &mut Report, config: &Config) {
    let repo = match config.notes_repo() {
        Ok(Some(p)) => p,
        Ok(None) => return r.add(Status::Ok, "notes", "not configured"),
        Err(e) => return r.add(Status::Fail, "notes", e.message),
    };
    let git_dir = repo.join(".git");
    if !git_dir.is_dir() {
        return r.add(
            Status::Fail,
            "notes",
            format!("{} is not a git checkout (main worktree)", repo.display()),
        );
    }
    let branch = Git::new(&repo)
        .output(["symbolic-ref", "--short", "-q", "HEAD"])
        .map(|o| o.line())
        .unwrap_or_default();
    if branch != "main" {
        r.add(
            Status::Fail,
            "notes",
            format!("{} is on {branch:?}, not main", repo.display()),
        );
    } else {
        r.add(Status::Ok, "notes", repo.display().to_string());
    }
    let journal = git_dir.join("baton/journal.json");
    if journal.exists() {
        r.add(
            Status::Warn,
            "notes-journal",
            format!(
                "{} exists: a sync was interrupted; the next `baton sync` finishes or reports it",
                journal.display()
            ),
        );
    } else {
        r.add(Status::Ok, "notes-journal", "none");
    }
    if lock_held(&git_dir.join("baton.lock")) == Some(true) {
        r.add(
            Status::Warn,
            "notes-lock",
            "held by another process (a sync running now, or a stuck one)",
        );
    } else {
        r.add(Status::Ok, "notes-lock", "free");
    }
}

/// Posts queued on local `main`: try to push them, report what is left.
/// Skipped (no line) while another baton holds the board lock.
fn check_queue(r: &mut Report, config: &Config) {
    let Ok(store) = Store::open(config, "") else {
        return;
    };
    let lock = store.path().join(".git/baton.lock");
    if lock_held(&lock) == Some(true) {
        return;
    }
    let Ok(_l) = store.lock() else { return };
    let before = match store.read() {
        Ok(s) => s.queued,
        Err(e) => return r.add(Status::Fail, "board-queue", e.message),
    };
    if before == 0 {
        return r.add(Status::Ok, "board-queue", "no queued posts");
    }
    match store.deliver_queued() {
        Ok(0) => r.add(
            Status::Ok,
            "board-queue",
            format!("pushed {before} queued commit(s)"),
        ),
        Ok(n) => r.add(
            Status::Fail,
            "board-queue",
            format!("{n} commit(s) still queued locally"),
        ),
        Err(e) => r.add(
            Status::Fail,
            "board-queue",
            format!(
                "{before} commit(s) queued locally, not pushed: {}",
                e.message
            ),
        ),
    }
}

fn check_waiters(r: &mut Report) {
    let Ok(dir) = sessions_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return r.add(Status::Ok, "waiters", "none");
    };
    let mut waiting = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".lock") && name != "arming.lock" && lock_held(&e.path()) == Some(true) {
            waiting += 1;
        }
    }
    r.add(
        Status::Ok,
        "waiters",
        format!("{waiting} session(s) waiting"),
    );
}

pub fn doctor(format: Format) -> Result<()> {
    let mut r = Report { checks: Vec::new() };
    let config = match Config::load() {
        Ok(c) => {
            r.add(
                Status::Ok,
                "config",
                format!(
                    "{}: machine {} of {}",
                    Config::path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    c.machine,
                    c.machines.join(",")
                ),
            );
            Some(c)
        }
        Err(e) => {
            r.add(Status::Fail, "config", e.message);
            None
        }
    };
    if let Some(c) = &config {
        let reachable = check_board(&mut r, c);
        check_ssh(&mut r, &c.board, reachable);
        check_notes(&mut r, c);
    }
    check_hooks(&mut r);
    check_waiters(&mut r);

    match format {
        Format::Json => {
            let items: Vec<Value> = r
                .checks
                .iter()
                .map(|(s, n, d)| json!({"status": s.as_str(), "check": n, "detail": d}))
                .collect();
            out!("{}", envelope("doctor", Value::Array(items)));
        }
        Format::Text => {
            out!("# status\tcheck\tdetail");
            for (s, n, d) in &r.checks {
                out!("{}\t{n}\t{}", s.as_str(), crate::output::field(d));
            }
        }
    }
    let failed = r.checks.iter().filter(|c| c.0 == Status::Fail).count();
    if failed > 0 {
        crate::fail!(Internal, "doctor: {failed} check(s) failed");
    }
    Ok(())
}
