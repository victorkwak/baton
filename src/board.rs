//! Board state: the records folded in first-parent commit order, the rules
//! (`docs/design.md`, "Rules"), and the views (inbox, tasks, thread).
//!
//! The same code checks a new commit before it is pushed ([`State::check_commit`])
//! and replays history ([`State::fold`]). A record on `main` that breaks a
//! rule (only possible if something other than `baton` wrote it) is skipped
//! and listed in [`State::rejected`]; every machine skips the same records,
//! because every machine folds the same order.

use std::collections::{BTreeMap, HashMap};

use crate::error::{Error, Kind, Result};
use crate::record::{Event, LeadEvent, Message, MsgKind, Record, Status, StatusEvent, parse_at};

/// Messages and claims older than this are stale in `tasks`.
pub const STALE_SECS: i64 = 4 * 3600;

/// A record and where it sits on `main`.
#[derive(Debug, Clone)]
pub struct Located {
    pub record: Record,
    /// The commit that added it (empty for a record not yet committed).
    pub commit: String,
    /// Position in board order, from 0. Stable: history only grows.
    pub seq: usize,
}

/// One message with its current state.
#[derive(Debug, Clone)]
pub struct MsgState {
    pub msg: Message,
    pub status: Status,
    /// For `to: any`: the machine whose status event claimed it.
    pub claimed_by: Option<String>,
    /// `at` of the last status change (the message's own `at` when none).
    pub status_at: String,
    pub seq: usize,
    pub commit: String,
    /// Ids of messages whose `reply_to` is this one, in board order.
    pub replies: Vec<String>,
}

impl MsgState {
    /// Who may change the status: `to`, or for `to: any` the claimer.
    pub fn addressee(&self) -> Option<&str> {
        if self.msg.to == "any" {
            self.claimed_by.as_deref()
        } else {
            Some(&self.msg.to)
        }
    }

    /// Is this message addressed to `machine` and waiting for it? Open,
    /// not its own, and `to` is the machine or an unclaimed `any`.
    pub fn actionable_for(&self, machine: &str) -> bool {
        self.status == Status::Open
            && self.msg.from != machine
            && (self.msg.to == machine || (self.msg.to == "any" && self.claimed_by.is_none()))
    }
}

/// One task.
#[derive(Debug, Clone)]
pub struct TaskState {
    pub task: String,
    pub plan: String,
    pub lead: String,
    pub closed: bool,
    /// `repo:branch` keys handed over within this task.
    pub branches: Vec<String>,
}

/// A record that `fold` skipped, and why.
#[derive(Debug, Clone)]
pub struct Rejected {
    pub id: String,
    pub commit: String,
    pub reason: String,
}

/// The folded board.
#[derive(Debug, Clone)]
pub struct State {
    human: String,
    msgs: HashMap<String, MsgState>,
    /// Message ids in board order.
    order: Vec<String>,
    tasks: BTreeMap<String, TaskState>,
    /// `repo:branch` → owner.
    branch_owner: BTreeMap<String, String>,
    /// Records seen (also the next `seq`).
    pub len: usize,
    pub rejected: Vec<Rejected>,
}

/// A rule refusal (exit 10). The message starts with the rule's name.
fn rule(name: &str, why: String) -> Error {
    Error::new(Kind::Rule, format!("rule {name}: {why}"))
}

fn unknown(id: &str) -> Error {
    Error::new(Kind::UnknownId, format!("no message {id}"))
}

impl State {
    pub fn new(human: &str) -> State {
        State {
            human: human.to_owned(),
            msgs: HashMap::new(),
            order: Vec::new(),
            tasks: BTreeMap::new(),
            branch_owner: BTreeMap::new(),
            len: 0,
            rejected: Vec::new(),
        }
    }

    /// Replay records in board order (`seq` order; one commit's records are
    /// adjacent and sorted by id).
    pub fn fold(human: &str, records: &[Located]) -> State {
        let mut state = State::new(human);
        let mut i = 0;
        while i < records.len() {
            let mut j = i + 1;
            while j < records.len() && records[j].commit == records[i].commit {
                j += 1;
            }
            let group: Vec<&Record> = records[i..j].iter().map(|l| &l.record).collect();
            for l in &records[i..j] {
                match state.check(&l.record, &group) {
                    Ok(()) => state.apply(&l.record, l.seq, &l.commit),
                    Err(e) => state.rejected.push(Rejected {
                        id: l.record.id().to_owned(),
                        commit: l.commit.clone(),
                        reason: e.message,
                    }),
                }
                state.len = state.len.max(l.seq + 1);
            }
            i = j;
        }
        state
    }

