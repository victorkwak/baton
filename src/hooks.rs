//! Claude Code integration core: session keys, arming, and the shell and git
//! parsing behind `hook guard`. Owner: agent C (`docs/plan.md`).
//! Behaviour: `docs/design.md`, "Claude Code hooks".
//!
//! **Session keys.** The hook's stdin `session_id` changes when a Claude Code
//! session restarts; the desktop app's `CLAUDE_CODE_HOST_SESSION_ID` stays.
//! `baton post/take/close`, run from an agent's Bash, arm the session under
//! every key its environment names (host id and `CLAUDE_CODE_SESSION_ID`);
//! the waiter reads the arming of every key it knows (host id from its
//! environment, stdin `session_id`, `CLAUDE_CODE_SESSION_ID`) and merges
//! them. So the two agree whichever of those values reach each process.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{expand, state_dir};
use crate::error::Result;
use crate::store::Store;

/// The desktop app's session id (stable across restarts of the CLI session).
pub const HOST_ENV: &str = "CLAUDE_CODE_HOST_SESSION_ID";
/// The CLI session id (the hook's stdin `session_id`, as far as known).
pub const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// A session key usable as a file name.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && !key.starts_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| valid_key(v))
}

fn push_key(keys: &mut Vec<String>, key: Option<String>) {
    if let Some(k) = key
        && valid_key(&k)
        && !keys.contains(&k)
    {
        keys.push(k);
    }
}

/// Keys an agent's Bash command arms: the host id, then the session id.
pub fn env_keys() -> Vec<String> {
    let mut keys = Vec::new();
    push_key(&mut keys, env_var(HOST_ENV));
    push_key(&mut keys, env_var(SESSION_ENV));
    keys
}

/// Keys the waiter knows: the host id, the stdin `session_id`, the env
/// session id. The first is the primary key (lock and waiter state).
pub fn hook_keys(stdin_session: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    push_key(&mut keys, env_var(HOST_ENV));
    push_key(&mut keys, stdin_session.map(str::to_owned));
    push_key(&mut keys, env_var(SESSION_ENV));
    keys
}

/// `<state>/sessions`.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("sessions"))
}

/// What a session waits for: tasks and messages it posted, took or replied
/// in, each with the board position (`Snapshot::remote_len`) at arming.
/// Only messages at or after that position wake it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arming {
    #[serde(default)]
    pub tasks: BTreeMap<String, usize>,
    /// Message ids: a reply to one of them (even without a task) wakes.
    #[serde(default)]
    pub messages: BTreeMap<String, usize>,
    /// Every key this session was armed under (so a waiter that knows only
    /// one of them finds the waiter state saved under the others).
    #[serde(default)]
    pub keys: std::collections::BTreeSet<String>,
}

impl Arming {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.messages.is_empty()
    }

    /// Union; the earliest position wins.
    pub fn merge(&mut self, other: &Arming) {
        for (k, v) in &other.tasks {
            let e = self.tasks.entry(k.clone()).or_insert(*v);
            *e = (*e).min(*v);
        }
        for (k, v) in &other.messages {
            let e = self.messages.entry(k.clone()).or_insert(*v);
            *e = (*e).min(*v);
        }
        self.keys.extend(other.keys.iter().cloned());
    }

    fn path(key: &str) -> Result<PathBuf> {
        Ok(sessions_dir()?.join(format!("{key}.json")))
    }

    /// The arming stored under `key` (empty when none or unreadable).
    pub fn load(key: &str) -> Result<Arming> {
        let path = Self::path(key)?;
        Ok(std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    /// Every key's arming, merged.
    pub fn load_all(keys: &[String]) -> Result<Arming> {
        let mut all = Arming::default();
        for k in keys {
            all.merge(&Arming::load(k)?);
        }
        Ok(all)
    }
}

/// Write `contents` to `path` atomically (temp file + rename).
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Arm this agent session for the tasks of `ids` (messages it just posted,
/// took, closed or replied to) and for those messages themselves. Does
/// nothing outside Claude Code or without a session key. Call it after the
/// board write, with the board lock held.
pub fn arm(store: &Store, ids: &[&str]) -> Result<()> {
    if std::env::var_os("CLAUDECODE").is_none() {
        return Ok(());
    }
    let keys = env_keys();
    if keys.is_empty() {
        return Ok(());
    }
    let snap = store.read()?;
    let pos = snap.remote_len;
    let mut add = Arming::default();
    for id in ids {
        let Some(m) = snap.state.message(id) else {
            continue;
        };
        add.messages.insert(m.msg.id.clone(), pos);
        if !m.msg.task.is_empty() && snap.state.task(&m.msg.task).is_none_or(|t| !t.closed) {
            add.tasks.insert(m.msg.task.clone(), pos);
        }
    }
    if add.is_empty() {
        return Ok(());
    }
    add.keys = keys.iter().cloned().collect();
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir)?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("arming.lock"))?;
    lock.lock()?;
    for key in &keys {
        let mut a = Arming::load(key)?;
        // Keep an earlier position for a task armed before.
        let mut merged = add.clone();
        merged.merge(&a);
        a = merged;
        let text = serde_json::to_vec_pretty(&a).map_err(anyhow::Error::from)?;
        write_atomic(&Arming::path(key)?, &text)?;
    }
    Ok(())
}

