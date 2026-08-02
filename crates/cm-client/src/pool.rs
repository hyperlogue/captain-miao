//! The pool client: a read-only session list over libshpool's wire codec, plus
//! an in-process `attach`.
//!
//! libshpool's own protocol client (`libshpool::protocol::Client`) is private,
//! and `libshpool::run` installs a global tracing subscriber (so it can't be
//! called twice in one process). For the read-only *list* we therefore speak
//! its wire codec directly: on connect the daemon writes a `VersionHeader`, then
//! the client sends a `ConnectHeader` and reads one reply, all as msgpack via
//! rmp-serde with struct-map (field-named) encoding. This is libshpool 0.11 /
//! shpool-protocol 0.4 behavior, pinned by `list_codec_roundtrips`; a daemon
//! protocol bump would surface here. `attach` goes through the single
//! `libshpool::run`, so the tracing-init constraint is never violated.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use shpool_protocol::{ConnectHeader, ListReply, Session, SessionStatus, VersionHeader};

use cm_core::backend::LocalBackend;
use cm_core::state::{LauncherState, pool_socket_path};

/// Serialize `value` the way libshpool's daemon expects (msgpack, struct-map).
fn shpool_encode<T: Serialize, W: Write>(value: &T, w: W) -> Result<()> {
    let mut ser = rmp_serde::Serializer::new(w).with_struct_map();
    value.serialize(&mut ser).context("encoding shpool frame")?;
    Ok(())
}

/// Read one msgpack value from `r`. rmp reads exactly the bytes of a single
/// value, so sequential calls on the same stream stay framed (how libshpool's
/// own client reads the version header then the reply).
fn shpool_decode<T: DeserializeOwned, R: Read>(r: R) -> Result<T> {
    rmp_serde::from_read(r).context("decoding shpool frame")
}

/// Query the local pool daemon for its live sessions. Read-only: nothing is
/// created or attached. Errors clearly when the daemon isn't up.
fn pool_list_sessions() -> Result<Vec<Session>> {
    let socket = pool_socket_path();
    let stream = UnixStream::connect(&socket).map_err(|e| {
        anyhow!(
            "cannot reach the pty pool at {} ({e}); is the daemon running? \
             start it with `captain-miao-server daemon ensure`",
            socket.display()
        )
    })?;
    // The daemon writes a VersionHeader to every fresh stream first. We consume
    // but don't gate on it — this binary and the daemon share one
    // shpool-protocol build, and libshpool itself only *warns* on a mismatch.
    let _version: VersionHeader =
        shpool_decode(&stream).context("reading pool daemon version header")?;
    shpool_encode(&ConnectHeader::List, &stream).context("sending pool list request")?;
    let reply: ListReply = shpool_decode(&stream).context("reading pool session list")?;
    Ok(reply.sessions)
}

/// A pool session joined with the metadata its launcher folded onto the state
/// file, for display. `attached` is libshpool's live client-attached bit; the
/// rest come from the matching `LauncherState` (absent for a session with no
/// launcher state yet, e.g. a bare shell).
#[derive(Serialize)]
struct SessionRow {
    name: String,
    attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    launcher_pid: Option<u32>,
}

impl SessionRow {
    fn build(session: &Session, state: Option<&LauncherState>) -> Self {
        SessionRow {
            name: session.name.clone(),
            attached: matches!(session.status, SessionStatus::Attached),
            agent: state.map(|s| s.agent.cli_subcommand().to_string()),
            dir: state.map(|s| abbreviate_home(&s.cwd)),
            // Same precedence the dashboard row uses, minus the resume index:
            // rename, else the folded first prompt.
            title: state.and_then(|s| s.name.clone().or_else(|| s.first_prompt.clone())),
            launcher_pid: state.map(|s| s.launcher_pid),
        }
    }
}

/// Build the display rows: every pool session, enriched by the launcher state
/// whose `pool_session` matches its name.
fn session_rows() -> Result<Vec<SessionRow>> {
    let sessions = pool_list_sessions()?;
    let states = LocalBackend::default().list_sessions();
    let by_pool: HashMap<&str, &LauncherState> = states
        .iter()
        .filter_map(|s| s.pool_session.as_deref().map(|p| (p, s)))
        .collect();
    Ok(sessions
        .iter()
        .map(|s| SessionRow::build(s, by_pool.get(s.name.as_str()).copied()))
        .collect())
}

/// `list` — print the pool as a table (or JSON).
pub fn list(json: bool) -> Result<()> {
    let rows = session_rows()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_table(&rows);
    }
    Ok(())
}

/// `attach` — reattach this terminal to a pooled session, but only if it exists
/// and isn't already attached elsewhere.
pub fn attach(name: String) -> Result<()> {
    let sessions = pool_list_sessions()?;
    let Some(session) = sessions.iter().find(|s| s.name == name) else {
        // Don't fall through to libshpool on an unknown name: it would *create*
        // a bare login shell for it, leaving a stray pool session.
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        if names.is_empty() {
            bail!("no pooled session named {name:?}; this host has no pooled sessions");
        }
        bail!(
            "no pooled session named {name:?}; available: {}",
            names.join(", ")
        );
    };
    if matches!(session.status, SessionStatus::Attached) {
        eprintln!("session {name:?} already has a terminal attached; not attaching");
        return Ok(());
    }
    // Detached → plain interactive reattach. A racing attach between the list
    // and here is still caught by libshpool's own busy guard.
    attach_pty(&name)
}

