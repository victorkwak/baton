//! `baton hook guard` (Claude Code `PreToolUse`). Behaviour: `docs/design.md`,
//! "Claude Code hooks".
//!
//! A backstop, not a boundary (D6): it never allows. It denies edits of the
//! board clone and git writes in the notes repo, asks when a command may
//! touch them or may push somewhere it should not, and says nothing for
//! simple safe commands. Silence leaves the call to Claude Code's normal
//! permissions. It does not parse shell: a command is either one simple
//! word list (no operators, expansions or escapes) that it can read, or text
//! that it only searches for the words `git`, `push` and the two paths.

use std::io::{IsTerminal as _, Read as _};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::board::State;
use crate::config::Config;
use crate::error::Result;
use crate::git::Git;
use crate::output::field;
use crate::store::Store;

const BOARD_WHY: &str =
    "the board clone belongs to baton; use `baton` commands, never git, shell or file edits there";

/// Read-only git subcommands allowed in the notes repo.
const NOTES_READS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
];

/// Options a notes read may use (the name before any `=`, exact: an
/// abbreviation or anything else asks). None runs a program or writes a file.
#[rustfmt::skip]
const SAFE_OPTS: &[&str] = &[
    // status
    "-s", "--short", "-b", "--branch", "-sb", "--porcelain", "--long", "-u", "-uno", "-unormal",
    "-uall", "--untracked-files", "--ignored", "-z", "--show-stash", "--ahead-behind",
    "--no-ahead-behind", "-v", "--verbose",
    // log, show, diff
    "--oneline", "--graph", "--decorate", "--no-decorate", "--stat", "--shortstat", "--numstat",
    "--name-only", "--name-status", "--summary", "--compact-summary", "--raw", "-p", "--patch",
    "--no-patch", "-n", "--max-count", "--skip", "--since", "--until", "--after", "--before",
    "--author", "--committer", "--grep", "-i", "--all-match", "--format", "--pretty",
    "--abbrev-commit", "--abbrev", "--no-merges", "--merges", "--first-parent", "--reverse",
    "--follow", "--date", "--all", "--topo-order", "--date-order", "--left-right", "--cherry-pick",
    "--cherry-mark", "--boundary", "--source", "--no-color", "--color", "--cached", "--staged",
    "--check", "--relative", "--no-renames", "--diff-filter", "-M", "-R", "-w",
    "--ignore-all-space", "--word-diff", "--unified", "--exit-code", "--quiet", "-q",
    "--no-ext-diff", "--no-textconv", "--dirstat", "--minimal",
    // rev-parse
    "--show-toplevel", "--git-dir", "--git-common-dir", "--absolute-git-dir", "--abbrev-ref",
    "--symbolic-full-name", "--symbolic", "--short", "--verify", "--is-inside-work-tree",
    "--is-inside-git-dir", "--show-prefix", "--show-cdup",
    // ls-files
    "-m", "--modified", "-o", "--others", "--exclude-standard", "-c", "-d", "--deleted", "--stage",
    "-t", "--full-name", "--error-unmatch", "--directory", "--unmerged",
    // blame, grep
    "-L", "-e", "-l", "--show-email", "--porcelain", "--line-porcelain", "--line-number",
    "--ignore-case", "--word-regexp", "--files-with-matches", "--count", "-E", "--extended-regexp",
    "-F", "--fixed-strings", "-h", "-H", "--invert-match", "-A", "-B", "-C", "--context",
    "--heading", "--break", "--max-depth", "--untracked", "-I", "--only-matching",
];

