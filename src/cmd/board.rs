//! Board commands: inbox, show, post, take, release, close, wait, tasks, human.
//! Owner: agent A (`docs/plan.md`). Behaviour: `docs/design.md`, "Commands".
//!
//! Only `show` prints subject or body text, and only inside the nonce fence.
//! Every other output carries ids and metadata.

use std::io::{IsTerminal as _, Read as _};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::Format;
use crate::board::{MsgState, State, age_secs, format_age};
use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::identity::Who;
use crate::out;
use crate::output::{envelope, field, sanitize};
use crate::record::{
    Event, LeadEvent, Message, MsgKind, Record, Status, StatusEvent, check_id, new_id, now,
    random_u64,
};
use crate::secrets;
use crate::store::Store;

#[derive(Debug, Args)]
pub struct InboxArgs {
    /// Every open message, not only this machine's.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub id: String,
    /// Follow reply_to both ways.
    #[arg(long)]
    pub thread: bool,
}

#[derive(Debug, Args)]
pub struct PostArgs {
    /// A machine, the human, or `any`.
    #[arg(long)]
    pub to: String,
    #[arg(long)]
    pub task: Option<String>,
    /// The first message of a task.
    #[arg(long, conflicts_with_all = ["handover", "close_task"])]
    pub plan: bool,
    /// Hand the task's lead to `--to`.
    #[arg(long, conflicts_with = "close_task")]
    pub handover: bool,
    /// End the task (lead only).
    #[arg(long)]
    pub close_task: bool,
    #[arg(long)]
    pub reply_to: Option<String>,
    /// A short reference: `[[note]]` or a URL. Repeatable.
    #[arg(long)]
    pub related: Vec<String>,
    /// `repo:branch@sha` for a branch handoff.
    #[arg(long)]
    pub branch: Option<String>,
    /// Message body.
    #[arg(short = 'm', long = "message", conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Read the body from a file (`-` for stdin).
    #[arg(long)]
    pub body_file: Option<std::path::PathBuf>,
    pub subject: String,
}

#[derive(Debug, Args)]
pub struct IdArgs {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct CloseArgs {
    pub id: String,
    #[arg(long)]
    pub wontdo: bool,
    /// Reply posted in the same commit (required with --wontdo).
    #[arg(short = 'm', long = "message")]
    pub reply: Option<String>,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub reply_to: Option<String>,
    /// e.g. `9m`, `90s`.
    #[arg(long, default_value = "9m")]
    pub timeout: String,
}

#[derive(Debug, Subcommand)]
pub enum HumanCommand {
    /// Make another machine lead a task.
    Handover { task: String, to: String },
    /// Close a task.
    CloseTask { task: String },
    /// Close any message.
    Close(CloseArgs),
    /// Release any claim.
    Release(IdArgs),
    /// Post, optionally overriding the secret scan.
    Post {
        #[command(flatten)]
        post: PostArgs,
        #[arg(long)]
        allow_secret: bool,
    },
}

/// What every board command needs.
struct Ctx {
    config: Config,
    who: Who,
    store: Store,
}

fn ctx() -> Result<Ctx> {
    let config = Config::load()?;
    let who = Who::detect(&config, None);
    let store = Store::open(&config, who.session())?;
    Ok(Ctx { config, who, store })
}

/// Ids are ULIDs; accept them in any case.
fn norm_id(id: &str) -> Result<String> {
    let id = id.trim().to_ascii_uppercase();
    check_id("id", &id).map_err(|_| Error::new(Kind::UnknownId, format!("no message {id}")))?;
    Ok(id)
}

fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "-".to_owned()
    } else {
        field(s)
    }
}

/// The `inbox` header (also used by `wait`).
pub const INBOX_HEADER: &str = "# id\tfrom\tto\ttask\tkind\tstatus\tage";

/// One `inbox` line: ids and metadata only, never subject or body.
pub fn inbox_line(m: &MsgState, now: jiff::Timestamp) -> String {
    [
        m.msg.id.clone(),
        or_dash(&m.msg.from),
        or_dash(&m.msg.to),
        or_dash(&m.msg.task),
        or_dash(m.msg.kind.as_str()),
        m.status.to_string(),
        format_age(age_secs(&m.msg.at, now)),
    ]
    .join("\t")
}