    /// Would this commit (records sorted by id) be accepted on top of the
    /// state? Every record must pass; nothing is changed.
    pub fn check_commit(&self, records: &[Record]) -> Result<()> {
        let mut s = self.clone();
        let group: Vec<&Record> = records.iter().collect();
        for (n, r) in records.iter().enumerate() {
            s.check(r, &group)?;
            let seq = s.len + n;
            s.apply(r, seq, "");
        }
        Ok(())
    }

    /// Append a commit's records (already checked) to the state.
    pub fn push_commit(&mut self, records: &[Record], commit: &str) {
        for r in records {
            let seq = self.len;
            self.apply(r, seq, commit);
            self.len += 1;
        }
    }

    pub fn human(&self) -> &str {
        &self.human
    }

    fn is_human(&self, name: &str) -> bool {
        name == self.human
    }

    pub fn message(&self, id: &str) -> Option<&MsgState> {
        self.msgs.get(id)
    }

    /// Every message in board order.
    pub fn messages(&self) -> impl Iterator<Item = &MsgState> {
        self.order.iter().map(|id| &self.msgs[id])
    }

    /// Messages added at `seq >= after` (a cursor: `State::len` from an earlier read).
    pub fn messages_since(&self, after: usize) -> impl Iterator<Item = &MsgState> {
        self.messages().filter(move |m| m.seq >= after)
    }

