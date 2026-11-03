//! Board records: messages (`messages/<ulid>.md`) and events
//! (`events/<ulid>.json`), with their validation, writer and strict parser.
//!
//! The parser accepts exactly what the writer produces: it parses, renders
//! the result again, and refuses the file when the bytes differ. So an
//! unknown key, a reordered key, a needless quote or a bad value all fail.
//! See `docs/design.md`, "Board repo".

use std::fmt;
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};
use ulid::{Generator, Ulid};

use crate::config::valid_name;
use crate::error::{Error, Kind, Result};

/// Longest subject, in characters.
pub const MAX_SUBJECT: usize = 200;
/// Largest body, in bytes.
pub const MAX_BODY: usize = 64 * 1024;
/// Most `related` entries on one message.
pub const MAX_RELATED: usize = 16;

static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// A new ULID; ids made by one process increase strictly.
pub fn new_id() -> String {
    let mut g = GENERATOR.lock().unwrap_or_else(|p| p.into_inner());
    match g.generate() {
        Ok(id) => id.to_string(),
        Err(overflow) => overflow.commit_overflow_increment().to_string(),
    }
}

/// Now, UTC, RFC 3339 with whole seconds.
pub fn now() -> String {
    format_at(jiff::Timestamp::now())
}

pub fn format_at(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parse an `at` value.
pub fn parse_at(at: &str) -> Option<jiff::Timestamp> {
    at.parse::<jiff::Timestamp>().ok()
}

/// Random bits for nonces and backoff jitter (no extra dependency: the ULID
/// generator already draws from the OS RNG).
pub fn random_u64() -> u64 {
    Ulid::generate().random() as u64
}

static RE_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap());
static RE_TASK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,47}$").unwrap());
static RE_SESSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").unwrap());
static RE_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([A-Za-z0-9][A-Za-z0-9._-]{0,99}):([A-Za-z0-9][A-Za-z0-9._/-]{0,199})@([0-9a-f]{7,64})$",
    )
    .unwrap()
});
static RE_WIKILINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\[[A-Za-z0-9][A-Za-z0-9 _./#|-]{0,199}\]\]$").unwrap());
static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https?://[A-Za-z0-9._~:/?#@%+=&,!*-]{1,240}$").unwrap());
static RE_SLUG_SAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").unwrap());

fn bad(field: &str, value: &str, want: &str) -> Error {
    Error::new(Kind::Internal, format!("invalid {field} {value:?}: {want}"))
}

pub fn check_id(field: &str, id: &str) -> Result<()> {
    // Canonical form only: 26 Crockford base32 characters, upper case.
    let ok = id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
        && Ulid::from_string(id).is_ok_and(|u| u.to_string() == id);
    if ok {
        Ok(())
    } else {
        Err(bad(field, id, "a ULID (26 upper-case base32 characters)"))
    }
}

pub fn check_at(at: &str) -> Result<()> {
    if RE_AT.is_match(at) && parse_at(at).is_some() {
        Ok(())
    } else {
        Err(bad(
            "at",
            at,
            "UTC RFC 3339 with seconds, e.g. 2026-01-02T03:04:05Z",
        ))
    }
}

/// A machine or the human (`from`, `by`, `lead`).
pub fn check_name(field: &str, name: &str) -> Result<()> {
    if valid_name(name) && name != "any" {
        Ok(())
    } else {
        Err(bad(field, name, "[a-z][a-z0-9-]{0,15}, not \"any\""))
    }
}

/// A message address (`to`): a name or `any`.
pub fn check_to(to: &str) -> Result<()> {
    if valid_name(to) {
        Ok(())
    } else {
        Err(bad("to", to, "a machine, the human, or \"any\""))
    }
}

pub fn check_task(task: &str) -> Result<()> {
    if RE_TASK.is_match(task) {
        Ok(())
    } else {
        Err(bad("task", task, "[a-z0-9][a-z0-9-]{0,47}"))
    }
}

pub fn check_session(session: &str) -> Result<()> {
    if RE_SESSION.is_match(session) {
        Ok(())
    } else {
        Err(bad("session", session, "[A-Za-z0-9][A-Za-z0-9._-]{0,127}"))
    }
}

pub fn check_branch(branch: &str) -> Result<()> {
    let ok = RE_BRANCH.captures(branch).is_some_and(|c| {
        let name = &c[2];
        !name.contains("..")
            && !name.contains("//")
            && !name.ends_with('/')
            && !name.ends_with(".lock")
            && !name.ends_with('.')
    });
    if ok {
        Ok(())
    } else {
        Err(bad("branch", branch, "repo:branch@sha (sha: 7-64 hex)"))
    }
}

/// `repo:branch` of a `repo:branch@sha` value (the branch-ownership key).
pub fn branch_key(branch: &str) -> &str {
    branch.rsplit_once('@').map_or(branch, |(k, _)| k)
}

pub fn check_related(r: &str) -> Result<()> {
    if RE_WIKILINK.is_match(r) || RE_URL.is_match(r) {
        Ok(())
    } else {
        Err(bad(
            "related",
            r,
            "[[note]] or an http(s) URL without spaces, quotes, $, ;, |, (, ) or backticks",
        ))
    }
}

pub fn check_subject(s: &str) -> Result<()> {
    if s.trim().is_empty() {
        return Err(bad("subject", s, "not empty"));
    }
    if s.contains(['\n', '\r']) {
        return Err(bad("subject", s, "one line"));
    }
    if s.chars().count() > MAX_SUBJECT {
        return Err(Error::new(
            Kind::Internal,
            format!("subject is longer than {MAX_SUBJECT} characters"),
        ));
    }
    if s.contains('\0') {
        return Err(bad("subject", s, "no NUL characters"));
    }
    Ok(())
}

pub fn check_body(b: &str) -> Result<()> {
    if b.len() > MAX_BODY {
        return Err(Error::new(
            Kind::Internal,
            format!("body is larger than {MAX_BODY} bytes"),
        ));
    }
    if b.contains('\0') {
        return Err(Error::new(Kind::Internal, "body contains a NUL character"));
    }
    Ok(())
}

/// A message's `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MsgKind {
    #[default]
    Plain,
    Plan,
    Handover,
    CloseTask,
}

impl MsgKind {
    /// The frontmatter value (empty for a plain message).
    pub fn as_str(self) -> &'static str {
        match self {
            MsgKind::Plain => "",
            MsgKind::Plan => "plan",
            MsgKind::Handover => "handover",
            MsgKind::CloseTask => "close-task",
        }
    }

    pub fn parse(s: &str) -> Option<MsgKind> {
        Some(match s {
            "" => MsgKind::Plain,
            "plan" => MsgKind::Plan,
            "handover" => MsgKind::Handover,
            "close-task" => MsgKind::CloseTask,
            _ => return None,
        })
    }

    /// Plan, handover and close-task need the network (D3).
    pub fn rule_bearing(self) -> bool {
        self != MsgKind::Plain
    }
}

