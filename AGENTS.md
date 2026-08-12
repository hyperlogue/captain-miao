# captain-miao

TUI dashboard to monitor and manage multiple Claude Code / Codex sessions across
Kitty, zellij and tmux.

This file is the **map and the house rules**: where things live, and the
constraints you can't discover by reading the file you're editing. Design
rationale lives in the module docs (dense on purpose) and in `docs/`;
user-facing behaviour lives in the README. Don't grow this file with either —
put it where the code is.

---

## Map

Unidirectional. The launcher is the single source of truth; the dashboard is a
pure viewer that re-reads state files on `notify` events and does no IPC.

```
Claude/Codex hook → miao hook → launcher (Unix socket)
                                    ↓ writes ~/.local/state/captain-miao/sessions/{pid}.json
                                dashboard (notify watcher) reads it
```

| Crate | Binary | What |
| --- | --- | --- |
| `crates/cm-core` | — | Shared logic + data. No ratatui, no libshpool, so it cross-compiles into the server. |
| `.` (root) | **`miao`** | The TUI (`src/app/`, `src/terminal/`), plus `claude`/`codex`/`hook`/`focus`. No pty pool. |
| `crates/cm-server` | **`miao-server`** | Headless per-host daemon + pty pool. Cross-compiled to Linux and deployed to remotes. |
| `crates/cm-client` | **`miao-client`** | Thin CLI over the *local* pool socket: `list`, `attach`. |
| `xtask` | — | `prepare-servers` (obtain) and `dist` (build variants). |

Four seams carry the whole design; each is documented at its definition.

- **`AgentControl`** (`cm-core/agent.rs`) — Claude vs Codex. A feature one agent
  lacks returns `None`/empty from its method; the UI gates on that.
- **`Backend`** (`src/backend.rs`) — where sessions run. `LocalBackend`
  (`cm-core/backend.rs`) is also the server-core.
- **`Terminal`** (`src/terminal/`) — per-emulator control. One `capabilities()`
  query is the whole capability seam: a new backend limitation is a new *field*
  there, not a new trait method.
- **`Keymap`** (`src/app/keymap.rs`) — every Normal-mode command is remappable;
  `run_command` is the one place a `Command` becomes a side effect.

`docs/remote-sessions.md` is the authority on remote hosts and the wire
protocol; `docs/crate-split.md` on the crate split and server payloads. The
`/release` skill has the release procedure.

### State files

`~/.local/state/captain-miao/` (paths in `cm-core/state.rs`), **owner-only** —
dirs `0700`, JSON `0600`, because state records the user's prompt text and cwds.
Write through `create_dir_all_private` / `write_json_atomic`, never `fs::write`.
All of it is safe to delete; each file regenerates or resets. Runtime sockets
live under `$XDG_RUNTIME_DIR/captain-miao/`.

---

## Invariants

Constraints that bite from a *different* file than the one that defines them.
Anything local to one module is in that module's doc instead.

### Never

- **`git add -A` / `.` / `-u`, `git commit -a` / `-am`, or a bare index-wide
  `git commit`.** Other agent sessions run in this same tree; each sweeps their
  work into your commit. Stage *and* commit by path — see Committing.
- **Snapshot the terminal from the launcher.** Window/tab lookup is
  presentation-only and a launcher may be headless or remote. The launcher only
  ever self-reports its own window from the env (`current_window`).
- **Parse a `SessionKey`** above the backend seam. It is opaque; the owning host
  re-resolves it to a pid at signal time.
- **Put `$HOME` on the wire.** Paths cross in the host-canonical `~` form
  (`cm_core::paths`); expand on receipt, collapse on return. A `~` path spliced
  into a *shell* command goes through `paths::shell_quote_host_path`.
- **Call `mark_dirty` before the mutation lands.** It reads the sort anchor from
  the current rows; running it early caches the stale order under the new
  version and panics the next reload.
- **Write hooks into `~/.claude/settings.json`.** They are injected per-session
  via `--settings` and torn down on exit.
- **Let zellij's `list-panes` onto a hot path** (~20ms *per pane* server-side) —
  never on focus, spawn, or restart. tmux's is cheap; don't generalize either
  way.
