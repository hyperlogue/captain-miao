//! `cm-core` — the logic and data captain-miao's two binaries share.
//!
//! The `captain-miao` dashboard (the ratatui TUI client) and `captain-miao-server`
//! (the per-host daemon + pty pool) both depend on this crate. It is deliberately
//! free of ratatui/crossterm (presentation lives in the dashboard) and libshpool
//! (the pool lives in the server), so it stays a portable data/logic layer that
//! cross-compiles cleanly as part of the server.
//!
//! What lives here: the session [`state`] files + types, the wire [`protocol`],
//! the per-backend [`agent`] dispatch (Claude/Codex parsing), the [`launcher`]
//! that supervises an agent process, the [`hooks`] forwarder, the local
//! server-core [`backend::LocalBackend`] + the shared open-session types, the
//! opaque [`terminal`] ids + the launcher's `current_window` self-report, plus
//! shared [`cli`] arg helpers and [`logging`] setup.

pub mod agent;
pub mod agents;
pub mod backend;
pub mod cli;
pub mod config;
pub mod hooks;
pub mod launcher;
pub mod learned;
pub mod logging;
pub mod protocol;
pub mod state;
pub mod terminal;
