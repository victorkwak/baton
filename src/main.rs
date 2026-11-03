use std::process::ExitCode;

use clap::{Parser, Subcommand};

use baton::Format;
use baton::cmd::{board, hooks, notes, setup};

/// Coordinate AI coding agents on several machines through git.
///
/// Output is for agents: one record per line, tab-separated, after a
/// `# field...` header; nothing when there are no records. `--format json`
/// prints one object: `{"schema_version":1,"command":...,"items":[...]}`.
/// Exit codes: see `docs/design.md`.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Actionable messages for this machine (ids and metadata only).
    Inbox(board::InboxArgs),
    /// One message, its body inside an untrusted-content fence.
    Show(board::ShowArgs),
    /// Post a message.
    Post(board::PostArgs),
    /// Claim a message (working).
    Take(board::IdArgs),
    /// Give a claim back (open).
    Release(board::IdArgs),
    /// Close a message (done, or wontdo with a reply).
    Close(board::CloseArgs),
    /// Block until an actionable message arrives.
    Wait(board::WaitArgs),
    /// Open tasks, leads, stale items.
    Tasks,
    /// The human's housekeeping (refused inside Claude Code).
    #[command(subcommand)]
    Human(board::HumanCommand),
    /// Commit, integrate and push the notes repo.
    Sync(notes::SyncArgs),
    /// One pass of the idle notes sweeper.
    Sweep,
    /// Claude Code hooks.
    #[command(subcommand)]
    Hook(hooks::HookCommand),
    /// Write the config and clone the board.
    Init(setup::InitArgs),
    /// Check the setup; report only.
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let f = cli.format;
    let result = match cli.command {
        Command::Inbox(a) => board::inbox(f, a),
        Command::Show(a) => board::show(f, a),
        Command::Post(a) => board::post(f, a),
        Command::Take(a) => board::take(f, a),
        Command::Release(a) => board::release(f, a),
        Command::Close(a) => board::close(f, a),
        Command::Wait(a) => board::wait(f, a),
        Command::Tasks => board::tasks(f),
        Command::Human(c) => board::human(f, c),
        Command::Sync(a) => notes::sync(f, a),
        Command::Sweep => notes::sweep(f),
        Command::Hook(c) => hooks::hook(c),
        Command::Init(a) => setup::init(a),
        Command::Doctor => setup::doctor(f),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("baton: {e}");
            ExitCode::from(e.code())
        }
    }
}
