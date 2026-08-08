# Crate split: `cm-core` / `captain-miao` / `captain-miao-server` / `captain-miao-client`

## Why

captain-miao was one crate that a remote host also had to build in full —
ratatui, the whole TUI, and the launcher — just to run the daemon + pty pool.
The goal of remote sessions is that a host's server be a **small, standalone
artifact** that cross-compiles cleanly to other arches without dragging the TUI
along (see `docs/remote-sessions.md`). That's the motivation for the split; a
cleaner client/server layering and faster incremental builds come for free.

## Shape

A Cargo workspace of four shipping packages — three at the time of the split,
plus `captain-miao-client`, added after — with `cm-payload` and `xtask` alongside
as build support (root stays the `captain-miao` package, so `cargo install
--path .` and the release binary path are unchanged):

- **`cm-core`** (`crates/cm-core/`) — lib. The logic + data the binaries share:
  `state`, `protocol`, `agent`/`agents`, `launcher`, `hooks`,
  `backend::LocalBackend` (the server-core) + the `OpenSpec`/`LaunchPlan` seam
  types, the opaque `terminal` ids + `current_window`, the `[launcher]`/`[debug]`
  `config`, and shared `cli`/`logging` helpers. **No ratatui, no libshpool** — so
  it cross-compiles as part of the server.
- **`captain-miao`** (root, `src/`) — bin, installed as **`miao`** (an explicit
  `[[bin]]` target; only the executable is short, the package keeps the project's
  name). The ratatui dashboard (TUI client) +
  the `claude`/`codex`/`hook` entrypoints (so a local launch needs only this one
  binary) + `focus`. Depends on `cm-core`. **No pty pool.**
- **`captain-miao-server`** (`crates/cm-server/`) — bin `miao-server`. The headless per-host
  daemon + pty pool a remote dashboard reaches over ssh. `daemon`/`attach`/
  `pty-daemon` + the pooled `claude`/`codex`/`hook`. Depends on `cm-core`.
  It **hosts** the pool (feature `pty-pool`, default on).
- **`cm-payload`** (`crates/cm-payload/`) — lib, `publish = false`. Two halves,
  split by who links which. `format` has no dependencies and is shared: the
  dashboard reads the payload slot with it, `xtask` writes the slot with it, and
  one module is what stops the writer and the reader drifting apart. The `build`
  feature adds the cross-compile, the compression and the injector, and only
  `xtask` turns it on — so the dashboard's dependency here costs it nothing.
- **`captain-miao-client`** (`crates/cm-client/`) — bin `miao-client`. Added after the original
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

Release CI (`build.yml`) still ships **only the dashboard**; pulling the server
into the release matrix is a packaging decision that waits on the `remote`
feature being on by default. CI (`ci.yml`) builds/tests/clippies the **whole
workspace** natively on ubuntu + macOS.

## Embed + auto-deploy the server — implemented

The dashboard can't upload *itself* as a remote server any more (it no longer
links libshpool), so it carries a real `miao-server` instead and pushes
that. Provisioning is zero-touch again: connect to a bare host and it gets a
server, with no manual deploy step.

**The dashboard is no longer one artifact.** The `bundle` cargo feature (which
implies `remote`) reserves room for servers and compiles the reader; without it
there is no slot, no reader, and a binary byte-for-byte what it would be if none
of this existed. There is one feature rather than one per architecture because
*which* servers a binary carries is decided when they are written in, not when it
is compiled — so a single dashboard build serves every combination.

**Payloads are written in after linking, and the point of that is decoupling.**
Compiling the payload in would make it an *input* to the dashboard's build, which
welds two questions together that have no business being one: where do servers
come from, and how is the dashboard built. Injecting keeps them apart, and the
tooling is split along the same line:

| `cargo xtask …` | does | needs |
|---|---|---|
| `server` | obtains server binaries | a cross toolchain, or a network |
| `bundle <cm>` | writes them into a linked dashboard | neither — no cargo either |
| `dist` | both, plus the dashboard build | whatever the source needs |

`bundle` is the one that could not exist otherwise. It patches a `cm` that is
already built — or already shipped — so a dashboard and the servers it carries
need never have been on the same machine, let alone in the same command.

**Where servers come from is therefore a flag, not an assumption.** Three sources
produce the same `Payload` and nothing downstream can tell which answered:

- `--servers build` cross-compiles from this workspace. The default, and what the
  dev loop wants: the server has to match the sources you are changing.
