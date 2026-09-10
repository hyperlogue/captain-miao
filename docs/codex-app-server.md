# Codex execution modes

Each execution host chooses how miao runs Codex. Open **Space h**, select the
host, press **e**, and edit **Codex** and **Endpoint**. The permanent **localhost**
entry configures this machine, including when its sessions use the local pool.
It cannot be removed or disconnected. Remote settings require a connected
`miao-server` that supports the setting; an older server shows an explicit
unavailable message.

The editor updates only `[codex]` in that host's
`~/.config/captain-miao/config.toml` (or its XDG equivalent), preserving comments
and other settings. Changes apply to new launches and explicit restarts. They
do not restart the Codex daemon or migrate running sessions automatically.

```toml
[codex]
mode = "native"          # "native" (default) or "app-server"
endpoint = "unix://"     # Codex's default control socket on this host
```

An explicit endpoint can use `unix:///path/to/socket` or `unix://~/codex.sock`.
The default resolves under the execution host's Codex home, honoring
`CODEX_HOME`. TCP endpoints are not supported by this adapter.

An invalid configuration or unavailable app-server fails the launch visibly.
There is no automatic switch to native. For migration, change the setting and
restart idle Codex sessions with the existing restart commands. Their saved
thread IDs are preserved. A running session keeps the mode selected when its
launcher started, so migration can proceed incrementally.

## Ownership and interface

`crates/cm-core/src/agents/codex/` contains two separate adapters:

| Operation | Native | App-server |
| --- | --- | --- |
| Launch, prompt, resume, fork | Codex CLI | Codex TUI attached with `--remote` |
| Session identity | Hooks; saved metadata seeds idle resumes | Immediate start/resume/fork response |
| Status and approvals | Hooks and rollout events | Thread, turn, item and server-request events |
| Model and context usage | Rollout reader | Lifecycle response, settings events and token-usage replay |
| Title | Host SQLite overlay | Thread response and name notifications |
| Resume inventory | Native persisted files | Paginated `thread/list` |
| Goal continuation | Native goal-store read | Goal notifications |
| Stop | End the Codex process | Pause an active goal, interrupt the current turn, clean background terminals |

`native.rs` owns SQLite, rollouts and the managed hook profile. `app_server/`
owns WebSocket transport, protocol observation and thread control. `tui.rs`
contains only shared terminal behavior and home resolution. Both adapters use
the existing Codex capability table: fork, approval reporting and context usage
are supported; agent-created worktrees remain unavailable.

The launcher owns a private Unix WebSocket relay between the Codex TUI and the
configured app-server. It forwards requests, replies, approvals and unknown
protocol extensions without interpreting their control behavior. Codex retains
its composer, slash commands, approval UI, queued prompts and reconnect logic.
The relay observes metadata and lifecycle events; it never answers an approval
or retries a user submission. Separate inventory/control connections do not
subscribe to threads.

The launcher remains the sole writer of its session state. The dashboard and
remote session subscription consume that state as before. App-server sessions
never enter the native title overlay. Thread identity uses `thread.id`, not the
`sessionId` shared by a root and its subagents; child events cannot replace the
root row.

Restart still opens the replacement terminal before closing the old one. Its
launcher waits for the previous owner to finish before resuming the thread.
This preserves terminal placement and prevents the previous controller's final
interrupt from reaching newly resumed work.

## Lifecycle and operational differences

The app-server owns execution; the TUI is a client. Closing an attached terminal
through miao requests cleanup of that managed session's work, while detaching
from a pooled terminal keeps its launcher and work running. The host waits for
the launcher to acknowledge cleanup before reporting success. Miao restores an
optimistically hidden row on failure, reports the error, and keeps its terminal
binding. It closes the terminal only after successful cleanup. The launcher
remains available for retry even if the terminal has already exited. A rollback
restores visibility; it does not reactivate a paused goal or restart an
interrupted turn. Teardown addresses only the
selected thread, preserves history, and never stops the shared daemon. Codex
may retain an idle thread in memory after clients disconnect.

Restarting the shared Codex daemon disconnects its clients and ends its loaded
runtimes. Saved conversations remain resumable. Miao marks the connection as
disconnected and observes Codex's subsequent reconnect/resume, which restores
metadata and context usage. It does not resend a prompt. Forced shutdown can
interrupt active work. Native sessions do not depend on this daemon.

Execution environment is an inherent difference: server-side tools and services
run in the daemon's environment. A launcher-side `direnv` environment is not
automatically inherited by an already-running server. Configure the daemon and
Codex's project/shell environment settings accordingly. Miao does not copy the
launcher's environment into persisted thread configuration. Native mode retains
its process-per-session environment behavior.

The adapter was developed against Codex 0.153.4's Unix WebSocket protocol. Some
control methods, including background-terminal cleanup, require the experimental
API capability. Protocol changes are handled by Codex's TUI wherever possible;
the monitor ignores fields and events it does not understand. See the
[upstream app-server documentation](https://developers.openai.com/codex/app-server).

Miao still reads its own configuration and session-state files. This adapter
parses protocol messages rather than Codex-owned rollout files, hook payloads,
or SQLite databases. Codex's TUI and daemon continue to manage their own files.

Acknowledged cleanup requires an updated dashboard, host server, and launcher.
An older app-server launcher without control support is refused by the new
backend rather than reported as successfully stopped.