/// [`arm`], warning on stderr instead of failing: the board write already
/// succeeded, and arming only decides what wakes the session later.
pub fn arm_or_warn(store: &Store, ids: &[&str]) {
    let ids: Vec<&str> = ids.iter().copied().filter(|i| !i.is_empty()).collect();
    if let Err(e) = arm(store, &ids) {
        eprintln!("baton: could not arm this session for wake-ups: {e}");
    }
}

// ---------------------------------------------------------------------------
// Shell parsing for `hook guard`.

/// One shell word. `dynamic`: it holds an expansion (`$x`, `$(...)`,
/// backticks) whose value is unknown here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub dynamic: bool,
}

impl Word {
    pub fn lit(text: &str) -> Word {
        Word {
            text: text.to_owned(),
            dynamic: false,
        }
    }
}

const MAX_DEPTH: usize = 8;

/// Split a shell command into simple commands (words each), in execution
/// order: command substitutions come before the command that holds them.
/// Separators: `;`, `&`, `&&`, `|`, `||`, newlines, `(`, `)`, `{`, `}`.
/// Redirection targets and here-documents are dropped. Errors on
/// unbalanced quotes or substitutions.
pub fn split_commands(src: &str) -> std::result::Result<Vec<Vec<Word>>, String> {
    lex(src, 0)
}

struct Lexer {
    c: Vec<char>,
    i: usize,
    depth: usize,
    cmds: Vec<Vec<Word>>,
    cur: Vec<Word>,
    word: Option<Word>,
    redirect: bool,
    heredocs: Vec<(String, bool)>,
}

fn lex(src: &str, depth: usize) -> std::result::Result<Vec<Vec<Word>>, String> {
    if depth > MAX_DEPTH {
        return Err("command nests too deeply".into());
    }
    let mut l = Lexer {
        c: src.chars().collect(),
        i: 0,
        depth,
        cmds: Vec::new(),
        cur: Vec::new(),
        word: None,
        redirect: false,
        heredocs: Vec::new(),
    };
    l.run()?;
    Ok(l.cmds)
}

impl Lexer {
    fn peek(&self, n: usize) -> Option<char> {
        self.c.get(self.i + n).copied()
    }

    fn w(&mut self) -> &mut Word {
        self.word.get_or_insert_with(Word::default)
    }

    fn end_word(&mut self) {
        if let Some(w) = self.word.take() {
            if self.redirect {
                self.redirect = false;
            } else {
                self.cur.push(w);
            }
        }
    }

    fn end_cmd(&mut self) {
        self.end_word();
        self.redirect = false;
        if !self.cur.is_empty() {
            self.cmds.push(std::mem::take(&mut self.cur));
        }
    }

