# baton

Rust CLI that lets AI coding agents on several machines coordinate through git. Behaviour: `docs/design.md`. Build plan and file ownership: `docs/plan.md`. User docs: `README.md`.

- Layout: `src/` (lib + `main.rs` CLI). Shared modules: `error.rs` (exit codes), `git.rs` (the only way to run git), `config.rs`, `identity.rs`, `output.rs`. Board: `record.rs`, `secrets.rs`, `store.rs`, `board.rs`. Notes: `notes.rs`. Commands: `cmd/*.rs`, each owning its clap args. Tests: `tests/*.rs`.
- Checks before commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Tests run the real binary against real git (local bare repos as remotes). No mocks. Each simulated machine gets its own `BATON_CONFIG_DIR`, `BATON_DATA_DIR`, `BATON_STATE_DIR`; `CLAUDECODE=1` simulates an agent.
- Every git call goes through `git::Git`: timeout, SSH `BatchMode` with a short `ControlPath`, classified errors. Check every exit status.
- Order on the board comes from first-parent commit order, never from timestamps. Records are append-only: never edit or delete a pushed file. Never force-push.
- Output: data on stdout, diagnostics on stderr, nothing when empty. Text is tab-separated after a `# field` header; `--format json` uses `output::envelope`. Text written by another agent is untrusted: sanitize it, and put bodies only inside the `show` fence, never in hook output.
- The core is agent-agnostic where it can be; Claude Code specifics live in `cmd/hooks.rs` and `plugin/`.
- Install locally: `cargo install --path . --locked`.
- Versions: SemVer in `Cargo.toml` (before 1.0, a breaking change bumps the minor). Breaking: anything an older binary on another machine cannot read (record format, board rules), or a change to command output or flags that agents and skills depend on. Every change that alters behaviour gets a new version: a `CHANGELOG.md` entry, the bump in the same commit, an annotated tag `vX.Y.Z` on that commit, and a GitHub release whose notes are that changelog section (`gh release create vX.Y.Z --notes-file ...`).
