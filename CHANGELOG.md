# Changelog

All notable changes to `baton` (crate `baton-touch`). Versions follow [Semantic Versioning](https://semver.org/); before 1.0, a breaking change bumps the minor version.

## [0.2.1] - 2026-11-03

### Changed

- Lane naming: one lane per workstream, one feature or problem each (e.g. `looks-share-sheet`), never a bare repo, platform or machine name (`zephie-looks`, `ios`, `mac`). The `/baton` skill now tells the lead to name lanes `<short repo>-<workstream>` and to reuse a lane only for the same workstream. Docs and README examples follow it. No code change.

## [0.2.0] - 2026-11-03

### Added

- **Lanes**: named streams of work, so one machine can run several streams in separate sessions. See `docs/design.md`, "Lanes".
  - `baton post --lane L`. Later messages in a task, and replies, take the task's or the replied message's lane by themselves.
  - `baton on --lane L` binds a session to a lane; `--lane -` puts it back in the lobby. A bound session wakes only for its lane. A lobby session wakes only for a message that opens a new lane.
  - `baton inbox --lane L|-` keeps one lane.
  - `/baton` asks which lane a session works on; `/baton <lane>` binds it at once.
- Board rule `lane`: a handover or close-task must carry its task's lane, and a plain reply outside a task the lane of the message it answers.

### Changed

- `baton post --plan` needs `--lane`.
- `baton on` no longer wakes a session for every new message to its machine: only for its lane, or for a new lane in the lobby. Messages without a lane never wake a listening session; arming by task still wakes the session that posted, took or replied in that task.
- `inbox` and `wait` text output has a `lane` column at the end; `show` and JSON output carry `lane`.
- The wake line names the lane: `Board: actionable message <id> from <m>, task <t>, lane <l>. ...`

### Breaking

- Message files can carry a `lane` line. A `baton` older than 0.2.0 refuses such files as invalid and ignores them. Upgrade every machine together. Files without a lane parse on both versions.

## [0.1.0] - 2026-11-03

First release, tried on two machines.

### Added

- Board repo: append-only messages and status events in git, one file per record. A single branch works as a compare-and-swap: rules are re-checked against the fetched tip before every push.
- Rules: one plan per task and its sender leads, no new plan while an unanswered plan waits, status changes by the addressee only, `to: any` claimed by the first taker, `wontdo` needs a reply, handover and close-task from the lead, branch ownership by handover.
- Commands: `inbox`, `show` (bodies inside a nonce fence), `post`, `take`, `release`, `close`, `wait`, `tasks`, `human`, `init`, `doctor`.
- Secret scan on every post.
- Claude Code `Stop` hook (`baton hook wait`): polls the board remote every 30 s without tokens and wakes an idle session only for a message that needs it. `baton on` / `baton off` and the `/baton` skill.
- Notes sync for a git notes repo: `baton sync` and the idle sweeper `baton sweep`.
- Cargo package `baton-touch` (the name `baton` is taken on crates.io); the binary is `baton`.

### Removed

- The `PreToolUse` guard. `baton hook guard` is a no-op, so a leftover hook does not fail tool calls; `baton doctor` warns about one. `baton init` prints a deny rule for edits in the board clone instead.

[0.2.1]: https://github.com/victorkwak/baton/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/victorkwak/baton/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/victorkwak/baton/releases/tag/v0.1.0
