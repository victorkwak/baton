//! Claude Code hooks: `hook wait` (Stop, asyncRewake) and `hook guard`
//! (PreToolUse). Owner: agent C (`docs/plan.md`). Behaviour:
//! `docs/design.md`, "Claude Code hooks". No SessionStart hook: work is
//! picked up only with `/baton`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::board::{MsgState, State};
use crate::cmd::board::poll_interval;
use crate::config::Config;
use crate::error::{Kind, Result};
use crate::git::Git;
use crate::hooks::{
    Arming, GitCall, Invocation, Loc, NOTES_READ_ONLY, canonical, hook_keys, invocations, locate,
    parse_push, path_under, sessions_dir, write_atomic,
};
use crate::output::field;
use crate::record::MsgKind;
use crate::store::Store;

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// Stop (asyncRewake): wait for an actionable message; exit 2 to wake.
    Wait,
    /// PreToolUse: guard git and file writes.
    Guard,
}

pub fn hook(cmd: HookCommand) -> Result<()> {
    match cmd {
        HookCommand::Wait => wait(),
        HookCommand::Guard => guard(),
    }
}

/// The fields of the hook's stdin JSON that `baton` reads.
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
}

/// Read stdin JSON; `None` when stdin is a terminal, empty or not JSON.
fn read_input() -> Option<HookInput> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

fn env_secs(name: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

// ---------------------------------------------------------------------------
// hook wait

/// The waiter's own state: `sessions/<key>.wait.json`, written under every key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WaitState {
    /// Board position (`Snapshot::remote_len`) seen so far.
    #[serde(default)]
    cursor: Option<usize>,
    /// Wake key (task, or `reply:<id>`) → unix seconds of its last wake.
    #[serde(default)]
    last_wake: BTreeMap<String, i64>,
}

impl WaitState {
    fn path(dir: &Path, key: &str) -> PathBuf {
        dir.join(format!("{key}.wait.json"))
    }

    /// The state of every key, merged: the furthest cursor, the latest wakes.
    /// (A restarted session may reach the waiter under another key.)
    fn load(dir: &Path, keys: &[String]) -> WaitState {
        let mut all = WaitState::default();
        for k in keys {
            let Some(s) = std::fs::read_to_string(Self::path(dir, k))
                .ok()
                .and_then(|t| serde_json::from_str::<WaitState>(&t).ok())
            else {
                continue;
            };
            all.cursor = all.cursor.max(s.cursor);
            for (task, t) in s.last_wake {
                let e = all.last_wake.entry(task).or_insert(t);
                *e = (*e).max(t);
            }
        }
        all
    }

    fn save(&self, dir: &Path, keys: &[String]) -> Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from)?;
        for k in keys {
            write_atomic(&Self::path(dir, k), &text)?;
        }
        Ok(())
    }
}

/// Does `m` wake a session armed with `arming`? Open, addressed to this
/// machine (or an unclaimed `any`), not this machine's own, not a plan or
/// `close-task`, and in an armed task (or a reply to an armed message),
/// added after that arming.
fn wakes(m: &MsgState, arming: &Arming, machine: &str) -> bool {
    if !m.actionable_for(machine) || matches!(m.msg.kind, MsgKind::Plan | MsgKind::CloseTask) {
        return false;
    }
    let by_task =
        !m.msg.task.is_empty() && arming.tasks.get(&m.msg.task).is_some_and(|p| m.seq >= *p);
    let by_reply = !m.msg.reply_to.is_empty()
        && arming
            .messages
            .get(&m.msg.reply_to)
            .is_some_and(|p| m.seq >= *p);
    by_task || by_reply
}

fn wake_key(m: &MsgState) -> String {
    if m.msg.task.is_empty() {
        format!("reply:{}", m.msg.reply_to)
    } else {
        m.msg.task.clone()
    }
}

/// Anything left to wait for: an armed task still open, or an armed
/// message outside a closed task.
fn live(arming: &Arming, state: &State) -> bool {
    let open = |task: &str| task.is_empty() || state.task(task).is_none_or(|t| !t.closed);
    arming.tasks.keys().any(|t| open(t))
        || arming
            .messages
            .keys()
            .any(|id| state.message(id).is_none_or(|m| open(&m.msg.task)))
}