/// A message status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Working,
    Done,
    Wontdo,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Working => "working",
            Status::Done => "done",
            Status::Wontdo => "wontdo",
        }
    }

    pub fn is_closed(self) -> bool {
        matches!(self, Status::Done | Status::Wontdo)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub at: String,
    pub from: String,
    pub to: String,
    pub task: String,
    pub kind: MsgKind,
    pub reply_to: String,
    pub related: Vec<String>,
    pub branch: String,
    pub session: String,
    pub subject: String,
    pub body: String,
}

impl Message {
    pub fn path(&self) -> String {
        format!("messages/{}.md", self.id)
    }

    /// Check every field against its character set.
    pub fn validate(&self) -> Result<()> {
        check_id("id", &self.id)?;
        check_at(&self.at)?;
        check_name("from", &self.from)?;
        check_to(&self.to)?;
        if !self.task.is_empty() {
            check_task(&self.task)?;
        }
        if !self.reply_to.is_empty() {
            check_id("reply_to", &self.reply_to)?;
        }
        if self.related.len() > MAX_RELATED {
            return Err(Error::new(
                Kind::Internal,
                format!("more than {MAX_RELATED} related entries"),
            ));
        }
        for r in &self.related {
            check_related(r)?;
        }
        if !self.branch.is_empty() {
            check_branch(&self.branch)?;
        }
        if !self.session.is_empty() {
            check_session(&self.session)?;
        }
        check_subject(&self.subject)?;
        check_body(&self.body)?;
        if self.kind != MsgKind::Plain && self.task.is_empty() {
            return Err(Error::new(
                Kind::Internal,
                format!("a {} message needs a task", self.kind.as_str()),
            ));
        }
        Ok(())
    }