- **Ask the terminal to `hold` a window you want to read after its command
  dies.** kitty's `--hold` starts the user's login shell when the command exits,
  so a dropped ssh leaves a live local shell wearing a session's title. The
  attach wrapper does its own holding (`ATTACH_REPORT_SCRIPT`).
- **Fold the attach wrapper's `HUP` trap back into the `EXIT` one.**
  `trap 'r 129' HUP` is separate on purpose: a terminal that ends a window by
  closing the pty master (rather than `killpg`-ing the group) signals only the
  session leader, so ssh is left to exit **255** on its own — and a trap
  inheriting `$?` then reports a deliberate close as a dropped link, silently
  disabling `[remote] on_window_close`.
- **Add a 9th `kitten @` command** without updating the README's rc allowlist —
  every user on the recommended config gets a hard denial on it.
- **Pass Claude's `--tmux`.** It makes its own tmux session on the same server,
  so the identity matches, the binding isn't classified foreign, and it then
  never resolves.

### Always

- **Give `App::mark_dirty` an explicit `Cursor`.** There is deliberately no
  default — invalidating the order says nothing about the index derived from it.
  `FollowSession` is the common case; `Follow(key)` to advance to one named
  *before* the mutation; `HoldIndex` when only rendering changed; `Top` for
  search.
- **Route every window-binding change through the `App` methods**
  (`record_window_binding`, `retire_window_binding`, `prune_detached_sessions`,
  `apply_detach_reports`) — never `window_bindings` directly. They mark dirty
  *and* re-anchor the cursor, both of which a raw write misses.
- **Retire a binding before closing the window yourself.** A detach report for a
  binding we still hold reads as the *user* closing that window, which ends the
  session under `[remote] on_window_close` (default `close`). Only status `129`
  counts as a user close — ssh's 255 (dropped link) and an in-session detach's 0
  must keep the session, or a flaky network becomes lost work — and a report
  drained at **startup** never ends anything, since a quitting terminal SIGHUPs
  every attach window on its way out.
- **Branch on host, capability, or connection state — never on locality.**
  `capabilities() -> {pooled, shell}` is what detach/steal/the detached tier key
  on, which is why they work under pooled-localhost.
- **Keep protocol changes additive** (`#[serde(default)]`). v4 is meant to be the
  last refusing bump; unknown frames decode to `Unknown` and are ignored.
- **Hide an unsupported affordance, don't offer a key that only errors.** `t` on
  zellij, `Space l` on tmux, `Ctrl-g` on Codex all do this. Render bindings via
  `keys_for`/`primary_key` so a remap shows through without touching `draw.rs`.
- **Wrap anything sent over ssh in `/bin/sh -c '<script>'`** (`login_shell_safe`)
  — the account's login shell is routinely fish. Such a script may contain **no
  single quote and no backslash**.
- **Namespace every persisted window id by the instance that minted it**
  (`zellij:<session>` / `tmux:<socket>,<server-pid>` / `kitty:<socket|pid>`) —
  those id spaces overlap. A row stamped with another instance is *foreign*:
  drawn dimmed, window ops inert, bindings carried verbatim through every
  rewrite so switching backends loses nothing.
- **Treat Claude's own session file as authoritative** on the
  working/idle/background-shell axis; mirror it, no edge-tracking. Refinement is
  **demote-only** — hooks own rest→`Active`. An unreadable or unrecognized read
  maps to `None` (leave unchanged), never a definite state.
- **Leave worktrees entirely to the agent.** captain-miao creates, names and
  cleans up nothing: `worktree_args` contributes `--worktree [name]` and the
  agent owns the branch, base ref, enforcement and cleanup. Resume and restart
  never pass the flag — the agent re-enters the session's own worktree.
