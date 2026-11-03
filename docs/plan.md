# baton v1 build plan

Behaviour is specified in `docs/design.md`; this plan splits the work. When this plan and the design disagree, the design wins; say so in your report.

## Ownership (so parallel work never edits the same file)

| Owner | Files |
|---|---|
| Lead (main session) | `error.rs`, `git.rs`, `config.rs`, `identity.rs`, `output.rs`, `lib.rs`, `main.rs`, `cmd/mod.rs`, `Cargo.toml`, `docs/` |
| Agent A: board | `record.rs`, `secrets.rs`, `store.rs`, `board.rs`, `cmd/board.rs`, `tests/board*.rs`, `tests/common/board.rs` |
| Agent B: notes | `notes.rs`, `cmd/notes.rs`, `tests/notes*.rs`, `tests/common/notes.rs` |
| Agent C: setup and hooks (after A) | `hooks.rs` (new), `cmd/hooks.rs`, `cmd/setup.rs`, `plugin/`, `tests/hooks*.rs`, `tests/setup*.rs` |

If you need a change in a file you do not own (a new dependency, a helper in `git.rs`, a new `Kind`), make the smallest change, list it under "Shared-file changes" in your report, and keep it in its own commit.

## Agent A: the board

1. **`record.rs`**
   - `Message`, `Event` (status or lead), `Record`.
   - ULID ids from the `ulid` crate; `at` from `jiff` (UTC, RFC 3339, seconds).
   - Field validation (`docs/design.md`, "Message file").
   - The frontmatter writer and a strict parser for exactly what the writer produces (flat `key: value`; `related` as a JSON array; strings JSON-quoted unless slug-safe). The parser refuses unknown keys and bad values.
   - Unit tests: round trip; every invalid field is refused.
2. **`secrets.rs`:** `scan(text) -> Vec<Finding>` with the patterns in the design. Unit tests: each pattern found; 40-hex SHAs, UUIDs and base64 test output not flagged.
3. **`store.rs`**, the board clone through `git::Git`:
   - `open(config)`: clone if missing, check the remote URL.
   - `lock()`: `File::lock` on `<clone>/.git/baton.lock`.
   - `fetch()`.
   - `records()`: every record in first-parent commit order (`git log --first-parent --reverse --name-status` + `cat-file --batch`), files in one commit by id.
   - `commit(records, parent, message)` through a temporary index; returns the new sha.
   - `push_cas(build)`: fetch → load state → call `build(&State) -> Result<Vec<Record>>` (re-checks the rules) → commit on the fetched tip → `git push origin <sha>:refs/heads/main`; on rejection, retry up to 3 times with jittered backoff; then `Kind::Remote`. Never force.
   - Offline: plain messages may be committed locally (to `refs/heads/main`) and pushed by the next write; rule-bearing records are refused with `Kind::Offline` and no local commit.
4. **`board.rs`:**
   - `State::fold(records)` applies the rules in order: addressee, transitions, `to: any` claims, plan uniqueness, the unanswered-plan rule, the lead (plan sender, then handovers and lead events), closed tasks, branch owners.
   - `State::check(&Who, &Action) -> Result<()>` with `Kind::Rule`; the message names the rule.
   - Views: `inbox(machine, all)`, `tasks()`, `thread(id)`.
   - Unit tests for every rule, both allowed and refused.
5. **`cmd/board.rs`:** the commands in the design: text and JSON output, the fence for `show` (random nonce; subject and body sanitized with `output::sanitize`), exit codes. `wait` polls with `ls-remote` (interval from `BATON_POLL_SECS`, default 30, for tests) and prints the inbox line of each new actionable record.
6. **Tests (`tests/board.rs`)** run the real binary against a local bare repo, with separate `BATON_CONFIG_DIR`/`BATON_DATA_DIR`/`BATON_STATE_DIR` per simulated machine and `CLAUDECODE=1` for agents. Cover:
   - post → inbox → show → take → close across two machines;
   - two machines racing 30 posts each (no loss, linear history);
   - two racing plans for one task (exactly one lands);
   - the unanswered-plan rule across different task slugs;
   - a `to: any` claim race;
   - transitions refused after `done`;
   - `wontdo` without a reply refused;
   - an agent closing a message to the human refused;
   - secrets refused (exit 11);
   - offline: plain post queued, take refused (exit 30), by pointing `origin` at a missing path;
   - control characters stripped in `show`;
   - `--format json` shape;
   - `wait` returns when the other machine posts, and times out (exit 3).

## Agent B: notes sync

1. **`notes.rs`**, the algorithm in `docs/design.md`, "Notes sync", steps 1–8, with `git::Git`:
   - preflight;
   - lock (`File::lock` on `.git/baton.lock`, waiting up to 120 s);
   - snapshot;
   - `add -N` + `commit --only` with trailers;
   - fetch;
   - rebase in the private worktree `.git/baton/wt`, abort on conflict;
   - the check command on the rebased tree;
   - journal + `reset --keep`, with collisions mapped to `Kind::SyncCollision` and the files named;
   - fast-forward push with 3 retries;
   - recovery from the journal.

   `--all` is refused when `CLAUDECODE` is set. Print what was committed, integrated and pushed; `--format json` too.
2. **`sweep`:** idle detection (no tracked file in the notes repo modified within `BATON_SWEEP_IDLE_SECS`, default 300; no lock held), then `sync --all` with `Baton-Session: sweep`. Files whose check fails are left out and reported. Also a plain pull when nothing is dirty.
3. **Tests (`tests/notes.rs`)**, real git with two clones of a bare repo:
   - new file via `--paths` reaches the other clone;
   - other sessions' edits stay uncommitted;
   - an untracked file at an incoming path → 20, nothing changed;
   - a dirty file changed upstream → 20;
   - an adjacent-line conflict → 21, no conflict markers in the notes, the local state kept;
   - a failing check → 22;
   - a push race between two clones resolves;
   - an interrupted apply is finished from the journal;
   - `--all` refused for agents;
   - sweep commits after idle and leaves a file with a failing check.

## Agent C: setup and hooks (after A is merged)

1. **`hook session-start`, `hook wait`, `hook guard`**, as in `docs/design.md`, "Claude Code hooks". Hooks read JSON on stdin (`session_id`, `cwd`, `tool_name`, `tool_input`) and answer as Claude Code expects: `wait` exits 2 with the wake line on stderr; `guard` prints a JSON `hookSpecificOutput` with `permissionDecision` `allow`/`ask`/`deny` and a reason. Check the exact formats in the Claude Code hooks docs.
2. **`init`**: write the config (refuse to overwrite), clone the board, print the settings JSON for the hooks and permissions, and the sweeper units (systemd user service + timer on Linux, LaunchAgent plist on macOS) as files to install, with the commands. It changes nothing outside `baton`'s own directories.
3. **`doctor`:** report config, clone, remote reachability, SSH agent, hooks found in `~/.claude/settings.json`, stale locks and journals.
4. **`plugin/`:** a Claude Code skill `board` (the command cheat-sheet, plan and reply templates).
5. **Tests:** feed hook JSON on stdin and check exit codes and output; `guard` decisions for each git case; `init` into temp directories; `wait` wakes on a new actionable message and not on events or the session's own records.

## Done means

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
- Every behaviour above has a test that runs the real binary against real git (no mocks).
- The report names the commits, the test counts, and any design point you changed or could not meet.