/// One message's metadata as JSON (no subject or body).
pub fn inbox_json(m: &MsgState, now: jiff::Timestamp) -> Value {
    json!({
        "id": m.msg.id,
        "from": m.msg.from,
        "to": m.msg.to,
        "task": m.msg.task,
        "kind": m.msg.kind.as_str(),
        "status": m.status.as_str(),
        "addressee": m.addressee(),
        "reply_to": m.msg.reply_to,
        "at": m.msg.at,
        "age_secs": age_secs(&m.msg.at, now),
    })
}

fn print_messages(format: Format, command: &str, msgs: &[&MsgState]) {
    let now = jiff::Timestamp::now();
    match format {
        Format::Json => {
            let items: Vec<Value> = msgs.iter().map(|m| inbox_json(m, now)).collect();
            out!("{}", envelope(command, Value::Array(items)));
        }
        Format::Text => {
            if msgs.is_empty() {
                return;
            }
            out!("{INBOX_HEADER}");
            for m in msgs {
                out!("{}", inbox_line(m, now));
            }
        }
    }
}

fn warn_rejected(state: &State) {
    if !state.rejected.is_empty() {
        eprintln!(
            "baton: {} board record(s) break a rule or do not parse and are ignored (first: {})",
            state.rejected.len(),
            field(&state.rejected[0].id)
        );
    }
}

pub fn inbox(format: Format, args: InboxArgs) -> Result<()> {
    let c = ctx()?;
    let _lock = c.store.lock()?;
    c.store.fetch_or_warn()?;
    let snap = c.store.read()?;
    warn_rejected(&snap.state);
    let msgs = snap.state.inbox(c.who.machine(), args.all);
    print_messages(format, "inbox", &msgs);
    Ok(())
}

pub fn show(format: Format, args: ShowArgs) -> Result<()> {
    let id = norm_id(&args.id)?;
    let c = ctx()?;
    let _lock = c.store.lock()?;
    let mut snap = c.store.read()?;
    if snap.state.message(&id).is_none() && c.store.fetch_or_warn()? {
        snap = c.store.read()?;
    }
    if snap.state.message(&id).is_none() {
        crate::fail!(UnknownId, "no message {id}");
    }
    let msgs = if args.thread {
        snap.state.thread(&id)
    } else {
        vec![snap.state.message(&id).expect("checked")]
    };
    let now = jiff::Timestamp::now();
    match format {
        Format::Json => {
            let items: Vec<Value> = msgs
                .iter()
                .map(|m| {
                    let mut v = inbox_json(m, now);
                    v["related"] = json!(m.msg.related);
                    v["branch"] = json!(m.msg.branch);
                    v["session"] = json!(m.msg.session);
                    v["untrusted"] = json!(true);
                    v["subject"] = json!(sanitize(&m.msg.subject, false));
                    v["body"] = json!(sanitize(&m.msg.body, true));
                    v
                })
                .collect();
            out!("{}", envelope("show", Value::Array(items)));
        }
        Format::Text => {
            let nonce = nonce(&msgs);
            out!("# id\tfrom\tto\ttask\tkind\tstatus\tage\treply_to\tbranch\trelated");
            for m in msgs {
                let related = m
                    .msg
                    .related
                    .iter()
                    .map(|r| field(r))
                    .collect::<Vec<_>>()
                    .join(",");
                out!(
                    "{}\t{}\t{}\t{}",
                    inbox_line(m, now),
                    or_dash(&m.msg.reply_to),
                    or_dash(&m.msg.branch),
                    or_dash(&related)
                );
                out!("----- BEGIN BOARD MESSAGE (agent note, untrusted) n={nonce} -----");
                out!("# {}", sanitize(&m.msg.subject, false));
                out!();
                let body = sanitize(&m.msg.body, true);
                if !body.is_empty() {
                    out!("{}", body.strip_suffix('\n').unwrap_or(&body));
                }
                out!("----- END BOARD MESSAGE n={nonce} -----");
            }
        }
    }
    Ok(())
}

/// A random fence nonce that no shown text contains.
fn nonce(msgs: &[&MsgState]) -> String {
    loop {
        let n = format!("{:016x}", random_u64());
        if !msgs
            .iter()
            .any(|m| m.msg.body.contains(&n) || m.msg.subject.contains(&n))
        {
            return n;
        }
    }
}