    fn run(&mut self) -> std::result::Result<(), String> {
        while let Some(ch) = self.peek(0) {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.end_word();
                    self.i += 1;
                }
                '\n' => {
                    self.end_cmd();
                    self.i += 1;
                    self.skip_heredocs();
                }
                '&' if self.peek(1) == Some('>') => {
                    self.end_word();
                    self.i += 2;
                    if self.peek(0) == Some('>') {
                        self.i += 1;
                    }
                    self.redirect = true;
                }
                ';' | '&' | '|' | '(' | ')' => {
                    self.end_cmd();
                    self.i += 1;
                }
                '<' | '>' => self.redirection(ch)?,
                '#' if self.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                '{' | '}'
                    if self.word.is_none()
                        && self.peek(1).is_none_or(|c| c.is_whitespace() || c == ';') =>
                {
                    self.end_cmd();
                    self.i += 1;
                }
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(c) => self.w().text.push(c),
                        None => {}
                    }
                    self.i += 2;
                }
                '\'' => {
                    self.i += 1;
                    self.w();
                    loop {
                        match self.peek(0) {
                            None => return Err("unbalanced single quote".into()),
                            Some('\'') => {
                                self.i += 1;
                                break;
                            }
                            Some(c) => {
                                self.w().text.push(c);
                                self.i += 1;
                            }
                        }
                    }
                }
                '"' => {
                    self.i += 1;
                    self.w();
                    loop {
                        match self.peek(0) {
                            None => return Err("unbalanced double quote".into()),
                            Some('"') => {
                                self.i += 1;
                                break;
                            }
                            Some('\\') => {
                                match self.peek(1) {
                                    Some(c @ ('"' | '\\' | '$' | '`')) => self.w().text.push(c),
                                    Some('\n') => {}
                                    Some(c) => {
                                        self.w().text.push('\\');
                                        self.w().text.push(c);
                                    }
                                    None => return Err("unbalanced double quote".into()),
                                }
                                self.i += 2;
                            }
                            Some('$') => self.dollar()?,
                            Some('`') => self.backtick()?,
                            Some(c) => {
                                self.w().text.push(c);
                                self.i += 1;
                            }
                        }
                    }
                }
                '$' => self.dollar()?,
                '`' => self.backtick()?,
                c => {
                    self.w().text.push(c);
                    self.i += 1;
                }
            }
        }
        if !self.heredocs.is_empty() {
            // A here-document without its body: nothing left to parse.
            self.heredocs.clear();
        }
        self.end_cmd();
        Ok(())
    }

    fn redirection(&mut self, ch: char) -> std::result::Result<(), String> {
        // A file descriptor number before the operator (`2>`) is not a word.
        let fd = self.word.as_ref().is_some_and(|w| {
            !w.dynamic && !w.text.is_empty() && w.text.chars().all(|c| c.is_ascii_digit())
        });
        if fd {
            self.word = None;
        } else {
            self.end_word();
        }
        if ch == '<' && self.peek(1) == Some('<') {
            if self.peek(2) == Some('<') {
                // Here-string: the next word is data.
                self.i += 3;
                self.redirect = true;
                return Ok(());
            }
            self.i += 2;
            let strip = self.peek(0) == Some('-');
            if strip {
                self.i += 1;
            }
            while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
                self.i += 1;
            }
            let mut delim = String::new();
            while let Some(c) = self.peek(0) {
                if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') {
                    break;
                }
                if !matches!(c, '\'' | '"' | '\\') {
                    delim.push(c);
                }
                self.i += 1;
            }
            if delim.is_empty() {
                return Err("here-document without a delimiter".into());
            }
            self.heredocs.push((delim, strip));
            return Ok(());
        }
        self.i += 1;
        while self.peek(0).is_some_and(|c| matches!(c, '>' | '&' | '|')) {
            self.i += 1;
        }
        self.redirect = true;
        Ok(())
    }

    /// After a newline: skip the bodies of pending here-documents.
    fn skip_heredocs(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            loop {
                if self.i >= self.c.len() {
                    return;
                }
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.c[start..self.i].iter().collect();
                self.i += 1;
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delim {
                    break;
                }
            }
        }
    }

    /// `$name`, `${...}`, `$(...)`, `$((...))` at `self.i`.
    fn dollar(&mut self) -> std::result::Result<(), String> {
        match self.peek(1) {
            Some('(') => {
                let close =
                    matching(&self.c, self.i + 1).ok_or_else(|| "unbalanced $(".to_owned())?;
                if self.peek(2) != Some('(') {
                    let inner: String = self.c[self.i + 2..close].iter().collect();
                    let sub = lex(&inner, self.depth + 1)?;
                    self.cmds.extend(sub);
                }
                let text: String = self.c[self.i..=close].iter().collect();
                let w = self.w();
                w.text.push_str(&text);
                w.dynamic = true;
                self.i = close + 1;
            }
            Some('{') => {
                let mut j = self.i + 2;
                while j < self.c.len() && self.c[j] != '}' {
                    j += 1;
                }
                if j >= self.c.len() {
                    return Err("unbalanced ${".into());
                }
                let text: String = self.c[self.i..=j].iter().collect();
                let w = self.w();
                w.text.push_str(&text);
                w.dynamic = true;
                self.i = j + 1;
            }
            Some(c)
                if c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '@' | '*' | '#' | '?' | '$' | '!' | '-') =>
            {
                let mut j = self.i + 1;
                if c.is_ascii_alphabetic() || c == '_' {
                    while j < self.c.len()
                        && (self.c[j].is_ascii_alphanumeric() || self.c[j] == '_')
                    {
                        j += 1;
                    }
                } else {
                    j += 1;
                }
                let text: String = self.c[self.i..j].iter().collect();
                let w = self.w();
                w.text.push_str(&text);
                w.dynamic = true;
                self.i = j;
            }
            _ => {
                self.w().text.push('$');
                self.i += 1;
            }
        }
        Ok(())
    }

    fn backtick(&mut self) -> std::result::Result<(), String> {
        let mut j = self.i + 1;
        let mut inner = String::new();
        loop {
            match self.c.get(j) {
                None => return Err("unbalanced backtick".into()),
                Some('`') => break,
                Some('\\') => {
                    if let Some(&n) = self.c.get(j + 1) {
                        inner.push(n);
                    }
                    j += 2;
                }
                Some(&c) => {
                    inner.push(c);
                    j += 1;
                }
            }
        }
        let sub = lex(&inner, self.depth + 1)?;
        self.cmds.extend(sub);
        let w = self.w();
        w.text.push_str("`...`");
        w.dynamic = true;
        self.i = j + 1;
        Ok(())
    }
}