/// Git subcommands that cannot push, for a simple command whose text says
/// `push` (a commit message, `git stash push`). None runs a program given
/// on the command line.
#[rustfmt::skip]
const NON_PUSH: &[&str] = &[
    "add", "commit", "status", "log", "diff", "show", "stash", "branch", "switch", "checkout",
    "restore", "reset", "rm", "mv", "tag", "blame", "ls-files", "rev-parse",
];

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

    /// The `PreToolUse` output, or `None` to leave it to the permissions.
    fn output(&self) -> Option<Value> {
        let (decision, reasons) = match (self.deny.is_empty(), self.ask.is_empty()) {
            (false, _) => ("deny", &self.deny),
            (true, false) => ("ask", &self.ask),
            (true, true) => return None,
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

/// Entry point of `baton hook guard`.
pub fn run() -> Result<()> {
    let mut d = Decision::default();
    let mut stdin = std::io::stdin();
    let mut text = String::new();
    let input: Option<Value> = (!stdin.is_terminal())
        .then(|| stdin.read_to_string(&mut text).ok())
        .flatten()
        .and_then(|_| serde_json::from_str(&text).ok());
    match input {
        Some(v) => decide(&v, &mut d),
        None => d.ask("guard could not read the hook input"),
    }
    if let Some(v) = d.output() {
        crate::out!("{v}");
    }
    Ok(())
}

struct Ctx {
    config: Option<Config>,
    board: Option<PathBuf>,
    notes: Option<PathBuf>,
    cwd: PathBuf,
}

fn decide(input: &Value, d: &mut Decision) {
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let config = Config::load().ok();
    let ctx = Ctx {
        board: config.as_ref().and_then(|c| c.board_clone().ok()),
        notes: config.as_ref().and_then(|c| c.notes_repo().ok().flatten()),
        config,
        cwd,
    };
    let tool = &input["tool_input"];
    match input["tool_name"].as_str() {
        Some("Bash") => match tool["command"].as_str() {
            Some(cmd) => bash(cmd, &ctx, d),
            None => d.ask("a Bash call without a command"),
        },
        Some("Write" | "Edit" | "MultiEdit" | "NotebookEdit") => {
            let Some(board) = &ctx.board else { return };
            match tool["file_path"]
                .as_str()
                .or_else(|| tool["notebook_path"].as_str())
            {
                Some(p) if inside(&ctx.cwd.join(p), board) => d.deny(BOARD_WHY),
                Some(_) => {}
                None => d.ask("a file edit without a path"),
            }
        }
        _ => {}
    }
}

fn bash(t: &str, ctx: &Ctx, d: &mut Decision) {
    let words = simple_words(t);
    if words.as_deref().is_some_and(|w| real_baton(w, &ctx.cwd)) {
        return;
    }
    let loose = loose_words(t);
    let (dirs, unresolved) = work_dirs(&loose, &ctx.cwd);
    let toks = tokens(t);
    let has = |w: &str| toks.iter().any(|t| t == w);
    let git = has("git");
    // Does the command run in, or name, something inside `root`?
    let hits = |root: &Path| {
        dirs.iter().any(|d| inside(d, root))
            || names(t, root)
            || loose
                .iter()
                .flatten()
                .any(|w| dirs.iter().any(|d| word_under(d, w, root)))
    };
    let roots = [ctx.board.as_deref(), ctx.notes.as_deref()];
    if unresolved
        && roots.iter().flatten().next().is_some()
        && (git
            || roots
                .iter()
                .flatten()
                .any(|r| loose.iter().flatten().any(|w| could_name(w, r))))
    {
        d.ask("a directory change baton cannot resolve, near git or a baton repo");
    }
    if let Some(board) = &ctx.board
        && hits(board)
    {
        d.deny(BOARD_WHY);
    }
    if git
        && let Some(notes) = &ctx.notes
        && hits(notes)
    {
        notes_case(words.as_deref(), notes, ctx, d);
    }
    if has("push")
        || ((has("gh") || has("glab")) && (has("merge") || has("refs")))
        || t.contains("send-pack")
    {
        push_case(words.as_deref(), ctx, d);
    }
}

// ---------------------------------------------------------------------------
// The two cases.

/// A simple `git` command: its globals (only `--no-pager` and one `-C`
/// are known), the directory it runs in, and the words after the
/// subcommand. `Err` says why it is not readable.
fn git_call<'a>(
    words: Option<&'a [String]>,
    cwd: &Path,
) -> std::result::Result<(PathBuf, &'a str, &'a [String]), &'static str> {
    let words = words.ok_or("a command with shell operators, expansions or variables")?;
    if words.first().map(String::as_str) != Some("git") {
        return Err("a command that runs git through another program");
    }
    let mut dir = cwd.to_path_buf();
    let mut i = 1;
    let mut seen_c = false;
    while let Some(w) = words.get(i) {
        match w.as_str() {
            "--no-pager" => i += 1,
            "-C" if !seen_c => {
                let p = words.get(i + 1).ok_or("git -C without a directory")?;
                dir = dir.join(expand_home(p));
                seen_c = true;
                i += 2;
            }
            w if w.starts_with('-') => return Err("git options before the subcommand"),
            _ => break,
        }
    }
    let sub = words.get(i).ok_or("git without a subcommand")?;
    Ok((dir, sub.as_str(), &words[i + 1..]))
}

