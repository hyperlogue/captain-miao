//! Temporary compatibility for daemons started before sockets moved into the
//! state tree. `ensure` publishes aliases at the fixed paths; all consumers
//! keep using those paths, including across the old daemon's eventual exit.
//! No endpoint registry or process termination is involved.

use std::os::unix::fs::symlink;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::state;

// Publish control last: reaching it means the pool alias is already in place.
#[cfg(feature = "pty-pool")]
const SOCKETS: &[&str] = &["pty-pool.sock", "server.sock"];
#[cfg(not(feature = "pty-pool"))]
const SOCKETS: &[&str] = &["server.sock"];

/// A listener must belong to the recorded daemon, not just accept connections.
/// Kernel peer credentials reject sockets owned by another process.
pub(crate) fn socket_belongs_to(path: &Path, pid: u32) -> bool {
    let Ok(stream) = UnixStream::connect(path) else {
        return false;
    };
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        result == 0 && credentials.pid > 0 && credentials.pid as u32 == pid
    }
    #[cfg(target_os = "macos")]
    {
        let mut peer: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut peer as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        result == 0 && peer > 0 && peer as u32 == pid
    }
}

pub(crate) fn sockets_belong_to(dir: &Path, pid: u32) -> bool {
    SOCKETS
        .iter()
        .all(|name| socket_belongs_to(&dir.join(name), pid))
}

/// Probe legacy locations only while an existing daemon holds the singleton
/// lock. On Linux, read its own original runtime environment so a caller with
/// a different (or absent) XDG_RUNTIME_DIR can find it. Never print that data.
fn legacy_dirs(pid: u32) -> Vec<PathBuf> {
    let mut dirs = vec![state::runtime_dir()];
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) {
            // Its startup environment is authoritative. In particular, an
            // isolated daemon must not probe another instance's runtime dir.
            dirs.clear();
            if let Some(value) = env
                .split(|b| *b == 0)
                .find_map(|entry| entry.strip_prefix(b"XDG_RUNTIME_DIR="))
                && !value.is_empty()
            {
                dirs.push(PathBuf::from(std::ffi::OsStr::from_bytes(value)).join("captain-miao"));
            }
        } else {
            // Restricted procfs: try the known historical default as well as
            // the caller's runtime dir, still checking both sockets' owner.
            dirs.push(PathBuf::from(format!(
                "/run/user/{}/captain-miao",
                unsafe { libc::geteuid() }
            )));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    dirs.retain(|dir| dir.is_absolute() && *dir != state::daemon_dir());
    dirs.sort();
    dirs.dedup();
    dirs
}

pub(crate) fn link_sockets(pid: u32) -> Result<bool> {
    let fixed = state::daemon_dir();
    for legacy in legacy_dirs(pid) {
        if !sockets_belong_to(&legacy, pid) || state::recorded_daemon_pid() != Some(pid) {
            continue;
        }
        for name in SOCKETS {
            publish_alias(&legacy.join(name), &fixed.join(name))?;
        }
        return Ok(state::recorded_daemon_pid() == Some(pid) && sockets_belong_to(&fixed, pid));
    }
    Ok(false)
}

fn publish_alias(target: &Path, link: &Path) -> Result<()> {
    // symlink publishes atomically and never overwrites anything. In
    // particular, if the old daemon exits during discovery and another ensure
    // starts its replacement, migration cannot replace the new live socket.
    match symlink(target, link) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read_link(link).is_ok_and(|path| path == target) {
                return Ok(()); // another ensure published the same alias
            }
            bail!(
                "cannot link legacy daemon socket: {} is already occupied",
                link.display()
            );
        }
        Err(error) => Err(error).context("publish legacy daemon socket alias"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn migration_requires_the_socket_owner_to_match_the_daemon_pid() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let socket = root.path().join("server.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(socket_belongs_to(&socket, std::process::id()));
        assert!(!socket_belongs_to(&socket, std::process::id() + 1));
        assert!(!socket_belongs_to(
            &root.path().join("absent"),
            std::process::id()
        ));
    }

    #[test]
    fn migration_preserves_existing_files_and_live_aliases() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let link = root.path().join("server.sock");
        let target = root.path().join("legacy.sock");
        let _listener = UnixListener::bind(&target).unwrap();
        std::fs::write(&link, "keep").unwrap();
        assert!(publish_alias(&target, &link).is_err());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "keep");
        std::fs::remove_file(&link).unwrap();
        publish_alias(&target, &link).unwrap();
        publish_alias(&target, &link).unwrap();
        assert!(publish_alias(&root.path().join("elsewhere"), &link).is_err());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
    }
}
