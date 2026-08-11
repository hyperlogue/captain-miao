# captain-miao

TUI dashboard to monitor and manage multiple Claude Code / Codex sessions across
Kitty, zellij and tmux.

This file is **guidance**: the rules to work by and the map to find things. The
*why* behind a design lives in the code it governs (module docs are dense on
purpose) and in `docs/`. Don't grow this file with rationale — put that where the
code is.

---

## Rules

**Committing** and **Dev commands** are at the bottom; read them before your
first commit. Everything else here is a constraint you can break by accident.

### Never

- **`git add -A` / `.` / `-u`, `git commit -a` / `-am`, or a bare index-wide
  `git commit`.** Other agent sessions run in this same tree; each sweeps their
  work into your commit. Stage *and* commit by path.
- **Snapshot the terminal from the launcher.** Window/tab lookup is
  presentation-only and a launcher may be headless or remote. The launcher only
  ever self-reports its own window from the env (`current_window`).
- **Parse a `SessionKey`** above the backend seam. It is opaque; the owning host
  re-resolves it to a pid at signal time.
- **Put `$HOME` on the wire.** Paths cross in the host-canonical `~` form
  (`cm_core::paths`); expand on receipt, collapse on return. A `~` path spliced
  into a *shell* command must go through `paths::shell_quote_host_path`.
- **Add a 9th `kitten @` command** without updating the README's rc allowlist —
  every user on the recommended config gets a hard denial on it.
- **Call `mark_dirty` before the mutation lands.** It reads the sort anchor from
  the current rows; running it early caches the stale order under the new
  version and panics the next reload.
- **Write hooks into `~/.claude/settings.json`.** They are injected per-session
  via `--settings` and torn down on exit.
- **Let `list-panes` onto a hot path (zellij).** ~20ms *per pane* server-side.
  Never on focus, spawn, or restart.

### Always

- **`cargo fmt --all` and `cargo clippy --workspace --all-targets --locked --
  -D warnings` before staging.** CI's first job is both; there is no local hook.
- **Give `App::mark_dirty` an explicit `Cursor`.** There is deliberately no
  default — invalidating the order says nothing about the index derived from it.
  `FollowSession` (stay on the session) is the common case; `Follow(key)` to
  advance to one named *before* the mutation; `HoldIndex` to let the next row
  arrive under the cursor, and the honest answer when only rendering changed;
  `Top` for search.
- **Route every window-binding change through the `App` methods**
  (`record_window_binding`, `retire_window_binding`, `prune_detached_sessions`,
  `apply_detach_reports`) — never `window_bindings` directly. They mark dirty
  *and* re-anchor the cursor, both of which a raw write misses.
- **Branch on host, capability, or connection state — never on locality.**
  `capabilities() -> {pooled, shell}` is what detach/steal/the detached tier key
  on, which is why they work under pooled-localhost.
- **Keep protocol changes additive** (`#[serde(default)]`). v4 is meant to be the
  last refusing bump; unknown frames decode to `Unknown` and are ignored.
- **Hide an unsupported affordance, don't offer a key that only errors.** `t` on
  zellij, `Space l` on tmux, `Ctrl-g` on Codex all do this.
- **Wrap anything sent over ssh in `/bin/sh -c '<script>'`** (`login_shell_safe`)
  — the account's login shell is routinely fish. Such a script may contain **no
  single quote and no backslash**.

---

## Architecture

Unidirectional. The launcher is the single source of truth; the dashboard is a
pure viewer that re-reads state files on `notify` events and does no IPC.

```
Claude/Codex hook → miao hook → launcher (Unix socket)
                                    ↓ writes ~/.local/state/captain-miao/sessions/{pid}.json
                                dashboard (notify watcher) reads it
```

- **Launcher** (`cm-core/src/launcher.rs`) — one per session. Wraps the agent,
  receives hooks, folds the transcript (context tokens, model, first prompt,
  session name), writes the state file.
- **Dashboard** (`src/app/`) — never reads transcripts. Everything it shows is
  already on `LauncherState`. Row title precedence: `name` → resume-index →
  `first_prompt` → random.
- **Hooks** (`cm-core/src/hooks.rs`) — thin forwarders, stdin JSON → socket.
- **Terminal** (`src/terminal/`) — per-emulator backend behind the `Terminal`
  trait. One `capabilities()` query is the whole capability seam: a new backend
  limitation is a new *field* there, not a new trait method.
