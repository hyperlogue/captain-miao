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
was pointed at. `cargo xtask dist` produces the named variants side by side — a
plain `miao` carrying nothing, single-arch bundles, a `miao-bundle-linux-all`
carrying every published server — and a plain `cargo build` is byte-for-byte what
it would be if none of this existed.

**Every published artifact is bundled**: what ships is
`bundle-linux-x86_64` (one server, x86-64 glibc) for all four host targets, plus
`bundle-linux-all` as a separate GitHub-only download. The plain build stays a
variant — it is what a bare `cargo build` gives you — but nothing publishes it.
The reasoning is that "most users want none" turned out to be the wrong read: the
payload's target is the *remote host's* architecture, not the laptop's, so a Mac
user carrying a Linux server is the common case rather than the odd one, and the
cost of being wrong is a first-run deploy that has to stop and fetch.

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
session on the host. That ruling was re-measured at −9.8% gzipped and re-affirmed:
tokio's task harness catches unwinds, so a panicking task becomes a `JoinError`
and the host's other sessions survive, where an abort would take the daemon and
every session on it.

**The server then got its own profile.** Once every published dashboard carried a
server, the payload's size started being paid by every npm install and every
GitHub download, so `[profile.server-release]` splits the server off from the
dashboard's `release`: `opt-level = "s"`, with `shpool-vterm` / `shpool_vt100` /
`vte` / `tokio` held at `3` so the pty byte path keeps full speed. `xtask` sets
`LIBSQLITE3_FLAGS` alongside it, dropping FTS3/FTS5/JSON1/R*Tree/STAT4 and the
loadable-extension machinery from the bundled amalgamation — the server's only
SQLite use is one read-only `SELECT` for Codex thread titles.

Measured on the real cross-build — `prepare-servers` through zigbuild, the same
path CI publishes from — for `x86_64-unknown-linux-gnu`, gzipped (the codec the
payload used at the time): **2,975,483 → 2,137,645, −28.2%** (raw 6,633,456 →
4,711,104, −29.0%). Roughly 818 KiB off every npm platform package and every
dashboard tarball, and ~3.2 MiB off the all-server artifact. A native
non-zigbuild build measures −30.0%; the zigbuild figure is the one to quote,
because it is the path a release is actually built on.

Two findings worth not re-deriving. `opt-level` reaches **C**, not just Rust —
`cc` scrapes cargo's `OPT_LEVEL` and passes `-O<level>` straight through — so
SQLite's amalgamation compiles `-Os` here, and a per-package override does
propagate into a build script. And that is where most of the win is: leaving all
Rust at `3` and dropping only `libsqlite3-sys` to `"s"` is −9.6% by itself, or
−16.6% with the trim, which is 55% of the total for zero effect on any Rust code.
That is the fallback if `"s"` ever turns out to cost real throughput — nobody has
benchmarked `s` against `3`, which is the honest gap in all of this.

**Then the codec, which compounds with the profile.** The embedded format is
entirely internal — `xtask` packs it, the dashboard unpacks it before uploading,
and the remote host only ever receives the plain binary — so both halves ship
from the same commit and there is no compatibility surface to preserve. That
freedom is the whole argument for picking on merit: gzip → **xz preset 6** took
the same x86-64 glibc payload 2,137,645 → 1,675,736 (−21.6%), where zstd measured
−16.1% and brotli −20.4%. Compounded with the profile, the payload a release
actually ships went **2,975,483 → 1,675,736, −43.7%** — about 1.24 MiB off every
npm install and every dashboard tarball.

