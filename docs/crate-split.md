# Crate split: `cm-core` / `captain-miao` / `captain-miao-server` / `captain-miao-client`

## Why

captain-miao was one crate that a remote host also had to build in full —
ratatui, the whole TUI, and the launcher — just to run the daemon + pty pool.
The goal of remote sessions is that a host's server be a **small, standalone
artifact** that cross-compiles cleanly to other arches without dragging the TUI
along (see `docs/remote-sessions.md`). That's the motivation for the split; a
cleaner client/server layering and faster incremental builds come for free.

## Shape

A Cargo workspace of four packages — three at the time of the split, plus
`captain-miao-client`, added after (root stays the `captain-miao` package, so
`cargo install --path .` and the release binary path are unchanged):

- **`cm-core`** (`crates/cm-core/`) — lib. The logic + data the binaries share:
  `state`, `protocol`, `agent`/`agents`, `launcher`, `hooks`,
  `backend::LocalBackend` (the server-core) + the `OpenSpec`/`LaunchPlan` seam
  types, the opaque `terminal` ids + `current_window`, the `[launcher]`/`[debug]`
  `config`, and shared `cli`/`logging` helpers. **No ratatui, no libshpool** — so
  it cross-compiles as part of the server.
- **`captain-miao`** (root, `src/`) — bin. The ratatui dashboard (TUI client) +
  the `claude`/`codex`/`hook` entrypoints (so a local launch needs only this one
  binary) + `focus`. Depends on `cm-core`. **No pty pool.**
- **`captain-miao-server`** (`crates/cm-server/`) — bin. The headless per-host
  daemon + pty pool a remote dashboard reaches over ssh. `daemon`/`attach`/
  `pty-daemon` + the pooled `claude`/`codex`/`hook`. Depends on `cm-core`.
  It **hosts** the pool (feature `pty-pool`, default on).
- **`captain-miao-client`** (`crates/cm-client/`) — bin. Added after the original
  three-way split: a thin user-facing CLI over the *local* pool socket, `list`
  and `attach`. The only other crate that links libshpool (for the in-process
  attach), but it hosts no daemon/pool — a pure client, so it stays separate from
  both the dashboard (which links no libshpool at all) and the server (which owns
  the daemon). `--no-default-features` drops libshpool → list-only, so it still
  builds on macOS.

## Boundary decisions

The single-crate code had a few UI-vs-core couplings that the split forced out —
each is a small, deliberate cut:

- **`state.rs` was not ratatui-free.** `SessionStatus::color()` (a `ratatui::Color`)
  was presentation living on the truth type. Moved to the dashboard as
  `app::format::status_color()`; the enum + `is_busy()`/`needs_attention()` stay
  in core.
- **`config.rs` split by who-reads-it.** The launcher/daemon read only
  `[launcher]` + `[debug]`; everything else (colors, ui, thresholds, polling,
  keybinds — the ratatui `Color` parsing) is the dashboard's. Core owns those two
  sections + the loader; the dashboard's `Config` reuses core's structs and adds
  the presentation ones. Both parse the same `config.toml` — serde ignores the
  keys each side doesn't know.
- **`terminal` split by data-vs-backend.** The opaque `WindowId`/`TabId` (they're
  serialized into `LauncherState` and ride the wire) and the launcher's
  `current_window()` self-report live in `cm-core`; the `Terminal` trait, the
  Kitty `kitten @` backend, and the snapshot policy stay in the dashboard, which
  re-exports the id types so `crate::terminal::…` paths are unchanged.
- **`backend.rs` split Local-vs-Remote.** `LocalBackend` (+ `OpenSpec`/`LaunchPlan`
  + the fs helpers) is core; the `Backend` enum, `RemoteBackend`, the ssh
  transport, and remote-binary provisioning are dashboard-only.
- **Bundled SQLite stays in core.** `agents/codex.rs` keeps the
  `read_thread_titles` SQLite read (the per-host title overlay in
  `backend::LocalBackend`) — fragmenting the Codex backend across crates to dodge
  a ~1 MB static amalgamation would break the `AgentControl` abstraction, and
  portable-C SQLite cross-compiles fine (unlike libshpool's platform linking).

Mechanically, each binary re-exports the core modules it uses at its crate root
(`pub use cm_core::{state, protocol, …}`), so the thousands of existing
`crate::state::…` paths resolve unchanged instead of being rewritten.

## Intermediate state (important)

The dashboard no longer builds libshpool, so it **can't upload itself** as a
functional remote server. Two consequences until the embed work lands:

- **Auto-provisioning is gone.** The upload path (`Provision::Upload` +
  `upload_binary`) was removed rather than left unreachable; what survives in
  `backend.rs` is a **read-only** probe that picks between a
  `captain-miao-server` on the remote's `PATH` and one at the cache path. Deploy
  it **manually** for now (`cargo build --release -p captain-miao-server` on the
  target and put it on `PATH`, or `redeploy.sh <host>` to push one to the cache
  path). Recover the upload code from git history if the embed work wants it.
- The dashboard's remote-exe name, probe, and attach argv were updated from
  `captain-miao` to **`captain-miao-server`** — so the remote path looks for the
  right binary the moment one is deployed.

Release CI (`build.yml`) still ships **only the dashboard**; pulling the server
into the cross-build matrix would drag libshpool cross-compilation into release
CI, which is the deferred spike below. CI (`ci.yml`) builds/tests/clippies the
**whole workspace** natively on ubuntu + macOS.

## Deferred: embed + auto-deploy the server

The next phase makes provisioning zero-touch again: build `captain-miao-server`
for the target arches, embed the blobs in the dashboard (or fetch on connect),
and push the right one over ssh on connect — reinstating an upload path against
the embedded blob instead of self.

The load-bearing unknown is **cross-compiling the libshpool server to a portable
binary**. Decision from discussion: target an **old glibc floor** (2.17 /
manylinux2014, or 2.28) rather than musl — musl's static NSS (no LDAP/SSSD) and
stubbed utmp are real downsides for mainstream server fleets. Prove it first via
a native build in an old-glibc container, with `cargo-zigbuild
--target x86_64-unknown-linux-gnu.2.17` as the cross-from-macOS alternative;
keep musl only as an Alpine/unknown-distro fallback. Build the per-arch server
blobs in a separate CI step and `include_bytes!` whatever's present, so the dev
`cargo build` never needs the cross toolchains.
