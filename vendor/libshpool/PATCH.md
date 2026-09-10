# Local libshpool extension

Source: the crates.io `libshpool` 0.11.0 package, under Apache-2.0 (`LICENSE`).
The manifest is the registry's normalized manifest; upstream source and its
copyright notices are retained.
Two upstream TODO author labels are omitted from comments.

This copy exports `SessionSpool` and adds the optional `Hooks::session_spool`
factory. The daemon passes the returned spool to its existing PTY output
thread. A caller returning `None` gets the upstream restore behavior.

captain-miao uses the factory to retain terminal modes while discarding screen
contents. The output thread owns the parser, processes detached output, and
sends restoration before subsequent live output. No new socket, sidecar, pool
wire frame, or second PTY relay is involved.

Patch points: `src/lib.rs`, `src/hooks.rs`, `src/daemon/server.rs`, and
`src/daemon/shell.rs`. Keep the extension confined to those points when
updating the pinned source. The caller and regression tests live in
`crates/cm-server`; the terminal mode parser lives in `crates/cm-core`.