Three constraints worth not re-deriving. **Preset 6, not 9**, and that is not a
compromise: on a ~4.7 MB server the two emit byte-identical output, because an
LZMA dictionary larger than the input buys nothing and 6's is already 8 MiB. What
9 would change is the *decoder* — the dictionary size rides in the stream header,
so it would oblige every dashboard to allocate 65 MiB to unpack rather than 9.
**Encode with C liblzma in `xtask`, decode with pure-Rust `lzma-rs` in the
dashboard**, an asymmetry that is measured rather than stylistic: `lzma-rs`'s
encoder does not compress at all (it turned a 4,711,104-byte server into
4,711,376), while its decoder is fine — and `xtask` runs on a build machine where
a C toolchain is a given, whereas the dashboard cross-compiles to four targets
where it is not. **No BCJ/x86 filter**, which would be the obvious next win on
executables: `lzma-rs` implements LZMA2 only, so a filtered stream would pack
smaller in `xtask` and fail to unpack in the dashboard. That is the realistic way
to break the pairing, so `xtask` round-trips `pack`'s real output through the
decoder the dashboard ships — the one place in the tree where both codecs exist.

**Building the variants: `cargo xtask dist`** builds the named release artifacts
into `dist/`; with no `--variant` it builds exactly what a release publishes
(`bundle-linux-x86_64` and `bundle-linux-all`). Each run obtains every server
once even when several variants want it, then verifies each artifact carries what
it was built to carry. That check earns its keep: a manifest reaches the compile
through an environment variable and a generated file, and that seam fails
*silently* — a variable that did not survive, an archive that moved, and the build
succeeds carrying nothing.

Verification is by **execution** wherever the artifact can run, which exercises
the real accessor rather than the mere presence of bytes. `--target` (release CI
needs it for x86-64 macOS, cross-built on an arm64 runner) can produce an artifact
this machine cannot exec; there the check falls back to scanning the image for
each payload's SHA-256, which survives `strip = true` because it is a
`&'static str` in `.rodata`. Weaker — it proves the table was populated, not that
the binary starts — but the alternative was not checking the cross build at all.