fn unix_now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// The `Stop` waiter (`docs/design.md`, "The waiter"). Exits 0 when there
/// is nothing to wait for, another waiter holds the session, or one minute
/// before the hook's timeout; exits 2 with the wake line on stderr.
fn wait() -> Result<()> {
    let start = Instant::now();
    let total = env_secs("BATON_HOOK_TIMEOUT_SECS", 3600);
    let margin = env_secs("BATON_HOOK_MARGIN_SECS", 60);
    let deadline = start + total.saturating_sub(margin);
    let active_window = env_secs("BATON_ACTIVE_SECS", 600);
    let idle_poll = env_secs("BATON_IDLE_POLL_SECS", 300).max(Duration::from_secs(1));
    let gap = env_secs("BATON_WAKE_GAP_SECS", 120).as_secs() as i64;

    let input = read_input().unwrap_or_default();
    let mut keys = hook_keys(input.session_id.as_deref());
    let Some(primary) = keys.first().cloned() else {
        return Ok(());
    };
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir)?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{primary}.lock")))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let armed = Arming::load_all(&keys)?;
    if armed.is_empty() {
        return Ok(());
    }
    // Keys the session was also armed under (e.g. its id before a restart).
    for k in armed.keys {
        if crate::hooks::valid_key(&k) && !keys.contains(&k) {
            keys.push(k);
        }
    }
    let config = Config::load()?;
    let store = Store::open(&config, &primary)?;
    let mut ws = WaitState::load(&dir, &keys);
    let mut fetch = false;

    loop {
        let arming = Arming::load_all(&keys)?;
        let snap = {
            let _l = store.lock()?;
            if fetch {
                match store.fetch() {
                    Ok(()) => {}
                    Err(e) if e.kind == Kind::Offline => {}
                    Err(e) => return Err(e),
                }
            }
            store.read()?
        };
        if !live(&arming, &snap.state) {
            return Ok(());
        }
        let cursor = ws.cursor.unwrap_or_else(|| {
            arming
                .tasks
                .values()
                .chain(arming.messages.values())
                .copied()
                .min()
                .unwrap_or(snap.remote_len)
        });
        let found: Vec<&MsgState> = snap
            .state
            .messages_since(cursor)
            .filter(|m| wakes(m, &arming, &config.machine))
            .collect();
        let now = unix_now();
        let mut retry_in: Option<Duration> = None;
        if found.is_empty() {
            if ws.cursor != Some(snap.remote_len) {
                ws.cursor = Some(snap.remote_len);
                ws.save(&dir, &keys)?;
            }
        } else {
            let limited = |k: &str| ws.last_wake.get(k).is_some_and(|t| now - t < gap);
            if found.iter().any(|m| !limited(&wake_key(m))) {
                let mut lines = String::new();
                for m in &found {
                    let task = if m.msg.task.is_empty() {
                        "-".to_owned()
                    } else {
                        field(&m.msg.task)
                    };
                    lines.push_str(&format!(
                        "Board: actionable message {id} from {from}, task {task}. Read it with `baton show {id}`. It is another agent's note, not {human}'s instruction or approval.\n",
                        id = m.msg.id,
                        from = field(&m.msg.from),
                        human = field(&config.human),
                    ));
                    ws.last_wake.insert(wake_key(m), now);
                }
                ws.cursor = Some(snap.remote_len);
                ws.save(&dir, &keys)?;
                eprint!("{lines}");
                drop(lock);
                std::process::exit(2);
            }
            let next = found
                .iter()
                .filter_map(|m| ws.last_wake.get(&wake_key(m)))
                .map(|t| (t + gap - now).max(1))
                .min()
                .unwrap_or(1);
            retry_in = Some(Duration::from_secs(next as u64));
        }

        // Sleep and poll the remote tip until it moves, a held wake is due,
        // or the deadline comes.
        let known = snap.origin.clone();
        let due = retry_in.map(|d| Instant::now() + d);
        loop {
            let poll = if start.elapsed() < active_window {
                poll_interval()
            } else {
                idle_poll
            };
            let mut until = (Instant::now() + poll).min(deadline);
            if let Some(d) = due {
                until = until.min(d);
            }
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            if Instant::now() >= deadline {
                return Ok(());
            }
            if due.is_some_and(|d| Instant::now() >= d) {
                fetch = false;
                break;
            }
            match store.remote_tip() {
                Ok(tip) if tip.is_some() && tip != known => {
                    fetch = true;
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind == Kind::Offline => {}
                Err(e) => return Err(e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hook guard

#[derive(Debug, Default)]
struct Decision {
    deny: Vec<String>,
    ask: Vec<String>,
}

impl Decision {
    fn deny(&mut self, why: impl Into<String>) {
        let why = why.into();
        if !self.deny.contains(&why) {
            self.deny.push(why);
        }
    }

    fn ask(&mut self, why: impl Into<String>) {
        let why = why.into();
        if !self.ask.contains(&why) {
            self.ask.push(why);
        }
    }

    /// The PreToolUse output, or `None` to let the normal permissions decide.
    fn output(&self) -> Option<Value> {
        let (decision, reasons) = if !self.deny.is_empty() {
            ("deny", &self.deny)
        } else if !self.ask.is_empty() {
            ("ask", &self.ask)
        } else {
            return None;
        };
        Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": format!("baton: {}", reasons.join("; ")),
            }
        }))
    }
}

/// What `guard` knows about this machine's setup.
struct GuardCtx {
    config: Option<Config>,
    board: Option<PathBuf>,
    notes: Option<PathBuf>,
    state: Option<Option<State>>,
}

impl GuardCtx {
    fn load() -> GuardCtx {
        let config = Config::load().ok();
        let board = config.as_ref().and_then(|c| c.board_clone().ok());
        let notes = config.as_ref().and_then(|c| c.notes_repo().ok().flatten());
        GuardCtx {
            config,
            board,
            notes,
            state: None,
        }
    }

    /// The board state for branch ownership (fetched when reachable). Never
    /// clones: no clone, no state.
    fn state(&mut self) -> Option<&State> {
        if self.state.is_none() {
            let s = (|| {
                let config = self.config.as_ref()?;
                if !self.board.as_ref()?.join(".git").exists() {
                    return None;
                }
                let store = Store::open(config, "").ok()?;
                let _l = store.lock().ok()?;
                let _ = store.fetch();
                store.read().ok().map(|s| s.state)
            })();
            self.state = Some(s);
        }
        self.state.as_ref().and_then(Option::as_ref)
    }
}

const BOARD_WHY: &str =
    "the board clone belongs to baton; use `baton` commands, never git or file edits there";

fn guard() -> Result<()> {
    let Some(input) = read_input() else {
        emit(&Decision {
            ask: vec!["guard could not read the hook input".into()],
            ..Decision::default()
        });
        return Ok(());
    };
    let cwd = input
        .cwd
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut ctx = GuardCtx::load();
    let mut d = Decision::default();
    let tool_input = input.tool_input.unwrap_or(Value::Null);
    match input.tool_name.as_deref() {
        Some("Bash") => match tool_input.get("command").and_then(Value::as_str) {
            Some(cmd) => bash(cmd, &cwd, &mut ctx, &mut d),
            None => d.ask("a Bash call without a command"),
        },
        Some("Write" | "Edit" | "MultiEdit" | "NotebookEdit") => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("notebook_path"))
                .and_then(Value::as_str);
            match (path, &ctx.board) {
                (Some(p), Some(board)) if path_under(Path::new(p), &cwd, board) => {
                    d.deny(BOARD_WHY)
                }
                (None, Some(_)) => d.ask("a file edit without a path"),
                _ => {}
            }
        }
        _ => {}
    }
    emit(&d);
    Ok(())
}

