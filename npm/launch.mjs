#!/usr/bin/env node
// captain-miao launcher — the npm package's only runtime file. It selects and
// execs the prebuilt native `miao` binary that ships in a per-platform
// optional-dependency package (`@hyperlogue/captain-miao-<os>-<arch>`). The
// package keeps the project's name while the command it installs is `miao`; npm's
// `bin` map decouples the two, so only the binary was renamed. npm/bun
// install only the package whose `os`/`cpu` match the host, so the matching
// binary is already on disk by the time this runs — no download, no network, no
// checksum dance. This file just resolves it and hands off, forwarding argv,
// stdio, and the exit code/signal.
//
// Deliberately plain, dependency-free Node ESM using only cross-runtime APIs, so
// the SAME file runs under `npx @hyperlogue/captain-miao` (Node >=18) and
// `bunx @hyperlogue/captain-miao` (Bun). The binary is a static Rust build, so
// whichever runtime ran this launcher is irrelevant to how captain-miao runs.
//
// `stdio: "inherit"` is load-bearing here in a way it isn't for an ordinary CLI:
// captain-miao's default subcommand is a full-screen ratatui TUI that needs the
// real tty for raw mode, resize signals, and the kitty graphics protocol. Piping
// any of the three would break it.

import { spawn } from "node:child_process";
import { chmodSync } from "node:fs";
import { createRequire } from "node:module";
import { constants } from "node:os";

const require = createRequire(import.meta.url);

// `${process.platform}-${process.arch}` -> the optional-dependency package that
// carries that platform's binary. Keep in sync with the PLATFORMS table in
// scripts/stage-npm-packages.sh (the producer) and the build matrix in
// .github/workflows/build.yml (the target triples).
const PACKAGES = {
  "darwin-arm64": "@hyperlogue/captain-miao-darwin-arm64",
  "darwin-x64": "@hyperlogue/captain-miao-darwin-x64",
  "linux-x64": "@hyperlogue/captain-miao-linux-x64",
  "linux-arm64": "@hyperlogue/captain-miao-linux-arm64",
};

const REPO = "https://github.com/hyperlogue/captain-miao";

function fail(msg) {
  process.stderr.write(`miao: ${msg}\n`);
  process.exit(1);
}

// The published Linux binaries are glibc builds (ubuntu-22.04, glibc 2.35). On
// musl (Alpine) npm >=9.6 skips the glibc package via its `libc` field so it
// won't resolve; older npm / Bun ignore that field and install it anyway, and it
// then dies at exec with a cryptic ENOENT (missing dynamic loader). Detect musl
// so both paths can say so plainly.
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    const report = process.report?.getReport?.();
    return Boolean(report && !report.header?.glibcVersionRuntime);
  } catch {
    return false; // can't tell — assume glibc and let exec surface any mismatch
  }
}

function muslFail() {
  fail(
    `this looks like a musl/Alpine system, but only glibc Linux binaries are published.\n` +
      `  Build from source: ${REPO}`,
  );
}

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PACKAGES[key];
  if (!pkg) {
    if (isMusl()) muslFail();
    fail(
      `no prebuilt binary for ${key}. Supported: ${Object.keys(PACKAGES).join(", ")}.\n` +
        `  captain-miao is Unix-only (it needs Unix sockets, $XDG_RUNTIME_DIR, and\n` +
        `  Kitty or zellij), so there is no Windows build.\n` +
        `  Build from source: ${REPO}`,
    );
  }

  // Refuse musl up front, whether or not the glibc package resolved: npm >=9.6
  // skips it (via `libc`) so resolve would fail, but older npm / Bun ignore that
  // field and install it anyway, and it'd then ENOENT cryptically at exec.
  if (isMusl()) muslFail();

  try {
    // Resolves to node_modules/<pkg>/bin/miao when the optional dependency
    // installed. Keep the basename in sync with BIN in
    // scripts/stage-npm-packages.sh, which is what puts the file there.
    return require.resolve(`${pkg}/bin/miao`);
  } catch {
    // The platform package didn't install. Common causes, in order of likelihood:
    // a stale lockfile (npm optional-deps bug npm/cli#4828), a --no-optional /
    // offline install, or a mirror that dropped the optional dep.
    fail(
      `the ${pkg} package for your platform (${key}) is not installed.\n` +
        `  This is usually a stale lockfile (npm optional-dependencies bug). Try:\n` +
        `    • reinstall:  rm -rf node_modules package-lock.json && npm install\n` +
        `    • clear the npx cache, then retry: npx --yes @hyperlogue/captain-miao …\n` +
        `    • download a prebuilt binary from ${REPO}/releases`,
    );
  }
}

const bin = resolveBinary();
// Best-effort exec bit — the packaged binary ships 0755, but a filesystem or a
// tarball tool that dropped the mode would otherwise EACCES on spawn.
try {
  chmodSync(bin, 0o755);
} catch {}

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
// Forward termination signals so `kill <launcher-pid>` (or a supervisor) reaches
// the captain-miao child instead of orphaning it — matters for the long-lived
// dashboard and for `miao claude`, which wraps an agent session.
// (SIGINT/Ctrl-C already reaches the child via the shared process group.)
for (const sig of ["SIGTERM", "SIGHUP"]) {
  process.on(sig, () => {
    try {
      child.kill(sig);
    } catch {}
  });
}
child.on("error", (err) => fail(`failed to exec ${bin}: ${err.message}`));
child.on("exit", (code, signal) => {
  if (signal) {
    // Re-raise so our exit status mirrors the child's; then fall back to the
    // conventional 128+signum for signals the runtime ignores (e.g. SIGPIPE),
    // where the self-kill is a no-op and we'd otherwise exit 0.
    process.kill(process.pid, signal);
    process.exit(128 + (constants.signals[signal] || 0));
  }
  process.exit(code ?? 0);
});
