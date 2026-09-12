//! Keep per-launcher endpoints reachable if their directory or inode is lost.
//! Rebinding restores the existing control contract; it never authorizes a kill.
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub(super) struct Listener {
    socket: UnixListener,
    path: Option<PathBuf>,
    identity: Option<(u64, u64)>,
    health: tokio::time::Interval,
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

impl Listener {
    pub(super) fn new(socket: UnixListener) -> Self {
        let path = socket
            .local_addr()
            .ok()
            .and_then(|addr| addr.as_pathname().map(Path::to_owned));
        let identity = path.as_deref().and_then(identity);
        let mut health = tokio::time::interval(Duration::from_secs(1));
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            socket,
            path,
            identity,
            health,
        }
    }

    pub(super) async fn accept(&mut self) -> io::Result<UnixStream> {
        loop {
            tokio::select! {
                accepted = self.socket.accept() => return accepted.map(|(stream, _)| stream),
                _ = self.health.tick() => {
                    if let Err(error) = self.restore() {
                        tracing::warn!("Could not restore Codex launcher socket: {error}");
                    }
                }
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let current = identity(path);
        if current.is_some() && (self.identity.is_none() || current == self.identity) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            crate::state::create_dir_all_private(parent)?;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let socket = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        self.socket = socket;
        self.identity = identity(path);
        tracing::warn!("Restored a lost Codex launcher socket");
        Ok(())
    }
}