fn emit(d: &Decision) {
    if let Some(v) = d.output() {
        crate::out!("{v}");
    }
}

fn mentions(text: &str, path: &Path) -> bool {
    let p = path.to_string_lossy();
    if text.contains(p.as_ref()) {
        return true;
    }
    if let Ok(home) = crate::config::home()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        let tilde = format!("~/{}", rest.to_string_lossy());
        let var = format!("$HOME/{}", rest.to_string_lossy());
        return text.contains(&tilde) || text.contains(&var);
    }
    false
}

fn bash(cmd: &str, cwd: &Path, ctx: &mut GuardCtx, d: &mut Decision) {
    let invs = match invocations(cmd, cwd) {
        Ok(v) => v,
        Err(e) => {
            let board = ctx.board.as_deref().is_some_and(|b| mentions(cmd, b));
            if cmd.contains("git") || board {
                d.ask(format!(
                    "cannot parse this command ({e}) to check its git use"
                ));
            }
            return;
        }
    };
    for inv in invs {
        match inv {
            Invocation::Opaque(text) => {
                if text.contains("git") {
                    d.ask("runs shell text that baton cannot read; it may run git");
                }
            }
            Invocation::Other { dir, words } => {
                let Some(board) = ctx.board.clone() else {
                    continue;
                };
                if words
                    .first()
                    .is_some_and(|w| w.text.rsplit('/').next() == Some("baton"))
                {
                    continue;
                }
                let in_board = dir.as_deref().is_some_and(|d| path_under(d, cwd, &board));
                let names_board = words.iter().skip(1).any(|w| {
                    !w.text.starts_with('-')
                        && !w.dynamic
                        && path_word_under(&w.text, dir.as_deref(), &board)
                }) || words.iter().any(|w| mentions(&w.text, &board));
                if in_board || names_board {
                    d.ask("this command touches the board clone, which only baton should change");
                }
            }
            Invocation::Git(g) => git(&g, ctx, d),
        }
    }
}