    /// The file's bytes.
    pub fn render(&self) -> String {
        let mut s = String::from("---\n");
        let mut kv = |k: &str, v: &str| {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(&scalar(v));
            s.push('\n');
        };
        kv("id", &self.id);
        kv("at", &self.at);
        kv("from", &self.from);
        kv("to", &self.to);
        kv("task", &self.task);
        kv("kind", self.kind.as_str());
        kv("reply_to", &self.reply_to);
        s.push_str("related: ");
        s.push_str(&serde_json::to_string(&self.related).expect("strings serialize"));
        s.push('\n');
        let mut kv = |k: &str, v: &str| {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(&scalar(v));
            s.push('\n');
        };
        kv("branch", &self.branch);
        kv("session", &self.session);
        s.push_str("---\n\n# ");
        s.push_str(&self.subject);
        s.push_str("\n\n");
        s.push_str(&self.body);
        s
    }

    /// Parse a message file. Refuses anything the writer would not produce.
    pub fn parse(text: &str) -> Result<Message> {
        let err = |why: &str| Error::new(Kind::Internal, format!("bad message file: {why}"));
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| err("no frontmatter"))?;
        let (front, after) = rest
            .split_once("\n---\n")
            .ok_or_else(|| err("unterminated frontmatter"))?;
        let mut fields: Vec<(&str, &str)> = Vec::new();
        for line in front.lines() {
            let (k, v) = line
                .split_once(": ")
                .ok_or_else(|| err("frontmatter line without `key: value`"))?;
            fields.push((k, v));
        }
        const KEYS: [&str; 10] = [
            "id", "at", "from", "to", "task", "kind", "reply_to", "related", "branch", "session",
        ];
        if fields.len() != KEYS.len() || fields.iter().zip(KEYS).any(|((k, _), want)| *k != want) {
            return Err(err(
                "frontmatter keys differ from the expected set and order",
            ));
        }
        let get =
            |i: usize| -> Result<String> { unscalar(fields[i].1).ok_or_else(|| err(KEYS[i])) };
        let related: Vec<String> = serde_json::from_str(fields[7].1)
            .map_err(|_| err("related is not a JSON string list"))?;
        let body_part = after
            .strip_prefix("\n# ")
            .ok_or_else(|| err("no `# subject` line"))?;
        let (subject, body) = body_part
            .split_once("\n\n")
            .ok_or_else(|| err("no blank line after the subject"))?;
        let msg = Message {
            id: get(0)?,
            at: get(1)?,
            from: get(2)?,
            to: get(3)?,
            task: get(4)?,
            kind: MsgKind::parse(&get(5)?).ok_or_else(|| err("unknown kind"))?,
            reply_to: get(6)?,
            related,
            branch: get(8)?,
            session: get(9)?,
            subject: subject.to_owned(),
            body: body.to_owned(),
        };
        msg.validate()?;
        if msg.render() != text {
            return Err(err("not in canonical form"));
        }
        Ok(msg)
    }
}

/// A value as written: bare when slug-safe, else JSON-quoted.
fn scalar(v: &str) -> String {
    if RE_SLUG_SAFE.is_match(v) {
        v.to_owned()
    } else {
        serde_json::to_string(v).expect("strings serialize")
    }
}

fn unscalar(v: &str) -> Option<String> {
    if v.starts_with('"') {
        serde_json::from_str(v).ok()
    } else if RE_SLUG_SAFE.is_match(v) {
        Some(v.to_owned())
    } else {
        None
    }
}

/// One event file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    Status(StatusEvent),
    Lead(LeadEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusEvent {
    pub id: String,
    pub at: String,
    pub by: String,
    pub msg: String,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeadEvent {
    pub id: String,
    pub at: String,
    pub by: String,
    pub task: String,
    pub lead: String,
}

impl Event {
    pub fn id(&self) -> &str {
        match self {
            Event::Status(e) => &e.id,
            Event::Lead(e) => &e.id,
        }
    }

    pub fn by(&self) -> &str {
        match self {
            Event::Status(e) => &e.by,
            Event::Lead(e) => &e.by,
        }
    }

    pub fn path(&self) -> String {
        format!("events/{}.json", self.id())
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Event::Status(e) => {
                check_id("id", &e.id)?;
                check_at(&e.at)?;
                check_name("by", &e.by)?;
                check_id("msg", &e.msg)
            }
            Event::Lead(e) => {
                check_id("id", &e.id)?;
                check_at(&e.at)?;
                check_name("by", &e.by)?;
                check_task(&e.task)?;
                check_name("lead", &e.lead)
            }
        }
    }

    pub fn render(&self) -> String {
        let mut s = serde_json::to_string(self).expect("events serialize");
        s.push('\n');
        s
    }

    pub fn parse(text: &str) -> Result<Event> {
        let err = |why: &str| Error::new(Kind::Internal, format!("bad event file: {why}"));
        let ev: Event = serde_json::from_str(text).map_err(|e| err(&e.to_string()))?;
        ev.validate()?;
        if ev.render() != text {
            return Err(err("not in canonical form"));
        }
        Ok(ev)
    }
}

/// A message or an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Message(Message),
    Event(Event),
}

