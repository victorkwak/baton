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
    if !crate::identity::is_agent() {
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
//
// The guard is hardening, not a boundary (`docs/design.md`, D6). When it
// cannot be sure what a command does to which repository, it asks.

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

/// What separates a command from the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    /// `;`, a newline, or the end.
    Seq,
    /// `&` (background).
    Amp,
    And,
    Or,
    /// `|` or `|&`.
    Pipe,
}

/// Where a command's standard input comes from (redirections only; a pipe
/// is known from the separator before it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdin {
    Inherit,
    /// A here-document or here-string with this text.
    Text(String),
    /// A file, or text with expansions.
    Unknown,
}

/// A simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub words: Vec<Word>,
    pub stdin: Stdin,
    /// Command substitutions in its words, each its own scope.
    pub subs: Vec<Vec<Item>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Cmd(Cmd),
    /// `( ... )` (a subshell) or `{ ...; }`.
    Group {
        list: Vec<Item>,
        subshell: bool,
    },
}

/// A node and the separator after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub node: Node,
    pub sep: Sep,
}

const MAX_DEPTH: usize = 8;

/// Parse a shell command line into a list of items. Errors on unbalanced
/// quotes, substitutions or groups, and on constructs it does not follow
/// (a `case` pattern's `)`).
pub fn parse_shell(src: &str) -> std::result::Result<Vec<Item>, String> {
    lex(src, 0)
}

#[derive(Debug)]
enum Tok {
    Cmd(Cmd),
    Sep(Sep),
    Open(bool),
    Close(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redir {
    Out,
    In,
    HereString,
}

struct Lexer {
    c: Vec<char>,
    i: usize,
    depth: usize,
    toks: Vec<Tok>,
    cur: Vec<Word>,
    cur_stdin: Stdin,
    cur_subs: Vec<Vec<Item>>,
    cur_heredocs: Vec<(String, bool)>,
    word: Option<Word>,
    redirect: Option<Redir>,
    /// Here-documents waiting for their body: (delimiter, strip tabs, token index).
    heredocs: Vec<(String, bool, usize)>,
}

fn lex(src: &str, depth: usize) -> std::result::Result<Vec<Item>, String> {
    if depth > MAX_DEPTH {
        return Err("command nests too deeply".into());
    }
    let mut l = Lexer {
        c: src.chars().collect(),
        i: 0,
        depth,
        toks: Vec::new(),
        cur: Vec::new(),
        cur_stdin: Stdin::Inherit,
        cur_subs: Vec::new(),
        cur_heredocs: Vec::new(),
        word: None,
        redirect: None,
        heredocs: Vec::new(),
    };
    l.run()?;
    let mut i = 0;
    let items = parse_items(&l.toks, &mut i, None)?;
    if i < l.toks.len() {
        return Err("unbalanced `)` or `}`".into());
    }
    Ok(items)
}

/// Build the item tree from tokens, up to the `Close` matching `until`.
fn parse_items(
    toks: &[Tok],
    i: &mut usize,
    until: Option<bool>,
) -> std::result::Result<Vec<Item>, String> {
    let mut items = Vec::new();
    while *i < toks.len() {
        let node = match &toks[*i] {
            Tok::Cmd(c) => {
                *i += 1;
                Node::Cmd(c.clone())
            }
            Tok::Open(sub) => {
                *i += 1;
                let list = parse_items(toks, i, Some(*sub))?;
                match toks.get(*i) {
                    Some(Tok::Close(s)) if s == sub => *i += 1,
                    _ => return Err("unbalanced `(` or `{`".into()),
                }
                Node::Group {
                    list,
                    subshell: *sub,
                }
            }
            Tok::Close(sub) => {
                if until == Some(*sub) {
                    return Ok(items);
                }
                return Err("unbalanced `)` or `}`".into());
            }
            Tok::Sep(_) => {
                // A separator with no command before it (`;;`, a leading `;`).
                *i += 1;
                continue;
            }
        };
        let sep = match toks.get(*i) {
            Some(Tok::Sep(s)) => {
                *i += 1;
                *s
            }
            _ => Sep::Seq,
        };
        items.push(Item { node, sep });
    }
    if until.is_some() {
        return Err("unbalanced `(` or `{`".into());
    }
    Ok(items)
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
            match self.redirect.take() {
                None => self.cur.push(w),
                Some(Redir::Out) => {}
                Some(Redir::In) => self.cur_stdin = Stdin::Unknown,
                Some(Redir::HereString) => {
                    self.cur_stdin = if w.dynamic {
                        Stdin::Unknown
                    } else {
                        Stdin::Text(format!("{}\n", w.text))
                    }
                }
            }
        }
    }

    fn end_cmd(&mut self) {
        self.end_word();
        self.redirect = None;
        let heredocs = std::mem::take(&mut self.cur_heredocs);
        let subs = std::mem::take(&mut self.cur_subs);
        let stdin = std::mem::replace(&mut self.cur_stdin, Stdin::Inherit);
        if self.cur.is_empty() && subs.is_empty() && heredocs.is_empty() {
            return;
        }
        let idx = self.toks.len();
        for (d, s) in heredocs {
            self.heredocs.push((d, s, idx));
        }
        self.toks.push(Tok::Cmd(Cmd {
            words: std::mem::take(&mut self.cur),
            stdin,
            subs,
        }));
    }

    fn sep(&mut self, s: Sep) {
        self.end_cmd();
        self.toks.push(Tok::Sep(s));
    }