/// Index of the `)` matching the `(` at `open`, skipping quoted text.
fn matching(c: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Git invocations in a shell command.

/// A `git` invocation found in a Bash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCall {
    /// The directory git runs in (after `cd`, `-C`, `--git-dir`); `None`
    /// when it cannot be known (a variable, `cd -`, `GIT_DIR=...`).
    pub dir: Option<PathBuf>,
    /// The subcommand; `None` when there is none or it is unknown.
    pub sub: Option<String>,
    /// `true` when the subcommand word is an expansion.
    pub sub_unknown: bool,
    pub args: Vec<Word>,
    /// Found as an argument of another program (`find -exec git ...`,
    /// an unknown wrapper), not as the command itself.
    pub embedded: bool,
}

/// What a Bash command runs, as far as `guard` cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Git(GitCall),
    /// Any other command, with its directory and words.
    Other {
        dir: Option<PathBuf>,
        words: Vec<Word>,
    },
    /// Text run through `sh -c "$x"` or `eval` that cannot be read.
    Opaque(String),
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn is_assignment(w: &Word) -> bool {
    let Some((name, _)) = w.text.split_once('=') else {
        return false;
    };
    let mut cs = name.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Resolve a path word against `dir`; `None` when unknown.
fn resolve(dir: Option<&Path>, w: &Word) -> Option<PathBuf> {
    if w.dynamic {
        return None;
    }
    let t = &w.text;
    let p = if t == "~" || t.starts_with("~/") {
        expand(t).ok()?
    } else if t.starts_with('~') {
        return None;
    } else {
        PathBuf::from(t)
    };
    if p.is_absolute() {
        Some(p)
    } else {
        dir.map(|d| d.join(p))
    }
}

/// Drop flags of a wrapper; `with_value` lists flags that take a separate value.
fn drop_flags(words: &mut Vec<Word>, with_value: &[&str]) {
    while let Some(w) = words.first() {
        let t = w.text.clone();
        if t == "--" {
            words.remove(0);
            break;
        }
        if !t.starts_with('-') || t == "-" {
            break;
        }
        words.remove(0);
        if with_value.contains(&t.as_str()) && !words.is_empty() {
            words.remove(0);
        }
    }
}

/// Every git invocation (and other command) in `command`, run from `cwd`.
pub fn invocations(command: &str, cwd: &Path) -> std::result::Result<Vec<Invocation>, String> {
    let mut out = Vec::new();
    walk(command, Some(cwd.to_path_buf()), 0, &mut out)?;
    Ok(out)
}

fn walk(
    command: &str,
    cwd: Option<PathBuf>,
    depth: usize,
    out: &mut Vec<Invocation>,
) -> std::result::Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("command nests too deeply".into());
    }
    let mut cwd = cwd;
    for cmd in split_commands(command)? {
        let mut words = cmd;
        let mut dir = cwd.clone();
        // Assignments before the command; a git location variable makes the repo unknown.
        let mut env_repo = false;
        loop {
            while words.first().is_some_and(is_assignment) {
                let w = words.remove(0);
                let name = w.text.split_once('=').map(|(n, _)| n).unwrap_or("");
                if matches!(name, "GIT_DIR" | "GIT_WORK_TREE" | "GIT_COMMON_DIR") {
                    env_repo = true;
                }
            }
            let Some(first) = words.first() else { break };
            if first.dynamic {
                break;
            }
            match basename(&first.text) {
                "sudo" | "doas" => {
                    words.remove(0);
                    drop_flags(
                        &mut words,
                        &["-u", "-g", "-C", "-D", "-p", "-r", "-t", "-U", "-h"],
                    );
                }
                "env" => {
                    words.remove(0);
                    while let Some(w) = words.first() {
                        let t = w.text.clone();
                        if t == "-C" || t == "--chdir" {
                            words.remove(0);
                            if !words.is_empty() {
                                let p = words.remove(0);
                                dir = resolve(dir.as_deref(), &p);
                            }
                        } else if let Some(p) = t.strip_prefix("--chdir=") {
                            dir = resolve(dir.as_deref(), &Word::lit(p));
                            words.remove(0);
                        } else if t == "-u" || t == "-S" || t == "--unset" {
                            words.remove(0);
                            if !words.is_empty() {
                                words.remove(0);
                            }
                        } else if t.starts_with('-') {
                            words.remove(0);
                        } else {
                            break;
                        }
                    }
                }
                "command" | "builtin" | "nohup" | "time" | "then" | "do" | "else" | "elif"
                | "if" | "while" | "until" | "!" | "exec" => {
                    words.remove(0);
                    drop_flags(&mut words, &["-a"]);
                }
                "nice" | "ionice" | "chrt" | "stdbuf" | "watch" => {
                    words.remove(0);
                    drop_flags(&mut words, &["-n", "-c", "-p", "-i", "-o", "-e"]);
                }
                "timeout" => {
                    words.remove(0);
                    drop_flags(&mut words, &["-s", "-k", "--signal", "--kill-after"]);
                    if !words.is_empty() {
                        words.remove(0);
                    }
                }
                "xargs" => {
                    words.remove(0);
                    drop_flags(
                        &mut words,
                        &[
                            "-I",
                            "-n",
                            "-P",
                            "-L",
                            "-d",
                            "-s",
                            "-a",
                            "-E",
                            "-e",
                            "--max-args",
                            "--max-procs",
                            "--delimiter",
                            "--arg-file",
                        ],
                    );
                }
                _ => break,
            }
        }
        if env_repo {
            dir = None;
        }
        let Some(first) = words.first().cloned() else {
            continue;
        };
        if first.dynamic {
            // `$GIT push`: an unknown program.
            out.push(Invocation::Opaque(
                words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
            continue;
        }
        match basename(&first.text) {
            "cd" | "pushd" => {
                let target = words.get(1);
                cwd = match target {
                    None => crate::config::home().ok(),
                    Some(w) if w.text == "-" => None,
                    Some(w) if w.text.starts_with('-') => None,
                    Some(w) => resolve(cwd.as_deref(), w),
                };
            }
            "popd" => cwd = None,
            "sh" | "bash" | "zsh" | "dash" | "ksh" => {
                let c_at = words.iter().skip(1).position(|w| {
                    !w.dynamic
                        && w.text.starts_with('-')
                        && !w.text.starts_with("--")
                        && w.text.contains('c')
                });
                match c_at.and_then(|p| words.get(p + 2)) {
                    Some(script) if !script.dynamic => {
                        walk(&script.text, dir.clone(), depth + 1, out)?
                    }
                    Some(script) => out.push(Invocation::Opaque(script.text.clone())),
                    None => out.push(Invocation::Other {
                        dir: dir.clone(),
                        words,
                    }),
                }
            }
            "eval" => {
                let text = words[1..]
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if words[1..].iter().any(|w| w.dynamic) {
                    out.push(Invocation::Opaque(text));
                } else {
                    walk(&text, dir.clone(), depth + 1, out)?;
                }
            }
            "git" => out.push(Invocation::Git(parse_git(&words[1..], dir, false))),
            _ => {
                if let Some(p) = words
                    .iter()
                    .skip(1)
                    .position(|w| !w.dynamic && basename(&w.text) == "git")
                {
                    out.push(Invocation::Git(parse_git(
                        &words[p + 2..],
                        dir.clone(),
                        true,
                    )));
                }
                out.push(Invocation::Other { dir, words });
            }
        }
    }
    Ok(())
}

/// Parse the words after `git`: global options, then the subcommand.
pub fn parse_git(ws: &[Word], dir: Option<PathBuf>, embedded: bool) -> GitCall {
    let mut dir = dir;
    let mut i = 0;
    while let Some(w) = ws.get(i) {
        let t = w.text.as_str();
        if !t.starts_with('-') || w.dynamic {
            break;
        }
        match t {
            "-C" => {
                dir = match ws.get(i + 1) {
                    Some(p) => resolve(dir.as_deref(), p),
                    None => None,
                };
                i += 2;
            }
            "--git-dir" | "--work-tree" => {
                dir = match ws.get(i + 1) {
                    Some(p) => resolve(dir.as_deref(), p),
                    None => None,
                };
                i += 2;
            }
            "-c" | "--config-env" | "--namespace" | "--exec-path" | "--super-prefix"
            | "--list-cmds" | "--attr-source" => i += 2,
            _ => {
                if let Some(p) = t
                    .strip_prefix("--git-dir=")
                    .or_else(|| t.strip_prefix("--work-tree="))
                {
                    dir = resolve(dir.as_deref(), &Word::lit(p));
                }
                i += 1;
            }
        }
    }
    let (sub, sub_unknown) = match ws.get(i) {
        Some(w) if w.dynamic => (None, true),
        Some(w) => (Some(w.text.clone()), false),
        None => (None, false),
    };
    GitCall {
        dir,
        sub,
        sub_unknown,
        args: ws.get(i + 1..).map(<[Word]>::to_vec).unwrap_or_default(),
        embedded,
    }
}

/// Read-only git subcommands allowed in the notes repo.
pub const NOTES_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
];