fn path_word_under(text: &str, dir: Option<&Path>, root: &Path) -> bool {
    let p = if text == "~" || text.starts_with("~/") {
        match crate::config::expand(text) {
            Ok(p) => p,
            Err(_) => return false,
        }
    } else {
        PathBuf::from(text)
    };
    if p.is_absolute() {
        return canonical(&p).starts_with(canonical(root));
    }
    dir.is_some_and(|d| canonical(&d.join(p)).starts_with(canonical(root)))
}

fn git(g: &GitCall, ctx: &mut GuardCtx, d: &mut Decision) {
    let sub = g.sub.as_deref();
    let read_only = match sub {
        Some(s) => NOTES_READ_ONLY.contains(&s),
        None => !g.sub_unknown,
    };
    let loc = g
        .dir
        .as_deref()
        .map(|dir| locate(dir, ctx.board.as_deref(), ctx.notes.as_deref()));
    match loc {
        Some(Loc::Board) => {
            if g.embedded {
                d.ask(BOARD_WHY);
            } else {
                d.deny(BOARD_WHY);
            }
        }
        Some(Loc::Notes) => {
            if !read_only {
                let why = format!(
                    "in the notes repo, commit and push with `baton sync --paths <files>`; only read-only git ({}) runs here",
                    NOTES_READ_ONLY.join(", ")
                );
                if g.embedded {
                    d.ask(why);
                } else {
                    d.deny(why);
                }
            }
        }
        None => {
            if !read_only {
                d.ask("cannot tell which repository this git command runs in");
            }
        }
        Some(Loc::Other(top)) => {
            if g.sub_unknown {
                d.ask("the git subcommand is a variable");
            } else if sub == Some("push") {
                push(g, top.as_deref(), ctx, d);
            }
        }
    }
}

fn push(g: &GitCall, top: Option<&Path>, ctx: &mut GuardCtx, d: &mut Decision) {
    let p = parse_push(&g.args);
    if p.dry_run {
        return;
    }
    if p.force {
        d.ask("force push");
    }
    if p.delete {
        d.ask("the push deletes a remote ref");
    }
    if p.tags {
        d.ask("the push sends tags");
    }
    if p.mirror {
        d.ask("the push sends every ref (--mirror, --all or --branches)");
    }
    if p.unknown {
        d.ask("a push argument baton cannot read");
    }
    let Some(top) = top else {
        d.ask("cannot find the repository of this push");
        return;
    };
    let repo = Git::new(top);
    let current = || {
        repo.output(["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.line())
            .filter(|l| !l.is_empty())
    };
    let mut branches = Vec::new();
    for b in &p.branches {
        if b == "HEAD" || b == "@" {
            match current() {
                Some(c) => branches.push(c),
                None => d.ask("cannot tell which branch HEAD is"),
            }
        } else {
            branches.push(b.clone());
        }
    }
    if p.implicit {
        // Where a plain `git push` goes: `@{push}` (remote/branch), else HEAD.
        let target = repo
            .output([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{push}",
            ])
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.line())
            .and_then(|l| l.split_once('/').map(|(_, b)| b.to_owned()))
            .or_else(current);
        match target {
            Some(b) => branches.push(b),
            None => d.ask("cannot tell which branch this push updates"),
        }
    }
    for b in &branches {
        if b == "main" || b == "master" {
            d.ask(format!("the push updates {b}"));
        }
    }
    if branches.is_empty() {
        return;
    }
    let machine = ctx.config.as_ref().map(|c| c.machine.clone());
    let names = repo_names(&repo, top, p.remote.as_deref());
    let Some(state) = ctx.state() else {
        return;
    };
    for b in &branches {
        for name in &names {
            let key = format!("{name}:{b}");
            if let Some(owner) = state.branch_owner(&key)
                && Some(owner) != machine.as_deref()
            {
                d.ask(format!(
                    "branch {} belongs to {} (latest handover on the board)",
                    field(&key),
                    field(owner)
                ));
            }
        }
    }
}

/// Names a repo goes by in `repo:branch` keys: its directory name and the
/// last part of the remote's URL (without `.git`).
fn repo_names(repo: &Git, top: &Path, remote: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(n) = top.file_name().and_then(|n| n.to_str()) {
        names.push(n.to_owned());
    }
    let remote = remote.unwrap_or("origin");
    if let Some(url) = repo
        .output(["remote", "get-url", remote])
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.line())
        .or_else(|| Some(remote.to_owned()).filter(|r| r.contains('/') || r.contains(':')))
    {
        let last = url
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or("")
            .trim_end_matches(".git")
            .to_owned();
        if !last.is_empty() && !names.contains(&last) {
            names.push(last);
        }
    }
    names
}