- **Agent** (`cm-core/src/agent.rs`) — `AgentControl` enum (`Claude`, `Codex`).
  A feature one agent lacks returns `None`/empty from its method (see
  `session_watch_path`, `bg_shells`, `worktree_args`); the UI gates on that.

---

## Crates

A workspace of four shipping members plus `xtask`. Rationale:
`docs/crate-split.md`.

| Crate | Binary | What |
| --- | --- | --- |
| `cm-core` | — | Shared logic + data. No ratatui, no libshpool, so it cross-compiles into the server. |
| `captain-miao` (root) | **`miao`** | The TUI, plus `claude`/`codex`/`hook`/`focus`. No pty pool. |
| `captain-miao-server` | **`miao-server`** | Headless per-host daemon + pty pool. Cross-compiled to Linux and deployed to remotes. |
| `captain-miao-client` | **`miao-client`** | Thin CLI over the *local* pool socket: `list`, `attach`. |
| `xtask` | — | `prepare-servers` (obtain) and `dist` (build variants). |

**Every shipping binary drops the `captain-` prefix; everything else keeps it** —
Cargo packages, npm packages, nix attrs, `~/.config` + `~/.local/state` +
`~/.cache` dirs. `xtask/src/server.rs` carries both `SERVER_PKG` and
`SERVER_BIN` because conflating them builds fine and then can't find the binary.

### Where things live

