//! Board commands: inbox, show, post, take, release, close, wait, tasks, human.
//! Owner: agent A (`docs/plan.md`).

use clap::{Args, Subcommand};

use crate::Format;
use crate::error::Result;

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

pub fn inbox(_format: Format, _args: InboxArgs) -> Result<()> {
    crate::fail!(Internal, "inbox: not implemented yet")
}
pub fn show(_format: Format, _args: ShowArgs) -> Result<()> {
    crate::fail!(Internal, "show: not implemented yet")
}
pub fn post(_format: Format, _args: PostArgs) -> Result<()> {
    crate::fail!(Internal, "post: not implemented yet")
}
pub fn take(_format: Format, _args: IdArgs) -> Result<()> {
    crate::fail!(Internal, "take: not implemented yet")
}
pub fn release(_format: Format, _args: IdArgs) -> Result<()> {
    crate::fail!(Internal, "release: not implemented yet")
}
pub fn close(_format: Format, _args: CloseArgs) -> Result<()> {
    crate::fail!(Internal, "close: not implemented yet")
}
pub fn wait(_format: Format, _args: WaitArgs) -> Result<()> {
    crate::fail!(Internal, "wait: not implemented yet")
}
pub fn tasks(_format: Format) -> Result<()> {
    crate::fail!(Internal, "tasks: not implemented yet")
}
pub fn human(_format: Format, _cmd: HumanCommand) -> Result<()> {
    crate::fail!(Internal, "human: not implemented yet")
}