/// Proxy the named session's pty to this terminal via libshpool. Mirrors the
/// server's attach primitive (pin our private pool socket, never daemonize,
/// plain reattach). `libshpool::run` must precede any thread — this binary is
/// single-threaded up to here, honoring that contract.
#[cfg(feature = "pty-pool")]
fn attach_pty(name: &str) -> Result<()> {
    use clap::Parser as _;
    let socket = pool_socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let socket = socket.to_string_lossy().into_owned();
    // Parse a synthetic argv: libshpool's `Commands::Attach` is
    // `#[non_exhaustive]`, so it can't be constructed directly, and parsing
    // stays correct if libshpool adds optional flags.
    let argv = [
        "captain-miao-client",
        "--socket",
        &socket,
        "--no-daemonize",
        "attach",
        name,
    ];
    let args = libshpool::Args::try_parse_from(argv).context("building libshpool args")?;
    // Safety: single-threaded process, no thread spawned before this point.
    unsafe { libshpool::run(args, None) }
}

#[cfg(not(feature = "pty-pool"))]
fn attach_pty(_name: &str) -> Result<()> {
    bail!("this build has no attach support (compiled without the pty-pool feature)");
}

/// Replace a leading `$HOME` with `~` for a shorter, readable path.
fn abbreviate_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) => format!("~{rest}"),
            None => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// Flatten whitespace and cap a cell so a long/multiline title can't wreck the
/// table layout.
fn oneline(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Print the session rows as a left-aligned, dynamically-sized table.
fn print_table(rows: &[SessionRow]) {
    if rows.is_empty() {
        println!("No pooled sessions on this host.");
        return;
    }
    let cell = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".to_string());
    let mut table: Vec<[String; 5]> = vec![[
        "NAME".into(),
        "STATUS".into(),
        "AGENT".into(),
        "DIR".into(),
        "TITLE".into(),
    ]];
    for r in rows {
        table.push([
            r.name.clone(),
            if r.attached { "attached" } else { "detached" }.to_string(),
            cell(&r.agent),
            cell(&r.dir),
            oneline(&cell(&r.title), 60),
        ]);
    }
    // Width every column to its widest cell; the last (TITLE) isn't padded.
    let mut widths = [0usize; 5];
    for row in &table {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    for row in &table {
        let mut line = String::new();
        for i in 0..4 {
            line.push_str(&row[i]);
            for _ in row[i].chars().count()..widths[i] + 2 {
                line.push(' ');
            }
        }
        line.push_str(&row[4]);
        println!("{}", line.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the wire codec: what our struct-map encoder writes (the shape the
    /// daemon emits) is what our decoder reads back, framed by value so a
    /// version header can precede the reply on one stream.
    #[test]
    fn list_codec_roundtrips() {
        let reply = ListReply {
            sessions: vec![
                Session {
                    name: "cm-claude-1-1".into(),
                    started_at_unix_ms: 0,
                    last_connected_at_unix_ms: None,
                    last_disconnected_at_unix_ms: None,
                    status: SessionStatus::Attached,
                },
                Session {
                    name: "cm-codex-2-2".into(),
                    started_at_unix_ms: 0,
                    last_connected_at_unix_ms: None,
                    last_disconnected_at_unix_ms: None,
                    status: SessionStatus::Disconnected,
                },
            ],
        };
        // Two values back-to-back (version header + reply), decoded in order off
        // one buffer — mirrors reading them off the socket.
        let mut buf: Vec<u8> = Vec::new();
        shpool_encode(
            &VersionHeader {
                version: "0.0.0".into(),
            },
            &mut buf,
        )
        .unwrap();
        shpool_encode(&reply, &mut buf).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let version: VersionHeader = shpool_decode(&mut cursor).unwrap();
        assert_eq!(version.version, "0.0.0");
        let got: ListReply = shpool_decode(&mut cursor).unwrap();
        assert_eq!(got.sessions.len(), 2);
        assert_eq!(got.sessions[0].name, "cm-claude-1-1");
        assert!(matches!(got.sessions[0].status, SessionStatus::Attached));
        assert!(matches!(
            got.sessions[1].status,
            SessionStatus::Disconnected
        ));
    }

    #[test]
    fn abbreviate_home_collapses_prefix() {
        // SAFETY: single-threaded test; we set HOME just for this assertion.
        unsafe { std::env::set_var("HOME", "/home/miao") };
        assert_eq!(abbreviate_home("/home/miao/projects/x"), "~/projects/x");
        assert_eq!(abbreviate_home("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn oneline_flattens_and_truncates() {
        assert_eq!(oneline("a\n  b\tc", 40), "a b c");
        assert_eq!(oneline("abcdef", 4), "abc…");
        assert_eq!(oneline("abc", 3), "abc");
    }
}