**`cm-core/src/`** — `state.rs` (`LauncherState`, `SessionStatus`, paths),
`protocol.rs` (length-prefixed JSON wire), `agent.rs` + `agents/{claude,codex}.rs`,
`launcher.rs`, `hooks.rs`, `backend.rs` (`LocalBackend` = the server-core, plus
`OpenSpec`/`LaunchPlan`), `terminal.rs` (opaque `WindowId`/`TabId` + the
launcher's env self-report), `config.rs`, `cli.rs`, `logging.rs`.

**`src/`** (dashboard) — `main.rs`; `app/` = `mod.rs` (state + wiring),
`run.rs` (loop), `draw.rs`, `keys.rs`, `keymap.rs` (configurable bindings),
`picker.rs`, `format.rs`, `hosts.rs`, `bindings.rs`, `logo.rs`, `tests.rs`;
`terminal/` = `mod.rs` (trait + pure policy + backend detection), `kitty.rs`,
`zellij.rs`, `tmux.rs`, `graphics.rs`; `backend.rs` (`Backend` enum + ssh
transport + server provisioning); `server_payload.rs`; `config.rs`; `sleep.rs`.

**`crates/cm-server/src/`** — `main.rs`, `server.rs` (daemon: pool + protocol),
`pty_pool.rs`, `server_pool.rs`.
**`crates/cm-client/src/`** — `main.rs`, `pool.rs`.

---

## State files

`~/.local/state/captain-miao/`, **owner-only** — dirs `0700`, JSON `0600`
(`create_dir_all_private` / `write_json_atomic`), because state records the
user's prompt text and cwds. An older tree is tightened in place on next run.

| Path | What |
| --- | --- |
| `sessions/{pid}.json` | launcher state (atomic temp+rename) |
| `sessions/bell-{pid}.flag` | bell sentinel from `miao focus` |
| `sessions/detach-{pid}.flag` | an attach window reporting its own end |
| `dashboard.pid` | singleton lock |
| `dashboard-window-id` | `<terminal-identity>\|<window-id>` for `focus` |
| `window-bindings.json` | window↔session bindings; foreign-terminal entries carried verbatim |
| `dashboard-overrides.json` | flags for direct-local rows, keep-awake, default agent/host, layout |
| `session-flags.json` | **host-owned** per-session flags, overlaid onto served rows |
| `dashboard-sessions.json` | restartable snapshot; its presence at startup means an unclean exit |
| `recent-cwds.json` | workdir picker history, host-canonical `~` form |
| `directory-marks.json` | per-cwd icon + colour |
| `work-tabs.json` | `(host, cwd)` → work tab, validated lazily |
| `codex-home/` | synthetic `$CODEX_HOME` (symlink farm + a **writable** `config.toml` copy) |
| `long-running-commands/` | learned at-rest background commands |
| `pending-sessions/{name}.json` | pool reservations (server only) |
| `logs/` | `launcher-{pid}.log`, `debug.log`, `keybinds.log` |

Runtime: `$XDG_RUNTIME_DIR/captain-miao/launchers/{pid}.sock` + `-settings.json`.

All of these are safe to delete; each regenerates or resets.

---

## Terminals

`terminal::get()` picks the backend **multiplexer-first**: explicit
`[terminal] backend` > live `ZELLIJ_SESSION_NAME` > parseable `TMUX` > Kitty.
Both multiplexers must beat the ambient Kitty env, because a nested session leaks
`KITTY_WINDOW_ID` into every pane and a Kitty backend would drive the outer
window. zellij is tried before tmux: the env can't say which nesting is inner, so
this is a guess either way, and this one leaves existing users unchanged.

**Every persisted window id is namespaced by the instance that minted it** —
`zellij:<session>` / `tmux:<socket>,<server-pid>` / `kitty:<socket|pid>` — since
those id spaces overlap. A row stamped with another instance is **foreign**:
drawn dimmed, window ops inert (`x` still kills by pid), bindings carried
verbatim through every rewrite so switching backends loses nothing.

The dashboard owns every session↔window binding: it mints a `--launch-id` onto
each local spawn, the launcher echoes it onto `LauncherState.launch_id`, and
`window_id_for_session` resolves through `WindowBindings`. The launcher
self-reports its own window **only when no token is set** (a hand-launched
`miao claude`).

| | Kitty | zellij | tmux |
| --- | --- | --- | --- |
| Control | `kitten @` (password + allowlist) | `zellij action` | `tmux -S <socket>` |
| Min version | — | 0.44 | 3.2 *claimed*, 3.7b tested |
| `move_to_tab` (`t`) | ✅ | ❌ no cross-tab reparent CLI | ✅ `break-pane`/`join-pane`, both `-d` |
| Stacked layout | shared stack tab | floating panes | ❌ neither, so `Space l` is hidden |
| Setup needed | **yes** — see README | none | none |

**Kitty is the only one with a config gate.** `verify_control` runs one
`kitten @ ls` at startup with a 3s timeout (a password kitty doesn't accept
produces *no reply* — it prompts in its own window), and exits 1 with a
diagnosis on failure. `kill_on_drop(true)` is required or the dropped probe
leaves an orphan. The rc allowlist is exactly eight commands.

**zellij gotchas**: plugin panes share the pane-id namespace (filter
`is_plugin == false`, address as `terminal_<n>`); pane commands inherit the
*server's* env, so wrap an argv in `/usr/bin/env PATH=…`; no `--dont-take-focus`,
so a `new-tab` that shouldn't keep focus snaps back to `ZELLIJ_PANE_ID`;
`focus-pane-id` on an already-focused pane exits non-zero and that means success;
tab ids recycle, pane ids don't.

**tmux gotchas**: `TMUX`'s session field is a bare **id**, and a bare `0` is a
*name* to every target lookup — `parse_tmux_env` sigils it to `$N` once, and
nothing may spend the raw field; chained `\;` option-sets target the session's
*current* pane, so `hold` is a second call with an explicit `-t %N`; ids reset
when the server restarts on a socket, hence the pid in the identity;
`capture-pane -S -N` returns the whole visible screen including blank rows, so
trim before `tail_lines`, and no `-J` (the preview clips rather than wraps).

**Session layout** is a spawn-time policy on *new* sessions only — toggling never
moves a running one (zellij can't reparent a pane), so `Space e`/`E` migrates by
restarting. `Capabilities::layout_is_a_choice()` (`window_stacking ||
floating_sessions`) gates the key, its help entry, and the header indicator.

---

## Session status

`SessionStatus::is_busy()` = `Active | Compacting | BackgroundActive` — the one
source of the busy/at-rest split used by the sort, the keep-awake inhibitor and
`active_since`.

- **`BackgroundActive`** ("Task", green, busy) — a short-term `run_in_background`
  shell the agent waits on.
- **`BackgroundServer`** ("Server", yellow, at-rest) — a long-running service the
  agent parked. Not busy: no keep-awake, idle-grouped. *Entering* it arms the
  row's follow-up bell.
- **`ReviewPending`** ("Review", yellow) — every running background shell is an
  `r3 watch`. Unconditionally needs attention, but ranks *below* follow-up rows:
  the agent isn't blocked on a live prompt.
- **`FailedToStart`** ("Failed", red) — the launcher holds the window and blocks
  until dismissed rather than vanishing. Needs attention, not busy. The
  *dashboard* focuses it (`newly_failed_windows`), never the launcher.

**Claude's own session file (`~/.claude/sessions/<pid>.json`) is authoritative**
on the working/idle/background-shell axis (`busy`/`shell`/`idle`); mirror it, no
edge-tracking. Refinement is **demote-only** — hooks own rest→`Active` — and
skips the fine-grained states. An unreadable or unrecognized read maps to `None`
(leave unchanged), never a definite state. `Stop` defers to the file, which is
what keeps a row `Active` while a background *subagent* runs (in-process, so no
`"shell"` and nothing in the process tree).

Which background kind a shell is comes from **classifying its command**, folded
from the live process tree (present-tense truth, so it can't go stale): a seed
heuristic for common dev servers, OR'd with a **learned** store — a command still
running past 1h is recorded in `long-running-commands/` and every future session
treats it as at-rest immediately. Precedence: any transient → busy, else any
long-running → `BackgroundServer`, else → `ReviewPending`.

Codex has no session file and no `run_in_background`, so it never refines into
`BackgroundServer`/`ReviewPending`; its interrupts come from the rollout's
`turn_aborted`.

### Codex specifics

- Hooks come from `$CODEX_HOME/hooks.json` in a **synthetic** home, so the
  command is socket-free (`CAPTAIN_MIAO_SOCK` env instead) and the file stays
  byte-identical across sessions for Codex's content-hashed trust. `config.toml`
  is a writable **copy**, not a symlink, and the mirror pass repairs shadow
  entries — otherwise a split-brain SQLite DB stops Codex starting at all.
- **Trust is pre-seeded** (`seed_hook_trust`) so no prompt ever fires and
  `--dangerously-bypass-hook-trust` is not passed. If Codex changes its hashing,
  the regression test fails loudly first.
- **macOS needs a stat poll**, not FSEvents: Codex appends through a long-held fd
  and FSEvents reports nothing until close. Hand-rolled, not notify's
  `PollWatcher` (which truncates mtime to whole seconds). Parked at Idle only.
- **Session names live in sqlite** (`state_5.sqlite`), read by the per-host
  overlay in `LocalBackend` — one throttled reader per host, gated on both a
  `(db, wal)` mtime change and a 30s floor. Uses bundled SQLite, so no runtime
  `sqlite3` dependency.
- `request_user_input` → `WaitingForDecision` via the `PreToolUse` hook; the
  paired `PostToolUse` resets it.

---

## Key bindings

`?` in the dashboard is the live list; the README has the highlights. Don't
duplicate either here.

Every Normal-mode command is remappable (`src/app/keymap.rs`): `keys.rs`
dispatches a `KeyEvent` → `Chord` → `Command` through a `Keymap`, and
`run_command` is the single place a `Command` becomes a side effect, so default
and remapped keys share one body. Overriding a command **replaces all** its
default keys (an empty list unbinds). After a prefix, a non-matching second key
is **swallowed**, never re-read as its single-key command — so `Space x` can't
fall through to kill. (`g g` is bespoke outside the keymap because it wants the
opposite.) Not remappable: `Ctrl-c`, the `g` prefix, the digit selectors. Only
Normal mode is configurable. Render bindings via `keys_for`/`primary_key` so a
remap shows through without touching `draw.rs`.

---

## Worktrees

`Ctrl-g` in the `O` picker launches into an isolated git worktree.
**captain-miao creates, names and cleans up nothing** — `worktree_args`
contributes `--worktree [name]`, `OpenSpec.worktree` carries it, and the agent
owns the branch, base ref, `.worktreeinclude`, the enforcement that blocks edits
reaching the main checkout, and cleanup. Do not reimplement any of it here.

- **Resume and restart never pass the flag** — the agent re-enters the session's
  own worktree, so asking again would make a *second* one beside it.
- `supports_worktrees()` is derived from `worktree_args`, so the UI gate and the
  argv can't disagree. Codex answers `None` (no such flag as of 0.147).
- The naming intercept must run **before every other picker binding**: a name is
  ordinary letters, and `Ctrl-t` disarms on an agent switch, so a typed `t` used
  to throw the request away silently.
- The key is `Ctrl-g` ("git"), not `Ctrl-w` — the path input binds that to
  readline delete-previous-word, as it does `a/e/b/f/d/u/k`.
- `split_worktree` resolves `<root>/.claude/worktrees/<name>` by **string only**
  (no `git rev-parse`): the dashboard answers this for remote rows and on render
  paths. Marks key on the repo root (shared identity); work tabs stay keyed on
  the real cwd (separate branches) and are titled `<repo>@<worktree>`.
- **Never pass Claude's `--tmux`.** It makes its own tmux session on the same
  server, so the identity matches, the binding isn't classified foreign, and it
  then never resolves.

---

## Remote hosts

`docs/remote-sessions.md` is the authority. **On by default since 0.3.0**; the
`remote` feature is kept so `--no-default-features` still gives a local-only
build. The gate is the runtime const `app::REMOTE_ENABLED`, not `#[cfg]`, and it
closes exactly two doors — `build_backends_from_config` and the `Space h`
handler. Funnel new entry points through those rather than adding a third.

- **The daemon is the single per-host process**: hosts the pty pool *and* serves
  the protocol. Self-daemonizing, singleton via flock, auto-exits when idle. Its
  accept loop **logs and continues** (it *is* the pool) and **rebinds** if its
  socket path vanishes. `loginctl enable-linger` is a documented host
  requirement.
- **`OpenSession` reserves; the first attach creates.** Eagerly creating a
  detached session was the root of both terminal complaints — the agent's TUI
  probed a pty nobody was reading and stayed on legacy key encoding for life.
  Claiming is the `remove_file`, so a race can't produce two creators.
- **Per-session flags are host-owned** for a pooled host (`session-flags.json`
  sidecar, never the launcher's state file — single-writer rule).
- **A binding is retired by an event**, not a poll: every attach is wrapped in a
  shell that reports its own exit. The periodic prune is the 60s backstop for a
  terminal killed outright. Neither may run on a **failed** snapshot — an absent
  snapshot is "we don't know", and feeding its empty live-set drops every
  binding.
- **Pooled localhost** (`[launcher] pooled`) *replaces* `backends[0]`, never sits
  alongside it — both read the same dir and `collect_sessions` doesn't dedup.
- `--force` bypasses only the *busy* guard; the stale-name guard is never
  forceable, since attaching to a dead name mints a bare login shell wearing it.

## Server payloads

`docs/crate-split.md` is the authority. A dashboard can carry `miao-server`
builds and push them on connect.

- **`CM_SERVER_PAYLOAD_MANIFEST` is the whole switch.** Unset (every ordinary
  build) the table is empty. Set *and wrong* is a hard build error, never a quiet
  empty table.
- `build.rs` watches the manifest and each archive, so **never rewrite one with
  identical bytes** — it forces a full LTO relink. `write_manifest` writes only
  on a real change.
- Resolution order: `$CAPTAIN_MIAO_SERVER_<TARGET>` → `$CAPTAIN_MIAO_SERVER_DIR`
  → embedded → cache → **download** (which asks first, and a refusal is the
  default). Embedded deliberately beats the cache: it is the only source needing
  no network and no prior state.
- **The host decides.** The staged binary is run there via `self-check` — not
  `--version`, which a static-musl build passes on an LDAP/SSSD host before
  failing on first attach — and the version is compared *in the script*, before
  the `mv`.
- **PATH is the user's, the cache path is ours.** Never overwrite the former.
- The marker is `<sha256> <target>`; the winning target is **sticky**, or a host
  that settled on musl re-deploys gnu every reconnect forever.
- A failed deploy is rate-limited by a map keyed on digest (not a single slot); a
  *decline* is a decision, not a failure, and carries no cooldown.

---

## Release

`Cargo.toml`'s `[workspace.package] version` is the single version source; every
npm version and pin is stamped from it. **Bump before you tag** — CI's `verify`
job fails the run otherwise — and refresh `Cargo.lock` with it (`build.yml` uses
`--locked`). The `/release` skill has the full procedure.

- Tags must be plain SemVer; `verify` enforces it, because `github.ref_name`
  flows into artifact names and shell scripts.
- **No `run:` body may interpolate a `${{ }}` expression** — values reach the
  shell through `env:`. Expressions are substituted into the script *text* before
  bash parses it.
- npm publish order is load-bearing: platform packages, poll until visible, then
  the launcher that pins them. Every step is idempotent.
- Linux builds pin **ubuntu-22.04** for the glibc 2.35 floor; the server payloads
  are zigbuilt to 2.28.
- `rust-toolchain.toml` and the flake must agree — `ci.yml` compares them and
  fails on drift. Bumping is three steps: `nix flake update`, read the version
  back out of the dev shell, write it into the file.

---

## Committing

- **Run `cargo fmt` (and clippy) before you commit.** CI's first job is `cargo
  fmt --all --check` then `cargo clippy … -D warnings`; unformatted code or a
  lint warning turns it red. There is no local pre-commit gate, so running both
  yourself before you stage is what keeps CI green.
- **Work on `main` directly.** This repo doesn't use feature branches: commit to
  `main`, and don't open PRs for routine work.
- **Before you stage, check for a concurrent committer.** captain-miao's whole
  premise is running many agent sessions at once, so another one may be
  mid-commit in this same tree. The workflow is _stage your files, then commit_,
  so a non-empty index you didn't create means someone else is inside their
  stage→commit window. Run `git diff --cached --name-only` first; if it lists
  anything, back off and re-check on a 5s → 10s → 30s schedule until the index is
  empty, then proceed. If files are _still_ staged after the 30s wait, **stop and
  report to the user** — never commit over another agent's staged work.
- **Commit only what you changed — stage _and_ commit by path.** Stage your files
  explicitly (`git add <path>…`) and commit them by path (`git commit -- <path>…`);
  verify with `git diff --cached --name-only` that the staged set is _only_ yours.
  **Never** `git add -A` / `.` / `-u`, and **never** `git commit -a` / `-am` or a
  bare index-wide `git commit` — each sweeps another session's staged or modified
  files into your commit. If staging or committing hits a blocker, pause and ask.
- **Commit once the work is fully complete.** As long as the work is done with no
  open decision or unanswered question left for the user, go ahead and commit it
  yourself (commit only; pushing still waits for the user). If there's still an
  open question, decision, or unexpected tradeoff, finish the work but leave
  `git add` / `commit` / `push` to the user unless they explicitly ask.
- **Prefer several small, self-contained commits over one large one.** Split the
  work along the seams it already has: a refactor that makes room for a change
  goes in ahead of the change, a fix travels with the test that pins it, a doc
  update rides with the behaviour it describes. The bar for each piece is that it
  **stands on its own** — it builds, its tests pass, and its subject names one
  thing. Where the two pull apart, **self-contained wins**: a single logical
  change that spans several crates stays one commit rather than becoming three
  that don't compile in sequence. Commit each piece as it lands rather than
  slicing the diff apart afterwards, and re-run the concurrent-committer check
  before each one.

### Commit message format

- **Subject: a capitalized, imperative summary** — no trailing period, ≤72 chars.
  One logical change per commit; else split it. A lightweight `doc:` prefix is
  fine for docs-only commits, but don't force a scope onto code changes.
- **Body** (blank line, wrapped ~72) whenever the _why_ isn't obvious from
  subject + diff: explain the motivation or the non-obvious constraint, don't
  narrate the diff. Close with a short verification note when you ran one.
- **No `Co-Authored-By` trailer.** Strip it if the harness appends one.

---

## Dev commands

```sh
cargo run                    # run TUI dashboard
cargo run -- claude .        # launch Claude in current dir with hooks
cargo run -- codex .         # launch Codex in current dir with hooks
cargo run -- focus --window-id $KITTY_WINDOW_ID
                             # focus dashboard AND ring the bell on this window's
                             # session (bind to a terminal key)

cargo build --workspace      # all four packages
cargo build --no-default-features   # strictly local-only dashboard (no remote hosts)
cargo test --workspace       # full suite
cargo watch -x run           # auto-reload the dashboard

# The daemon + pty pool is the separate `miao-server` binary:
cargo run -p captain-miao-server -- daemon ensure|status|stop
cargo run -p captain-miao-client -- list
cargo run -p captain-miao-client -- attach <name> [--force]

# Embedded server payloads. A plain `cargo build` bundles nothing.
cargo xtask dist                   # the release variants, into dist/
cargo xtask dist --list            # the variants, and what each carries
cargo xtask dist --from release    # download published servers instead of building
cargo xtask dist --server x86_64-unknown-linux-gnu=/path/to/miao-server
cargo xtask prepare-servers --out dist/servers   # what release CI runs
miao --version                     # what an already-built binary embeds

# Ignored tests that need a live host / server:
CM_TEST_SSH_TARGET=box cargo test -p captain-miao --features bundle-linux-x86_64 -- \
  --ignored provisions_a_real_host
cargo test -p captain-miao -- --ignored drives_a_real_tmux_server
```
