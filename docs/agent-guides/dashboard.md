# Dashboard and terminal rules

Use for dashboard state, keybindings, capability rendering, terminal control,
previews, and window attachment. These constraints cross module boundaries;
local rationale remains beside the implementation.

## Mutations and cursor anchoring

- Mutate first, then call `App::mark_dirty` with an explicit `Cursor`.
  Invalidating early caches stale sort order under the new version and panics
  on reload. Use `FollowSession` normally, `Follow(key)` for a session named
  before mutation, `HoldIndex` for rendering-only changes, and `Top` for search.
- Change bindings through `record_window_binding`, `retire_window_binding`,
  `prune_detached_sessions` or `apply_detach_reports`. Direct writes to
  `window_bindings` miss both invalidation and cursor anchoring.

## Capabilities and presentation

- `Terminal::capabilities()` in `src/terminal/` is the capability seam. Add a
  field for a new limitation, rather than another trait query. Ghostty's
  `capture: false` is the worked example.
- Branch on host, capability or connection state. Locality is not a capability:
  `capabilities() -> {pooled, shell}` also governs pooled-localhost sessions.
- Hide unsupported commands and render bindings through `keys_for` /
  `primary_key` so remaps appear. Gate recurring reads at the call site too:
  the preview loop interprets failed `capture_text` as a stale binding, so
  `capture: false` must prevent the fetch.
- Gate absent data on `AgentControl::capabilities()`. An unsupported Context
  cell reads `n/a`; empty means a value has yet to arrive. Attention summaries
  must identify an agent that cannot report approval prompts instead of implying
  every session was checked.

## Window identity and lifecycle

- Namespace persisted window IDs by the instance that minted them:
  `zellij:<session>`, `tmux:<socket>,<server-pid>`, `kitty:<socket|pid>`.
  Foreign rows are dimmed, window operations inert, and their bindings carried
  verbatim through rewrites. Ghostty surface IDs are UUIDs; `ghostty_identity`
  explains why it needs no instance namespace.
- Retire a binding before closing its window programmatically. Otherwise the
  detach report looks like a user close and `[remote] on_window_close` can end
  the session. Only status `129` is a user close: ssh's `255` means a dropped
  link and `0` an in-session detach, both preserving the session. Reports drained
  at startup never end sessions; a quitting terminal SIGHUPs all attach windows.
- Keep `trap 'r 129' HUP` separate from the attach wrapper's `EXIT` trap. A
  terminal closing the pty master can signal only the session leader, leaving
  ssh to exit `255`; inheriting that status misclassifies a deliberate close.
- Use `ATTACH_REPORT_SCRIPT` to hold an exited attach command. Kitty's `--hold`
  starts a login shell on command exit, leaving a live local shell wearing the
  remote session's title after a dropped connection.

## Terminal-specific constraints

- Keep zellij's `list-panes` off focus, spawn, restart and recurring paths:
  it costs about 20ms per pane server-side. tmux's equivalent is cheap.
- Update the README's remote-control allowlist whenever adding a `kitten @`
  command; the recommended config denies commands outside that list.
- Launchers use only `current_window` from their environment. Terminal window
  snapshots and tab lookup belong to presentation; launchers can be headless.