    pub fn task(&self, task: &str) -> Option<&TaskState> {
        self.tasks.get(task)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &TaskState> {
        self.tasks.values()
    }

    /// The owner of `repo:branch` (the `to` of its latest handover).
    pub fn branch_owner(&self, key: &str) -> Option<&str> {
        self.branch_owner.get(key).map(String::as_str)
    }

    /// Check one record against the rules. `commit` holds every record of
    /// the commit it is part of (for "wontdo needs a reply in the same commit").
    pub fn check(&self, rec: &Record, commit: &[&Record]) -> Result<()> {
        rec.validate()?;
        match rec {
            Record::Message(m) => self.check_message(m),
            Record::Event(Event::Status(e)) => self.check_status(e, commit),
            Record::Event(Event::Lead(e)) => self.check_lead(e),
        }
    }

    fn check_message(&self, m: &Message) -> Result<()> {
        if self.msgs.contains_key(&m.id) {
            return Err(rule(
                "append-only",
                format!("message {} already exists", m.id),
            ));
        }
        if !m.reply_to.is_empty() && !self.msgs.contains_key(&m.reply_to) {
            return Err(unknown(&m.reply_to));
        }
        match m.kind {
            MsgKind::Plain => Ok(()),
            MsgKind::Plan => self.check_plan(m),
            MsgKind::Handover => {
                let t = self.open_task(&m.task)?;
                self.check_lead_or_human(t, &m.from, "hand over")?;
                if m.to == "any" || m.to == self.human {
                    return Err(rule(
                        "handover",
                        format!("the new lead must be a machine, not {}", m.to),
                    ));
                }
                if m.to == t.lead {
                    return Err(rule(
                        "handover",
                        format!("{} already leads task {}", m.to, t.task),
                    ));
                }
                Ok(())
            }
            MsgKind::CloseTask => {
                let t = self.open_task(&m.task)?;
                self.check_lead_or_human(t, &m.from, "close")
            }
        }
    }

    fn check_plan(&self, m: &Message) -> Result<()> {
        if let Some(t) = self.tasks.get(&m.task) {
            if t.closed {
                return Err(rule(
                    "task-closed",
                    format!("task {} is closed; its slug cannot be reused", t.task),
                ));
            }
            return Err(rule(
                "plan-unique",
                format!(
                    "task {} already has a plan ({}), led by {}",
                    t.task, t.plan, t.lead
                ),
            ));
        }
        if !self.is_human(&m.from)
            && let Some(p) = self.unanswered_plan(&m.from)
        {
            return Err(rule(
                "unanswered-plan",
                format!(
                    "{} has an open plan {} (task {}) addressed to {} with no reply from it; reply first (post --reply-to {})",
                    p.msg.from, p.msg.id, p.msg.task, m.from, p.msg.id
                ),
            ));
        }
        Ok(())
    }

    /// The first open plan from another machine addressed to `machine` (or
    /// an unclaimed `any`, or `any` claimed by it) that it has not replied to.
    pub fn unanswered_plan(&self, machine: &str) -> Option<&MsgState> {
        self.messages().find(|p| {
            p.msg.kind == MsgKind::Plan
                && p.msg.from != machine
                && !p.status.is_closed()
                && self
                    .tasks
                    .get(&p.msg.task)
                    .is_some_and(|t| !t.closed && t.plan == p.msg.id)
                && (p.msg.to == machine
                    || (p.msg.to == "any" && p.claimed_by.as_deref().is_none_or(|c| c == machine)))
                && !p.replies.iter().any(|r| self.msgs[r].msg.from == machine)
        })
    }

    fn open_task(&self, task: &str) -> Result<&TaskState> {
        match self.tasks.get(task) {
            None => Err(rule("task", format!("task {task} has no plan"))),
            Some(t) if t.closed => Err(rule("task-closed", format!("task {task} is closed"))),
            Some(t) => Ok(t),
        }
    }

    fn check_lead_or_human(&self, t: &TaskState, who: &str, what: &str) -> Result<()> {
        if who == t.lead || self.is_human(who) {
            Ok(())
        } else {
            Err(rule(
                "lead",
                format!(
                    "only the lead ({}) or {} can {what} task {}",
                    t.lead, self.human, t.task
                ),
            ))
        }
    }

    fn check_status(&self, e: &StatusEvent, commit: &[&Record]) -> Result<()> {
        let ms = self.msgs.get(&e.msg).ok_or_else(|| unknown(&e.msg))?;
        let human = self.is_human(&e.by);
        let (cur, new) = (ms.status, e.status);
        let allowed = match cur {
            Status::Open => matches!(new, Status::Working | Status::Done | Status::Wontdo),
            Status::Working => matches!(new, Status::Done | Status::Wontdo | Status::Open),
            Status::Done | Status::Wontdo => false,
        };
        if !allowed {
            let hint = if cur.is_closed() {
                "; nothing follows done or wontdo (reply to it instead)"
            } else {
                ""
            };
            return Err(rule(
                "transition",
                format!("{} is {cur}; it cannot become {new}{hint}", e.msg),
            ));
        }
        if !human && new.is_closed() && ms.msg.to == self.human {
            return Err(rule(
                "human-message",
                format!(
                    "an agent cannot close {}: it is addressed to {}",
                    e.msg, self.human
                ),
            ));
        }
        // The human may close or release any message, but claims only its own.
        let human_override = human && new != Status::Working;
        match ms.addressee() {
            Some(a) if a != e.by && !human_override => {
                return Err(if ms.msg.to == "any" {
                    rule(
                        "claim",
                        format!("{} is claimed by {a}; only {a} can change it", e.msg),
                    )
                } else {
                    rule(
                        "addressee",
                        format!("{} is addressed to {a}; only {a} can change it", e.msg),
                    )
                });
            }
            _ => {}
        }
        if new == Status::Wontdo {
            let replied = commit
                .iter()
                .any(|r| matches!(r, Record::Message(m) if m.reply_to == e.msg && m.from == e.by));
            if !replied {
                return Err(rule(
                    "wontdo-reply",
                    format!("wontdo on {} needs a reply in the same commit", e.msg),
                ));
            }
        }
        Ok(())
    }

    fn check_lead(&self, e: &LeadEvent) -> Result<()> {
        if !self.is_human(&e.by) {
            return Err(rule(
                "human-only",
                format!("only {} can change a task's lead directly", self.human),
            ));
        }
        let t = self.open_task(&e.task)?;
        if e.lead == t.lead {
            return Err(rule(
                "handover",
                format!("{} already leads task {}", e.lead, t.task),
            ));
        }
        if e.lead == self.human {
            return Err(rule("handover", "the lead must be a machine".to_owned()));
        }
        Ok(())
    }

    fn apply(&mut self, rec: &Record, seq: usize, commit: &str) {
        match rec {
            Record::Message(m) => {
                if !m.reply_to.is_empty()
                    && let Some(parent) = self.msgs.get_mut(&m.reply_to)
                {
                    parent.replies.push(m.id.clone());
                }
                match m.kind {
                    MsgKind::Plain => {}
                    MsgKind::Plan => {
                        self.tasks.insert(
                            m.task.clone(),
                            TaskState {
                                task: m.task.clone(),
                                plan: m.id.clone(),
                                lead: m.from.clone(),
                                closed: false,
                                branches: Vec::new(),
                            },
                        );
                    }
                    MsgKind::Handover => {
                        if let Some(t) = self.tasks.get_mut(&m.task) {
                            t.lead = m.to.clone();
                            if !m.branch.is_empty() {
                                let key = crate::record::branch_key(&m.branch).to_owned();
                                if !t.branches.contains(&key) {
                                    t.branches.push(key.clone());
                                }
                                self.branch_owner.insert(key, m.to.clone());
                            }
                        }
                    }
                    MsgKind::CloseTask => {
                        if let Some(t) = self.tasks.get_mut(&m.task) {
                            t.closed = true;
                        }
                    }
                }
                self.order.push(m.id.clone());
                self.msgs.insert(
                    m.id.clone(),
                    MsgState {
                        msg: m.clone(),
                        status: Status::Open,
                        claimed_by: None,
                        status_at: m.at.clone(),
                        seq,
                        commit: commit.to_owned(),
                        replies: Vec::new(),
                    },
                );
            }
            Record::Event(Event::Status(e)) => {
                let human = self.is_human(&e.by);
                if let Some(ms) = self.msgs.get_mut(&e.msg) {
                    ms.status = e.status;
                    ms.status_at = e.at.clone();
                    if ms.msg.to == "any" {
                        match e.status {
                            Status::Open => ms.claimed_by = None,
                            _ if ms.claimed_by.is_none() && !human => {
                                ms.claimed_by = Some(e.by.clone())
                            }
                            _ => {}
                        }
                    }
                }
            }
            Record::Event(Event::Lead(e)) => {
                if let Some(t) = self.tasks.get_mut(&e.task) {
                    t.lead = e.lead.clone();
                }
            }
        }
    }

    /// `inbox`: for `machine`, open messages waiting for it and its own
    /// `working` claims; with `all`, every open or working message.
    pub fn inbox(&self, machine: &str, all: bool) -> Vec<&MsgState> {
        self.messages()
            .filter(|m| {
                if all {
                    !m.status.is_closed()
                } else {
                    m.actionable_for(machine)
                        || (m.status == Status::Working && m.addressee() == Some(machine))
                }
            })
            .collect()
    }

    /// The thread around `id`: its root (following `reply_to` up) and every
    /// reply below it, in board order.
    pub fn thread(&self, id: &str) -> Vec<&MsgState> {
        let Some(mut root) = self.msgs.get(id) else {
            return Vec::new();
        };
        while let Some(p) = self.msgs.get(&root.msg.reply_to) {
            root = p;
        }
        let mut ids = vec![root.msg.id.as_str()];
        let mut i = 0;
        while i < ids.len() {
            for r in &self.msgs[ids[i]].replies {
                ids.push(r);
            }
            i += 1;
        }
        let mut out: Vec<&MsgState> = ids.iter().map(|i| &self.msgs[*i]).collect();
        out.sort_by_key(|m| m.seq);
        out
    }

    /// Open tasks with their counts, for `tasks`.
    pub fn task_summaries(&self, now: jiff::Timestamp) -> Vec<TaskSummary<'_>> {
        self.tasks
            .values()
            .filter(|t| !t.closed)
            .map(|t| {
                let mut s = TaskSummary {
                    task: t,
                    plan_at: self.msgs[&t.plan].msg.at.clone(),
                    open: 0,
                    working: 0,
                    for_human: Vec::new(),
                    stale: Vec::new(),
                    branches: t
                        .branches
                        .iter()
                        .map(|k| (k.clone(), self.branch_owner[k].clone()))
                        .collect(),
                };
                for m in self.messages().filter(|m| m.msg.task == t.task) {
                    let age = age_secs(&m.status_at, now);
                    match m.status {
                        Status::Open => {
                            s.open += 1;
                            if m.msg.to == self.human {
                                s.for_human.push(m.msg.id.clone());
                            }
                            // An open message nobody answered for 4 hours.
                            let answered = m.replies.iter().any(|r| {
                                let reply = &self.msgs[r].msg;
                                reply.from == m.msg.to || m.claimed_by.as_ref() == Some(&reply.from)
                            });
                            if age > STALE_SECS && !answered {
                                s.stale.push(m.msg.id.clone());
                            }
                        }
                        Status::Working => {
                            s.working += 1;
                            if age > STALE_SECS {
                                s.stale.push(m.msg.id.clone());
                            }
                        }
                        _ => {}
                    }
                }
                s
            })
            .collect()
    }
}