fn notes_case(words: Option<&[String]>, notes: &Path, ctx: &Ctx, d: &mut Decision) {
    let (dir, sub, args) = match git_call(words, &ctx.cwd) {
        Ok(c) => c,
        Err(why) => return d.ask(format!("{why} near the notes repo")),
    };
    if !inside(&dir, notes) {
        return d.ask("git that names the notes repo but runs elsewhere");
    }
    if !NOTES_READS.contains(&sub) {
        return d.deny(format!(
            "in the notes repo, commit and push with `baton sync --paths <files>`; only read-only git ({}) runs here",
            NOTES_READS.join(", ")
        ));
    }
    // Every word that looks like an option asks unless allowlisted, also
    // after `--` and after an option that takes a value.
    for a in args {
        if !a.starts_with('-') || a == "-" || a == "--" {
            continue;
        }
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let count = digits(&a[1..]) || a.strip_prefix("-U").is_some_and(digits);
        if !count && !SAFE_OPTS.contains(&name) {
            d.ask(format!(
                "git option {} in the notes repo is not on the read-only list",
                field(a)
            ));
        }
    }
}

fn push_case(words: Option<&[String]>, ctx: &Ctx, d: &mut Decision) {
    let (dir, sub, args) = match git_call(words, &ctx.cwd) {
        Ok(c) => c,
        Err(why) => return d.ask(format!("{why} that may push or merge")),
    };
    match sub {
        "push" => push(&dir, args, ctx, d),
        s if NON_PUSH.contains(&s) => {}
        s => d.ask(format!("`git {}` with `push` in the command", field(s))),
    }
}

