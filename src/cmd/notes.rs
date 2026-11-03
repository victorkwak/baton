//! Notes commands: sync, sweep. Owner: agent B (`docs/plan.md`).

use std::time::Duration;

use clap::Args;

use crate::Format;
use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::identity::Who;
use crate::notes::{self, Options, Repo, Report, Select};
use crate::output;

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Files or directories this caller changed (relative to the notes repo, or absolute inside it).
    #[arg(long, num_args = 1.., conflicts_with = "all", required_unless_present = "all")]
    pub paths: Vec<std::path::PathBuf>,
    /// Every change in the notes repo (the human and the sweeper only).
    #[arg(long)]
    pub all: bool,
    /// Commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
}

fn is_agent() -> bool {
    std::env::var_os("CLAUDECODE").is_some()
}

/// The config and its notes repo, or a usage error.
fn notes_config() -> Result<(Config, std::path::PathBuf)> {
    let config = Config::load()?;
    let Some(repo) = config.notes_repo()? else {
        crate::fail!(Internal, "no [notes] repo in {}", Config::path()?.display());
    };
    Ok((config, repo))
}

fn check_of(config: &Config) -> Vec<String> {
    config
        .notes
        .as_ref()
        .map(|n| n.check.clone())
        .unwrap_or_default()
}

pub fn sync(format: Format, args: SyncArgs) -> Result<()> {
    if args.all && is_agent() {
        crate::fail!(
            Internal,
            "sync --all is for the human and the sweeper; an agent passes --paths with the files it changed"
        );
    }
    let (config, path) = notes_config()?;
    let repo = Repo::open(&path)?;
    let select = if args.all {
        Select::All
    } else {
        Select::Paths(
            args.paths
                .iter()
                .map(|p| notes::normalize_path(repo.root(), p))
                .collect::<Result<_>>()?,
        )
    };
    let who = Who::detect(&config, None);
    let session = match &who {
        Who::Agent { session, .. } => session.clone(),
        Who::Human { .. } => "human".to_owned(),
        Who::Machine { .. } => "cli".to_owned(),
    };
    let opts = Options {
        select,
        message: args.message,
        machine: config.machine.clone(),
        session,
        check: check_of(&config),
    };
    let mut report = Report::default();
    let result = repo
        .lock(notes::LOCK_WAIT)
        .and_then(|_lock| repo.sync_locked(&opts, &mut report));
    print(format, "sync", &report);
    result
}

pub fn sweep(format: Format) -> Result<()> {
    if is_agent() {
        crate::fail!(
            Internal,
            "sweep is for the timer and the human; an agent runs baton sync --paths"
        );
    }
    let config = Config::load()?;
    let Some(path) = config.notes_repo()? else {
        // No notes repo: nothing to sweep.
        return Ok(());
    };
    let repo = Repo::open(&path)?;
    let Some(_lock) = repo.try_lock()? else {
        // Another baton is working on the repo; the next pass catches up.
        return Ok(());
    };
    let idle = match std::env::var("BATON_SWEEP_IDLE_SECS") {
        Ok(v) => Duration::from_secs(v.trim().parse().map_err(|_| {
            Error::new(
                Kind::Internal,
                format!("BATON_SWEEP_IDLE_SECS={v:?} is not a number of seconds"),
            )
        })?),
        Err(_) => notes::SWEEP_IDLE,
    };
    let opts = Options {
        select: Select::All,
        message: None,
        machine: config.machine.clone(),
        session: "sweep".to_owned(),
        check: check_of(&config),
    };
    let mut report = Report::default();
    let result = repo.sweep_locked(&opts, idle, &mut report);
    print(format, "sweep", &report);
    let left_out = result?;
    if !left_out.is_empty() {
        crate::fail!(
            CheckFailed,
            "check failed for {}; left uncommitted, the rest synced",
            left_out.join(", ")
        );
    }
    Ok(())
}

fn print(format: Format, command: &str, report: &Report) {
    match format {
        Format::Json => {
            let items = serde_json::to_value(&report.items).unwrap_or_default();
            crate::out!("{}", output::envelope(command, items));
        }
        Format::Text => {
            if report.items.is_empty() {
                return;
            }
            crate::out!("# step\tsha\tpaths");
            for i in &report.items {
                let paths: Vec<String> = i.paths.iter().map(|p| output::field(p)).collect();
                crate::out!("{}\t{}\t{}", i.step, i.sha, paths.join(" "));
            }
        }
    }
}