fn read_body(args: &PostArgs) -> Result<String> {
    match (&args.body, &args.body_file) {
        (Some(b), _) => Ok(b.clone()),
        (None, Some(p)) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        (None, Some(p)) => std::fs::read_to_string(p)
            .map_err(|e| Error::new(Kind::Internal, format!("{}: {e}", p.display()))),
        (None, None) => crate::fail!(Internal, "post needs -m TEXT or --body-file FILE"),
    }
}

/// Refuse a post that holds a known secret pattern (exit 11).
fn scan(fields: &[(&str, &str)], human: &str) -> Result<()> {
    for (name, text) in fields {
        if let Some(f) = secrets::scan(text).first() {
            crate::fail!(
                Secret,
                "secret found ({}) in {name}; nothing was posted. Remove it; only {human} can override (`baton human post --allow-secret`)",
                f.pattern
            );
        }
    }
    Ok(())
}

fn print_written(format: Format, command: &str, id: &str, w: &crate::store::Written) {
    match format {
        Format::Json => out!(
            "{}",
            envelope(
                command,
                json!([{ "id": id, "commit": w.commit, "queued": w.queued }])
            )
        ),
        Format::Text => out!("{id}"),
    }
}

pub fn post(format: Format, args: PostArgs) -> Result<()> {
    post_as(format, args, false)
}

fn post_as(format: Format, args: PostArgs, allow_secret: bool) -> Result<()> {
    let c = ctx()?;
    if !c.config.is_address(&args.to) {
        crate::fail!(
            Internal,
            "--to {:?} is not a machine, {}, or any (machines: {})",
            args.to,
            c.config.human,
            c.config.machines.join(", ")
        );
    }
    let kind = if args.plan {
        MsgKind::Plan
    } else if args.handover {
        MsgKind::Handover
    } else if args.close_task {
        MsgKind::CloseTask
    } else {
        MsgKind::Plain
    };
    let body = read_body(&args)?;
    let reply_to = args.reply_to.as_deref().map(norm_id).transpose()?;
    let msg = Message {
        id: new_id(),
        at: now(),
        from: c.who.from().to_owned(),
        to: args.to.clone(),
        task: args.task.clone().unwrap_or_default(),
        kind,
        reply_to: reply_to.unwrap_or_default(),
        related: args.related.clone(),
        branch: args.branch.clone().unwrap_or_default(),
        session: c.who.session().to_owned(),
        subject: args.subject.clone(),
        body,
    };
    msg.validate()?;
    if !allow_secret {
        let related = msg.related.join(" ");
        scan(
            &[
                ("subject", &msg.subject),
                ("body", &msg.body),
                ("related", &related),
                ("branch", &msg.branch),
            ],
            &c.config.human,
        )?;
    }
    let _lock = c.store.lock()?;
    let rec = Record::Message(msg.clone());
    let summary = match kind {
        MsgKind::Plain => format!("post {}", msg.id),
        k => format!("{} {} (task {})", k.as_str(), msg.id, msg.task),
    };
    let w = c.store.push_cas(&summary, |_| Ok(vec![rec.clone()]))?;
    print_written(format, "post", &msg.id, &w);
    Ok(())
}

fn status_event(who: &Who, id: &str, status: Status) -> Record {
    Record::Event(Event::Status(StatusEvent {
        id: new_id(),
        at: now(),
        by: who.from().to_owned(),
        msg: id.to_owned(),
        status,
    }))
}

fn set_status(format: Format, c: &Ctx, command: &str, id: &str, status: Status) -> Result<()> {
    let id = norm_id(id)?;
    let _lock = c.store.lock()?;
    let ev = status_event(&c.who, &id, status);
    let w = c
        .store
        .push_cas(&format!("{command} {id}"), |_| Ok(vec![ev.clone()]))?;
    print_written(format, command, &id, &w);
    Ok(())
}

pub fn take(format: Format, args: IdArgs) -> Result<()> {
    let c = ctx()?;
    set_status(format, &c, "take", &args.id, Status::Working)
}

