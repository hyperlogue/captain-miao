//! Recovery must leave a healthy daemon time to restore a vanished socket.
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(tempfile::TempDir);

impl Daemon {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_miao-server"));
        for (variable, directory) in [
            ("XDG_RUNTIME_DIR", "run"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            command.env(variable, self.0.path().join(directory));
        }
        command
            .env("TERM", "dumb")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn socket(&self) -> PathBuf {
        self.0.path().join("state/captain-miao/run/server.sock")
    }

    fn pid(&self) -> String {
        std::fs::read_to_string(self.0.path().join("state/captain-miao/server.pid")).unwrap()
    }

    fn ensure(&self) {
        assert!(
            self.command()
                .args(["daemon", "ensure"])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(self.socket()).is_err() {
            assert!(
                Instant::now() < deadline,
                "private daemon did not become reachable"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.command().args(["daemon", "stop", "--force"]).status();
    }
}

#[test]
fn ensure_preserves_a_daemon_rebinding_its_socket() {
    let daemon = Daemon(
        tempfile::Builder::new()
            .prefix("cm-rebind-")
            .tempdir_in("/tmp")
            .unwrap(),
    );
    daemon.ensure();
    let original = daemon.pid();
    // Let the first, immediate socket tick pass. Recovery must tolerate losing
    // the socket just after a check, when the full interval remains.
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_file(daemon.socket()).unwrap();
    daemon.ensure();
    assert_eq!(
        daemon.pid(),
        original,
        "ensure killed a healthy daemon before its next socket check"
    );
}
