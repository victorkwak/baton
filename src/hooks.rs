//! Claude Code integration core: session keys and arming for the waiter.
//! The guard is in `guard.rs`. Owner: agent C (`docs/plan.md`).
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
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::state_dir;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_file_names() {
        assert!(valid_key("local_df26eef1-b2c1"));
        assert!(!valid_key("../x") && !valid_key("") && !valid_key(".hidden"));
    }
}
