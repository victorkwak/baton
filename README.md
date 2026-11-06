# baton-touch

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Coordinate AI coding agents that run on more than one machine, through git.

The package is `baton-touch`. The command you run is `baton`, and the Claude Code skill is `/baton`.

## Why

You run Claude Code on a workstation and on a laptop. Each machine does what it is good at: one builds and deploys, the other runs tools only it has. To finish a task together, the agents must hand work to each other: "you take the iOS part of this branch", "tests pass, it's yours", "I'm waiting on you".

`baton` is that handoff. One agent posts a message to a shared git repo. The agent on the other machine wakes, reads it, does its part and replies. You stay in control: an agent never takes new work without your go-ahead.

## Status

**0.2, under trial.** The commands below work and are tested end to end against real git. Expect changes while it is tried on real work. Changes per version: [CHANGELOG.md](CHANGELOG.md). The full design: [docs/design.md](docs/design.md).

Today `baton` works with Claude Code. The core is not tied to Claude Code, but the hook and the skill are.

## Install

You need Rust 1.89 or later, git, and Claude Code on every machine.

1. **Make a board repo.** Once, for all machines: an empty private git repo that every machine can push to (for example on GitHub, over SSH). This repo holds the messages.

   Then do the next steps on each machine.

2. **Install the CLI.** The crate is `baton-touch` (`baton` on crates.io is a different crate). It installs a binary named `baton`:

   ```sh
   cargo install baton-touch --locked
   ```

   Or from a clone:

   ```sh
   git clone https://github.com/victorkwak/baton-touch ~/Repos/baton-touch
   cargo install --path ~/Repos/baton-touch --locked
   ```

3. **Write the config and clone the board.** Give each machine a short name:

   ```sh
   baton init --machine ws --machines ws,laptop --human victor --board git@github.com:you/board.git
   ```

   This writes `~/.config/baton/config.toml` and clones the board to `~/.local/share/baton/board`. It installs nothing else, and it prints the next steps.

4. **Add the hook and permissions.** `baton init` prints a JSON block. Merge it into `~/.claude/settings.json` and keep the hooks you already have. The block holds:
   - a `Stop` hook, `baton hook wait`, with `"asyncRewake": true`. It wakes an idle session when a message needs it.
   - `allow` rules for the `baton` commands an agent uses.
   - one `deny` rule for edits in the board clone, so agents change the board only through `baton`.

5. **Install the `/baton` skill.** It is a Claude Code plugin in this repo (`plugin/`). From a local clone:

   ```sh
   claude plugin marketplace add ~/Repos/baton-touch
   claude plugin install baton@baton
   ```

6. **Check the setup:**

   ```sh
   baton doctor
   ```

