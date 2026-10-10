# Session lifecycle verification

Run the process-level lifecycle scenario with:

```sh
nix develop --command cargo test -p captain-miao-server --test session_lifecycle --locked
```

It is also part of `cargo test --workspace --locked`, so the existing Linux and
macOS CI jobs run it on every change. The default `pty-pool` feature is required.
The scenario starts a private miao-server daemon, two app-server launchers and
one native Codex launcher. A temporary Codex executable and scripted WebSocket
peer replace the external agent; no login, model request, terminal emulator,
SSH target or existing session is used. All configuration, state and runtime
sockets belong to an atomically reserved temporary directory.

The scenario verifies:

- Session identity, titles and prompts survive internal-helper activity and
  host connection loss.
- A delayed cleanup does not block another active session's updates or a
  fragmented host request. Goal pause precedes turn interruption and background
  terminal cleanup.
- Rejected cleanup retains the launcher and publishes a retryable failure.
- App-server loss is observed, reconnection restores the original conversations,
  and no prompt is replayed. The native session stays available.
- Strict cleanup followed by replacement preserves a saved conversation.
- Explicit Kill reports missing-thread and unreachable-server removals
  separately and removes the launcher state and sockets.
- A host daemon restart rediscovers a surviving direct launcher. Pool upgrades
  continue to use the existing stop-and-resume workflow.

RPC arrivals and observed state changes drive each step. Deadlines bound a
broken test; they are not used to guess when startup or cleanup has finished.
The harness owns and tears down its fixture processes on failure as well.

The scripted peer tests miao's protocol handling, not compatibility with a
particular installed Codex release. Actual PTY attach/detach, clipboard input and
terminal restoration are covered separately by `pool_modes`; dashboard tests
cover optimistic rollback, diagnostic rendering, and remapped actions. Live
SSH provisioning still requires an explicitly designated disposable host.

## Real Codex and direnv

With `codex` and `direnv` installed on the execution host, run:

```sh
nix develop -c cargo test -p cm-core --locked real_codex_loads_direnv_for_agent_and_manual_commands -- --ignored --nocapture
```

This test runs the actual relay, an isolated Codex app-server and synthetic,
explicitly approved direnv projects. All homes and XDG directories are private
temporary fixtures. A loopback Responses server supplies deterministic tool
calls; it uses no account, credentials or external model API. The sandbox must
allow this local listener and Codex's subprocess sandbox.

It checks bare project binaries and pipelines through both agent and manual
commands, separate projects, subdirectories, non-login commands, and cold
resume after restarting the isolated server. Changing `.envrc` to fail after
capture verifies that commands use the saved environment. An unprinted dummy
secret must not appear in Codex's files. The regular suite separately checks
approval failures, private file modes, quoting, removal of variables, child
shells, and preservation of unrelated relay configuration. The zsh startup test
runs when zsh is on PATH; Bash coverage is unconditional.