impl Record {
    pub fn id(&self) -> &str {
        match self {
            Record::Message(m) => &m.id,
            Record::Event(e) => e.id(),
        }
    }

    /// The author (`from` or `by`).
    pub fn author(&self) -> &str {
        match self {
            Record::Message(m) => &m.from,
            Record::Event(e) => e.by(),
        }
    }

    pub fn path(&self) -> String {
        match self {
            Record::Message(m) => m.path(),
            Record::Event(e) => e.path(),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Record::Message(m) => m.render(),
            Record::Event(e) => e.render(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Record::Message(m) => m.validate(),
            Record::Event(e) => e.validate(),
        }
    }

    /// Does this record need the network (D3)? Every event does.
    pub fn rule_bearing(&self) -> bool {
        match self {
            Record::Message(m) => m.kind.rule_bearing(),
            Record::Event(_) => true,
        }
    }

    /// Parse a board file from its path and bytes.
    pub fn parse(path: &str, text: &str) -> Result<Record> {
        let check_name = |id: &str| -> Result<()> {
            check_id("file name", id)?;
            Ok(())
        };
        if let Some(id) = path
            .strip_prefix("messages/")
            .and_then(|p| p.strip_suffix(".md"))
        {
            check_name(id)?;
            let m = Message::parse(text)?;
            if m.id != id {
                crate::fail!(Internal, "{path}: id {} differs from the file name", m.id);
            }
            Ok(Record::Message(m))
        } else if let Some(id) = path
            .strip_prefix("events/")
            .and_then(|p| p.strip_suffix(".json"))
        {
            check_name(id)?;
            let e = Event::parse(text)?;
            if e.id() != id {
                crate::fail!(Internal, "{path}: id {} differs from the file name", e.id());
            }
            Ok(Record::Event(e))
        } else {
            crate::fail!(Internal, "{path}: not a board record path")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> Message {
        Message {
            id: new_id(),
            at: "2026-11-03T10:20:30Z".into(),
            from: "ws".into(),
            to: "laptop".into(),
            task: "fix-login".into(),
            kind: MsgKind::Plan,
            reply_to: String::new(),
            related: vec![
                "[[Projects/baton/overview]]".into(),
                "https://example.com/a?b=c&d=e".into(),
            ],
            branch: "baton:feat/board@0123abc".into(),
            session: "4f0c-a1".into(),
            subject: "Plan: \"quoted\" subject: with colon".into(),
            body: "line one\n\nline two\n---\n# not a heading\n".into(),
        }
    }

    #[test]
    fn message_round_trip() {
        let m = msg();
        let text = m.render();
        assert_eq!(Message::parse(&text).unwrap(), m);
        let rec = Record::parse(&m.path(), &text).unwrap();
        assert_eq!(rec, Record::Message(m.clone()));

        let mut plain = m.clone();
        plain.kind = MsgKind::Plain;
        plain.task.clear();
        plain.branch.clear();
        plain.related.clear();
        plain.session.clear();
        plain.body.clear();
        let text = plain.render();
        assert!(text.contains("kind: \"\"\n") && text.contains("related: []\n"));
        assert_eq!(Message::parse(&text).unwrap(), plain);
    }

    #[test]
    fn parser_refuses_non_canonical_files() {
        let m = msg();
        let text = m.render();
        // Unknown key.
        let t = text.replacen("session:", "extra: x\nsession:", 1);
        assert!(Message::parse(&t).is_err());
        // Needless quotes.
        let t = text.replacen("from: ws", "from: \"ws\"", 1);
        assert!(Message::parse(&t).is_err());
        // Reordered keys.
        let t = text.replacen("from: ws\nto: laptop", "to: laptop\nfrom: ws", 1);
        assert!(Message::parse(&t).is_err());
        // Missing subject line.
        let t = text.replacen("\n# ", "\n", 1);
        assert!(Message::parse(&t).is_err());
        // Unknown kind.
        let t = text.replacen("kind: plan", "kind: order", 1);
        assert!(Message::parse(&t).is_err());
        // Id/file name mismatch.
        assert!(Record::parse(&format!("messages/{}.md", new_id()), &text).is_err());
        assert!(Record::parse("other/x.md", &text).is_err());
    }

    #[test]
    fn every_invalid_field_is_refused() {
        type Mutate = Box<dyn Fn(&mut Message)>;
        let cases: Vec<Mutate> = vec![
            Box::new(|m| m.id = "01abc".into()),
            Box::new(|m| m.id = m.id.to_lowercase()),
            Box::new(|m| m.at = "2026-11-03 10:20:30".into()),
            Box::new(|m| m.at = "2026-11-03T10:20:30.5Z".into()),
            Box::new(|m| m.at = "2026-13-03T10:20:30Z".into()),
            Box::new(|m| m.from = "WS".into()),
            Box::new(|m| m.from = "any".into()),
            Box::new(|m| m.from = "ws;rm".into()),
            Box::new(|m| m.to = "lap top".into()),
            Box::new(|m| m.to = "".into()),
            Box::new(|m| m.task = "-task".into()),
            Box::new(|m| m.task = "Task".into()),
            Box::new(|m| m.task = "a".repeat(49)),
            Box::new(|m| m.task = "a$(x)".into()),
            Box::new(|m| m.reply_to = "nope".into()),
            Box::new(|m| m.related = vec!["note".into()]),
            Box::new(|m| m.related = vec!["https://x/$(id)".into()]),
            Box::new(|m| m.related = vec!["[[a]]; rm -rf".into()]),
            Box::new(|m| m.related = vec!["[[a]]".into(); MAX_RELATED + 1]),
            Box::new(|m| m.branch = "repo:main".into()),
            Box::new(|m| m.branch = "repo:a..b@0123abc".into()),
            Box::new(|m| m.branch = "repo:main@xyz1234".into()),
            Box::new(|m| m.branch = "re po:main@0123abc".into()),
            Box::new(|m| m.session = "a b".into()),
            Box::new(|m| m.session = "$x".into()),
            Box::new(|m| m.subject = "".into()),
            Box::new(|m| m.subject = "two\nlines".into()),
            Box::new(|m| m.subject = "x".repeat(MAX_SUBJECT + 1)),
            Box::new(|m| m.body = "nul\0".into()),
            Box::new(|m| m.body = "x".repeat(MAX_BODY + 1)),
            Box::new(|m| m.task = String::new()), // a plan needs a task
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut m = msg();
            f(&mut m);
            assert!(m.validate().is_err(), "case {i} accepted: {m:?}");
        }
        assert!(msg().validate().is_ok());
    }

    #[test]
    fn event_round_trip_and_refusals() {
        let s = Event::Status(StatusEvent {
            id: new_id(),
            at: now(),
            by: "laptop".into(),
            msg: new_id(),
            status: Status::Working,
        });
        let text = s.render();
        assert!(text.starts_with("{\"id\":"));
        assert_eq!(Event::parse(&text).unwrap(), s);
        let l = Event::Lead(LeadEvent {
            id: new_id(),
            at: now(),
            by: "victor".into(),
            task: "t1".into(),
            lead: "laptop".into(),
        });
        assert_eq!(Event::parse(&l.render()).unwrap(), l);

        let bad_status = text.replace("working", "closed");
        assert!(Event::parse(&bad_status).is_err());
        let extra = text.replacen('{', "{\"x\":1,", 1);
        assert!(Event::parse(&extra).is_err());
        let spaced = text.replacen(':', ": ", 1);
        assert!(Event::parse(&spaced).is_err());
        let bad_by = text.replace("laptop", "Laptop");
        assert!(Event::parse(&bad_by).is_err());
    }

    #[test]
    fn ids_increase() {
        let a = new_id();
        let b = new_id();
        assert!(a < b);
        check_id("id", &a).unwrap();
        assert!(check_at(&now()).is_ok());
    }
}