/// A simple `git push [remote] [refspec...]` in `dir`.
fn push(dir: &Path, args: &[String], ctx: &Ctx, d: &mut Decision) {
    let mut pos = Vec::new();
    for a in args {
        match a.as_str() {
            "-n" | "--dry-run" => return,
            "-u" | "--set-upstream" => {}
            o if o.starts_with('-') => d.ask(format!("push option {}", field(o))),
            p => pos.push(p),
        }
    }
    let repo = Git::new(dir);
    let get = |args: &[&str]| {
        repo.output(args)
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.line())
            .filter(|l| !l.is_empty())
    };
    let Some(top) = get(&["rev-parse", "--show-toplevel"]).map(PathBuf::from) else {
        return d.ask("cannot find the repository of this push");
    };
    let current = || get(&["symbolic-ref", "--short", "-q", "HEAD"]);
    let mut branches = Vec::new();
    if pos.len() < 2 {
        // `git push [remote]`: push.default decides; only the modes that
        // push the current branch to one known name pass.
        if get(&["config", "--get-regexp", r"^remote\..*\.push$"]).is_some() {
            d.ask("a configured remote.*.push refspec decides what this push sends");
        }
        let Some(cur) = current() else {
            return d.ask("cannot tell which branch HEAD is");
        };
        match get(&["config", "--get", "push.default"]).as_deref() {
            None | Some("simple" | "current") => branches.push(cur),
            Some("upstream" | "tracking") => {
                match get(&["config", "--get", &format!("branch.{cur}.merge")]) {
                    Some(m) => branches.push(m.trim_start_matches("refs/heads/").to_owned()),
                    None => d.ask("cannot tell which branch this push updates"),
                }
            }
            Some(m) => d.ask(format!(
                "push.default={} may push several branches",
                field(m)
            )),
        }
    }
    for spec in pos.iter().skip(1) {
        if spec.starts_with('+') {
            d.ask("force push");
            continue;
        }
        if spec.starts_with(':') {
            d.ask("the push deletes a remote ref");
            continue;
        }
        if spec.contains('*') {
            d.ask("a wildcard refspec");
            continue;
        }
        let (src, dst) = spec.split_once(':').unwrap_or((spec, spec));
        let dst = if dst == "HEAD" || dst == "@" {
            match current() {
                Some(c) => c,
                None => {
                    d.ask("cannot tell which branch HEAD is");
                    continue;
                }
            }
        } else {
            dst.to_owned()
        };
        if src == dst
            && get(&["rev-parse", "--verify", "-q", &format!("refs/tags/{src}")]).is_some()
        {
            d.ask(format!("the push sends tag {}", field(src)));
            continue;
        }
        match dst.strip_prefix("refs/heads/") {
            Some(b) => branches.push(b.to_owned()),
            None if dst.starts_with("refs/") => d.ask(format!("the push updates {}", field(&dst))),
            None => branches.push(dst),
        }
    }
    for b in &branches {
        if ["main", "master"].contains(&b.as_str())
            || b.ends_with("/main")
            || b.ends_with("/master")
        {
            d.ask(format!("the push updates {}", field(b)));
        }
    }
    if branches.is_empty() {
        return;
    }
    let remote = pos.first().copied().unwrap_or("origin");
    let Some((state, me)) = board_state(ctx) else {
        return;
    };
    for name in repo_names(&repo, &top, remote) {
        for b in &branches {
            let key = format!("{name}:{b}");
            if let Some(owner) = state.branch_owner(&key)
                && owner != me
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

/// The board as the local clone has it (no fetch: a hook must be quick),
/// and this machine's name. Never clones.
fn board_state(ctx: &Ctx) -> Option<(State, String)> {
    let config = ctx.config.as_ref()?;
    if !ctx.board.as_ref()?.join(".git").exists() {
        return None;
    }
    let store = Store::open(config, "").ok()?;
    let _lock = store.lock().ok()?;
    Some((store.read().ok()?.state, config.machine.clone()))
}

/// Names a repo goes by in `repo:branch` keys: its directory name and the
/// last part of the remote's URL (without `.git`).
fn repo_names(repo: &Git, top: &Path, remote: &str) -> Vec<String> {
    let mut names: Vec<String> = top
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .into_iter()
        .collect();
    let url = repo
        .output(["remote", "get-url", remote])
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.line())
        .unwrap_or_else(|| remote.to_owned());
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git");
    if !last.is_empty() && !names.iter().any(|n| n == last) {
        names.push(last.to_owned());
    }
    names
}

// ---------------------------------------------------------------------------
// Text.

/// The words of `t` when it is one simple command: no unquoted shell
/// operator, expansion, glob, brace, comment or escape, no `$`, backtick or
/// backslash inside double quotes, no leading variable assignment.
fn simple_words(t: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => loop {
                in_word = true;
                match chars.next()? {
                    '\'' => break,
                    c => cur.push(c),
                }
            },
            '"' => loop {
                in_word = true;
                match chars.next()? {
                    '"' => break,
                    '$' | '`' | '\\' => return None,
                    c => cur.push(c),
                }
            },
            ';' | '&' | '|' | '<' | '>' | '$' | '`' | '(' | ')' | '\n' | '\r' | '\\' | '*'
            | '?' | '[' | ']' | '{' | '}' => return None,
            '#' if !in_word => return None,
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    let first = words.first()?;
    if let Some((name, _)) = first.split_once('=')
        && !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(words)
}

/// Lowercase word tokens of `t`, also with quotes and backslashes removed
/// (so `g"i"t` and `g\it` still read as `git`).
fn tokens(t: &str) -> Vec<String> {
    let squeezed: String = t
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    let mut out = Vec::new();
    for s in [t, squeezed.as_str()] {
        for w in s.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if !w.is_empty() {
                out.push(w.to_lowercase());
            }
        }
    }
    out
}

/// Does `t` name `root` as text (raw, physical, `~/`, `$HOME/`)?
fn names(t: &str, root: &Path) -> bool {
    let mut forms = vec![root.to_path_buf(), physical(root)];
    if let Ok(home) = crate::config::home() {
        for f in forms.clone() {
            if let Ok(rest) = f.strip_prefix(&home) {
                let rest = rest.to_string_lossy();
                for h in ["~", "$HOME", "${HOME}"] {
                    forms.push(PathBuf::from(format!("{h}/{rest}")));
                }
            }
        }
    }
    forms
        .iter()
        .any(|f| t.contains(f.to_string_lossy().as_ref()))
}

