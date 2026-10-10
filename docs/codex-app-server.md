# Codex execution modes

Each execution host chooses how miao runs Codex. Open **Space h**, select the
host, press **e**, and edit **Codex connection** and **Codex endpoint**. The
permanent **localhost** entry configures this machine, including when its
sessions use the local pool.
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

The app-server resume picker includes unarchived interactive conversations from
both the Codex CLI and editor. Codex's default source filter excludes internal
subagent threads and noninteractive runs. The configured
`launcher.resume_list_limit` bounds the combined picker across agents, with the
most recently updated sessions first.

`native.rs` owns SQLite, rollouts and the managed hook profile. `app_server/`
owns WebSocket transport, protocol observation and thread control. `tui.rs`
contains only shared terminal behavior and home resolution. Both adapters use
the existing Codex capability table: fork, approval reporting and context usage
are supported; agent-created worktrees remain unavailable.

The launcher owns a private Unix WebSocket relay between the Codex TUI and the
configured app-server. It adds the project environment to user thread
start/resume/fork requests, and forwards replies, approvals and unknown
protocol extensions without interpreting their control behavior. Codex retains
its composer, slash commands, approval UI, queued prompts and reconnect logic.
The relay observes metadata and lifecycle events; it never answers an approval
or retries a user submission. Separate inventory/control connections do not
subscribe to threads.

Codex's in-session resume picker (`/resume`) opens a second connection to that
same `--remote` endpoint while the session TUI stays attached. The relay
proxies the extra connection on its own upstream socket. Only the session
connection is observed, so picker traffic cannot retarget the row and closing
the picker does not disconnect the session. Cleanup still fences new requests
on every connection.

The launcher remains the sole writer of its session state. The dashboard and
remote session subscription consume that state as before. App-server sessions
never enter the native title overlay. Thread identity uses `thread.id`, not the
`sessionId` shared by a root and its subagents. Lifecycle requests and responses
also exclude non-user `threadSource` values: Codex's internal features, such as
catch-up summaries, create temporary root threads on the same connection.
Their prompts, context usage and closure cannot replace or disconnect the
managed conversation. User conversations still support ephemeral mode, and
older threads without source metadata remain supported.

`/btw` and `/side` create ephemeral forks classified as user threads. Miao keeps
tracking the managed parent conversation when it observes such a fork, so a
finished side question cannot leave the main session stuck at Idle. Side-thread
metadata and errors do not replace the parent's row or cleanup target. Starting
a launcher directly with an ephemeral conversation or fork remains supported.

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

The **Detail** panel shows the full session ID; **y** copies it, and the panel's
bottom border shows the current copy binding if you remap it. App-server rows
also show their connection state. When disconnected, the panel offers the
available restart/removal keys and explains whether the Codex terminal is still
running. A host connection failure takes precedence over cached agent health.
An idle session's old update time alone does not imply a broken connection.

During cleanup, an updated launcher reports that it is waiting for the agent's
confirmation. If cleanup fails, the panel keeps the failure and a retry hint
even when subsequent agent events update the row. Narrow layouts prioritize
these diagnostics over model and context details. This progress information
requires an updated launcher; older launchers still show their last error.

The app-server owns execution; the TUI is a client. Closing an attached terminal
through miao requests cleanup of that managed session's work, while detaching
from a pooled terminal keeps its launcher and work running. The host waits for
the launcher to acknowledge cleanup before reporting success. Explicit **Kill**
also removes the session when the app-server is unreachable or reports that the
selected thread is missing. New launchers kill and reap their TUI; the host uses
signals for older launchers, then removes their state, sockets and pasted images.
An unreachable app-server may still be running work: removing its client cannot
guarantee that server-side turns, goals or background commands have ended.
With an updated host backend, the result explicitly says **Session removed**
and identifies the unreachable app-server or missing thread. An unreachable
server also produces a warning that server-side work may still be running.
These results remain available in the message log (**Space m**). Older host
backends cannot report the distinction and retain their generic success reply.

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

An updated launcher can also clean up a thread before its first user message.
Codex reports that such a thread is not yet materialized when miao reads its
turn history; miao treats that specific response as an empty history and still
requires goal and background-terminal cleanup to succeed. Already-running
launchers keep their existing cleanup behavior until replaced.

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

## Project environments

App-server sessions automatically capture an approved `direnv` environment on
the execution host. Bash and zsh commands can use project binaries by name,
including plain `! cargo test`, pipelines and commands in subdirectories.
The TUI's `/resume` picker resolves the selected thread's project before loading
its environment. Projects without an `.envrc` retain Codex's normal behavior.
An unapproved `.envrc` or failed capture reports an error; miao never approves
the file automatically. Parent-directory `.envrc` files are supported too.

