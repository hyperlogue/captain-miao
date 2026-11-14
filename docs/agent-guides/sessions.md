# Agent, state and host rules

Use for agent integration, hooks, session status, persistence, backend operations
and transports. For the host architecture and protocol rationale, consult
[remote sessions](../remote-sessions.md).

## Ownership and persistence

Session status flows from the agent hook through `miao hook` to the launcher's
Unix socket. The launcher writes session JSON; the dashboard re-reads it on
`notify` events rather than fetching status through IPC.

- State paths live in `crates/cm-core/src/state.rs` under
  `~/.local/state/captain-miao/`. Use `create_dir_all_private` (directories
  `0700`) and `write_json_atomic` (JSON `0600`): state contains prompts and cwds.
  State files regenerate or reset when deleted.
- Daemon control and pool sockets use `state::daemon_dir()` under the same
  state root as `server.pid`, independent of `XDG_RUNTIME_DIR`. `daemon ensure`
  preserves a live lock holder even when unreachable; only explicit stop can
  terminate it. Legacy socket aliases are owned by `daemon_compat`.
- Launcher and clipboard sockets use `$XDG_RUNTIME_DIR/captain-miao/`, falling
  back to `~/.local/state/captain-miao/run/`. macOS reaps `$TMPDIR`, so it cannot
  host long-lived sockets. ssh's `ssh_sock_dir` is the documented exception.
- All per-session flags are host-owned in `session-flags.json`, separate
  from the launcher's state file; preserve the launcher's single-writer rule.
  `cm-core::session_flags::SessionFlagsStore` owns automatic transitions,
  explicit writes and legacy migration for both direct-local and daemon use.
  Its file lock coordinates writers; the dashboard only adopts served flags.

## Agent integration

- `AgentKind` is serialized identity. Its single definition resolver selects
  behavior declared in each `agents/<name>/adapter.rs`; keep agent differences
  there rather than adding per-operation matches. Optional features return
  `None` or empty results. Capability declarations must agree with their argv,
  hook configuration or status observation; change those together.
- Configured `AgentAdapter`s own settings and return opaque `SessionTask`s.
  Load settings at the host operation, inject them when configuring the adapter,
  and capture them for each task. Hook and RPC execution stay private to the
  integration. Shared launcher code must not select an agent's execution mode.
- Tasks publish facts through `SessionPublisher`; only the launcher writes
  session JSON. Stop is cooperative: keep control alive after a retryable
  cleanup failure or persistence error. Long-lived Codex environment snapshots
  retain their lease-based lifetime beyond a launcher exit.
- Hook instances and transcript readers own mutable session state. Constructing
  a hook parser must not start observers. The running hook task activates them;
  do not recreate the hook instance per message or put private cursors in shared
  transcript facts.
- Inject Claude hooks per session via `--settings` and tear them down on exit.
  Keep them out of `~/.claude/settings.json`.
- Keep Claude's `--tmux` out of launch arguments. Its self-created tmux session
  matches the server identity but its binding never resolves.
- Mirror Claude's session file on the working/idle/background-shell axis,
  without edge tracking. Unreadable or unrecognized content maps to `None`
  (unchanged). Refinement is demote-only **except from `Idle`**, which the file
  can promote to `Active`. A queued prompt's `UserPromptSubmit` fires while it
  is queued, not when dequeued; only the file announces that turn via an about
  20ms idle blip followed by busy. Other promotions need outside corroboration:
  `promote_stale_background` requires the process tree to disprove background
  status, not just the file.
- captain-miao leaves worktree creation, naming, branches, base refs, enforcement
  and cleanup to the launched agent. `worktree_args` adds `--worktree [name]`
  only for a new session. Resume and restart omit it; the agent re-enters its
  own worktree.
- A synthetic home may return only entries the **agent** minted because the
  real home had no existing name to mirror (`SynthHome::adopted`). Refuse
  anything in `owned` or `copied`: those can contain our hooks and would enable
  them in unrelated sessions against a nonexistent socket. For agent state
  that is a directory, seed it in the real home (Kimi's `credentials/`) so temp
  files and renames both land there and cannot form a shadow.

## Tower backend

`CAPTAIN_MIAO_TOWER` selects Tower's embedded launcher for this machine's
sessions and each SSH host's Tower daemon for its sessions. `src/tower.rs`
owns the local side and the record-to-row mapping both sides use;
`src/backend/tower_remote.rs` owns the remote side.

- The older launcher, its hooks and `miao-server` stay in place and unchanged.
  Both launchers' sessions are always listed, stopped and flagged; the flag
  only selects the launcher for new sessions. Rows on the other one, and
  remote rows while Tower is selected, get `needs_restart`; the dashboard
  stamps it, like `host`.
- A window does not inherit the dashboard's environment. The launch argv
  carries `--tower`, so the flag need only be set where the dashboard starts.
- Tower calls this binary as `hook <harness> <event>` and `__agent env`.
  `main` answers those before clap parses anything; neither takes an option,
  which is what tells them from `miao hook <event> --agent …`.
- A Tower row keeps the launcher pid as its `SessionKey` and the dashboard's
  launch id as its Tower session id and binding token. Tower's records live
  below `tower/` in the state root; the launcher removes its own on exit.
- Pin and follow-up are Tower tags on those sessions, not `session-flags.json`
  entries. Tower sessions get no automatic flag transitions yet.
- On an SSH host, `RemoteBackend` holds a `TowerLink` beside its
  `miao-server` connection. It runs the host's `tower-cli` over the shared
  ssh master in the login shell: `events --topics agents` for records, `rpc`
  for `session.open` (deferred until the first attach), `session.kill` and
  `session.tags.patch`, and `attach` in the window. Nothing is provisioned.
- `miao-server` still serves resume candidates, directory queries, vitals,
  Codex config and VCS. Its rows are merged with Tower's and marked.
- A remote Tower row is keyed by a hash of its Tower id with the high bit
  set, clear of any real launcher pid; its `pool_session` is the Tower id,
  which is the binding token and decides the attach argv. Only owned, live
  records are listed. Kill confirmation for such a key waits on Tower alone.
- Trying the flag must not harm sessions on the other backend: marking is
  display only, every restart is the user's, and neither launcher removes the
  other's hooks (Grok's live in separate files; each hook routes only through
  its own launcher's environment). With the flag unset the link still
  follows each host's Tower, passing `--no-start`, so its sessions stay
  operable without starting a daemon; new sessions go to `miao-server`.
- `tower-cli` treats a caller with `CLAUDE_CODE_SESSION_ID` or
  `CODEX_THREAD_ID` set as an agent and refuses admin calls; ssh does not
  forward them by default.

## Backend and transport boundaries

- `SessionKey` is opaque above `Backend`. The owning host resolves it to a pid
  at signal time. `LocalBackend` in `crates/cm-core/src/backend.rs` is shared
  with the server; gate operations by host, capability or connection state.
- Keep protocol changes additive with `#[serde(default)]`. v4 is intended as
  the last refusing bump; unknown frames decode to `Unknown` and are ignored.
- Wire paths use host-canonical `~` form through `cm_core::paths`; expand on
  receipt and collapse on return. Keep `$HOME` off the wire. Shell commands
  containing host paths use `paths::shell_quote_host_path`.
- Wrap ssh scripts with `login_shell_safe` (`/bin/sh -c '<script>'`), since the
  account's login shell may be fish. Its inner script may contain neither a
  single quote nor a backslash.

For window creation and attachment, also use the
[dashboard lifecycle rules](dashboard.md#window-identity-and-lifecycle): report
statuses, binding retirement and terminal identity affect session survival.