pub fn release(format: Format, args: IdArgs) -> Result<()> {
    let c = ctx()?;
    set_status(format, &c, "release", &args.id, Status::Open)
}

pub fn close(format: Format, args: CloseArgs) -> Result<()> {
    let c = ctx()?;
    close_with(format, &c, args)
}

fn close_with(format: Format, c: &Ctx, args: CloseArgs) -> Result<()> {
    let id = norm_id(&args.id)?;
    let status = if args.wontdo {
        Status::Wontdo
    } else {
        Status::Done
    };
    if args.wontdo && args.reply.as_deref().is_none_or(|r| r.trim().is_empty()) {
        crate::fail!(
            Rule,
            "rule wontdo-reply: --wontdo needs a reply (-m REPLY), posted in the same commit"
        );
    }
    if let Some(r) = &args.reply {
        scan(&[("reply", r)], &c.config.human)?;
    }
    let _lock = c.store.lock()?;
    let ev = status_event(&c.who, &id, status);
    let reply_id = new_id();
    let at = now();
    let w = c
        .store
        .push_cas(&format!("close {id} ({status})"), |state| {
            let mut recs = vec![ev.clone()];
            if let Some(text) = &args.reply {
                let orig = state
                    .message(&id)
                    .ok_or_else(|| Error::new(Kind::UnknownId, format!("no message {id}")))?;
                let reply = Message {
                    id: reply_id.clone(),
                    at: at.clone(),
                    from: c.who.from().to_owned(),
                    to: orig.msg.from.clone(),
                    task: orig.msg.task.clone(),
                    kind: MsgKind::Plain,
                    reply_to: id.clone(),
                    related: Vec::new(),
                    branch: String::new(),
                    session: c.who.session().to_owned(),
                    subject: format!("{status}: {id}"),
                    body: text.clone(),
                };
                reply.validate()?;
                recs.push(Record::Message(reply));
            }
            Ok(recs)
        })?;
    print_written(format, "close", &id, &w);
    Ok(())
}

/// `9m`, `90s`, `1h`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| {
        Error::new(
            Kind::Internal,
            format!("bad duration {s:?}: use e.g. 9m, 90s, 1h"),
        )
    })?;
    Ok(Duration::from_secs(n * mult))
}

/// Poll interval for `wait`: `BATON_POLL_SECS` (tests), default 30.
pub fn poll_interval() -> Duration {
    let secs = std::env::var("BATON_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30)
        .max(1);
    Duration::from_secs(secs)
}

pub fn wait(format: Format, args: WaitArgs) -> Result<()> {
    let timeout = parse_duration(&args.timeout)?;
    let reply_to = args.reply_to.as_deref().map(norm_id).transpose()?;
    let c = ctx()?;
    let machine = c.who.machine().to_owned();
    let deadline = Instant::now() + timeout;
    let filtered = args.task.is_some() || reply_to.is_some();
    let matches = |m: &MsgState| {
        m.actionable_for(&machine)
            && args.task.as_ref().is_none_or(|t| &m.msg.task == t)
            && reply_to.as_ref().is_none_or(|r| &m.msg.reply_to == r)
    };

    let (mut cursor, mut tip) = {
        let _lock = c.store.lock()?;
        c.store.fetch_or_warn()?;
        let snap = c.store.read()?;
        // With a filter, a matching message that is already waiting counts:
        // a reply may land between the post and the wait.
        if filtered {
            let found: Vec<&MsgState> = snap.state.messages().filter(|m| matches(m)).collect();
            if !found.is_empty() {
                print_messages(format, "wait", &found);
                return Ok(());
            }
        }
        (snap.remote_len, snap.origin)
    };
    let poll = poll_interval();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            crate::fail!(
                Timeout,
                "wait: no actionable message within {}",
                args.timeout
            );
        }
        std::thread::sleep(poll.min(left));
        let remote = match c.store.remote_tip() {
            Ok(t) => t,
            Err(e) if e.kind == Kind::Offline => continue,
            Err(e) => return Err(e),
        };
        if remote.is_none() || remote == tip {
            continue;
        }
        let _lock = c.store.lock()?;
        match c.store.fetch() {
            Ok(()) => {}
            Err(e) if e.kind == Kind::Offline => continue,
            Err(e) => return Err(e),
        }
        let snap = c.store.read()?;
        let found: Vec<&MsgState> = snap
            .state
            .messages_since(cursor)
            .filter(|m| matches(m))
            .collect();
        if !found.is_empty() {
            print_messages(format, "wait", &found);
            return Ok(());
        }
        cursor = snap.remote_len;
        tip = snap.origin;
    }
}

