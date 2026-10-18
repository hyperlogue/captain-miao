# Session lifecycle verification

## Real Pi

With Pi installed, run:

```sh
nix develop -c cargo test -p cm-core --locked real_pi_loads_extension_and_tracks_conversations -- --ignored --nocapture
```

Set `CM_TEST_PI_BIN` to an absolute executable path if Pi is not on `PATH`.
Verified with Pi 1.1.0. The test uses private temporary Pi configuration and
XDG directories, and a loopback OpenAI-compatible model fixture. It needs no
account, credentials or external inference. The sandbox must allow the local
listener. It removes its files and stops its fixture processes afterward.

The real CLI loads miao's generated extension and performs a read tool call,
drafting, rename, model change, interrupted inference, manual compaction, a new
conversation, cold resume and fork. Captured hook payloads pass through the real
Rust parser and status dispatcher. RPC replies, provider request arrival and
observed hook writes synchronize the test; each phase has a watchdog longer
than the hook delivery deadline.

The regular suite separately covers resume discovery, malformed/large records,
storage environment overrides, event ordering, delivery failures and metadata
refresh after tree navigation. The live test captures hook subprocess output;
it does not exercise the launcher's socket, terminal attach/detach, keyboard
Escape or clipboard paste.

## Pooled sessions

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

## Real Codex disconnect recovery

With `codex` installed and Node available (provided by the development shell), run:

```sh
nix develop -c cargo test -p cm-core --locked real_codex_serializes_recovery_after_disconnected_startup -- --ignored --nocapture
```

Verified against Codex 0.163.0. The test starts an isolated app-server with a
private home and XDG roots. It uses a paused goal to persist its fixture thread
without inference, and a local MCP initialize gate to hold resume or creation
in progress. No account, credentials, external endpoint, or existing session
is used. The sandbox must allow local sockets and child processes.

After disconnecting the requester, it verifies that inventory can still be empty
while startup is unfinished. Recovery of a known thread must wait for the real
serialized read; recovery of an unidentified creation must refuse that empty
inventory. It then replaces the daemon on the same socket, verifies both Kill
and Restart cleanup despite the lost creation reply, and resumes the saved
conversation. This exercises process replacement with the installed binary;
it does not test every pair of Codex upgrade versions.
The regular suite covers delayed upstream receipts, auxiliary
connections, daemon identity and exit, cleanup failures, and asynchronous
background-terminal removal.

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
resume after restarting the isolated server. It exercises effective daemon
exclusions, automatic secret-name exclusions, explicit overrides, and a thread
with `inherit = "none"` and an allowlist. Changing `.envrc` to fail after capture
verifies that commands use the saved environment. Unchanged launcher credentials
and excluded direnv values must not appear in snapshot files; an unprinted dummy
secret must not appear in Codex's files. Restarting the daemon makes the inactive
launcher's snapshots collectible.

The regular suite separately checks approval failures, private file modes,
quoting, removal of variables, child shells, and preservation of unrelated relay
configuration. Collection tests retain active leases, loaded descendants, and
uncertain requests, skip inventory or ancestry RPCs when retention is already
known, and refuse cleanup when inventory is unavailable. Broker tests reject
enabled configuration before persistence and stop Bash/zsh commands before raw
credentials can be injected, with or without an existing broker replacement.
They exercise the runtime marker using dummy values, not a live credential
broker. Other regressions keep unrelated Codex overrides out of snapshots and
start the client TUI without evaluating an unapproved `.envrc`. The zsh
startup tests run when zsh is on PATH; Bash coverage is unconditional. An
additional isolated test verifies capture from an already activated project:

```sh
nix develop -c cargo test -p cm-core --locked real_direnv_capture_recovers_changes_from_an_already_activated_project -- --ignored --nocapture
```
