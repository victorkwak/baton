# baton

Coordinate AI coding agents that run on more than one machine, through git.

You run Claude Code on a workstation and on a laptop. Each machine does what it is good at: one builds and deploys, the other runs tools only it has. To finish a task together, the agents need to hand work to each other: "you take the iOS part of this branch", "tests pass, it's yours", "I'm waiting on you". `baton` is that handoff.

**Status: v1, under trial.** The commands below work and are tested end to end against real git; the design is in [docs/design.md](docs/design.md). Expect changes while it is tried on real work.

## How it works

- **A board repo.** Messages live in a small private git repo that every machine clones. It is append-only: each message, and each status change, is a new file. Two agents posting at once never touch the same file, so a push that loses a race is fixed by a rebase that cannot conflict.
- **First one wins.** The board's single branch acts as a compare-and-swap. Before every push, `baton` re-checks its rules against the latest state. Only one plan per task, only the addressee changes a message's status, and only one machine can claim an open message. Order comes from git commits, never from clocks.
- **A lead and a follower.** The machine where you started a task leads. It posts the plan; the other machine follows, does its items and reports back. A machine cannot post a plan while it has an unanswered plan from the other one, so two agents never plan the same work.
- **No polling turns.** A Claude Code `Stop` hook waits in the background, polling git (not the model) and costing no tokens. It wakes an idle session only when a message needs action from it.
- **Untrusted by default.** A message is another agent's note. `baton` shows bodies inside a marked fence, keeps them out of hook output, scans posts for secrets, and never treats a message as your approval.
- **Optional notes sync.** For a notes vault (e.g. Obsidian) kept in git and shared by the same machines, `baton sync` commits only the paths an agent changed. It rebases out of sight, so your notes never see conflict markers, and it never overwrites uncommitted work.

## Commands

```
baton inbox                         # what needs me (ids and metadata only)
baton show <id> [--thread]          # one message, fenced
baton post --to mac --task t --plan "Subject" -m "…"
baton take <id> / baton close <id> [--wontdo] -m "…"
baton wait --task t                 # block until the other machine answers
baton tasks                         # what is in flight, who leads, what is stale
baton on / baton off                # listen for new messages / stop waking
baton sync --paths <files>          # commit and share notes safely
```

## License

Apache-2.0. See [LICENSE](LICENSE).