The client TUI starts without evaluating direnv; capture is deferred until a
thread request, once per project per launcher. Editing `.envrc` does not change
a running thread's environment. Shell snapshots and login
initialization for agent commands are disabled for these sessions, and managed
startup files restore the capture for Bash and zsh. Shell functions and aliases
are not captured; explicitly choosing another execution shell bypasses these
startup files. Miao reads the daemon's effective project configuration and merges
thread overrides before applying its environment policy to direnv's changes.
This preserves `inherit`, explicit exclusions, include-only filters, automatic
secret-name exclusions when enabled, and explicit `set` values. Both legacy
arrays and the newer `filters` table are supported; matches are case-insensitive.
An explicit Codex `set` value wins over direnv and exclusions, subject to the
include-only filter, following [Codex's normal precedence](https://learn.chatgpt.com/docs/config-file/config-advanced#shell-environment-policy). `inherit = "none"`
therefore prevents direnv additions unless explicitly supplied through `set`.

Only added, changed, and removed variables are captured, comparing with direnv's
unloaded environment even when the launcher starts inside an activated project.
Unchanged launcher variables and direnv's internal metadata are omitted. The
daemon still supplies its ordinary filtered environment. Startup controls
(`BASH_ENV`, `ZDOTDIR`, and `SHLVL`) are the small policy exception needed to load
the private hook. Bash login profiles are filtered again before applying the
changes. Runtime Codex identity and managed proxy values remain authoritative.
Unrecognized policy settings and `experimental_use_profile = true` fail with an
error instead of silently dropping restrictions. Credential brokering is not
supported: miao rejects enabled `features.network_proxy.credential_broker`
settings before writing a snapshot. The startup hook also stops the shell if
Codex activates the broker later, before applying any captured values. Threads
already loaded with older hooks must unload before receiving this protection.
MCP servers and the shared daemon itself still use their own environments.

Codex ignores configuration overrides when resuming an **already-loaded
thread**, so it keeps its original environment. This also means a thread started
before this integration needs to be unloaded before direnv can take effect.
Close its clients and wait for Codex to unload it, or restart the Codex daemon
when its sessions are idle, then resume. A new launcher captures fresh values
for new or unloaded threads. Reconnection after a daemon restart reapplies the
existing launcher's capture without replaying a prompt.

Resume requests still prepare environment overrides as a fallback if the thread
has unloaded. Codex has no atomic loaded-only resume; checking its status first
could race with unloading and lose the environment. Consequently a new launcher
still requires a valid `.envrc` even when resuming a loaded thread. Identical
hooks within a launcher are reused without rewriting their files.

Captured values are stored in immutable files under
`~/.local/state/captain-miao/codex-environments/` (honoring `XDG_STATE_HOME`),
with directories `0700` and files `0600`. Only startup-file paths are passed in
Codex configuration: putting values directly in its environment overrides can
copy secrets into Codex's trace logs. Snapshots contain the allowed direnv delta
and explicit Codex overrides only for names in that delta. Unrelated `set`
values remain in Codex's native environment and are not copied into snapshots;
ordinary shell startup behavior applies to those values.

Each launcher holds a filesystem lease and records the threads using its files.
Cleanup runs when a relay starts, every minute while it runs, and when its
launcher exits. It removes an inactive launcher's snapshots only after Codex
confirms its threads and their descendants are unloaded. A lost creation reply
retains the files until its owning daemon process has exited: an empty inventory
alone cannot rule out an in-flight creation. The daemon PID comes from kernel
credentials on the actual relay socket. Missing credentials or a reused PID
conservatively delay cleanup. Unreachable daemons,
unsupported inventory, and incomplete ancestry information defer collection;
failed inventory is never treated as proof that a file is unused. Crashed
launchers release their leases automatically, allowing a later relay to clean
up. If the last launcher exits before Codex unloads its thread, cleanup waits
for a subsequent miao app-server session.

Snapshots from the older implementation have no ownership records and are
retained. The entire directory can be removed after stopping all Codex daemons
that used it; the next launch recreates the needed snapshots. As with other
local state, treat these files as private when backing up or sharing diagnostics.
Native mode keeps its existing process-per-session environment behavior.

The integration is exercised against Codex 0.162.0 by the optional
[live environment test](testing-sessions.md#real-codex-and-direnv).

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

The [session lifecycle tests](testing-sessions.md) exercise connection loss,
cleanup failures and recovery with isolated processes and a scripted Codex peer.