/// Words of `t` for a loose scan, not a parse: quotes removed, a backslash
/// keeps the next character, `None` between commands (`; & | ( )`, newline).
fn loose_words(t: &str) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = t.chars();
    let flush = |out: &mut Vec<Option<String>>, cur: &mut String, in_word: &mut bool| {
        if *in_word {
            out.push(Some(std::mem::take(cur)));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => cur.extend(chars.next()),
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                in_word = true;
                cur.extend(chars.next().filter(|&n| n != '\n'));
            }
            (None, ';' | '&' | '|' | '(' | ')' | '\n') => {
                flush(&mut out, &mut cur, &mut in_word);
                out.push(None);
            }
            (None, c) if c.is_whitespace() || c == '<' || c == '>' => {
                flush(&mut out, &mut cur, &mut in_word);
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    flush(&mut out, &mut cur, &mut in_word);
    out
}

const MAX_DIRS: usize = 64;

/// Every directory a part of the command may run in: the cwd, then each
/// target of `cd`/`pushd` and of `-C` (`git -C`, `env -C`) resolved against
/// every directory found so far (so both cumulatively and on its own),
/// physically and logically. `true` when a target cannot be resolved.
fn work_dirs(words: &[Option<String>], cwd: &Path) -> (Vec<PathBuf>, bool) {
    let mut dirs = vec![physical(cwd)];
    let mut unresolved = false;
    for (i, w) in words.iter().enumerate() {
        // A `-C` only after `git` or `env` (elsewhere it is often a count,
        // as in `grep -C 3` or `git diff -C`).
        let after_git =
            i > 0 && matches!(words[i - 1].as_deref(), Some("git" | "env" | "--no-pager"));
        let target = match w.as_deref() {
            Some("cd" | "pushd") => {
                let mut j = i + 1;
                while let Some(Some(o)) = words.get(j)
                    && ["-P", "-L", "-e", "-@"].contains(&o.as_str())
                {
                    j += 1;
                }
                match words.get(j) {
                    Some(Some(t)) => t.clone(),
                    _ => "~".to_owned(),
                }
            }
            Some("-C") if after_git => match words.get(i + 1) {
                Some(Some(t)) => t.clone(),
                _ => {
                    unresolved = true;
                    continue;
                }
            },
            _ => continue,
        };
        let is_cd = w.as_deref() != Some("-C");
        let target = expand_home(&target);
        let text = target.to_string_lossy();
        if text == "-" || text.starts_with('~') || text.contains(['$', '`', '*', '?', '[', '{']) {
            unresolved = true;
            continue;
        }
        for d in dirs.clone() {
            let p = d.join(&target);
            let real = physical(&p);
            if is_cd && !real.is_dir() {
                unresolved = true;
            }
            for c in [real, physical(&lexical(&p))] {
                if !dirs.contains(&c) {
                    dirs.push(c);
                }
            }
        }
        if dirs.len() > MAX_DIRS {
            dirs.truncate(MAX_DIRS);
            unresolved = true;
        }
    }
    (dirs, unresolved)
}

/// Does word `w` (or its part after `=`, or any `:`-separated part), read
/// as a path from `dir` (a physical directory), lead inside `root`?
fn word_under(dir: &Path, w: &str, root: &Path) -> bool {
    let real_root = physical(root);
    let tail = w.split_once('=').map(|(_, v)| v);
    std::iter::once(w)
        .chain(tail)
        .flat_map(|x| std::iter::once(x).chain(x.split(':')))
        .filter(|x| {
            !x.is_empty()
                && (!x.contains(['$', '`']) || x.starts_with("$HOME/") || x.starts_with("${HOME}/"))
        })
        .any(|x| {
            let x = expand_home(x);
            let s = x.to_string_lossy();
            if !s.contains('/') && s != "." && s != ".." {
                // One component: one lookup.
                let p = dir.join(&x);
                return p.canonicalize().unwrap_or(p).starts_with(&real_root);
            }
            under(&dir.join(&x), root)
        })
}

/// Could word `w` name `root` once some unknown directory is prepended
/// (the last components of `root`, such as `board` or `baton/board`)?
fn could_name(w: &str, root: &Path) -> bool {
    let w = w.trim_end_matches('/');
    !w.is_empty()
        && !matches!(w, "." | "..")
        && !w.contains('$')
        && (root.ends_with(w) || physical(root).ends_with(w))
}

fn expand_home(w: &str) -> PathBuf {
    for h in ["~", "$HOME", "${HOME}"] {
        if let Some(rest) = w.strip_prefix(h)
            && (rest.is_empty() || rest.starts_with('/'))
            && let Ok(home) = crate::config::home()
        {
            return home.join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(w)
}

/// Is `words` one invocation of this baton binary (by name through `PATH`,
/// or by a path that resolves to it)?
fn real_baton(words: &[String], cwd: &Path) -> bool {
    let Some(w) = words.first() else { return false };
    if w.rsplit('/').next() != Some("baton") {
        return false;
    }
    let Some(me) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
    else {
        return false;
    };
    let found = if w.contains('/') {
        cwd.join(w).canonicalize().ok()
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join(w))
                .find(|c| c.is_file())
                .and_then(|c| c.canonicalize().ok())
        })
    };
    found == Some(me)
}