- **Let per-session flags be host-owned** for a pooled host (`session-flags.json`
  sidecar, never the launcher's state file — single-writer rule).
- **Keep `CM_SERVER_PAYLOAD_MANIFEST` the only switch for embedded servers.**
  Unset (every ordinary build) the table is empty; set *and wrong* is a hard
  build error, never a quiet empty table. `build.rs` watches each archive, so
  never rewrite one with identical bytes — it forces a full LTO relink.
- **Land a built executable on a fresh inode** (`xtask`'s `install`), never by
  copying over the file already there. macOS binds a signature to the vnode an
  executable ran from, and a path rewritten in place then dies on every exec with
  `SIGKILL (Code Signature Invalid)` while the same bytes run fine elsewhere.
- **Drop the `captain-` prefix on shipping binaries and keep it everywhere
  else** — Cargo/npm packages, nix attrs, `~/.config` + `~/.local/state` +
  `~/.cache` dirs. `xtask/src/server.rs` carries both `SERVER_PKG` and
  `SERVER_BIN` because conflating them builds fine and then can't find the
  binary.
- **Bump `[workspace.package] version` before you tag** (CI's `verify` fails
  otherwise) and refresh `Cargo.lock` with it. Tags are plain SemVer. **No
  `run:` body may interpolate a `${{ }}` expression** — values reach the shell
  through `env:`.

---

## Committing

- **Run `cargo fmt --all` and `cargo clippy --workspace --all-targets --locked
  -- -D warnings` before you stage.** CI's first job is both; there is no local
  hook.
- **Work on `main` directly.** No feature branches, no PRs for routine work.
- **Before you stage, check for a concurrent committer.** captain-miao's whole
  premise is running many agent sessions at once, so another one may be
  mid-commit in this same tree. The workflow is _stage, then commit_, so a
  non-empty index you didn't create means someone else is inside their
  stage→commit window. Run `git diff --cached --name-only` first; if it lists
  anything, back off and re-check on a 5s → 10s → 30s schedule until the index
  is empty. If files are _still_ staged after the 30s wait, **stop and report to
  the user** — never commit over another agent's staged work.
- **Commit only what you changed — stage _and_ commit by path.** `git add
  <path>…` then `git commit -- <path>…`; verify with `git diff --cached
  --name-only` that the staged set is _only_ yours. If staging or committing
  hits a blocker, pause and ask.
- **Commit once the work is fully complete.** With no open decision left for the
  user, commit it yourself (commit only; pushing waits for the user). If there's
  still an open question or an unexpected tradeoff, finish the work but leave
  `git add` / `commit` / `push` to the user.
- **Prefer several small, self-contained commits over one large one.** Split
  along the seams the work already has: a refactor lands ahead of the change it
  makes room for, a fix travels with the test that pins it, a doc update rides
  with the behaviour it describes. Each piece must build with its tests passing
  and name one thing in its subject. Where that pulls against self-containment,
  **self-contained wins**: one logical change spanning several crates stays one
  commit rather than three that don't compile in sequence. Re-run the
  concurrent-committer check before each one.

**Message format** — subject: capitalized imperative, ≤72 chars, no trailing
period, no forced scope (a `doc:` prefix is fine for docs-only commits). Body
(blank line, wrapped ~72) whenever the _why_ isn't obvious from subject + diff:
the motivation or the non-obvious constraint, not a narration of the diff; close
with a short verification note when you ran one. **No `Co-Authored-By` trailer**
— strip it if the harness appends one.

---

## Dev commands

```sh
cargo run                    # run TUI dashboard
cargo run -- claude .        # launch Claude in current dir with hooks
cargo run -- focus --window-id $KITTY_WINDOW_ID
                             # focus dashboard AND ring this window's bell
cargo build --workspace
cargo build --no-default-features   # strictly local-only dashboard (no remote hosts)
cargo test --workspace

cargo run -p captain-miao-server -- daemon ensure|status|stop
cargo run -p captain-miao-client -- list | attach <name> [--force]

# Embedded server payloads. A plain `cargo build` bundles nothing.
cargo xtask dist [--list|--from release|--server <target>=<path>]
cargo xtask prepare-servers --out dist/servers   # what release CI runs
miao --version                                   # what a built binary embeds

# Ignored tests that need a live host / server:
CM_TEST_SSH_TARGET=box cargo test -p captain-miao --features bundle-linux-x86_64 -- \
  --ignored provisions_a_real_host
cargo test -p captain-miao -- --ignored drives_a_real_tmux_server
```