pub fn tasks(format: Format) -> Result<()> {
    let c = ctx()?;
    let _lock = c.store.lock()?;
    c.store.fetch_or_warn()?;
    let snap = c.store.read()?;
    warn_rejected(&snap.state);
    let now = jiff::Timestamp::now();
    let rows = snap.state.task_summaries(now);
    let branches = |b: &[(String, String)]| {
        b.iter()
            .map(|(k, o)| format!("{k}={o}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    match format {
        Format::Json => {
            let items: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "task": r.task.task,
                        "lead": r.task.lead,
                        "plan": r.task.plan,
                        "age_secs": age_secs(&r.plan_at, now),
                        "open": r.open,
                        "working": r.working,
                        "for_human": r.for_human,
                        "stale": r.stale,
                        "branches": r.branches.iter().map(|(k, o)| json!({"branch": k, "owner": o})).collect::<Vec<_>>(),
                    })
                })
                .collect();
            out!("{}", envelope("tasks", Value::Array(items)));
        }
        Format::Text => {
            if rows.is_empty() {
                return Ok(());
            }
            out!("# task\tlead\tplan\tage\topen\tworking\tfor_human\tstale\tbranches");
            for r in &rows {
                out!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    r.task.task,
                    r.task.lead,
                    r.task.plan,
                    format_age(age_secs(&r.plan_at, now)),
                    r.open,
                    r.working,
                    or_dash(&r.for_human.join(",")),
                    or_dash(&r.stale.join(",")),
                    or_dash(&branches(&r.branches)),
                );
            }
        }
    }
    Ok(())
}

pub fn human(format: Format, cmd: HumanCommand) -> Result<()> {
    if std::env::var_os("CLAUDECODE").is_some() || !std::io::stdin().is_terminal() {
        crate::fail!(
            Rule,
            "rule human-only: `baton human` is for the human at a terminal; refused inside Claude Code or without a TTY"
        );
    }
    let c = ctx()?;
    if !c.who.is_human() {
        crate::fail!(Rule, "rule human-only: not running as the human");
    }
    match cmd {
        HumanCommand::Post { post, allow_secret } => post_as(format, post, allow_secret),
        HumanCommand::Close(args) => close_with(format, &c, args),
        HumanCommand::Release(args) => set_status(format, &c, "release", &args.id, Status::Open),
        HumanCommand::Handover { task, to } => {
            if !c.config.machines.contains(&to) {
                crate::fail!(Internal, "{to:?} is not a machine in the config");
            }
            let _lock = c.store.lock()?;
            let ev = Record::Event(Event::Lead(LeadEvent {
                id: new_id(),
                at: now(),
                by: c.who.from().to_owned(),
                task: task.clone(),
                lead: to,
            }));
            let id = ev.id().to_owned();
            let w = c
                .store
                .push_cas(&format!("lead {task}"), |_| Ok(vec![ev.clone()]))?;
            print_written(format, "human handover", &id, &w);
            Ok(())
        }
        HumanCommand::CloseTask { task } => {
            let _lock = c.store.lock()?;
            let id = new_id();
            let at = now();
            let w = c.store.push_cas(&format!("close-task {task}"), |state| {
                let lead = state
                    .task(&task)
                    .map(|t| t.lead.clone())
                    .unwrap_or_else(|| c.config.machine.clone());
                Ok(vec![Record::Message(Message {
                    id: id.clone(),
                    at: at.clone(),
                    from: c.who.from().to_owned(),
                    to: lead,
                    task: task.clone(),
                    kind: MsgKind::CloseTask,
                    reply_to: String::new(),
                    related: Vec::new(),
                    branch: String::new(),
                    session: String::new(),
                    subject: format!("Task {task} closed by {}", c.config.human),
                    body: String::new(),
                })])
            })?;
            print_written(format, "human close-task", &id, &w);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("9m").unwrap(), Duration::from_secs(540));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("1d").is_err());
    }
}