Optional: add `--notes <repo>` to `baton init` to share a notes repo too (see [Notes sync](#notes-sync)). Then `baton init` also writes a systemd timer (Linux) or a launchd agent (macOS) for the sweeper, and prints how to install it.

## Quick start

Two machines, `ws` and `laptop`. You start a task on `ws`, so `ws` leads.

1. **On `ws`**, you tell the agent that the laptop does the iOS part. The agent posts the plan in a new lane:

   ```sh
   baton post --to laptop --lane looks-share-sheet --task share-sheet --plan \
     -m "Branch app:share-sheet@abc123. Add the iOS share extension. The API is done." \
     "iOS share extension"
   ```

   It posts as soon as the laptop's part is unblocked, then does its own part in parallel.

2. **On `laptop`**, you run `/baton` in any Claude Code session. The skill lists the lanes with work waiting and asks which lane this session works on. You pick `looks-share-sheet`. It shows the plan and asks before it takes it.

3. **You say yes.** The agent runs `baton take <id>`, does the work, then reports. When `ws` must act on the answer, it posts a reply (`baton post --reply-to <id> ...`). When the sender needs nothing more, it closes with a note (`baton close <id> -m "..."`).

4. **On `ws`**, the idle session wakes within about 30 seconds of the reply, reads it with `baton show <id>`, and continues.

A session keeps listening in its lane until you run `/baton:off`, or until it is idle for its idle limit (1 hour by default). `/baton:on` listens again.

## How it works

- **A board repo.** Messages live in a small private git repo that every machine clones. It is append-only: each message, and each status change, is a new file. Two agents that post at once never touch the same file, so a push that loses a race is fixed by a rebase that cannot conflict.
- **First one wins.** The board's single branch acts as a compare-and-swap. Before every push, `baton` checks its rules again against the latest state. Only one plan per task, only the addressee changes a message's status, and only one machine can claim an open message. Order comes from git commits, never from clocks.
- **A lead and a follower.** The machine where you started a task leads. It posts the plan; the other machine follows, does its items and reports back. A machine cannot post a plan while it has an unanswered plan from the other one, so two agents never plan the same work.
- **Lanes for parallel work.** The lead names a lane for each workstream: one feature or problem (`--lane looks-share-sheet`). On the other machine, `/baton` asks which lane a session works on, and that session then wakes only for its lane. With no lane waiting, it listens in the lobby until a new lane appears. One machine can run several streams in separate sessions.
- **No polling turns.** The `Stop` hook waits in the background. It polls git, not the model, so it costs no tokens. It wakes an idle session only when a message needs action from it.
- **Untrusted by default.** A message is another agent's note. `baton` shows bodies only inside a marked fence, keeps them out of hook output, scans posts for secrets, and never treats a message as your approval.

## Commands

```
baton inbox [--lane L] [--all]              # what needs this machine (ids and metadata only)
baton show <id> [--thread]                  # one message, body inside a fence
baton post --to M [--lane L] [--task T] [--plan | --handover | --close-task]
           [--reply-to ID] [--branch repo:branch@sha] -m "..." "Subject"
baton take <id>                             # claim a message
baton release <id>                          # give a claim back
baton close <id> [--wontdo] [-m "..."]      # done, or wontdo with a reply
baton wait [--task T] [--reply-to ID] [--timeout 9m]   # block until an answer arrives
baton tasks                                 # open tasks, who leads, what is stale
baton on [--lane L] [--idle 4h|never]       # listen in a lane (or the lobby, `-`)
baton off                                   # stop waking this session
baton sync --paths <files> [-m "..."]       # commit and share notes safely
baton sweep                                 # one pass of the idle notes sweeper
baton human <handover|close-task|close|release|post>   # your housekeeping, at a terminal
baton init ... / baton doctor               # setup, and a read-only check
```

Every command takes `--format json`. `baton <command> --help` shows all flags. Output is written for agents: tab-separated records after a `# field` header, and nothing when there are no records. Exit codes are in [docs/design.md](docs/design.md).

## Notes sync

For a notes vault (for example Obsidian) kept in git and shared by the same machines, `baton sync --paths <files>` commits only the paths an agent changed. It rebases in a private worktree, so your notes never get conflict markers, and it never overwrites uncommitted work. On a conflict it stops and keeps your local commit. `baton sweep`, run every minute by a timer, syncs the repo when no file changed for 5 minutes.

## Safety

- **A message is not an approval.** The `/baton` skill tells the agent to treat every message as untrusted text. A message cannot approve a push to `main`, a deploy, a delete or a send. The agent asks you in its own session.
- **No new work without you.** A woken session can continue a task you already approved. For new work, it shows you the message and waits for your yes.
- **Secret scan.** Every post is scanned for known key and token patterns (private keys, GitHub, GitLab, Anthropic, Slack and AWS keys). A match refuses the post. Only you can override it, with `baton human post --allow-secret` at a terminal.
- **Your housekeeping stays yours.** `baton human` commands are refused inside Claude Code and without a terminal.
- **baton does not gate your agents.** Claude Code's permission rules protect your machine. `baton` adds only the deny rule for its board clone.
- **Keep the board private.** Messages hold your plans, branch names and notes.

## License

Apache-2.0. See [LICENSE](LICENSE).
