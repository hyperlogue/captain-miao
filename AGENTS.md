# captain-miao

TUI dashboard for coding-agent sessions across terminal emulators and pooled
hosts. The launcher owns session state; the dashboard watches its state files.

## Working agreement

- Carry the requested change through implementation, relevant verification and
  the commit below. Resolve routine choices from the code and session context;
  ask when missing information would change the result or authorized scope.
- Follow explicit user instructions over skill guidance. If a repository rule
  or skill blocks completion, cite its file and the exact requirement, explain
  the blocker, and finish independent work before asking for a decision.
- Keep status updates and the final report concise: outcome, checks, and any
  remaining blocker. State assumptions that affect the result.

## Map and task-specific rules

| Location | Responsibility |
| --- | --- |
| `crates/cm-core` | Shared agent, launcher, state and protocol logic; no ratatui or libshpool. |
| `src/` | `miao`: dashboard, terminal control, launch/hook/focus; no pty pool. |
| `crates/cm-server` | `miao-server`: per-host daemon and pty pool. |
| `crates/cm-client` | `miao-client`: CLI over the local pool socket. |
| `xtask` | Obtain server payloads and build distribution variants. |

Read the guide for the behavior being changed, including changes in callers
outside the named directories:

- **Dashboard, keybindings, terminal control, previews or attach/detach:**
  [dashboard rules](docs/agent-guides/dashboard.md). `Keymap` lives in
  `src/app/keymap.rs`; `run_command` in `src/app/keys.rs` owns command effects.
- **Agent CLIs, hooks, session status, persistence, hosts or transports:**
  [session rules](docs/agent-guides/sessions.md).
  `AgentControl` is in `crates/cm-core/src/agent.rs`; `Backend` is in
  `src/backend/mod.rs`, sharing `LocalBackend` with the server.
- **Build scripts, packaging, CI or releases:**
  [distribution rules](docs/agent-guides/distribution.md).
- **Remote architecture or wire protocol design:**
  [remote sessions](docs/remote-sessions.md). For crate boundaries or embedded
  payload design, use [crate split](docs/crate-split.md).
- **Changelog, version bump or release execution:**
  [release skill](.claude/skills/release/SKILL.md), shared with Codex through
  `.agents/skills/release`. Use only the workflow the user requested.

Module docs own local constraints and rationale; the README owns user-facing
behavior. Keep this file to shared working rules and conditional pointers.

## Verification

Use the toolchain in `nix develop`, as CI does. For Rust or Cargo changes, run
`cargo fmt --all` and
`cargo clippy --workspace --all-targets --locked -- -D warnings` before staging,
plus tests covering the changed behavior. `cargo test --workspace --locked` is
the full suite; broader checks are warranted by cross-crate changes or release
preparation. Rerun affected checks after fixes; a passing check needs repeating
only if subsequent changes affect it.

For documentation or skill-only changes, check links, examples, instruction
consistency and skill frontmatter; Rust tests add no coverage to prose. Changes
to executable scripts or CI need checks of their actual behavior.

The ignored `drives_a_real_tmux_server` test starts an isolated local tmux
server; run it when changing tmux integration. Remote provisioning tests need a
user-designated disposable host and a server payload; see the recipe in
`src/backend/provision.rs`. Selecting a remote test is not permission to deploy
to an arbitrary configured host.

## Committing

- Work on `main` directly for routine work; no feature branches or PRs.
- Other sessions share this tree. Before **each** staging operation, run
  `git diff --cached --name-only`. If another session has staged files, wait
  5s, then 10s, then 30s, rechecking after each wait. If the index is still
  occupied after the last wait, stop and report it; leave those files alone.
- Stage **and** commit only your paths: `git add <path>…`, verify the staged
  names are only yours, then `git commit -- <path>…`. Never use `git add -A`,
  `git add .`, `git add -u`, `git commit -a` / `-am`, or an index-wide commit.
  If staging or committing is blocked, preserve the work and report the blocker.
- Commit when the work is complete and no user decision remains. An open
  question or unexpected tradeoff leaves staging and committing to the user.
  Push only when the user requests it.
- Prefer small, self-contained commits that each build and pass their relevant
  tests; one logical change spanning crates stays together. Recheck the index
  before each commit's staging step.

Commit subjects are capitalized imperatives, at most 72 characters, with no
trailing period or forced scope (`doc:` is fine for documentation). Add a body,
wrapped around 72 columns, when the motivation or constraint is not clear from
the subject and diff; include a short verification note when checks ran.
Use no `Co-Authored-By` trailer, including one added by a harness.