- `--servers release[:<version>]` downloads a published one. A bundled build then
  needs `curl` and `tar` and nothing else — no zig, no cross `rust-std`s, not
  even the server's sources. A bare `release` means this workspace's version,
  since that is the only defensible reading of an omitted one.
- `--server <target>=<path>` takes a binary the caller already has. The escape
  hatch, and what release CI uses when its own jobs have already produced them.

The rest still follows: `build.rs` reads one environment variable and writes one
constant, `cargo build` / `clippy` / `check` need no cross toolchain, and one
dashboard compile serves every architecture combination.

Two properties of the fetch path are load-bearing, and both are pinned by tests
over real tarballs rather than over a live download — a test can be handed a
hostile archive, it cannot be handed a hostile release. `--proto =https` is
re-asserted on every redirect hop, because GitHub bounces release downloads to
S3 and pinning only the URL we started from would check nothing. And the archive
member is extracted **by name**, so a `../` entry has nothing to land on; a
symlink wearing that name is refused rather than read through, since `tar`
extracts one happily and reading it would pack a file from anywhere on the box.

**A version match is not identity**, which is why provenance is recorded rather
than inferred. The workspace version is the only thing a released artifact is
keyed on and it does not move between dev builds, so a `0.2.1` server says
nothing about *which* `0.2.1`. `Provenance` names the source for the human;
each payload's sha256 is what actually tells two builds apart, on `cm --version`
and in the marker the deploy leaves on the host. It also decides what may be
warned about: only a build we ran has a glibc floor we chose, so `unpinned_floor`
is reached through `Provenance::strategy()` and stays quiet about a binary
somebody else linked.

**The mechanism is a reserved slot, and the reason is `strip`.** Three ways to
get bytes into a linked binary were measured:

| | survives `strip` | ELF + Mach-O | size cost |
|---|---|---|---|
| append + trailer | **no — silently wiped** | yes | exact |
| `objcopy --add-section` | yes | ELF only | exact |
| **reserved `.rodata` slot** | **yes** | **yes** | fixed reservation |

`strip` does not edit a file, it rewrites it from the ELF structure, so trailing
bytes are simply not carried across — an appended payload disappears with no
error and `cm --version` then honestly reports carrying nothing. A section added
by `objcopy` survives (verified, including `--strip-all`), but Mach-O has no
post-link equivalent: `ld -sectcreate` is link-time and `segedit -replace` is
same-size-only, which is Apple having reached the same conclusion. A slot the
linker already placed is allocated, loaded, and referenced, so `strip` cannot
remove it without breaking the program — and overwriting bytes in place disturbs
nothing either format cares about, so one implementation covers both.

Three things that had to be right, each found by trying it:

- **`UnsafeCell`, not a plain `static`.** An immutable static lets the compiler
  constant-fold reads of its initializer, and under release LTO it does — every
  read of the "how many bytes were injected" field would fold to the compile-time
  zero while the payload sat in the file unread. Interior mutability forces a real
  load. It also moves the slot from `.rodata` to `.data`; both are `PROGBITS`, so
  strip-immunity is unaffected.
- **The sentinel, not just the magic.** Injecting over an already-bundled binary
  means scanning across bytes that are themselves a payload and could contain the
  magic by chance. A candidate is only accepted when its recorded capacity lands
  the sentinel exactly where the header implies, and two survivors is an error
  rather than a coin flip.
- **A malformed slot decodes to nothing.** Half a payload would deploy and then
  fail to exec on someone else's machine, which is a worse failure than carrying
  nothing and saying so.

**The reservation is sized, not chosen.** `CM_PAYLOAD_RESERVE` is read by
`build.rs`; `xtask` sets it from the servers it just compressed, plus headroom,
rounded up to a megabyte so variants needing similar amounts share a dashboard
compile. Unset — every ordinary build — means a zero-length slot, so `cargo
build`, `clippy` and `check` behave identically with the feature on or off and
need no cross toolchain.

The rounding slack is close to free where it matters. A run of identical filler
bytes costs **5,232 bytes** in a gzipped release tarball (measured against 4.5 MiB
of slack), and both distribution channels are gzipped, so the reservation shows up
in on-disk footprint and essentially nowhere else.

**Release builds gained `lto = "fat"` + `codegen-units = 1`** while measuring
this: 16% off `miao-server` (8.61 → 7.21 MB) and the same order off `miao`,
which every npm and GitHub download pays for and a bundled build pays for twice.
Deliberately not `panic = "abort"`, the usual companion — the server is a daemon
hosting the pty pool, so unwinding drops one task where aborting would kill every
session on the host.