**Release CI publishes the servers** (`build.yml`'s `server` job), which is what
gives `--from release` something to fetch. One x86_64 runner cross-compiles
every Linux target through `nix develop --command cargo xtask prepare-servers` — the same
code path a laptop runs, so the strategy choice, the pinned glibc floor and the
architecture check cannot drift between what CI publishes and what a developer
builds. One runner rather than two, deliberately: zigbuild pins the floor at 2.28
where a native arm64 build would inherit the runner's glibc, and the flake
declares no `aarch64-linux` system to enter a dev shell on anyway. The assets are
flat tarballs holding just `miao-server`, which is the other half of the
by-name extraction contract above. The workflow needs no edit to gain a target:
it runs `prepare-servers` bare and packages whatever lands in `dist/servers/`,
so the list lives in `xtask` alone.

### Which libc a payload is built against

**glibc is preferred, and musl is a verified fallback.** These are two different
questions and the answer to each is on a different axis, so they are stated
together here rather than left to be inferred from either one.

glibc is preferred because **NSS is load-bearing, not a nicety.** A static musl
build compiles NSS out, so on a host whose users come from LDAP/SSSD it cannot
see a `passwd` entry for a perfectly valid uid — and libshpool resolves the user
with `getpwuid_r` and *errors* when the lookup finds nothing. The session does
not degrade to `/bin/sh`; it fails to attach at all, and `home_dir` and the shell
go with it. (`utmpx` is stubbed too, so pooled sessions won't appear in `who`/`w`
— real, but the smaller loss.)

musl is carried anyway because it is the only thing that reaches a host with **no
generic loader at all**. NixOS, Alpine and distroless have no
`/lib64/ld-linux-x86-64.so.2`, so a glibc binary cannot start there — not because
they are old, but because "generic glibc" is not a universal ABI. A static musl
build has no `PT_INTERP`, no `PT_DYNAMIC`, and is `ET_EXEC`; it costs nothing in
size (6.7 MiB against glibc's 6.9, and 3.1 MiB compressed either way — musl's
libc is negligible beside 6 MiB of Rust, bundled SQLite and libshpool).

**Which one a given host gets is not decided here.** `uname` cannot report a
libc, so the choice is deferred to the host: the deploy offers candidates in
preference order and keeps the first one the host proves it can run. Every
combination resolves without a guess, including the honest failure — a NixOS host
with LDAP/SSSD users can be served by *no* payload we could ship, and is told so
rather than handed a binary that breaks at first attach.

**What is embedded and what is published deliberately differ.** A release
publishes all four targets but embeds only the gnu pair (`PUBLISHED_TARGETS` vs
`LINUX_TARGETS` in `xtask`). musl's audience is Nix hosts, which have a better
answer already — a server built against their own libc, on their own PATH, no
deploy at all — so making every downloader carry ~6 MiB aimed at the one platform
that does not need it is the wrong default. A dashboard that *meets* such a host
downloads the published musl asset instead. The consequence, stated rather than
discovered: a released dashboard now needs network to reach a no-loader host. The
offline guarantee was always about the mainstream Linux fleet, which gnu covers
completely.

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

The same override also carries `-D_DARWIN_C_SOURCE`, which is the *third* face of
that one build-script bug and only appears once musl is in the target set: with
clang aimed back at the host, the macOS SDK's `net/if.h` still compiles its Apple
body out unless that macro is defined, leaving `struct if_data` a forward
declaration and killing bindgen with "field has incomplete type". Faces one and
two were the missing per-target variable and its dashed-vs-underscored spelling.

**On Nix, prefer the link farm to embedding.** `captain-miao-with-servers` wraps
the dashboard with `CAPTAIN_MIAO_SERVER_DIR` pointing at
`captain-miao-server-payloads`, a directory of `<triple>/miao-server` built by
`prepare-servers`. Both are `callPackage`d, so the fleet is one override away:

```nix
packages.captain-miao-with-servers.override {
  targets = [ "x86_64-unknown-linux-musl" "aarch64-unknown-linux-gnu" ];
}
```

The default is a single target, `x86_64-unknown-linux-musl` — a static build
runs on any x86-64 Linux host whatever its libc, including the
NixOS/Alpine/distroless boxes a glibc build cannot start on, so it is the one
binary covering the most fleet. This beats embedding *on this path* for the
reason embedding exists elsewhere: a download wants one self-contained file,
whereas in the store the servers are paths shared between generations, and adding
an architecture costs a server build rather than relinking `miao`.

**Nix does not embed at all — the flake carries no bundled variants.** They
existed for parity with what a release publishes and nobody wanted them: a Nix
build downloads nothing, so the one property embedding buys — a single
self-contained file — is worth nothing there, while every widening of the fleet
cost a full relink of `miao`.

**Two builds of the same server, and confusing them is the mistake.**
`packages.captain-miao-server` is an ordinary nixpkgs build: `rustToolchain`,
plain `cargo build --release`, linked against the store's own glibc with an
absolute `/nix/store/…/ld-linux` interpreter. That is right for its only job —
the Home Manager module putting `miao-server` on *this* machine's PATH, where a
dashboard finds it locally and no deploy happens — and wrong anywhere else,
because that loader exists on no other host. Anything a dashboard *deploys* comes
from `packages.captain-miao-server-payloads`, which cross-builds through cargo-zigbuild
against the pinned glibc floor.

That distinction is **asserted, not assumed**. `choose_strategy` prefers zigbuild
but falls back to a native build when zig is not on `PATH`, and for a
`-linux-gnu` target on an x86-64 builder that fallback *succeeds* — producing
precisely the store-linked binary above, which would pass every other check and
fail at exec on the first host it reached. So `nix/server-payloads.nix` refuses any
output whose ELF interpreter is a store path. A musl build is static and has none,
which is the expected case rather than a skipped one.

Both need `devToolchain` for the cross `rust-std`s (a second `craneLib`, so
`nix build .#captain-miao` stays byte-identical) and a **writable `HOME`** —
cargo-zigbuild keeps a cache under it and nix points `HOME` at the non-existent
`/homeless-shelter`, so the cross dies on a permission error before zig is even
invoked. The whole sequence runs offline, which is also why it stays on
`--from build`: a nix build has no network to fetch a published server over.

The wrapper uses `--set-default`, so a user's own `CAPTAIN_MIAO_SERVER_DIR` still
wins.

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
