//! Everything about the `miao-server` payloads a dashboard carries: the
//! slot format both sides speak, and the cross-build + injector that fills it.
//!
//! Split by who links which half. [`format`] has no dependencies and is shared —
//! the dashboard reads the slot with it, `xtask` writes the slot with it, and one
//! module means the writer and the reader cannot drift apart. The build half is
//! behind the `build` feature, which only `xtask` enables, so the dashboard's
//! dependency on this crate costs it nothing.
//!
//! Design rationale in `docs/crate-split.md`; the short version is that payloads
//! are written into an already-linked binary rather than compiled into it, so
//! which servers a `cm` carries is decided at bundling time and the dashboard's
//! own build needs no cross toolchain.

pub mod format;

#[cfg(feature = "build")]
mod server_build;

#[cfg(feature = "build")]
pub use server_build::*;