**Building the variants: `cargo xtask dist`** builds the named release artifacts
into `dist/`: `miao` (plain), `miao-remote`, `miao-bundle-linux`, plus the single-arch
bundles. Each run obtains every server once even when several variants want it,
then verifies each artifact by running it and checking it reports what was just
injected — the only check that can catch a bad patch. `bundle` runs the same
check but merely warns when the artifact will not start, since bundling a Linux
`cm` on a mac is a thing it exists for; a binary that *runs* and reports the
wrong payloads is an error either way.

**Release CI publishes the servers** (`build.yml`'s `server` job), which is what
gives `--servers release` something to fetch. One x86_64 runner cross-compiles
both Linux arches through `nix develop --command cargo xtask server` — the same
code path a laptop runs, so the strategy choice, the pinned glibc floor and the
architecture check cannot drift between what CI publishes and what a developer
builds. One runner rather than two, deliberately: zigbuild pins the floor at 2.28
where a native arm64 build would inherit the runner's glibc, and the flake
declares no `aarch64-linux` system to enter a dev shell on anyway. The assets are
flat tarballs holding just `miao-server`, which is the other half of the
by-name extraction contract above.

**Nix has the same variants**: `packages.captain-miao-bundle-linux` and the two
single-arch ones. They delegate to `cargo xtask dist` rather than reimplementing
it, because the reservation has to be sized from the *compressed* servers and a
nix expression could only guess — and a guessed number is what this design set
out to remove. The whole sequence runs offline: every cargo invocation resolves
from the vendored registry crane already set up. Two things they need that the
plain packages don't, both found by trying it: `devToolchain` for the cross
`rust-std`s (a second `craneLib`, so `nix build .#captain-miao` stays
byte-identical), and a **writable `HOME`** — cargo-zigbuild keeps a cache under it
and nix points `HOME` at the non-existent `/homeless-shelter`, so the cross dies
on a permission error before zig is even invoked.

**Deploying: `backend.rs`.** The connect probe gained a fifth line (the digest
marker beside the deployed binary) and `Provision` gained an `Upload` arm. The
binary is streamed into `cat` over the ssh connection the probe just opened — no
local temp file, no second round trip, and no decompressor requirement on a host
whose distinguishing feature is that nothing is installed on it. It is staged,
`chmod`ed, and **run on the host** before being moved into place, so a truncated
transfer or a wrong-ABI payload never becomes the binary the next connect
invokes. That check is also the only thing that can catch glibc-vs-musl, which
`uname` cannot report.

Three things that turned out to matter, each verified rather than assumed:

- **`ssh host <cmd>` runs `<cmd>` through the account's login shell**, which is
  routinely `fish`. A POSIX-sh deploy script came back as *"fish: Unsupported use
  of '='"*. Everything we send is now wrapped as `/bin/sh -c '<script>'`, which
  parses identically in sh/bash/zsh/fish/csh — provided the script contains no
  single quote and no backslash, since only fish honours escapes inside single
  quotes. That constraint is why the script writes its marker with `echo` rather
  than `printf '%s\n'` and clears its temp file up front rather than with an
  `EXIT` trap. Pinned by a test that runs the deploy under every shell installed
  on the machine.
- **A version match is not identity.** Dev builds never bump the version, so
  `0.2.1` on a host says nothing about *which* `0.2.1`. The digest marker closes
  that: rebuild, reconnect, and the host gets the new server. This is what
  retires `redeploy.sh` for payload-carrying builds — and the same observation is
  why staging by version could not tell a current payload from a stale one.
- **A failed deploy must not repeat on every reconnect.** The backoff caps at
  30s, so a host that accepts ssh but refuses the write would be re-sent
  megabytes twice a minute forever. `UploadGate` remembers the failure, keyed on
  the payload digest so a *new* build always gets a fresh attempt.

Ownership rule the whole thing rests on: **PATH is the user's, the cache path is
ours.** A version-matching binary the user installed always wins and is never
overwritten; the cache path is refreshed to match our payload whenever it
doesn't.

Verified end to end over real ssh (probe → deploy → verify → re-probe → second
connect declines to re-send → `daemon status` runs on the deployed binary) by the
`provisions_a_real_host_end_to_end` test, which is `#[ignore]`d and takes its
target from `CM_TEST_SSH_TARGET`. An sshd on localhost exercises every line of
it, so it does not need a remote machine. Run it with a bundle feature on — that
is what puts a payload in the test binary.