// ---------------------------------------------------------------------------
// Paths.

/// The physical path: symlinks resolved component by component before a
/// following `..` applies. Components that do not exist are kept as they
/// are (so a file not yet written is placed under its nearest real parent).
fn physical(path: &Path) -> PathBuf {
    let mut cur = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => cur = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            Component::Normal(n) => {
                cur.push(n);
                if let Ok(real) = cur.canonicalize() {
                    cur = real;
                }
            }
        }
    }
    cur
}

/// `..` and `.` removed lexically (how a shell's logical `cd` sees a path).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Is `path` (absolute) inside `root`, physically or logically?
fn under(path: &Path, root: &Path) -> bool {
    let root = physical(root);
    physical(path).starts_with(&root) || physical(&lexical(path)).starts_with(&root)
}

/// Is `path` (absolute) inside `root`, physically or logically, or in a
/// linked worktree whose `.git` file points into `root`?
fn inside(path: &Path, root: &Path) -> bool {
    if under(path, root) {
        return true;
    }
    let root = physical(root);
    let p = physical(path);
    let mut dir = Some(p.as_path());
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        if dotgit.is_dir() {
            return false;
        }
        if dotgit.is_file() {
            return std::fs::read_to_string(&dotgit).ok().is_some_and(|t| {
                t.lines()
                    .next()
                    .and_then(|l| l.strip_prefix("gitdir:"))
                    .is_some_and(|g| physical(&d.join(g.trim())).starts_with(&root))
            });
        }
        dir = d.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str) -> Option<Vec<String>> {
        simple_words(t)
    }

    #[test]
    fn simple_words_reads_only_one_plain_command() {
        assert_eq!(
            w("git log --format='%h %s' -- \"a b.md\""),
            Some(
                vec!["git", "log", "--format=%h %s", "--", "a b.md"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        for t in [
            "git status; ls",
            "git log | head",
            "git log > x",
            "git log $X",
            "git log \"$X\"",
            "echo `x`",
            "git log \\-\\-output=x",
            "git log *",
            "git log --{output=x,}",
            "A=1 git log",
            "git log 'open",
            "git log\ngit push",
            "git status # x",
            "",
        ] {
            assert_eq!(w(t), None, "{t}");
        }
    }

    #[test]
    fn tokens_find_git_inside_strings_and_split_words() {
        let has = |t: &str, x: &str| tokens(t).iter().any(|k| k == x);
        assert!(has("python3 -c \"run(['git','push'])\"", "git"));
        assert!(has("g\"i\"t push", "git"));
        assert!(has("$GIT commit", "git"));
        assert!(has("cat .git/config", "git"));
        assert!(!has("gh repo view github.com/x/legit", "git"));
        assert!(!has("echo GIT_DIR", "git"));
    }
}
