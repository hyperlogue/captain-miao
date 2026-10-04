#![cfg(all(feature = "pty-pool", target_os = "linux"))]

//! Exercise discovery through real CLI processes and a real pooled child. Every
//! state, config and socket path belongs to the fixture, never the user's daemon.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use shpool_protocol::{ConnectHeader, ListReply, VersionHeader};

struct Host {
    root: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("cm-paths-")
            .tempdir_in("/tmp")
            .unwrap();
        for dir in ["home", "state", "config", "cache", "runtime-a", "runtime-b"] {
            cm_core::state::create_dir_all_private(&root.path().join(dir)).unwrap();
        }
        // The pool clears PATH before exec; Nix's tools are not in /usr/bin.
        // Record readiness from inside the PTY and verify the child, since a
        // detached pool entry can outlive a command that failed to exec.
        let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|dir| dir.join("sleep").is_file())
            // Preserve the executable name: Nix coreutils can dispatch on
            // argv[0] through symlinks into a multicall binary.
            .find_map(|dir| dir.canonicalize().ok().map(|dir| dir.join("sleep")))
            .expect("sleep in the test toolchain");
        std::fs::write(
            root.path().join("pooled-child.sh"),
            format!(
                "printf '%s\\n' \"$$\" > \"$1\"\nexec {} 120\n",
                shell_words::quote(&sleep.to_string_lossy())
            ),
        )
        .unwrap();
        Self { root }
    }

    fn command(&self, runtime: Option<&str>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_miao-server"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.root.path().join("home"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null());
        if let Some(runtime) = runtime {
            cmd.env("XDG_RUNTIME_DIR", self.root.path().join(runtime));
        }
        cmd
    }

    fn ensure(&self, runtime: Option<&str>) -> Output {
        self.command(runtime)
            .args(["daemon", "ensure"])
            .output()
            .unwrap()
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state/captain-miao")
    }

    fn pid(&self) -> u32 {
        std::fs::read_to_string(self.state().join("server.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn start(&self) -> PathBuf {
        let result = self.ensure(Some("runtime-a"));
        assert_success(&result);
        let socket = output_path(&result);
        until(|| UnixStream::connect(&socket).is_ok());
        socket
    }

    fn create_session(&self, runtime: Option<&str>, name: &str) -> u32 {
        let ready = self.root.path().join(format!("{name}.pid"));
        let cmd = format!(
            "/bin/sh {} {}",
            shell_words::quote(&self.root.path().join("pooled-child.sh").to_string_lossy()),
            shell_words::quote(&ready.to_string_lossy())
        );
        let result = self
            .command(runtime)
            .args(["attach", name, "--background", "--cmd", &cmd])
            .arg("--dir")
            .arg(self.root.path())
            .arg("--log-file")
            .arg(self.root.path().join("attach.log"))
            .output()
            .unwrap();
        assert_success(&result);
        let read_pid = || {
            std::fs::read_to_string(&ready)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
        };
        until(|| read_pid().is_some());
        let pid = read_pid().unwrap();
        assert!(cm_core::state::is_process_alive(pid));
        pid
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // The pid file is inside our private fixture. Resume a paused daemon
        // before asking it to stop, including when an assertion failed.
        if let Ok(pid) = std::fs::read_to_string(self.state().join("server.pid"))
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            if pid <= 0 || pid as u32 == std::process::id() {
                return; // the stale-pid fixture never owns the test runner
            }
            unsafe { libc::kill(pid, libc::SIGCONT) };
        }
        let _ = self
            .command(None)
            .args(["daemon", "stop", "--force"])
            .output();
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_path(output: &Output) -> PathBuf {
    PathBuf::from(String::from_utf8(output.stdout.clone()).unwrap().trim())
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sessions(control: &Path) -> Vec<String> {
    let mut stream = UnixStream::connect(control.with_file_name("pty-pool.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _: VersionHeader = rmp_serde::from_read(&stream).unwrap();
    ConnectHeader::List
        .serialize(&mut rmp_serde::Serializer::new(&mut stream).with_struct_map())
        .unwrap();
    stream.flush().unwrap();
    let reply: ListReply = rmp_serde::from_read(&stream).unwrap();
    let mut names: Vec<_> = reply.sessions.into_iter().map(|s| s.name).collect();
    names.sort();
    names
}

#[test]
fn changing_runtime_environment_preserves_the_daemon_and_pool() {
    let host = Host::new();
    let socket = host.start();
    let pid = host.pid();
    let child = host.create_session(Some("runtime-a"), "survivor");
    assert_eq!(sessions(&socket), ["survivor"]);

    let mut expected = vec!["survivor".to_string()];
    for (runtime, name) in [(None, "second"), (Some("runtime-b"), "third")] {
        let result = host.ensure(runtime);
        assert_success(&result);
        until(|| UnixStream::connect(output_path(&result)).is_ok());
        assert_eq!(host.pid(), pid, "ensure replaced the session-owning daemon");
        assert!(cm_core::state::is_process_alive(child), "pooled child died");
        assert_eq!(output_path(&result), socket);
        assert_eq!(sessions(&socket), expected);
        host.create_session(runtime, name);
        expected.push(name.to_string());
        expected.sort();
        assert_eq!(sessions(&socket), expected);
        assert!(cm_core::state::is_process_alive(child), "pooled child died");
    }
    assert_eq!(socket, host.state().join("run/server.sock"));
}

#[test]
fn an_unreachable_live_daemon_is_not_terminated() {
    let host = Host::new();
    let socket = host.start();
    let pid = host.pid();
    let child = host.create_session(Some("runtime-a"), "survivor");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
    // Stop self-rebinding to model a genuinely unavailable daemon. Its PTY
    // and child remain live, so a timeout must not turn into a restart.
    std::fs::remove_file(&socket).unwrap();
    let result = host.ensure(None);
    assert!(
        !result.status.success(),
        "unreachable daemon reported ready"
    );
    assert!(
        result.stdout.is_empty(),
        "failure advertised a usable socket"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("leaving the daemon"));
    assert_eq!(host.pid(), pid);
    assert!(cm_core::state::is_process_alive(pid));
    assert!(cm_core::state::is_process_alive(child));
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGCONT) }, 0);
    until(|| UnixStream::connect(&socket).is_ok());
    assert_eq!(sessions(&socket), ["survivor"]);
}

#[test]
fn legacy_sockets_are_reused_until_the_daemon_exits() {
    let host = Host::new();
    let socket = host.start();
    let pid = host.pid();
    let child = host.create_session(Some("runtime-a"), "survivor");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
    let legacy = host.root.path().join("runtime-a/captain-miao");
    cm_core::state::create_dir_all_private(&legacy).unwrap();
    // Moving the two live listener inodes models the old daemon's placement.
    // Pause first so its rebind tick cannot recreate the canonical pathname.
    for name in ["server.sock", "pty-pool.sock"] {
        std::fs::rename(socket.with_file_name(name), legacy.join(name)).unwrap();
    }
    let migrations: Vec<_> = [Some("runtime-b"), None, Some("runtime-a")]
        .into_iter()
        .map(|runtime| {
            host.command(runtime)
                .args(["daemon", "ensure"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in migrations {
        let result = child.wait_with_output().unwrap();
        assert_success(&result);
        assert_eq!(output_path(&result), socket);
    }
    assert_eq!(host.pid(), pid);
    for name in ["server.sock", "pty-pool.sock"] {
        assert_eq!(
            std::fs::read_link(socket.with_file_name(name)).unwrap(),
            legacy.join(name)
        );
    }
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGCONT) }, 0);
    assert_eq!(sessions(&socket), ["survivor"]);
    host.create_session(None, "second");
    assert_eq!(sessions(&socket), ["second", "survivor"]);
    assert!(cm_core::state::is_process_alive(child));

    assert_success(
        &host
            .command(None)
            .args(["daemon", "stop", "--force"])
            .output()
            .unwrap(),
    );
    host.start();
    assert_ne!(host.pid(), pid);
    assert!(std::fs::read_link(&socket).is_err());
    assert!(std::fs::read_link(socket.with_file_name("pty-pool.sock")).is_err());
    assert!(sessions(&socket).is_empty());
}

#[test]
fn concurrent_ensure_calls_share_one_daemon_across_environments() {
    let host = Host::new();
    let children: Vec<_> = [Some("runtime-a"), None, Some("runtime-b"), None]
        .into_iter()
        .map(|runtime| {
            host.command(runtime)
                .args(["daemon", "ensure"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let result = child.wait_with_output().unwrap();
        assert_success(&result);
        assert_eq!(output_path(&result), host.state().join("run/server.sock"));
    }
    let pid = host.pid();
    for runtime in [None, Some("runtime-b")] {
        assert_success(&host.ensure(runtime));
        assert_eq!(host.pid(), pid);
    }
}

#[test]
fn stale_pid_and_socket_files_do_not_prevent_a_new_daemon() {
    let host = Host::new();
    let dir = host.state().join("run");
    cm_core::state::create_dir_all_private(&dir).unwrap();
    // A live unrelated PID is not a daemon: only the held flock establishes
    // ownership. No recovery path may signal the process named in stale data.
    std::fs::write(
        host.state().join("server.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    for name in ["server.sock", "pty-pool.sock"] {
        drop(std::os::unix::net::UnixListener::bind(dir.join(name)).unwrap());
    }
    let socket = host.start();
    assert_ne!(host.pid(), std::process::id());
    assert!(sessions(&socket).is_empty());
}

#[test]
fn an_overlong_fixed_socket_path_fails_before_starting_a_daemon() {
    let host = Host::new();
    let result = host
        .command(None)
        .env("XDG_STATE_HOME", host.root.path().join("x".repeat(120)))
        .args(["daemon", "ensure"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("socket path is too long"));
    assert!(!host.state().join("server.pid").exists());
}