// ---------------------------------------------------------------------------
// Locations.

/// `path` made absolute and canonical as far as it exists (the rest is
/// appended lexically, with `..` resolved).
pub fn canonical(path: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            c => lexical.push(c.as_os_str()),
        }
    }
    let mut existing = lexical.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (
            existing.file_name().map(|n| n.to_owned()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return lexical,
        }
    }
}

/// Where a directory is, for `guard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loc {
    Board,
    Notes,
    /// Another repo (its top level when one was found).
    Other(Option<PathBuf>),
}

/// Classify `dir` against the board clone and the notes repo. A linked
/// worktree whose `.git` file points into one of them counts as it.
pub fn locate(dir: &Path, board: Option<&Path>, notes: Option<&Path>) -> Loc {
    let dir = canonical(dir);
    let inside = |p: &Path, root: Option<&Path>| root.is_some_and(|r| p.starts_with(canonical(r)));
    if inside(&dir, board) {
        return Loc::Board;
    }
    if inside(&dir, notes) {
        return Loc::Notes;
    }
    let mut cur = Some(dir.as_path());
    while let Some(d) = cur {
        let dotgit = d.join(".git");
        if dotgit.is_file() {
            if let Some(gitdir) = std::fs::read_to_string(&dotgit).ok().and_then(|t| {
                t.lines()
                    .next()
                    .and_then(|l| l.strip_prefix("gitdir:"))
                    .map(|s| s.trim().to_owned())
            }) {
                let g = canonical(&d.join(gitdir));
                if inside(&g, board) {
                    return Loc::Board;
                }
                if inside(&g, notes) {
                    return Loc::Notes;
                }
            }
            return Loc::Other(Some(d.to_path_buf()));
        }
        if dotgit.is_dir() {
            return Loc::Other(Some(d.to_path_buf()));
        }
        cur = d.parent();
    }
    Loc::Other(None)
}

