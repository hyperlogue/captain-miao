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
- Runtime sockets belong under `$XDG_RUNTIME_DIR/captain-miao/`, falling back
  to `~/.local/state/captain-miao/run/`. macOS reaps `$TMPDIR`, so it cannot host
  long-lived runtime sockets. ssh's `ssh_sock_dir` is the documented exception.
- Pooled per-session flags are host-owned in `session-flags.json`, separate
  from the launcher's state file; preserve the single-writer rule.

## Agent integration

- `AgentControl` in `crates/cm-core/src/agent.rs` owns CLI differences.
  Unsupported features return `None` or empty results; structural UI limits
  belong in its declared `capabilities()` table. Each capability is checked
  against its owning argv or hook configuration; change those together.
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
