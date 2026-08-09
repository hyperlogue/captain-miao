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
plus `captain-miao-client`, added after — with `xtask` alongside as build
support (root stays the `captain-miao` package, so `cargo install
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

**The dashboard is no longer one artifact.** A build carries whatever servers it
was pointed at; most users want none. `cargo xtask dist` produces the named
variants side by side — a plain `miao` carrying nothing, a `miao-bundle-linux`
carrying both Linux arches, single-arch ones in between — and a plain `cargo
build` is byte-for-byte what it would be if none of this existed.

**Obtaining a server and building a dashboard are separate steps**, with the seam
between them at *where servers come from* rather than at *how they get in*.
`xtask` has one subcommand per half:

| `cargo xtask …` | does | needs |
|---|---|---|
| `server` | obtains server binaries | a cross toolchain, or a network |
| `dist` | that, plus the dashboard build | whatever the source needs |

**Where servers come from is a flag, not an assumption.** Three sources produce
the same `Payload` and nothing downstream can tell which answered:

- `--from build` cross-compiles from this workspace. The default, and what the
  dev loop wants: the server has to match the sources you are changing.
- `--from release[:<version>]` downloads a published one. A bundled build then
  needs `curl` and `tar` and nothing else — no zig, no cross `rust-std`s, not
  even the server's sources. A bare `release` means this workspace's version,
  since that is the only defensible reading of an omitted one.
- `--server <target>=<path>` takes a binary the caller already has. The escape
  hatch, and what release CI uses when its own jobs have already produced them.

Two properties of the fetch path are load-bearing, and both are pinned by tests
over real tarballs rather than over a live download — a test can be handed a
hostile archive, it cannot be handed a hostile release. `--proto =https` is
re-asserted on every redirect hop, because GitHub bounces release downloads to
S3 and pinning only the URL we started from would check nothing. And the archive
member is extracted **by name**, so a `../` entry has nothing to land on; a
symlink wearing that name is refused rather than read through, since `tar`
extracts one happily and reading it would pack a file from anywhere on the box.

**The payload reaches the compile through one environment variable.** `xtask`
writes a per-variant TSV manifest — `<target>\t<sha256>\t<gz path>` — and points
`CM_SERVER_PAYLOAD_MANIFEST` at it; `build.rs` `include_bytes!`es each archive
into `server_payload.rs`'s table. Unset, which is every ordinary `cargo build`,
the table is empty and the binary is what it would be if none of this existed.
That variable is the whole switch, so there is no cargo feature beside it — a
bundling variant simply also passes `--features remote`, since the deploy path
lives behind that gate and a server carried without it would be dead weight.

A manifest that is *set* and malformed is a **hard build error**. It is always a
mistake — a stale exported variable, an archive that moved — and the lenient
reading produces a dashboard that carries nothing while every sign says it
should.

**Two watched files, and neither may be touched needlessly.** `build.rs`
`rerun-if-changed`s the manifest and each archive, so rewriting either with
identical bytes bumps its mtime, re-runs the build script, and forces a full LTO
relink for no reason. Hence `write_manifest` writes only on a real change, and
`build.rs` stages a *copy* of each archive into `OUT_DIR` to embed from rather
than `include_bytes!`ing the file it watches. Both mistakes were made here once
each before being understood.

### Post-link injection: built, measured, dropped

An earlier iteration put the payloads into the *linked* binary instead — a
reserved slot the linker placed, overwritten afterwards by an injector. It is
recorded here because the reasoning is easy to have again.

`strip` is what pushes you toward it. Three ways to get bytes into a linked
binary, measured:

| | survives `strip` | ELF + Mach-O | size cost |
|---|---|---|---|
| append + trailer | **no — silently wiped** | yes | exact |
| `objcopy --add-section` | yes | ELF only | exact |
| reserved slot | yes | yes | fixed reservation |

`strip` does not edit a file, it rewrites it from the ELF structure, so trailing
bytes are simply not carried across — an appended payload disappears with no
error. A section added by `objcopy` survives (verified, including `--strip-all`),
but Mach-O has no post-link equivalent: `ld -sectcreate` is link-time and
`segedit -replace` is same-size-only. A slot the linker already placed survives
both, and can be overwritten in place on either format.

It worked. Three things had to be right, each found by trying it: the slot had to
be an **`UnsafeCell`**, because release LTO constant-folds reads of an immutable
`static`'s initializer and every read of the used-length field folded to the
compile-time zero; `find` had to require a **sentinel** at the capacity the
header claimed, since re-injecting scans across bytes that are themselves a
payload; and a malformed slot had to decode to **nothing**, because half a
payload would deploy and then fail to exec on someone else's machine.

It was dropped anyway, because the question is not "does it work" but "what does
it buy over `include_bytes!`". Measured:

- **Its one structural claim was false.** "One dashboard compile serves every
  architecture combination" — the three bundled variants reserve 5, 4 and 7 MiB,
  so no two ever shared a compile. `dist --all` costs five either way.
- **The remaining benefit is 58 seconds** — a warm release relink of the
  dashboard, which is what re-bundling avoids. And it does not avoid *cargo*: the
  injector is `xtask`, so you need a toolchain regardless.
- **The cost was ~600 lines** (a binary format with magic, sentinel and capacity
  header; a slot module; an injector), two `unsafe` blocks, a `codesign` step on
  macOS that patching a Mach-O makes mandatory, and ~1 MiB of reservation slack
  in every bundled artifact — `miao-bundle-linux` went from 13.7 to 12.6 MiB when
  it came out.

`include_bytes!` gives up nothing that mattered. It is allocated, referenced data,
so `strip` cannot remove it either; the decoupling lives in `--from`, which is
orthogonal to the embedding; and the "payload is not a compile input" property —
no cross toolchain for `cargo build`, `clippy`, `check` — is preserved by the
environment variable simply being unset.

**Release builds gained `lto = "fat"` + `codegen-units = 1`** while measuring
this: 16% off `miao-server` (8.61 → 7.21 MB) and the same order off `miao`,
which every npm and GitHub download pays for and a bundled build pays for twice.
Deliberately not `panic = "abort"`, the usual companion — the server is a daemon
hosting the pty pool, so unwinding drops one task where aborting would kill every
session on the host.

**Building the variants: `cargo xtask dist`** builds the named release artifacts
into `dist/`: `miao` (plain), `miao-remote`, `miao-bundle-linux`, plus the single-arch
bundles. Each run obtains every server once even when several variants want it,
then verifies each artifact by running it and checking it reports what it was
built to carry. That check earns its keep: a manifest reaches the compile through
an environment variable and a generated file, and that seam fails *silently* — a
variable that did not survive, an archive that moved, and the build succeeds
carrying nothing.

**Release CI publishes the servers** (`build.yml`'s `server` job), which is what
gives `--from release` something to fetch. One x86_64 runner cross-compiles
both Linux arches through `nix develop --command cargo xtask prepare-servers` — the same
code path a laptop runs, so the strategy choice, the pinned glibc floor and the
architecture check cannot drift between what CI publishes and what a developer
builds. One runner rather than two, deliberately: zigbuild pins the floor at 2.28
where a native arm64 build would inherit the runner's glibc, and the flake
declares no `aarch64-linux` system to enter a dev shell on anyway. The assets are
flat tarballs holding just `miao-server`, which is the other half of the
by-name extraction contract above.

**A macOS host needs one environment override to cross at all** (`cross_build_env`).
`libproc` is libshpool's dependency, so it is in every server build, and it gates
its bindgen call on `#[cfg(target_os = "macos")]` *inside build.rs* — where a cfg
describes the host, not the target. On a Mac it therefore runs while compiling
for Linux, feeding the macOS SDK headers to a clang aimed at `aarch64-unknown-linux-gnu`,
which fails with `error: Unsupported architecture` before any of our code
compiles. Aiming clang back at the host triple makes the headers parse and the
bindings it writes are dead code (libproc's *library* includes them under the
same cfg, which there means the target). Which variable is not a free choice:
bindgen reads `BINDGEN_EXTRA_CLANG_ARGS_<target>` — dash-spelled — ahead of both
the underscored spelling and the plain one, and cargo-zigbuild writes that same
variable, *appending* zig's sysroot flags rather than replacing what is there.
So the dashed one is the only spelling that both survives zigbuild and is the
one bindgen consults; a value left in the underscored spelling is silently never
read. Upstream's fix would be to gate on `CARGO_CFG_TARGET_OS`; 0.14.11 is the
latest and does not.

**Nix has the same variants**: `packages.captain-miao-bundle-linux` and the two
single-arch ones. They delegate to `cargo xtask dist` rather than reimplementing
it, because obtaining the servers, writing each variant's manifest and building
against it is exactly what `dist` already does — a nix expression would be a
second copy of it, free to drift. The whole sequence runs offline: every cargo
invocation resolves from the vendored registry crane already set up, which is
also why these stay on `--from build` (the default) rather than offering the
`release` source — a nix build has no network to fetch one over. Two things they
need that the
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
it, so it does not need a remote machine. Run it with
`CM_SERVER_PAYLOAD_MANIFEST` set — that is what puts a payload in the test
binary.
