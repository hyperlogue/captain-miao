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
`sessionId` shared by a root and its subagents. Lifecycle requests and responses
also exclude non-user `threadSource` values: Codex's internal features, such as
catch-up summaries, create temporary root threads on the same connection.
Their prompts, context usage and closure cannot replace or disconnect the
managed conversation. User conversations still support ephemeral mode, and
older threads without source metadata remain supported.

Restart still opens the replacement terminal before closing the old one. Its
launcher waits for the previous owner to finish before resuming the thread.
The waiting replacement already serves launcher control: Kill cancels it
without contacting Codex or affecting the previous owner. Connection setup is
cancellable the same way, before the new TUI starts.
This preserves terminal placement and prevents the previous controller's final
interrupt from reaching newly resumed work. The handoff deadline exceeds the
complete cleanup budget. Cleanup runs outside the dashboard event loop and the
host connection loop, so other sessions continue updating during a restart.
Resume inventory also runs outside the host connection loop; a slow history
read does not delay session updates or cleanup requests.
Repeated commands share the pending restart. After a cleanup failure, an early
retry reuses the waiting replacement. Once too little time remains for a full
cleanup attempt, miao preserves both windows until the replacement reports
`FailedToStart`, then permits a fresh restart. A confirmed removal of the
replacement also permits a fresh attempt; a disconnected host does not.

## Lifecycle and operational differences

The app-server owns execution; the TUI is a client. Closing an attached terminal
through miao requests cleanup of that managed session's work, while detaching
from a pooled terminal keeps its launcher and work running. The host waits for
the launcher to acknowledge cleanup before reporting success. Explicit **Kill**
also removes the session when the app-server is unreachable or reports that the
selected thread is missing. New launchers kill and reap their TUI; the host uses
signals for older launchers, then removes their state, sockets and pasted images.
An unreachable app-server may still be running work: removing its client cannot
guarantee that server-side turns, goals or background commands have ended.

Other cleanup failures, including a slow RPC when the server still answers a
fresh connection check, restore the optimistically hidden row, report the error,
and keep its terminal binding. It closes the terminal only after cleanup or
forced removal succeeds. **Restart** always requires confirmed cleanup before
resuming the saved thread; it never uses the unreachable-server fallback. The launcher
remains available for retry even if the terminal has already exited. A rollback
restores visibility; it does not reactivate a paused goal or restart an
interrupted turn. Teardown addresses only the
selected thread, preserves history, and never stops the shared daemon. Codex
may retain an idle thread in memory after clients disconnect.

Before cleanup, the relay stops forwarding new mutations and waits for already
forwarded lifecycle, turn and goal requests to settle. Responses continue to
flow while it waits. A lost response leaves cleanup uncertain: reconnecting a
different thread cannot resolve it, and a lost creation reply without a thread
ID cannot safely be guessed. Miao reports that uncertainty and retains control.
After a daemon restart, cleanup can also resolve lost replies by verifying that
every affected thread is absent from the daemon's loaded-thread inventory. A
lost creation reply with no identity requires that inventory to be empty.
Metadata and inventory reads do not create uncertainty.

Explicit **Kill** makes one narrow exception: a lost `thread/start` reply for
an internal, ephemeral `system` helper does not block main-thread cleanup.
Miao still pauses the main thread's goal, interrupts its turn, and cleans its
background terminals before removing the launcher. Other cleanup errors still
retain control unless the server is unreachable or the main thread is missing.
**Restart**, user-created ephemeral threads, unknown thread sources, and lost
turn replies retain the strict checks above. This exception requires an updated
host backend and launcher; older launchers retain strict behavior.

Codex submits a recap/title helper's prompt only after receiving its creation
reply, so a lost creation reply leaves no helper generation to interrupt.
Already-running helpers are separate threads: main-thread cleanup does not
cancel them. Codex's remote client makes a best-effort unsubscribe attempt;
unsubscribing does not interrupt an active turn. Miao does not claim that
removing a launcher stops every internal helper on the shared daemon.

State-store failures also retain the launcher, TUI and cleanup controller.
Writes retry on a paced timer and recreate a deleted session-state directory.
Control ownership is persisted before the TUI can create server-owned work.
The launcher restores a deleted or replaced control socket, recreating its
private directory when needed. Relay listeners also recover their socket while
waiting for a TUI connection. Recovery preserves the normal cleanup checks;
an unavailable launcher socket alone does not authorize forced removal.

Restarting the shared Codex daemon disconnects its clients and ends its loaded
runtimes. Saved conversations remain resumable. Miao marks the connection as
disconnected and observes Codex's subsequent reconnect/resume, which restores
metadata and context usage. It does not resend a prompt. Forced shutdown can
interrupt active work. Native sessions do not depend on this daemon.

Disconnected app-server rows also support **x**, **Space e** (restart selected)
and **Space E** (restart all). Restart preserves the saved thread ID and waits
for acknowledged cleanup before resuming it. If cleanup reports an error, miao
checks the daemon's loaded-thread inventory: a thread confirmed absent is
already stopped. For restart, a connection failure or failed inventory read
keeps the row available for retry. Explicit Kill can force removal under the
conditions above. Losing the dashboard's connection to the host itself still
restores the row: only that host can terminate its processes and remove its state.

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

The force-removal policy requires an updated dashboard and host server. Older
clients and servers retain strict cleanup behavior, including during restart.
The host recognizes the specific unreachable-server and missing-thread cleanup
errors emitted by older app-server launchers.

Acknowledged cleanup requires an updated dashboard, host server, and launcher.
An older app-server launcher without control support is refused by the new
backend rather than reported as successfully stopped.
