//! Sizes the payload slot the dashboard reserves, and nothing else.
//!
//! `CM_PAYLOAD_RESERVE` is how many bytes `src/server_payload.rs` sets aside for
//! `miao-server` builds that `cargo xtask dist` writes in after linking.
//! Unset (the normal case) means zero, so a regular `cargo build` reserves
//! nothing and carries nothing.
//!
//! The size is deliberately **not a constant in the source**: `xtask` measures
//! the servers it just compressed and passes the figure it actually needs, so the
//! reservation follows the payload rather than a number somebody guessed once and
//! nobody revisited. See `docs/crate-split.md`.
//!
//! Note what this script does *not* do — it does not build, download, or read
//! anything. Deciding the contents after linking is what buys that: the payload
//! is no longer an input to the compile, so a bundled build needs no cross
//! toolchain and `cargo build`/`clippy`/`check` behave identically with the
//! feature on or off.

use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CM_PAYLOAD_RESERVE");

    // Anything unparseable is a zero-size slot rather than a build failure: the
    // feature is then inert and `cm --version` says it carries nothing, which is
    // both true and recoverable. A hard error here would take `cargo clippy
    // --all-features` down with it.
    let reserve: usize = std::env::var("CM_PAYLOAD_RESERVE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("reserve.rs");
    std::fs::write(
        &dest,
        format!("pub(crate) const RESERVE: usize = {reserve};\n"),
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", dest.display()));
}