/// One row of `tasks`.
#[derive(Debug)]
pub struct TaskSummary<'a> {
    pub task: &'a TaskState,
    pub plan_at: String,
    pub open: usize,
    pub working: usize,
    /// Open messages addressed to the human.
    pub for_human: Vec<String>,
    /// Working claims and unanswered open messages older than 4 hours.
    pub stale: Vec<String>,
    /// `repo:branch` → owner.
    pub branches: Vec<(String, String)>,
}

/// Seconds since `at` (0 when unparsable or in the future).
pub fn age_secs(at: &str, now: jiff::Timestamp) -> i64 {
    parse_at(at).map_or(0, |t| (now.as_second() - t.as_second()).max(0))
}

/// `45s`, `12m`, `3h`, `2d`.
pub fn format_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{new_id, now};

    const HUMAN: &str = "victor";

    fn msg(from: &str, to: &str, task: &str, kind: MsgKind) -> Message {
        Message {
            id: new_id(),
            at: now(),
            from: from.into(),
            to: to.into(),
            task: task.into(),
            kind,
            reply_to: String::new(),
            related: Vec::new(),
            branch: String::new(),
            session: String::new(),
            subject: "s".into(),
            body: String::new(),
        }
    }

    fn reply(from: &str, to: &MsgState) -> Message {
        let mut m = msg(from, &to.msg.from, &to.msg.task, MsgKind::Plain);
        m.reply_to = to.msg.id.clone();
        m
    }

    fn status(by: &str, id: &str, s: Status) -> Record {
        Record::Event(Event::Status(StatusEvent {
            id: new_id(),
            at: now(),
            by: by.into(),
            msg: id.into(),
            status: s,
        }))
    }

    /// A board that applies commits as `push_cas` does.
    struct Board {
        state: State,
        n: usize,
    }

    impl Board {
        fn new() -> Board {
            Board {
                state: State::new(HUMAN),
                n: 0,
            }
        }

        fn commit(&mut self, mut recs: Vec<Record>) -> Result<()> {
            recs.sort_by(|a, b| a.id().cmp(b.id()));
            self.state.check_commit(&recs)?;
            self.n += 1;
            self.state.push_commit(&recs, &format!("c{}", self.n));
            Ok(())
        }

        fn post(&mut self, m: &Message) -> Result<()> {
            self.commit(vec![Record::Message(m.clone())])
        }

        fn ms(&self, m: &Message) -> MsgState {
            self.state.message(&m.id).unwrap().clone()
        }

        fn rule_of(r: Result<()>) -> String {
            let e = r.unwrap_err();
            assert_eq!(e.kind, Kind::Rule, "{e}");
            e.message
                .strip_prefix("rule ")
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .to_owned()
        }
    }

    #[test]
    fn transitions_allowed_and_refused() {
        let mut b = Board::new();
        let m = msg("ws", "laptop", "", MsgKind::Plain);
        b.post(&m).unwrap();
        b.commit(vec![status("laptop", &m.id, Status::Working)])
            .unwrap();
        assert_eq!(
            Board::rule_of(b.commit(vec![status("laptop", &m.id, Status::Working)])),
            "transition"
        );
        b.commit(vec![status("laptop", &m.id, Status::Open)])
            .unwrap();
        b.commit(vec![status("laptop", &m.id, Status::Done)])
            .unwrap();
        for s in [Status::Open, Status::Working, Status::Done] {
            assert_eq!(
                Board::rule_of(b.commit(vec![status("laptop", &m.id, s)])),
                "transition"
            );
        }
        // Even the human cannot reopen.
        assert_eq!(
            Board::rule_of(b.commit(vec![status(HUMAN, &m.id, Status::Open)])),
            "transition"
        );
        assert_eq!(b.ms(&m).status, Status::Done);
    }

    #[test]
    fn only_the_addressee_changes_status_but_the_human_may_close_or_release() {
        let mut b = Board::new();
        let m = msg("ws", "laptop", "", MsgKind::Plain);
        b.post(&m).unwrap();
        assert_eq!(
            Board::rule_of(b.commit(vec![status("ws", &m.id, Status::Working)])),
            "addressee"
        );
        assert_eq!(
            Board::rule_of(b.commit(vec![status(HUMAN, &m.id, Status::Working)])),
            "addressee"
        );
        b.commit(vec![status("laptop", &m.id, Status::Working)])
            .unwrap();
        b.commit(vec![status(HUMAN, &m.id, Status::Open)]).unwrap();
        b.commit(vec![status(HUMAN, &m.id, Status::Done)]).unwrap();
    }

    #[test]
    fn unknown_message_is_unknown_id() {
        let b = Board::new();
        let e = b
            .state
            .check_commit(&[status("ws", &new_id(), Status::Working)])
            .unwrap_err();
        assert_eq!(e.kind, Kind::UnknownId);
        let mut m = msg("ws", "laptop", "", MsgKind::Plain);
        m.reply_to = new_id();
        let e = b.state.check_commit(&[Record::Message(m)]).unwrap_err();
        assert_eq!(e.kind, Kind::UnknownId);
    }

    #[test]
    fn any_is_claimed_by_the_first_status_event() {
        let mut b = Board::new();
        let m = msg("ws", "any", "", MsgKind::Plain);
        b.post(&m).unwrap();
        assert!(b.ms(&m).actionable_for("laptop"));
        assert!(!b.ms(&m).actionable_for("ws"), "not your own message");
        b.commit(vec![status("laptop", &m.id, Status::Working)])
            .unwrap();
        assert_eq!(b.ms(&m).addressee(), Some("laptop"));
        assert_eq!(
            Board::rule_of(b.commit(vec![status("mac", &m.id, Status::Done)])),
            "claim"
        );
        // A release makes it claimable again.
        b.commit(vec![status("laptop", &m.id, Status::Open)])
            .unwrap();
        b.commit(vec![status("mac", &m.id, Status::Working)])
            .unwrap();
        assert_eq!(b.ms(&m).addressee(), Some("mac"));
    }

    #[test]
    fn wontdo_needs_a_reply_in_the_same_commit() {
        let mut b = Board::new();
        let m = msg("ws", "laptop", "", MsgKind::Plain);
        b.post(&m).unwrap();
        assert_eq!(
            Board::rule_of(b.commit(vec![status("laptop", &m.id, Status::Wontdo)])),
            "wontdo-reply"
        );
        // A reply in an earlier commit does not count.
        let r = reply("laptop", &b.ms(&m));
        b.post(&r).unwrap();
        assert_eq!(
            Board::rule_of(b.commit(vec![status("laptop", &m.id, Status::Wontdo)])),
            "wontdo-reply"
        );
        // A reply from someone else does not count either.
        let other = reply("ws", &b.ms(&m));
        assert_eq!(
            Board::rule_of(b.commit(vec![
                Record::Message(other),
                status("laptop", &m.id, Status::Wontdo)
            ])),
            "wontdo-reply"
        );
        let r2 = reply("laptop", &b.ms(&m));
        b.commit(vec![
            Record::Message(r2),
            status("laptop", &m.id, Status::Wontdo),
        ])
        .unwrap();
        assert_eq!(b.ms(&m).status, Status::Wontdo);
    }

    #[test]
    fn an_agent_cannot_close_a_message_to_the_human() {
        let mut b = Board::new();
        let m = msg("ws", HUMAN, "", MsgKind::Plain);
        b.post(&m).unwrap();
        assert_eq!(
            Board::rule_of(b.commit(vec![status("ws", &m.id, Status::Done)])),
            "human-message"
        );
        b.commit(vec![status(HUMAN, &m.id, Status::Done)]).unwrap();
    }

    #[test]
    fn one_plan_per_task_and_closed_slugs_stay_closed() {
        let mut b = Board::new();
        let p = msg("ws", "laptop", "t1", MsgKind::Plan);
        b.post(&p).unwrap();
        assert_eq!(b.state.task("t1").unwrap().lead, "ws");
        let p2 = msg("ws", "laptop", "t1", MsgKind::Plan);
        assert_eq!(Board::rule_of(b.post(&p2)), "plan-unique");
        // Only the lead (or the human) closes.
        let c = msg("laptop", "ws", "t1", MsgKind::CloseTask);
        assert_eq!(Board::rule_of(b.post(&c)), "lead");
        let c = msg("ws", "laptop", "t1", MsgKind::CloseTask);
        b.post(&c).unwrap();
        let p3 = msg("laptop", "ws", "t1", MsgKind::Plan);
        assert_eq!(Board::rule_of(b.post(&p3)), "task-closed");
        // The human can close a task too.
        let p4 = msg("laptop", "ws", "t2", MsgKind::Plan);
        b.post(&p4).unwrap();
        let c = msg(HUMAN, "laptop", "t2", MsgKind::CloseTask);
        b.post(&c).unwrap();
        assert!(b.state.task("t2").unwrap().closed);
        // Handover and close-task need an open task.
        let h = msg("ws", "laptop", "nope", MsgKind::Handover);
        assert_eq!(Board::rule_of(b.post(&h)), "task");
    }

    #[test]
    fn unanswered_plan_blocks_a_plan_for_another_task() {
        let mut b = Board::new();
        let p = msg("ws", "laptop", "a", MsgKind::Plan);
        b.post(&p).unwrap();
        let q = msg("laptop", "ws", "b", MsgKind::Plan);
        assert_eq!(Board::rule_of(b.post(&q)), "unanswered-plan");
        // The plan's own sender and other machines are not blocked.
        b.post(&msg("ws", "laptop", "c", MsgKind::Plan)).unwrap();
        b.post(&msg("mac", "ws", "d", MsgKind::Plan)).unwrap();
        // Taking it is not answering it.
        b.commit(vec![status("laptop", &p.id, Status::Working)])
            .unwrap();
        // ws's plan "c" is also addressed to laptop: answer both.
        assert_eq!(Board::rule_of(b.post(&q)), "unanswered-plan");
        b.post(&reply("laptop", &b.ms(&p))).unwrap();
        assert_eq!(Board::rule_of(b.post(&q)), "unanswered-plan");
        let c = b.state.task("c").unwrap().plan.clone();
        b.post(&reply("laptop", &b.state.message(&c).unwrap().clone()))
            .unwrap();
        b.post(&q).unwrap();
        // ws now has unanswered plans from mac (d) and laptop (b); the human is never blocked.
        assert_eq!(
            Board::rule_of(b.post(&msg("ws", "mac", "e", MsgKind::Plan))),
            "unanswered-plan"
        );
        b.post(&msg(HUMAN, "ws", "f", MsgKind::Plan)).unwrap();
    }

    #[test]
    fn unanswered_plan_counts_any_and_ends_when_closed() {
        let mut b = Board::new();
        let p = msg("ws", "any", "a", MsgKind::Plan);
        b.post(&p).unwrap();
        assert_eq!(
            Board::rule_of(b.post(&msg("laptop", "ws", "b", MsgKind::Plan))),
            "unanswered-plan"
        );
        // Once mac claims it, it is mac's, not laptop's.
        b.commit(vec![status("mac", &p.id, Status::Working)])
            .unwrap();
        b.post(&msg("laptop", "ws", "b", MsgKind::Plan)).unwrap();
        assert_eq!(
            Board::rule_of(b.post(&msg("mac", "ws", "c", MsgKind::Plan))),
            "unanswered-plan"
        );
        // A closed task's plan no longer blocks.
        b.post(&msg("ws", "mac", "a", MsgKind::CloseTask)).unwrap();
        b.post(&msg("mac", "ws", "c", MsgKind::Plan)).unwrap();
    }

    #[test]
    fn lead_follows_handovers_and_lead_events() {
        let mut b = Board::new();
        b.post(&msg("ws", "laptop", "t", MsgKind::Plan)).unwrap();
        let mut h = msg("laptop", "ws", "t", MsgKind::Handover);
        assert_eq!(Board::rule_of(b.post(&h)), "lead");
        h = msg("ws", "laptop", "t", MsgKind::Handover);
        h.branch = "baton:feat/x@0123abc".into();
        b.post(&h).unwrap();
        assert_eq!(b.state.task("t").unwrap().lead, "laptop");
        assert_eq!(b.state.branch_owner("baton:feat/x"), Some("laptop"));
        assert_eq!(
            Board::rule_of(b.post(&msg("laptop", "any", "t", MsgKind::Handover))),
            "handover"
        );
        assert_eq!(
            Board::rule_of(b.post(&msg("laptop", "laptop", "t", MsgKind::Handover))),
            "handover"
        );
        let lead = |by: &str, to: &str| {
            Record::Event(Event::Lead(LeadEvent {
                id: new_id(),
                at: now(),
                by: by.into(),
                task: "t".into(),
                lead: to.into(),
            }))
        };
        assert_eq!(
            Board::rule_of(b.commit(vec![lead("laptop", "ws")])),
            "human-only"
        );
        b.commit(vec![lead(HUMAN, "ws")]).unwrap();
        assert_eq!(b.state.task("t").unwrap().lead, "ws");
        // The human may hand over directly too.
        b.post(&msg(HUMAN, "mac", "t", MsgKind::Handover)).unwrap();
        assert_eq!(b.state.task("t").unwrap().lead, "mac");
    }

    #[test]
    fn fold_skips_invalid_records_and_matches_checked_commits() {
        let p = msg("ws", "laptop", "t", MsgKind::Plan);
        let p2 = msg("laptop", "ws", "t", MsgKind::Plan);
        let take = status("ws", &p.id, Status::Working);
        let ok = status("laptop", &p.id, Status::Working);
        let recs = [
            Record::Message(p.clone()),
            Record::Message(p2.clone()),
            take,
            ok,
        ];
        let located: Vec<Located> = recs
            .iter()
            .enumerate()
            .map(|(i, r)| Located {
                record: r.clone(),
                commit: format!("c{i}"),
                seq: i,
            })
            .collect();
        let s = State::fold(HUMAN, &located);
        assert_eq!(s.len, 4);
        assert_eq!(s.rejected.len(), 2);
        assert_eq!(s.task("t").unwrap().plan, p.id);
        assert!(s.message(&p2.id).is_none());
        assert_eq!(s.message(&p.id).unwrap().status, Status::Working);
    }

    #[test]
    fn inbox_thread_and_tasks_views() {
        let mut b = Board::new();
        let p = msg("ws", "laptop", "t", MsgKind::Plan);
        b.post(&p).unwrap();
        let r = reply("laptop", &b.ms(&p));
        b.post(&r).unwrap();
        let rr = reply("ws", &b.ms(&r));
        b.post(&rr).unwrap();
        let h = msg("laptop", HUMAN, "t", MsgKind::Plain);
        b.post(&h).unwrap();
        let ids = |v: Vec<&MsgState>| v.iter().map(|m| m.msg.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(b.state.inbox("laptop", false)),
            [p.id.clone(), rr.id.clone()]
        );
        assert_eq!(ids(b.state.inbox("ws", false)), std::slice::from_ref(&r.id));
        assert_eq!(b.state.inbox("ws", true).len(), 4);
        assert_eq!(
            ids(b.state.thread(&rr.id)),
            [p.id.clone(), r.id.clone(), rr.id.clone()]
        );
        b.commit(vec![status("laptop", &p.id, Status::Working)])
            .unwrap();
        assert_eq!(
            ids(b.state.inbox("laptop", false)),
            [p.id.clone(), rr.id.clone()]
        );

        let later = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(5);
        let sums = b.state.task_summaries(later);
        assert_eq!(sums.len(), 1);
        let s = &sums[0];
        assert_eq!((s.open, s.working), (3, 1));
        assert_eq!(s.for_human, std::slice::from_ref(&h.id));
        // The working claim and the unanswered messages are stale; r was answered by rr.
        assert_eq!(s.stale.len(), 3, "{:?}", s.stale);
        assert!(!s.stale.contains(&r.id));
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(3 * 3600 + 5), "3h");
    }
}