    fn run(&mut self) -> std::result::Result<(), String> {
        while let Some(ch) = self.peek(0) {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.end_word();
                    self.i += 1;
                }
                '\n' => {
                    self.sep(Sep::Seq);
                    self.i += 1;
                    self.read_heredocs();
                }
                '&' if self.peek(1) == Some('>') => {
                    self.end_word();
                    self.i += 2;
                    if self.peek(0) == Some('>') {
                        self.i += 1;
                    }
                    self.redirect = Some(Redir::Out);
                }
                '&' if self.peek(1) == Some('&') => {
                    self.sep(Sep::And);
                    self.i += 2;
                }
                '&' => {
                    self.sep(Sep::Amp);
                    self.i += 1;
                }
                '|' if self.peek(1) == Some('|') => {
                    self.sep(Sep::Or);
                    self.i += 2;
                }
                '|' => {
                    self.sep(Sep::Pipe);
                    self.i += if self.peek(1) == Some('&') { 2 } else { 1 };
                }
                ';' => {
                    self.sep(Sep::Seq);
                    self.i += 1;
                }
                '(' => {
                    self.end_cmd();
                    self.toks.push(Tok::Open(true));
                    self.i += 1;
                }
                ')' => {
                    self.end_cmd();
                    self.toks.push(Tok::Close(true));
                    self.i += 1;
                }
                '<' | '>' => self.redirection(ch)?,
                '#' if self.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                '{' if self.word.is_none()
                    && (self.cur.is_empty()
                        || (self.cur.len() == 2 && self.cur[0].text == "function"))
                    && self.peek(1).is_none_or(char::is_whitespace) =>
                {
                    self.end_cmd();
                    self.toks.push(Tok::Open(false));
                    self.i += 1;
                }
                '}' if self.word.is_none()
                    && self.cur.is_empty()
                    && self.peek(1).is_none_or(|c| {
                        c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')')
                    }) =>
                {
                    self.end_cmd();
                    self.toks.push(Tok::Close(false));
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
        self.end_cmd();
        if !self.heredocs.is_empty() {
            return Err("here-document without its body".into());
        }
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
                self.i += 3;
                self.redirect = Some(Redir::HereString);
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
            self.cur_heredocs.push((delim, strip));
            return Ok(());
        }
        let input = ch == '<' && self.peek(1) != Some('>');
        self.i += 1;
        while self.peek(0).is_some_and(|c| matches!(c, '>' | '&' | '|')) {
            self.i += 1;
        }
        self.redirect = Some(if input { Redir::In } else { Redir::Out });
        Ok(())
    }

    /// After a newline: read the bodies of pending here-documents into
    /// their commands' stdin.
    fn read_heredocs(&mut self) {
        for (delim, strip, idx) in std::mem::take(&mut self.heredocs) {
            let mut body = String::new();
            let mut closed = false;
            while self.i < self.c.len() {
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.c[start..self.i].iter().collect();
                self.i += 1;
                let line = if strip {
                    line.trim_start_matches('\t').to_owned()
                } else {
                    line
                };
                if line == delim {
                    closed = true;
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if let Some(Tok::Cmd(cmd)) = self.toks.get_mut(idx) {
                cmd.stdin = match &cmd.stdin {
                    Stdin::Text(t) if closed => Stdin::Text(format!("{t}{body}")),
                    _ if closed => Stdin::Text(body),
                    _ => Stdin::Unknown,
                };
            }
        }
    }

    /// `$name`, `${...}`, `$(...)`, `$((...))` at `self.i`.
    fn dollar(&mut self) -> std::result::Result<(), String> {
        match self.peek(1) {
            Some('(') => {
                let close =
                    matching(&self.c, self.i + 1).ok_or_else(|| "unbalanced $(".to_owned())?;
                // `$((...))` is arithmetic only when the inner `(` closes
                // right before the outer one; `$( (cmd) )` is a substitution.
                let arithmetic = self.peek(2) == Some('(')
                    && close >= 1
                    && self.c[close - 1] == ')'
                    && matching(&self.c, self.i + 2) == Some(close - 1);
                let inner: String = if arithmetic {
                    self.c[self.i + 3..close - 1].iter().collect()
                } else {
                    self.c[self.i + 2..close].iter().collect()
                };
                // Arithmetic text can hold substitutions too: parse it either way.
                let sub = lex(&inner, self.depth + 1)?;
                self.cur_subs.push(sub);
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
        self.cur_subs.push(sub);
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
// Paths.

/// `..` and `.` removed lexically (how the shell's logical `cd` sees a path).
pub fn lexical(path: &Path) -> PathBuf {
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

/// The physical path: symlinks resolved component by component, before a
/// following `..` applies (how `chdir`, `git -C` and `--git-dir` see it).
/// Components that do not exist are appended as they are.
pub fn physical(path: &Path) -> PathBuf {
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

/// Kept for callers that only need "the real place of this path".
pub fn canonical(path: &Path) -> PathBuf {
    physical(path)
}

// ---------------------------------------------------------------------------
// Git invocations in a shell command.

/// A path given to git through an option or variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Unset,
    Known(PathBuf),
    /// Set, but to something this parser cannot read.
    Unknown,
}

/// A `git` invocation found in a Bash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCall {
    /// The directory git runs in (after `cd`, `env -C`, `-C`); `None` when
    /// it cannot be known.
    pub dir: Option<PathBuf>,
    /// The repository: `--git-dir`, `GIT_DIR`.
    pub git_dir: Spec,
    /// `--work-tree`, `GIT_WORK_TREE`.
    pub work_tree: Spec,
    /// Keys of `-c key=value` options, lowercase.
    pub configs: Vec<String>,
    /// Global options that change how git runs commands (`--config-env`,
    /// `--exec-path`), or that this parser does not know.
    pub risky_globals: bool,
    /// An environment variable that changes what git runs or where it
    /// pushes (`GIT_CONFIG_*`, `GIT_PAGER`, `GIT_SSH_COMMAND`, ...).
    pub risky_env: bool,
    /// The subcommand; `None` when there is none or it is unknown.
    pub sub: Option<String>,
    /// `true` when the subcommand cannot be read (an expansion, or
    /// arguments come from stdin through `xargs`).
    pub sub_unknown: bool,
    pub args: Vec<Word>,
    /// Found as an argument of another program (`find -exec git ...`).
    pub embedded: bool,
}

/// What a Bash command runs, as far as `guard` cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Git(GitCall),
    /// Any other command, with its directory (`None`: unknown) and words.
    Other {
        dir: Option<PathBuf>,
        words: Vec<Word>,
        /// No `PATH` change, function, alias or sourced file before it:
        /// a bare program name means what `PATH` says.
        trusted_path: bool,
    },
    /// A program whose name is an expansion (`$GIT push`): the words.
    Opaque(String),
    /// Something that cannot be read at all: always ask, with this reason.
    Unknown(String),
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn assignment(w: &Word) -> Option<(&str, &str)> {
    let (name, value) = w.text.split_once('=')?;
    let mut cs = name.chars();
    let ok = cs
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then_some((name, value))
}

const GIT_LOCATION_VARS: &[&str] = &["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"];

/// Environment variables that act like `-c` or `--exec-path`: they change
/// what git runs, which config it reads, or where it pushes.
pub fn risky_var(name: &str) -> bool {
    name.starts_with("GIT_CONFIG")
        || matches!(
            name,
            "GIT_PAGER"
                | "PAGER"
                | "GIT_EXTERNAL_DIFF"
                | "GIT_EXEC_PATH"
                | "GIT_SSH_COMMAND"
                | "GIT_SSH"
                | "GIT_ASKPASS"
                | "SSH_ASKPASS"
                | "GIT_EDITOR"
                | "EDITOR"
                | "VISUAL"
                | "GIT_SEQUENCE_EDITOR"
                | "GIT_TEMPLATE_DIR"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_INDEX_FILE"
                | "GIT_NAMESPACE"
                | "GIT_PROXY_COMMAND"
                | "GIT_TRACE2_EVENT"
                | "LESSOPEN"
        )
}

/// Expand `~`; `None` for an expansion or `~user`.
fn word_path(w: &Word) -> Option<PathBuf> {
    if w.dynamic {
        return None;
    }
    let t = &w.text;
    if t == "~" || t.starts_with("~/") {
        expand(t).ok()
    } else if t.starts_with('~') {
        None
    } else {
        Some(PathBuf::from(t))
    }
}

/// A path word against `dir`, physically resolved; `None` when unknown.
fn resolve(dir: Option<&Path>, w: &Word) -> Option<PathBuf> {
    let p = word_path(w)?;
    let abs = if p.is_absolute() { p } else { dir?.join(p) };
    Some(physical(&abs))
}

/// Possible working directories: `None` = unknown (anything).
pub type Cands = Option<std::collections::BTreeSet<PathBuf>>;

fn union(a: &Cands, b: &Cands) -> Cands {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.union(y).cloned().collect()),
        _ => None,
    }
}

fn single(p: PathBuf) -> Cands {
    Some(std::iter::once(p).collect())
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
    let items = parse_shell(command)?;
    let mut w = Walker {
        out: Vec::new(),
        env_unknown: false,
        env_risky: false,
        path_tainted: false,
        cond: 0,
        depth: 0,
    };
    w.list(&items, single(physical(cwd)))?;
    Ok(w.out)
}

struct Walker {
    out: Vec<Invocation>,
    /// `GIT_DIR` and friends were set for the rest of the command.
    env_unknown: bool,
    /// A variable that changes what git runs (`GIT_CONFIG_*`, `GIT_PAGER`,
    /// ...) was set for the rest of the command.
    env_risky: bool,
    /// `PATH`, a function, an alias or a sourced file may have redefined
    /// `baton` for the rest of the command.
    path_tainted: bool,
    /// Inside `if`/`while`/`until`/`for`: every `cd` is conditional.
    cond: usize,
    depth: usize,
}

impl Walker {
    /// Walk a list from `base`; returns the possible directories after it.
    fn list(&mut self, items: &[Item], base: Cands) -> std::result::Result<Cands, String> {
        let mut base = base;
        // (directories if the chain so far succeeded, if it failed)
        let mut chain: Option<(Cands, Cands)> = None;
        let mut prev = Sep::Seq;
        let mut pipe_in: Cands = None;
        for item in items {
            if self.cond > 0 && matches!(prev, Sep::And | Sep::Or) {
                prev = Sep::Seq;
            }
            let input = match (prev, &chain) {
                (Sep::Pipe, _) => pipe_in.clone(),
                (Sep::And, Some((s, _))) => s.clone(),
                (Sep::Or, Some((_, f))) => f.clone(),
                (_, Some((s, f))) => {
                    base = union(s, f);
                    chain = None;
                    base.clone()
                }
                (_, None) => base.clone(),
            };
            let in_pipe = prev == Sep::Pipe || item.sep == Sep::Pipe;
            if item.sep == Sep::Pipe && prev != Sep::Pipe {
                pipe_in = input.clone();
            }
            // Every command is checked: reachability (an `exit` that may
            // or may not run) only ever adds directories, never removes a
            // command from checking.
            let (mut s, mut f) = self.node(&item.node, input.clone(), prev == Sep::Pipe)?;
            if in_pipe || item.sep == Sep::Amp {
                // A pipeline element or a background job runs in a subshell.
                s = input.clone();
                f = input.clone();
            }
            chain = Some(match (prev, chain.take()) {
                (Sep::Pipe, Some(c)) => c,
                (Sep::And, Some((_, cf))) => (s, union(&cf, &f)),
                (Sep::Or, Some((cs, _))) => (union(&cs, &s), f),
                _ => (s, f),
            });
            prev = item.sep;
        }
        Ok(match chain {
            Some((s, f)) => union(&s, &f),
            None => base,
        })
    }

    /// Walk one node from `input`; returns (dirs on success, on failure).
    fn node(
        &mut self,
        node: &Node,
        input: Cands,
        piped: bool,
    ) -> std::result::Result<(Cands, Cands), String> {
        match node {
            Node::Group { list, subshell } => {
                if *subshell && list.is_empty() {
                    // `name() { ... }`: a function may shadow a program.
                    self.path_tainted = true;
                }
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("command nests too deeply".into());
                }
                let after = self.list(list, input.clone())?;
                self.depth -= 1;
                Ok(if *subshell {
                    (input.clone(), input)
                } else {
                    (after.clone(), after)
                })
            }
            Node::Cmd(cmd) => self.cmd(cmd, input, piped),
        }
    }

    fn emit_each(&mut self, dirs: &Cands, mut f: impl FnMut(Option<PathBuf>) -> Invocation) {
        match dirs {
            None => {
                let inv = f(None);
                self.out.push(inv);
            }
            Some(set) => {
                for d in set {
                    let inv = f(Some(d.clone()));
                    self.out.push(inv);
                }
            }
        }
    }

    fn cmd(
        &mut self,
        cmd: &Cmd,
        input: Cands,
        piped: bool,
    ) -> std::result::Result<(Cands, Cands), String> {
        for sub in &cmd.subs {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err("command nests too deeply".into());
            }
            self.list(sub, input.clone())?;
            self.depth -= 1;
        }
        let same = || Ok((input.clone(), input.clone()));
        let mut words = cmd.words.clone();
        let mut dirs = input.clone();
        let mut git_dir = Spec::Unset;
        let mut work_tree = Spec::Unset;
        let mut stdin_args = false;
        let mut risky_env = false;
        let mut path_local = false;
        let spec = |value: &str, w: &Word, dirs: &Cands| -> Spec {
            if w.dynamic {
                return Spec::Unknown;
            }
            let p = PathBuf::from(value);
            if p.is_absolute() {
                Spec::Known(physical(&p))
            } else {
                match dirs {
                    Some(s) if s.len() == 1 => {
                        Spec::Known(physical(&s.iter().next().expect("one").join(p)))
                    }
                    _ => Spec::Unknown,
                }
            }
        };
        loop {
            while let Some((name, value)) = words.first().and_then(assignment) {
                let (name, value) = (name.to_owned(), value.to_owned());
                let w = words.remove(0);
                let only_assignments = words.iter().all(|w| assignment(w).is_some());
                if risky_var(&name) {
                    if only_assignments {
                        self.env_risky = true;
                    } else {
                        risky_env = true;
                    }
                }
                if name == "PATH" {
                    if only_assignments {
                        self.path_tainted = true;
                    } else {
                        path_local = true;
                    }
                }
                if GIT_LOCATION_VARS.contains(&name.as_str()) {
                    if only_assignments {
                        // A shell variable for the rest of the command.
                        self.env_unknown = true;
                    } else if name == "GIT_DIR" {
                        git_dir = spec(&value, &w, &dirs);
                    } else if name == "GIT_WORK_TREE" {
                        work_tree = spec(&value, &w, &dirs);
                    } else {
                        git_dir = Spec::Unknown;
                    }
                }
            }
            let Some(first) = words.first() else { break };
            if first.dynamic {
                break;
            }
            match basename(&first.text) {
                "sudo" | "doas" => {
                    words.remove(0);
                    while let Some(w) = words.first() {
                        let t = w.text.clone();
                        if t == "-D" || t == "--chdir" {
                            words.remove(0);
                            if !words.is_empty() {
                                let p = words.remove(0);
                                dirs = chdir(&dirs, &p);
                            }
                        } else if t.starts_with("--chdir=") || (t.starts_with("-D") && t.len() > 2)
                        {
                            dirs = None;
                            words.remove(0);
                        } else if matches!(
                            t.as_str(),
                            "-u" | "-g" | "-C" | "-p" | "-r" | "-t" | "-U" | "-h"
                        ) {
                            words.remove(0);
                            if !words.is_empty() {
                                words.remove(0);
                            }
                        } else if t == "--" {
                            words.remove(0);
                            break;
                        } else if t.starts_with('-') {
                            words.remove(0);
                        } else {
                            break;
                        }
                    }
                }
                "env" => {
                    words.remove(0);
                    while let Some(w) = words.first() {
                        let t = w.text.clone();
                        if t == "-C" || t == "--chdir" {
                            words.remove(0);
                            if !words.is_empty() {
                                let p = words.remove(0);
                                dirs = chdir(&dirs, &p);
                            }
                        } else if let Some(p) = t.strip_prefix("--chdir=") {
                            dirs = chdir(&dirs, &Word::lit(p));
                            words.remove(0);
                        } else if t == "-u" || t == "-S" || t == "--unset" || t == "--split-string"
                        {
                            // `-S` splits a string into a command: unreadable.
                            if t == "-S" || t == "--split-string" {
                                self.out.push(Invocation::Unknown(
                                    "env -S runs a command this guard cannot read".into(),
                                ));
                                return same();
                            }
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
                "command" | "builtin" | "nohup" | "time" | "exec" => {
                    words.remove(0);
                    drop_flags(&mut words, &["-a"]);
                }
                "then" | "do" | "else" | "!" => {
                    words.remove(0);
                }
                "if" | "elif" | "while" | "until" => {
                    if basename(&first.text) != "elif" {
                        self.cond += 1;
                    }
                    words.remove(0);
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
                    stdin_args = true;
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
        if self.env_unknown {
            git_dir = Spec::Unknown;
        }
        let risky_env = risky_env || self.env_risky;
        let trusted_path = !path_local && !self.path_tainted;
        let Some(first) = words.first().cloned() else {
            return same();
        };
        if first.dynamic {
            let text = words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            self.out.push(Invocation::Opaque(text));
            return same();
        }
        match basename(&first.text) {
            "fi" | "done" | "esac" => {
                self.cond = self.cond.saturating_sub(1);
                same()
            }
            "for" | "select" | "case" => {
                self.cond += 1;
                same()
            }
            "cd" | "pushd" => {
                let mut args = words[1..].iter().filter(|w| w.text != "--");
                let mut physical_only = false;
                let mut target = None;
                for w in args.by_ref() {
                    match w.text.as_str() {
                        "-P" => physical_only = true,
                        "-L" | "-e" | "-@" => {}
                        _ => {
                            target = Some(w.clone());
                            break;
                        }
                    }
                }
                let to: Cands = match target {
                    None => crate::config::home()
                        .ok()
                        .map(|h| physical(&h))
                        .and_then(single),
                    Some(w) if w.text == "-" || w.text.starts_with('-') => None,
                    Some(w) => cd_target(&input, &w, physical_only),
                };
                if piped {
                    return same();
                }
                let ok = if self.cond > 0 {
                    union(&input, &to)
                } else {
                    to
                };
                Ok((ok, input))
            }
            "popd" | "source" | "." => {
                // Changes the directory (and maybe the environment) in ways
                // this parser cannot follow.
                if basename(&first.text) != "popd" {
                    self.env_unknown = true;
                    self.env_risky = true;
                    self.path_tainted = true;
                }
                self.emit_each(&dirs, |dir| Invocation::Other {
                    dir,
                    words: words.clone(),
                    trusted_path,
                });
                Ok((None, input))
            }
            "export" | "declare" | "typeset" | "local" | "readonly" => {
                for w in &words[1..] {
                    let name = w.text.split('=').next().unwrap_or("");
                    if w.dynamic || GIT_LOCATION_VARS.contains(&name) {
                        self.env_unknown = true;
                    }
                    if w.dynamic || risky_var(name) {
                        self.env_risky = true;
                    }
                    if w.dynamic || name == "PATH" || w.text == "-f" {
                        self.path_tainted = true;
                    }
                }
                same()
            }
            "alias" | "function" | "hash" | "enable" => {
                self.path_tainted = true;
                same()
            }
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
                self.shell(&words, cmd, &dirs, piped)?;
                same()
            }
            "eval" => {
                let text = words[1..]
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if words[1..].iter().any(|w| w.dynamic) {
                    self.out.push(Invocation::Unknown(
                        "eval of text this guard cannot read".into(),
                    ));
                } else {
                    self.nested(&text, &dirs)?;
                }
                same()
            }
            "git" => {
                let rest = words[1..].to_vec();
                let (gd, wt) = (git_dir.clone(), work_tree.clone());
                self.emit_each(&dirs, |dir| {
                    let mut g = parse_git(&rest, dir, gd.clone(), wt.clone(), stdin_args, false);
                    g.risky_env = risky_env;
                    Invocation::Git(g)
                });
                same()
            }
            prog => {
                // A program that runs its arguments as a command: `find
                // -exec git ...`, `parallel git ...`. Elsewhere `git` is data.
                let runner = matches!(
                    prog,
                    "parallel" | "flock" | "unbuffer" | "strace" | "ltrace" | "systemd-run"
                );
                let exec_flag = |i: usize| {
                    i > 0
                        && matches!(
                            words[i - 1].text.as_str(),
                            "-exec" | "-execdir" | "-ok" | "-okdir"
                        )
                };
                if let Some(p) = words.iter().enumerate().skip(1).position(|(i, w)| {
                    !w.dynamic && basename(&w.text) == "git" && (runner || exec_flag(i))
                }) {
                    let rest = words[p + 2..].to_vec();
                    let (gd, wt) = (git_dir.clone(), work_tree.clone());
                    self.emit_each(&dirs, |dir| {
                        let mut g = parse_git(&rest, dir, gd.clone(), wt.clone(), true, true);
                        g.risky_env = risky_env;
                        Invocation::Git(g)
                    });
                }
                self.emit_each(&dirs, |dir| Invocation::Other {
                    dir,
                    words: words.clone(),
                    trusted_path,
                });
                same()
            }
        }
    }

    /// Parse and walk shell text run by a nested shell or `eval`.
    fn nested(&mut self, text: &str, dirs: &Cands) -> std::result::Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("command nests too deeply".into());
        }
        let items = parse_shell(text)?;
        self.list(&items, dirs.clone())?;
        self.depth -= 1;
        Ok(())
    }

    /// `sh -c TEXT`, `sh -s`, `sh <<EOF`, `... | sh`, `sh script`.
    fn shell(
        &mut self,
        words: &[Word],
        cmd: &Cmd,
        dirs: &Cands,
        piped: bool,
    ) -> std::result::Result<(), String> {
        let mut i = 1;
        let mut c_flag = false;
        let mut s_flag = false;
        while let Some(w) = words.get(i) {
            let t = &w.text;
            if w.dynamic {
                self.out.push(Invocation::Unknown(
                    "a shell with options this guard cannot read".into(),
                ));
                return Ok(());
            }
            if t == "--" {
                i += 1;
                break;
            }
            if t.starts_with("--") {
                i += 1;
                continue;
            }
            if let Some(flags) = t.strip_prefix('-').or_else(|| t.strip_prefix('+'))
                && !flags.is_empty()
            {
                c_flag |= flags.contains('c');
                s_flag |= flags.contains('s');
                // `-o option` takes a value.
                if flags.ends_with('o') || flags.ends_with('O') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            break;
        }
        if c_flag {
            match words.get(i) {
                Some(script) if !script.dynamic => self.nested(&script.text, dirs)?,
                _ => self.out.push(Invocation::Unknown(
                    "a shell runs text this guard cannot read".into(),
                )),
            }
            return Ok(());
        }
        if words.get(i).is_some() && !s_flag {
            // A script file: like running `./script`, not inspected.
            self.emit_each(dirs, |dir| Invocation::Other {
                dir,
                words: words.to_vec(),
                trusted_path: false,
            });
            return Ok(());
        }
        // The shell reads its commands from stdin.
        match (&cmd.stdin, piped) {
            (Stdin::Text(t), false) => self.nested(t, dirs)?,
            _ => self.out.push(Invocation::Unknown(
                "a shell reads commands from stdin, which this guard cannot read".into(),
            )),
        }
        Ok(())
    }
}

/// `env -C` / `sudo -D`: each candidate moves to the physical target.
fn chdir(dirs: &Cands, w: &Word) -> Cands {
    let p = word_path(w)?;
    if p.is_absolute() {
        return single(physical(&p));
    }
    let set = dirs.as_ref()?;
    Some(set.iter().map(|d| physical(&d.join(&p))).collect())
}

/// `cd` target: the logical place (bash's default) and the physical one;
/// both are possible when they differ.
fn cd_target(input: &Cands, w: &Word, physical_only: bool) -> Cands {
    let p = word_path(w)?;
    let bases: Vec<PathBuf> = if p.is_absolute() {
        vec![PathBuf::new()]
    } else {
        input.as_ref()?.iter().cloned().collect()
    };
    let mut out = std::collections::BTreeSet::new();
    for b in bases {
        let joined = if p.is_absolute() {
            p.clone()
        } else {
            b.join(&p)
        };
        out.insert(physical(&joined));
        if !physical_only {
            out.insert(physical(&lexical(&joined)));
        }
    }
    Some(out)
}

/// Parse the words after `git`: global options, then the subcommand.
pub fn parse_git(
    ws: &[Word],
    dir: Option<PathBuf>,
    git_dir: Spec,
    work_tree: Spec,
    stdin_args: bool,
    embedded: bool,
) -> GitCall {
    let mut g = GitCall {
        dir,
        git_dir,
        work_tree,
        configs: Vec::new(),
        risky_globals: false,
        risky_env: false,
        sub: None,
        sub_unknown: false,
        args: Vec::new(),
        embedded,
    };
    let path_spec = |dir: &Option<PathBuf>, w: Option<&Word>| -> Spec {
        match w.and_then(|w| resolve(dir.as_deref(), w)) {
            Some(p) => Spec::Known(p),
            None => Spec::Unknown,
        }
    };
    let mut i = 0;
    while let Some(w) = ws.get(i) {
        let t = w.text.as_str();
        if w.dynamic {
            // An option or the subcommand: either way, unknown.
            break;
        }
        if !t.starts_with('-') {
            break;
        }
        match t {
            "-C" => {
                g.dir = ws.get(i + 1).and_then(|p| resolve(g.dir.as_deref(), p));
                i += 2;
            }
            "--git-dir" => {
                g.git_dir = path_spec(&g.dir, ws.get(i + 1));
                i += 2;
            }
            "--work-tree" => {
                g.work_tree = path_spec(&g.dir, ws.get(i + 1));
                i += 2;
            }
            "-c" => {
                match ws.get(i + 1) {
                    Some(kv) if !kv.dynamic => g
                        .configs
                        .push(kv.text.split('=').next().unwrap_or("").to_ascii_lowercase()),
                    _ => g.risky_globals = true,
                }
                i += 2;
            }
            "--config-env" | "--exec-path" => {
                g.risky_globals = true;
                i += 2;
            }
            "--namespace" | "--super-prefix" | "--list-cmds" | "--attr-source" => i += 2,
            "-p"
            | "-P"
            | "--paginate"
            | "--no-pager"
            | "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
            | "--no-lazy-fetch"
            | "--no-advice"
            | "--version"
            | "--help"
            | "-h"
            | "-v" => i += 1,
            _ => {
                if let Some(p) = t.strip_prefix("--git-dir=") {
                    g.git_dir = path_spec(&g.dir, Some(&Word::lit(p)));
                } else if let Some(p) = t.strip_prefix("--work-tree=") {
                    g.work_tree = path_spec(&g.dir, Some(&Word::lit(p)));
                } else if t.starts_with("--namespace=") || t.starts_with("--list-cmds=") {
                } else {
                    // `--config-env=`, `--exec-path=`, or unknown.
                    g.risky_globals = true;
                }
                i += 1;
            }
        }
    }
    match ws.get(i) {
        Some(w) if w.dynamic => g.sub_unknown = true,
        Some(w) => g.sub = Some(w.text.clone()),
        None if stdin_args => g.sub_unknown = true,
        None => {}
    }
    g.args = ws.get(i + 1..).map(<[Word]>::to_vec).unwrap_or_default();
    if stdin_args {
        g.args.push(Word {
            text: "<stdin>".into(),
            dynamic: true,
        });
    }
    g
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

/// Arguments of read-only commands that run programs or write files.
pub fn unsafe_read_arg(sub: &str, arg: &Word) -> bool {
    if arg.dynamic {
        return true;
    }
    let t = arg.text.as_str();
    if t.starts_with("--output")
        || t.starts_with("--open-files-in-pager")
        || t == "--ext-diff"
        || t == "--textconv"
        || t.starts_with("--exec")
        || t.starts_with("--upload-pack")
        || t.starts_with("--run")
    {
        return true;
    }
    // `git grep -O[pager]`, also inside a bundle of short flags.
    sub == "grep" && t.starts_with('-') && !t.starts_with("--") && t.contains('O')
}

/// Built-in git commands (`git help -a`, main and plumbing). A subcommand
/// not listed here may be an alias or an external `git-<name>` program.
pub const GIT_BUILTINS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bugreport",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "describe",
    "diagnose",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "difftool",
    "fetch",
    "fetch-pack",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "hash-object",
    "help",
    "index-pack",
    "init",
    "interpret-trailers",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-tree",
    "mergetool",
    "mktag",
    "mktree",
    "mv",
    "name-rev",
    "notes",
    "pack-objects",
    "pack-refs",
    "prune",
    "pull",
    "push",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "remote",
    "repack",
    "replace",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "send-pack",
    "shortlog",
    "show",
    "show-branch",
    "show-ref",
    "sparse-checkout",
    "stash",
    "status",
    "stripspace",
    "submodule",
    "switch",
    "symbolic-ref",
    "tag",
    "unpack-objects",
    "update-index",
    "update-ref",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
];

// ---------------------------------------------------------------------------
// Locations.

/// Where a directory is, for `guard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loc {
    Board,
    Notes,
    /// Another repo (its top level when one was found).
    Other(Option<PathBuf>),
}

impl Loc {
    /// Board, notes or other, ignoring which other repo.
    pub fn kind(&self) -> u8 {
        match self {
            Loc::Board => 0,
            Loc::Notes => 1,
            Loc::Other(_) => 2,
        }
    }
}

/// Classify `dir` (a work tree directory or a git dir) against the board
/// clone and the notes repo. A linked worktree whose `.git` file points into
/// one of them counts as it.
pub fn locate(dir: &Path, board: Option<&Path>, notes: Option<&Path>) -> Loc {
    let dir = physical(dir);
    let inside = |p: &Path, root: Option<&Path>| root.is_some_and(|r| p.starts_with(physical(r)));
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
                let g = physical(&d.join(gitdir));
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
    physical(&p).starts_with(physical(root)) || physical(&lexical(&p)).starts_with(physical(root))
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

    fn cmds(items: &[Item]) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for it in items {
            match &it.node {
                Node::Cmd(c) => {
                    for s in &c.subs {
                        out.extend(cmds(s));
                    }
                    out.push(c.words.iter().map(|w| w.text.clone()).collect());
                }
                Node::Group { list, .. } => out.extend(cmds(list)),
            }
        }
        out
    }

    fn gits(command: &str, cwd: &str) -> Vec<GitCall> {
        invocations(command, Path::new(cwd))
            .unwrap()
            .into_iter()
            .filter_map(|i| match i {
                Invocation::Git(g) => Some(g),
                _ => None,
            })
            .collect()
    }

    fn dirs(command: &str, cwd: &str) -> Vec<Option<PathBuf>> {
        gits(command, cwd).into_iter().map(|g| g.dir).collect()
    }

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    #[test]
    fn splits_chains_quotes_and_redirections() {
        let c = parse_shell(r#"cd /x && git commit -m "a; b" | tee out.log 2>&1; echo 'q' >f"#)
            .unwrap();
        assert_eq!(
            cmds(&c),
            vec![
                vec!["cd", "/x"],
                vec!["git", "commit", "-m", "a; b"],
                vec!["tee", "out.log"],
                vec!["echo", "q"],
            ]
        );
        let seps: Vec<Sep> = c.iter().map(|i| i.sep).collect();
        assert_eq!(seps, vec![Sep::And, Sep::Pipe, Sep::Seq, Sep::Seq]);
        assert!(parse_shell("echo 'open").is_err());
        assert!(parse_shell("echo $(open").is_err());
        assert!(parse_shell("(echo").is_err());
    }

    #[test]
    fn substitutions_heredocs_and_arithmetic() {
        let c = parse_shell(
            "echo $(git rev-parse HEAD) `git status`\ncat <<'EOF'\ngit push origin main\nEOF\nls",
        )
        .unwrap();
        assert_eq!(
            cmds(&c),
            vec![
                vec!["git", "rev-parse", "HEAD"],
                vec!["git", "status"],
                vec!["echo", "$(git rev-parse HEAD)", "`...`"],
                vec!["cat"],
                vec!["ls"],
            ]
        );
        match &c[1].node {
            Node::Cmd(cmd) => assert_eq!(cmd.stdin, Stdin::Text("git push origin main\n".into())),
            n => panic!("{n:?}"),
        }
        // `$((` is arithmetic only when it closes with `))`.
        let c = parse_shell("echo $((1 + 2))").unwrap();
        assert_eq!(cmds(&c).last().unwrap(), &vec!["echo", "$((1 + 2))"]);
        let c = parse_shell("echo $( (git push origin main) )").unwrap();
        assert!(cmds(&c).contains(&vec![
            "git".into(),
            "push".into(),
            "origin".into(),
            "main".into()
        ]));
        let c = parse_shell("echo $((cd /n; git commit) )").unwrap();
        assert!(cmds(&c).contains(&vec!["git".into(), "commit".into()]));
        let c = parse_shell("echo $(( $(git push) + 1 ))").unwrap();
        assert!(cmds(&c).contains(&vec!["git".into(), "push".into()]));
    }

    #[test]
    fn git_calls_follow_cd_and_c() {
        let g = gits(
            "cd /repo && git -C sub -c user.name=x push origin HEAD:main; sudo git status",
            "/start",
        );
        assert_eq!(g[0].dir, p("/repo/sub"));
        assert_eq!(g[0].sub.as_deref(), Some("push"));
        assert_eq!(g[0].configs, vec!["user.name"]);
        // After `cd X && ...;` the cd may have failed: both places.
        assert_eq!(g[1].dir, p("/repo"));
        assert_eq!(g[2].dir, p("/start"));
        let g = gits("GIT_DIR=/x git commit", "/s");
        assert_eq!(g[0].git_dir, Spec::Known(PathBuf::from("/x")));
        let g = gits("bash -lc 'cd /n && git add .'", "/s");
        assert_eq!(g[0].dir, p("/n"));
    }

    #[test]
    fn cd_scoping() {
        // A subshell or substitution keeps its cd to itself.
        assert_eq!(
            dirs("(cd /a && git status); git status", "/s"),
            vec![p("/a"), p("/s")]
        );
        assert_eq!(dirs("echo $(cd /a); git status", "/s"), vec![p("/s")]);
        // `cd X || exit` style: after `||`, the next command runs only if cd failed.
        assert_eq!(dirs("cd /a || git status", "/s"), vec![p("/s")]);
        // `exit` never hides a later command; it can only add directories.
        assert_eq!(
            dirs("cd /a || exit 1; git status", "/s"),
            vec![p("/a"), p("/s")]
        );
        assert_eq!(dirs("f() { exit; }; git status", "/s"), vec![p("/s")]);
        // Conditional cd: both places afterwards.
        assert_eq!(
            dirs("if true; then cd /a; fi; git status", "/s"),
            vec![p("/a"), p("/s")]
        );
        assert_eq!(
            dirs("true && cd /a; git status", "/s"),
            vec![p("/a"), p("/s")]
        );
        // A cd in a pipeline does not change the shell's directory.
        assert_eq!(dirs("cd /a | cat; git status", "/s"), vec![p("/s")]);
        // Unknown target.
        assert_eq!(dirs("cd $X && git status", "/s"), vec![None]);
        assert_eq!(dirs("source env.sh; git status", "/s"), vec![None]);
    }

    #[test]
    fn exported_git_dir_makes_the_repo_unknown() {
        for cmd in [
            "export GIT_DIR=/x; git commit",
            "GIT_DIR=/x; git commit",
            "declare -x GIT_WORK_TREE=/y\ngit add .",
        ] {
            let g = gits(cmd, "/s");
            assert_eq!(g[0].git_dir, Spec::Unknown, "{cmd}");
        }
    }

    #[test]
    fn shells_reading_stdin_are_unknown_or_inspected() {
        let unknown = |cmd: &str| {
            invocations(cmd, Path::new("/s"))
                .unwrap()
                .iter()
                .any(|i| matches!(i, Invocation::Unknown(_)))
        };
        assert!(unknown("echo 'git push' | bash"));
        assert!(unknown("bash -s < script.sh"));
        assert!(unknown("sh -c \"$X\""));
        // Here-text is inspected.
        assert_eq!(
            gits("bash <<< 'git push origin main'", "/s")[0]
                .sub
                .as_deref(),
            Some("push")
        );
        assert_eq!(
            gits("sh <<EOF\ncd /n\ngit commit\nEOF", "/s")[0].dir,
            p("/n")
        );
    }

    #[test]
    fn symlinks_resolve_before_dotdot() {
        let t = tempfile::TempDir::new().unwrap();
        let real = physical(t.path());
        std::fs::create_dir_all(real.join("a/b")).unwrap();
        std::os::unix::fs::symlink(real.join("a/b"), real.join("link")).unwrap();
        // `link/..` is `a` physically, `real` logically.
        assert_eq!(physical(&real.join("link/..")), real.join("a"));
        let r = real.to_str().unwrap();
        assert_eq!(
            dirs(&format!("git -C {r}/link/.. status"), "/"),
            vec![Some(real.join("a"))]
        );
        let mut d = dirs(&format!("cd {r}/link/.. && git status"), "/");
        d.sort();
        assert_eq!(d, vec![Some(real.clone()), Some(real.join("a"))]);
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