/// Is `path` (absolute, or relative to `cwd`) under `root`?
pub fn path_under(path: &Path, cwd: &Path, root: &Path) -> bool {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    canonical(&p).starts_with(canonical(root))
}

// ---------------------------------------------------------------------------
// `git push` analysis.

/// What a `git push` does that needs the human.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Push {
    pub force: bool,
    pub delete: bool,
    pub tags: bool,
    pub mirror: bool,
    pub dry_run: bool,
    /// Something could not be read (an expansion).
    pub unknown: bool,
    /// The remote named, if any.
    pub remote: Option<String>,
    /// Destination branches named by refspecs (`HEAD` kept as `HEAD`).
    pub branches: Vec<String>,
    /// No refspec: the current branch's push target.
    pub implicit: bool,
}

pub fn parse_push(args: &[Word]) -> Push {
    let mut p = Push::default();
    let mut positional = Vec::new();
    let mut i = 0;
    let mut flags_done = false;
    while let Some(w) = args.get(i) {
        i += 1;
        let t = w.text.as_str();
        if w.dynamic {
            p.unknown = true;
            continue;
        }
        if flags_done || !t.starts_with('-') || t == "-" {
            positional.push(t.to_owned());
            continue;
        }
        match t {
            "--" => flags_done = true,
            "--force" | "--force-if-includes" => p.force = true,
            t if t.starts_with("--force-with-lease") => p.force = true,
            "--delete" | "--prune" => p.delete = true,
            "--tags" | "--follow-tags" => p.tags = true,
            "--mirror" | "--all" | "--branches" => p.mirror = true,
            "--dry-run" => p.dry_run = true,
            "-o" | "--push-option" | "--repo" | "--receive-pack" | "--exec" => i += 1,
            t if t.starts_with("--") => {}
            t => {
                // A bundle of short flags (`-uf`).
                for c in t[1..].chars() {
                    match c {
                        'f' => p.force = true,
                        'd' => p.delete = true,
                        'n' => p.dry_run = true,
                        _ => {}
                    }
                }
                if t.ends_with('o') {
                    i += 1;
                }
            }
        }
    }
    let mut refspecs = positional.into_iter();
    p.remote = refspecs.next();
    let mut any = false;
    while let Some(r) = refspecs.next() {
        any = true;
        if r == "tag" {
            p.tags = true;
            refspecs.next();
            continue;
        }
        let r = match r.strip_prefix('+') {
            Some(rest) => {
                p.force = true;
                rest.to_owned()
            }
            None => r,
        };
        let (src, dst) = match r.split_once(':') {
            Some((s, d)) => (s.to_owned(), d.to_owned()),
            None => (r.clone(), r.clone()),
        };
        if src.is_empty() {
            p.delete = true;
        }
        if dst.starts_with("refs/tags/") || src.starts_with("refs/tags/") {
            p.tags = true;
            continue;
        }
        if dst.is_empty() {
            p.unknown = true;
            continue;
        }
        let dst = dst.strip_prefix("refs/heads/").unwrap_or(&dst).to_owned();
        if dst.starts_with("refs/") || dst.contains('*') {
            p.unknown = true;
            continue;
        }
        p.branches.push(dst);
    }
    p.implicit = !any && !p.mirror && !p.tags;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(cmds: &[Vec<Word>]) -> Vec<Vec<String>> {
        cmds.iter()
            .map(|c| c.iter().map(|w| w.text.clone()).collect())
            .collect()
    }

    #[test]
    fn splits_chains_quotes_and_redirections() {
        let c = split_commands(r#"cd /x && git commit -m "a; b" | tee out.log 2>&1; echo 'q' >f"#)
            .unwrap();
        assert_eq!(
            texts(&c),
            vec![
                vec!["cd", "/x"],
                vec!["git", "commit", "-m", "a; b"],
                vec!["tee", "out.log"],
                vec!["echo", "q"],
            ]
        );
    }

    #[test]
    fn substitutions_and_heredocs() {
        let c = split_commands(
            "echo $(git rev-parse HEAD) `git status`\ncat <<'EOF'\ngit push origin main\nEOF\nls",
        )
        .unwrap();
        assert_eq!(
            texts(&c),
            vec![
                vec!["git", "rev-parse", "HEAD"],
                vec!["git", "status"],
                vec!["echo", "$(git rev-parse HEAD)", "`...`"],
                vec!["cat"],
                vec!["ls"],
            ]
        );
        assert!(c[2][1].dynamic);
        assert!(split_commands("echo 'open").is_err());
        assert!(split_commands("echo $(open").is_err());
    }

    #[test]
    fn git_calls_follow_cd_and_c() {
        let inv = invocations(
            "cd /repo && git -C sub -c user.name=x push origin HEAD:main; sudo git status",
            Path::new("/start"),
        )
        .unwrap();
        let gits: Vec<&GitCall> = inv
            .iter()
            .filter_map(|i| match i {
                Invocation::Git(g) => Some(g),
                _ => None,
            })
            .collect();
        assert_eq!(gits[0].dir.as_deref(), Some(Path::new("/repo/sub")));
        assert_eq!(gits[0].sub.as_deref(), Some("push"));
        assert_eq!(gits[1].dir.as_deref(), Some(Path::new("/repo")));
        assert_eq!(gits[1].sub.as_deref(), Some("status"));
        let inv = invocations("GIT_DIR=/x git commit", Path::new("/s")).unwrap();
        assert!(matches!(&inv[0], Invocation::Git(g) if g.dir.is_none()));
        let inv = invocations("bash -lc 'cd /n && git add .'", Path::new("/s")).unwrap();
        assert!(matches!(&inv[0], Invocation::Git(g) if g.dir.as_deref() == Some(Path::new("/n"))));
    }

    #[test]
    fn push_parsing() {
        let w = |s: &str| s.split_whitespace().map(Word::lit).collect::<Vec<_>>();
        let p = parse_push(&w("origin feat/x"));
        assert_eq!(p.branches, vec!["feat/x"]);
        assert!(!p.force && !p.implicit);
        assert!(parse_push(&w("-uf origin x")).force);
        assert!(parse_push(&w("origin +x")).force);
        assert!(parse_push(&w("origin :x")).delete);
        assert!(parse_push(&w("origin --delete x")).delete);
        assert!(parse_push(&w("origin v1 tag v2")).tags);
        assert!(parse_push(&w("origin refs/tags/v1")).tags);
        assert!(parse_push(&w("--mirror")).mirror);
        assert_eq!(
            parse_push(&w("origin HEAD:refs/heads/main")).branches,
            vec!["main"]
        );
        assert!(parse_push(&w("")).implicit);
        assert!(parse_push(&w("-u origin")).implicit);
    }

    #[test]
    fn keys_are_file_names() {
        assert!(valid_key("local_df26eef1-b2c1"));
        assert!(!valid_key("../x") && !valid_key("") && !valid_key(".hidden"));
    }
}
