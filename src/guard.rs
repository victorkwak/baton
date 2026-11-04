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
    let toks = tokens(t);
    post_body(t, &toks, words.as_deref(), ctx, d);
    if words.as_deref().is_some_and(|w| real_baton(w, &ctx.cwd)) {
        return;
    }
    let has = |w: &str| toks.iter().any(|t| t == w);
    let git = has("git");
    let reeval = re_evaluates(t);
    let loose = loose_words(t, reeval);
    let roots: Vec<&Path> = [ctx.board.as_deref(), ctx.notes.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    // An expansion is unknown. It matters where it decides the directory
    // or repository (or anywhere, when the cwd is already in a baton repo).
    // Text evaluated again (`bash -c '…'`, `eval`) is not read at all: any
    // expansion in it counts.
    let strict = reeval || roots.iter().any(|r| inside(&ctx.cwd, r));
    let mut unresolved = located_expansion(&loose, &roots, strict);
    // Variables assigned in the command: also check the text with their
    // values put in.
    let vars = assignments(&loose);
    let texts: Vec<String> = if vars.is_empty() {
        vec![t.to_owned()]
    } else {
        vec![t.to_owned(), substitute(t, &vars)]
    };
    // `CDPATH` (set here or in the environment) can send a relative `cd`
    // anywhere.
    let cdpath = has("cdpath") || std::env::var_os("CDPATH").is_some_and(|v| !v.is_empty());
    // Word lists: the loose scan, and the raw text split on quotes and
    // operators (a quoted string such as `bash -c "cd x && …"` can hold
    // paths and `cd`s).
    let mut lists: Vec<Vec<Option<String>>> = Vec::new();
    for text in &texts {
        lists.push(
            loose_words(text, reeval)
                .into_iter()
                .map(|w| w.map(|w| w.text))
                .collect(),
        );
        lists.push(raw_words(text));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for list in &lists {
        let (more, u) = work_dirs(list, &ctx.cwd, cdpath);
        unresolved |= u;
        for d in more {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    let mut cands: Vec<&str> = lists
        .iter()
        .flatten()
        .flatten()
        .map(String::as_str)
        .collect();
    cands.sort_unstable();
    cands.dedup();
    // Does the command run in, or name, something inside `root`?
    let hits = |root: &Path| {
        dirs.iter().any(|d| inside(d, root))
            || texts.iter().any(|x| names(x, root))
            || cands
                .iter()
                .any(|w| dirs.iter().any(|d| word_under(d, w, root)))
    };
    if unresolved
        && !roots.is_empty()
        && (git || roots.iter().any(|r| cands.iter().any(|w| could_name(w, r))))
    {
        d.ask("an expansion or directory change baton cannot resolve, near git or a baton repo");
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
    // `push` only in the body of a quoted here-document (source text an
    // edit script writes) is data, unless git or a shell reads it.
    let shell = ["sh", "bash", "zsh", "dash", "ksh", "source"]
        .iter()
        .any(|s| has(s));
    let push_word = if git || reeval || shell {
        has("push")
    } else {
        tokens(&without_literal_heredocs(t))
            .iter()
            .any(|w| w == "push")
    };
    if push_word
        || ((has("gh") || has("glab")) && (has("merge") || has("refs")))
        || t.contains("send-pack")
    {
        push_case(words.as_deref(), ctx, d);
    }
}

/// Where a `baton post` body comes from. `baton post` is allowed without a
/// prompt, and a post stays in the board history for good, so a body read
/// from a file or produced by a command asks: it could carry a `.env` or a
/// credentials file that the secret scan does not know. Silent: `-m` with
/// literal text (a quoted here-document too), and `--body-file` in the temp
/// dir, outside `$HOME` and any git work tree (where agents write long
/// bodies).
fn post_body(t: &str, toks: &[String], words: Option<&[String]>, ctx: &Ctx, d: &mut Decision) {
    if !(toks.iter().any(|w| w.ends_with("baton")) && toks.iter().any(|w| w == "post")) {
        return;
    }
    const WHY: &str =
        "a baton post body read from a file or a command: posts stay in the board history for good";
    // One simple command fed a quoted here-document: literal text on stdin.
    let heredoc = stdin_heredoc(t);
    let words = match (words, &heredoc) {
        (Some(w), _) => w,
        (None, Some(w)) => w.as_slice(),
        (None, None) => {
            // Not one simple command: a redirect, a pipe or an expansion
            // can feed the body.
            if t.contains("--body-file") || !only_literal_expansions(t) {
                d.ask(WHY);
            }
            return;
        }
    };
    let mut it = words.iter();
    while let Some(w) = it.next() {
        let file = match w.strip_prefix("--body-file") {
            Some("") => it.next().map(String::as_str),
            Some(v) => v.strip_prefix('='),
            None => continue,
        };
        match file {
            // The stdin of one simple command is the tool's (empty).
            Some("-") => {}
            Some(f) => {
                let path = physical(&ctx.cwd.join(expand_home(f)));
                let home = std::env::var_os("HOME").map(|h| physical(Path::new(&h)));
                let tmp = physical(&std::env::temp_dir());
                let scratch = under(&path, &tmp)
                    && !home.is_some_and(|h| under(&path, &h))
                    && !path
                        .ancestors()
                        .skip(1)
                        .take_while(|a| under(a, &tmp))
                        .any(|a| a.join(".git").exists());
                if !scratch {
                    d.ask(WHY);
                }
            }
            None => d.ask(WHY),
        }
    }
}

/// The words of `t` when it is one simple command followed by a single
/// here-document with a quoted delimiter (`cmd <<'X'` … `X`), and nothing
/// after it.
fn stdin_heredoc(t: &str) -> Option<Vec<String>> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"^([^\n]*?)\s*<<-?\s*(?:'(\w+)'|"(\w+)")[ \t]*\n"#).unwrap()
    });
    let caps = re.captures(t)?;
    let delim = caps.get(2).or_else(|| caps.get(3))?.as_str();
    let body = &t[caps.get(0)?.end()..];
    let mut lines = body.split_inclusive('\n');
    lines.find(|l| l.trim_start_matches('\t').trim_end_matches('\n') == delim)?;
    if lines.any(|l| !l.trim().is_empty()) {
        return None;
    }
    simple_words(caps.get(1)?.as_str())
}

/// Are the only command substitutions in `t` literal here-documents
/// (`$(cat <<'X'` … `X` `)`)? Backticks and `<(…)` never are.
fn only_literal_expansions(t: &str) -> bool {
    if t.contains('`') || t.contains("<(") {
        return false;
    }
    let mut rest = t;
    while let Some(i) = rest.find("$(") {
        match heredoc_group_len(&rest[i..]) {
            Some(len) => {
                let skip: usize = rest[i..].chars().take(len).map(char::len_utf8).sum();
                rest = &rest[i + skip..];
            }
            None => return false,
        }
    }
    true
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

/// A word of the loose scan. `dynamic`: it holds an expansion (`$x`,
/// `${x}`, `$(…)`, backticks) outside single quotes, so its value is unknown.
struct Loose {
    text: String,
    dynamic: bool,
    /// The separator before this word's command (`\0` at the start).
    after: char,
    /// Unquoted `(` depth (a subshell or group) at this word.
    paren: i32,
}

/// Words of `t` for a loose scan, not a parse: quotes removed, a backslash
/// keeps the next character, `None` between commands (`; & | ( )`, newline).
/// A `$(…)`, `${…}` or backtick group stays inside its word.
///
/// With `reeval` (the text holds a command that evaluates text again, such
/// as `bash -c` or `eval`), `$` and backticks in single quotes and quoted
/// here-documents count as expansions too.
fn loose_words(t: &str, reeval: bool) -> Vec<Option<Loose>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut dynamic = false;
    let mut quote: Option<char> = None;
    let (mut after, mut paren) = ('\0', 0i32);
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    let flush = |out: &mut Vec<Option<Loose>>,
                 cur: &mut String,
                 in_word: &mut bool,
                 dy: &mut bool,
                 after: char,
                 paren: i32| {
        if *in_word {
            out.push(Some(Loose {
                text: std::mem::take(cur),
                dynamic: std::mem::take(dy),
                after,
                paren,
            }));
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if quote == Some('\'') && reeval && (c == '$' || c == '`') {
            dynamic = true;
        }
        if quote != Some('\'') && (c == '$' || c == '`') {
            in_word = true;
            let end = group_end(&chars, i - 1);
            let group: String = chars[i - 1..end].iter().collect();
            if reeval || !literal_heredoc(&group) {
                dynamic = true;
            }
            cur.push_str(&group);
            i = end;
            continue;
        }
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(&n) = chars.get(i) {
                    cur.push(n);
                    i += 1;
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                in_word = true;
                if let Some(&n) = chars.get(i) {
                    if n != '\n' {
                        cur.push(n);
                    }
                    i += 1;
                }
            }
            (None, ';' | '&' | '|' | '(' | ')' | '\n') => {
                flush(&mut out, &mut cur, &mut in_word, &mut dynamic, after, paren);
                out.push(None);
                after = c;
                paren += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
            }
            (None, c) if c.is_whitespace() || c == '<' || c == '>' => {
                flush(&mut out, &mut cur, &mut in_word, &mut dynamic, after, paren);
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    flush(&mut out, &mut cur, &mut in_word, &mut dynamic, after, paren);
    out
}

/// Does `t` hold a command that evaluates text again: `sh`/`bash`/`zsh`/
/// `dash`/`ksh` with `-c`, `eval`, `xargs`, `env -S`, `source` or `.`?
fn re_evaluates(t: &str) -> bool {
    let words = loose_words(t, false);
    words.split(Option::is_none).any(|seg| {
        let seg: Vec<&str> = seg.iter().flatten().map(|w| w.text.as_str()).collect();
        let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_owned();
        let shell_c = seg.iter().enumerate().any(|(i, w)| {
            matches!(base(w).as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh")
                && seg[i + 1..]
                    .iter()
                    .any(|o| o.starts_with('-') && !o.starts_with("--") && o.contains('c'))
        });
        let env_s = seg.iter().enumerate().any(|(i, w)| {
            base(w) == "env"
                && seg[i + 1..]
                    .iter()
                    .any(|o| o.starts_with("-S") || o.starts_with("--split-string"))
        });
        shell_c
            || env_s
            || seg
                .iter()
                .any(|w| matches!(*w, "eval" | "xargs" | "source"))
            || seg.first() == Some(&".")
    })
}

/// The end (exclusive) of the expansion starting at `chars[start]` (`$` or
/// a backtick): a balanced `$(…)`/`${…}`, a backtick group, a quoted
/// here-document inside `$(cat <<'X' … X)`, or a plain `$name`. An
/// unbalanced group runs to the end.
fn group_end(chars: &[char], start: usize) -> usize {
    let n = chars.len();
    if chars[start] == '`' {
        let mut i = start + 1;
        while i < n && chars[i] != '`' {
            i += if chars[i] == '\\' { 2 } else { 1 };
        }
        return (i + 1).min(n);
    }
    let (open, close) = match chars.get(start + 1) {
        Some('(') => ('(', ')'),
        Some('{') => ('{', '}'),
        Some(c) if c.is_alphanumeric() || *c == '_' => {
            let mut i = start + 1;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            return i;
        }
        Some(_) => return start + 1,
        None => return n,
    };
    let rest: String = chars[start..].iter().collect();
    if let Some(len) = heredoc_group_len(&rest) {
        return start + len;
    }
    let (mut depth, mut i, mut q) = (0usize, start + 1, None::<char>);
    while i < n {
        let c = chars[i];
        match (q, c) {
            (Some(x), c) if c == x => q = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => q = Some(c),
            (None, '\\') => i += 1,
            (None, c) if c == open => depth += 1,
            (None, c) if c == close => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn heredoc_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"^\$\(\s*cat\s+<<-?\s*(?:'(\w+)'|"(\w+)")[ \t]*\n"#).unwrap()
    })
}

/// The length in chars of `$(cat <<'X'` … `X` `)` at the start of `s`, with
/// a quoted delimiter (no expansion in the body), or `None`.
fn heredoc_group_len(s: &str) -> Option<usize> {
    let caps = heredoc_re().captures(s)?;
    let delim = caps.get(1).or_else(|| caps.get(2))?.as_str();
    let head = caps.get(0)?.end();
    let mut pos = head;
    for line in s[head..].split_inclusive('\n') {
        pos += line.len();
        if line.trim_start_matches('\t').trim_end_matches('\n') == delim {
            let after = &s[pos..];
            let close = after.find(|c: char| !c.is_whitespace())?;
            return (after[close..].starts_with(')')).then(|| s[..pos + close + 1].chars().count());
        }
    }
    None
}

/// `t` without the bodies of here-documents whose delimiter is quoted
/// (`<<'X'`, `<<"X"`): no expansion happens in them. An unterminated body
/// runs to the end.
fn without_literal_heredocs(t: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"<<-?\s*(?:'(\w+)'|"(\w+)")"#).unwrap());
    let mut out = String::new();
    let mut delims: Vec<String> = Vec::new();
    for line in t.split_inclusive('\n') {
        if let Some(d) = delims.first() {
            if line.trim_start_matches('\t').trim_end_matches('\n') == d {
                delims.remove(0);
            }
            continue;
        }
        out.push_str(line);
        for caps in re.captures_iter(line) {
            if let Some(d) = caps.get(1).or_else(|| caps.get(2)) {
                delims.push(d.as_str().to_owned());
            }
        }
    }
    out
}

/// Is `group` exactly `$(cat <<'X'` … `X` `)` (a literal text)?
fn literal_heredoc(group: &str) -> bool {
    heredoc_group_len(group).is_some_and(|l| l == group.chars().count())
}

/// Does an expansion (other than `$HOME/…`) sit where it decides which
/// directory or repository a command acts in? That is: the target of
/// `cd`/`pushd`/`popd`, a `-C`, `--git-dir`, `--work-tree` or
/// `--namespace` value, a `GIT_DIR=`/`GIT_WORK_TREE=`/`GIT_COMMON_DIR=`
/// value, a word before `git` in its command (`env $E git`) or among git's
/// options before the subcommand, or a word that could name a baton repo.
/// With `strict` (the cwd is in a baton repo), any expansion counts.
fn located_expansion(words: &[Option<Loose>], roots: &[&Path], strict: bool) -> bool {
    let all: Vec<&Loose> = words.iter().flatten().collect();
    // A command that names HOME anywhere outside a `$HOME`/`${HOME}`
    // expansion (an assignment, `read HOME`, `unset HOME`, a nameref) loses
    // the `$HOME/…` and `~` exemptions.
    let home_reassigned = all.iter().any(|w| {
        w.text
            .replace("${HOME}", "")
            .replace("$HOME", "")
            .contains("HOME")
    });
    // Builtins that set variables at run time (their words are not
    // expansions, so the per-word rule below cannot see them), and a bare
    // `export NAME` of a variable that decides where git acts.
    let base = |w: &Loose| w.text.rsplit('/').next().unwrap_or(&w.text).to_owned();
    let runtime_setter = all.iter().any(|w| {
        !w.dynamic
            && matches!(
                base(w).as_str(),
                "read" | "mapfile" | "readarray" | "getopts" | "select"
            )
    }) || (all.iter().any(|w| !w.dynamic && base(w) == "printf")
        && all.iter().any(|w| w.text.starts_with("-v")));
    // A variable that decides where git acts, *named* outside an expansion
    // (`GIT_DIR=…`, `export GIT_DIR`, `declare -n x=GIT_DIR`, `for PWD in
    // …`, `read CDPATH`): whatever builtin names it may set it. Reading it
    // (`$PWD`, `${GIT_DIR}`) is an expansion and handled below.
    let named_location = all.iter().any(|w| location_var_named(&w.text));
    if runtime_setter || named_location {
        return true;
    }
    const LOC_OPTS: &[&str] = &["-C", "--git-dir", "--work-tree", "--namespace"];
    const LOC_PREFIXES: &[&str] = &[
        "--git-dir=",
        "--work-tree=",
        "--namespace=",
        "GIT_DIR=",
        "GIT_WORK_TREE=",
        "GIT_COMMON_DIR=",
    ];
    for seg in words.split(Option::is_none) {
        let seg: Vec<&Loose> = seg.iter().flatten().collect();
        let git_at = seg.iter().position(|w| !w.dynamic && w.text == "git");
        // The first plain word after `git` that is not an option or an
        // option's value: the subcommand.
        let sub_at = git_at.map(|g| {
            let mut i = g + 1;
            while let Some(w) = seg.get(i) {
                let value = matches!(seg[i - 1].text.as_str(), "-C" | "-c");
                if w.dynamic || (!w.text.starts_with('-') && !value) {
                    break;
                }
                i += 1;
            }
            i
        });
        let mut after_cd = false;
        for (i, w) in seg.iter().enumerate() {
            let prev = i.checked_sub(1).map(|p| seg[p].text.as_str());
            let cd_target = after_cd && !w.text.starts_with('-');
            after_cd = matches!(w.text.as_str(), "cd" | "pushd" | "popd")
                || (after_cd && !cd_target && w.text.starts_with('-'));
            // With HOME reassigned, `~` no longer means the real home.
            if home_reassigned && !w.dynamic && w.text.starts_with('~') {
                return true;
            }
            if !w.dynamic || (home_only(&w.text) && !home_reassigned) {
                continue;
            }
            // Any `GIT_…=` assignment can change where git acts, as can the
            // shell's own directory variables.
            let git_var = w
                .text
                .find("GIT_")
                .is_some_and(|at| w.text[at..].contains('='));
            let dir_var = ["OLDPWD=", "CDPATH=", "PWD=", "HOME=", "XDG_CONFIG_HOME="]
                .iter()
                .any(|p| w.text.contains(p));
            // An argument of a builtin that sets variables (`export "$V"`,
            // `declare -x "$A"`, `env $E`) may set any of those.
            // Anywhere in the segment, by basename, so `if export …`,
            // `builtin declare …` or `/usr/bin/env …` count too.
            let setter = seg.iter().any(|c| {
                !c.dynamic
                    && matches!(
                        c.text.rsplit('/').next().unwrap_or(&c.text),
                        "export"
                            | "declare"
                            | "typeset"
                            | "readonly"
                            | "local"
                            | "env"
                            | "set"
                            | "printf"
                            | "read"
                            | "mapfile"
                            | "readarray"
                            | "getopts"
                    )
            });
            let located = strict
                || cd_target
                || prev.is_some_and(|p| LOC_OPTS.contains(&p))
                || LOC_PREFIXES.iter().any(|p| w.text.contains(p))
                || git_var
                || dir_var
                || setter
                || git_at.is_some_and(|g| i < g)
                || git_at.zip(sub_at).is_some_and(|(g, s)| i > g && i <= s)
                || roots.iter().any(|r| could_name(&w.text, r));
            if located {
                return true;
            }
        }
    }
    false
}

/// Does `text` name a variable that decides where git acts (`GIT_*`,
/// `PWD`, `OLDPWD`, `CDPATH`, `XDG_CONFIG_HOME`, `HOME`) other than by
/// expanding it (`$NAME`, `${NAME…}`)?
fn location_var_named(text: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(GIT_[A-Z0-9_]+|OLDPWD|PWD|CDPATH|XDG_CONFIG_HOME|HOME)\b")
            .expect("valid regex")
    });
    RE.find_iter(text).any(|m| {
        let before = &text[..m.start()];
        let prev = before.chars().next_back();
        // Part of a longer name (`MY_HOME`, `XDG_CONFIG_HOME` matched as HOME),
        // or an expansion (`$HOME`, `${HOME}`, `${HOME:-x}`).
        // An attached option (`printf -vGIT_DIR`) still names it; for `GIT_*`
        // a longer identifier is rare enough that asking is the safe side.
        let longer = prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && !before.ends_with("-v")
            && !m.as_str().starts_with("GIT_");
        !(longer || before.ends_with('$') || before.ends_with("${"))
    })
}

/// Is a dynamic word only `$HOME/…` or `${HOME}/…` with a literal rest?
fn home_only(w: &str) -> bool {
    ["$HOME", "${HOME}"].iter().any(|h| {
        w.strip_prefix(h)
            .is_some_and(|r| (r.is_empty() || r.starts_with('/')) && !r.contains(['$', '`']))
    })
}

/// Plain assignments in the command (`X=…`, `export X=…`, `declare -x X=…`,
/// `local X=…` at the start of a command): name and value. Only a variable
/// assigned exactly once, unconditionally (at the top level, not after
/// `&&`, `||` or `|`, not in a loop, conditional or subshell) and to a value
/// without expansions is returned; others stay unresolved.
fn assignments(words: &[Option<Loose>]) -> Vec<(String, String)> {
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let mut depth = 0i32;
    for seg in words.split(Option::is_none) {
        let seg: Vec<&Loose> = seg.iter().flatten().collect();
        let mut i = 0;
        while let Some(w) = seg.get(i) {
            match w.text.as_str() {
                "if" | "for" | "while" | "until" | "case" | "select" | "{" => depth += 1,
                "fi" | "done" | "esac" | "}" => depth -= 1,
                "then" | "do" | "else" | "elif" | "!" | "time" => {}
                _ => break,
            }
            i += 1;
        }
        if seg.get(i).is_some_and(|w| {
            matches!(
                w.text.as_str(),
                "export" | "declare" | "local" | "readonly" | "typeset"
            )
        }) {
            i += 1;
            while seg.get(i).is_some_and(|w| w.text.starts_with('-')) {
                i += 1;
            }
        }
        while let Some(w) = seg.get(i) {
            let Some((name, value)) = w.text.split_once('=') else {
                break;
            };
            let ok = !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                break;
            }
            let plain = depth == 0
                && w.paren == 0
                && matches!(w.after, '\0' | ';' | '\n')
                && (!w.dynamic || home_only(value));
            seen.push((name.to_owned(), plain.then(|| value.to_owned())));
            i += 1;
        }
    }
    seen.iter()
        .filter(|(n, _)| seen.iter().filter(|(m, _)| m == n).count() == 1)
        .filter_map(|(n, v)| Some((n.clone(), v.clone()?)))
        .collect()
}

/// `t` with `$NAME` and `${NAME}` replaced by the values assigned in it.
fn substitute(t: &str, vars: &[(String, String)]) -> String {
    let mut out = t.to_owned();
    for (name, value) in vars {
        out = out.replace(&format!("${{{name}}}"), value);
        let re = regex::Regex::new(&format!(r"\${}\b", regex::escape(name))).unwrap();
        out = re.replace_all(&out, regex::NoExpand(value)).into_owned();
    }
    out
}

/// The raw text split on whitespace, quotes, backticks, `=`, `:`, `<`,
/// `>`; `None` at `; & | ( )` and newlines. Nothing is unquoted.
fn raw_words(t: &str) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in t.chars() {
        let sep = matches!(c, ';' | '&' | '|' | '(' | ')' | '\n');
        if sep || c.is_whitespace() || "'\"`=:<>".contains(c) {
            if !cur.is_empty() {
                out.push(Some(std::mem::take(&mut cur)));
            }
            if sep {
                out.push(None);
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(Some(cur));
    }
    out
}

const MAX_DIRS: usize = 64;

/// Every directory a part of the command may run in: the cwd, then each
/// target of `cd`/`pushd` and of `-C` (`git -C`, `env -C`) resolved against
/// every directory found so far (so both cumulatively and on its own),
/// physically and logically. `true` when a target cannot be resolved.
fn work_dirs(words: &[Option<String>], cwd: &Path, cdpath: bool) -> (Vec<PathBuf>, bool) {
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
        let via_cdpath = is_cd
            && cdpath
            && !target.is_absolute()
            && !matches!(text.as_ref(), "." | "..")
            && !text.starts_with("./")
            && !text.starts_with("../");
        if via_cdpath
            || text == "-"
            || text.starts_with('~')
            || text.contains(['$', '`', '*', '?', '[', '{'])
        {
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
        .map(|x| {
            // An expansion: only its literal prefix is known.
            if x.starts_with("$HOME/") || x.starts_with("${HOME}/") {
                return x;
            }
            x.find(['$', '`']).map_or(x, |i| &x[..i])
        })
        .filter(|x| !x.is_empty())
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
/// With an expansion (`$D/board`, `` `pwd`/board ``), the literal part
/// after it is checked; any leading part of the word counts.
fn could_name(w: &str, root: &Path) -> bool {
    let w = match w.rfind(['$', '`']) {
        Some(i) => match w[i + 1..].split_once('/') {
            Some((_, rest)) => rest,
            None => return false,
        },
        None => w,
    };
    // Any leading part (`board`, `board/messages`) may end the root path.
    let real = physical(root);
    let mut part = PathBuf::new();
    for c in Path::new(w).components() {
        let Component::Normal(n) = c else {
            part.clear();
            continue;
        };
        part.push(n);
        if root.ends_with(&part) || real.ends_with(&part) {
            return true;
        }
    }
    false
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

    #[test]
    fn location_variables_named_but_not_expanded() {
        for t in [
            "GIT_DIR=x",
            "r=GIT_DIR",
            "GIT_WORK_TREE",
            "PWD",
            "CDPATH=/a",
            "OLDPWD",
            "XDG_CONFIG_HOME=/x",
            "HOME=/tmp",
            "-vGIT_DIR",
        ] {
            assert!(location_var_named(t), "{t}");
        }
        for t in [
            "$PWD",
            "${HOME}",
            "${HOME:-x}/a",
            "$GIT_DIR",
            "MY_HOME=1",
            "--git-dir=x",
            "home",
            "XPWD",
        ] {
            assert!(!location_var_named(t), "{t}");
        }
    }

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
