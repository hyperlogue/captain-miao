#![cfg(feature = "pty-pool")]

//! A real private pool daemon and a tiny terminal application. No agent login,
//! user's pool, or terminal emulator is involved: inspect the actual bytes a
//! newly attached terminal receives before the application's next response.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};
use shpool_protocol::{
    AttachHeader, AttachReplyHeader, AttachStatus, ConnectHeader, DetachReply, DetachRequest,
    KillReply, KillRequest, TtySize, VersionHeader,
};

fn send(stream: &UnixStream, message: &impl Serialize) {
    message
        .serialize(&mut rmp_serde::Serializer::new(stream).with_struct_map())
        .unwrap();
}

fn receive<T: DeserializeOwned>(stream: &UnixStream) -> T {
    rmp_serde::from_read(stream).unwrap()
}

struct Pool {
    root: tempfile::TempDir,
    daemon: Child,
}

impl Pool {
    fn new() -> Self {
        // Clock readings can repeat across parallel tests. Reserve a private
        // directory atomically, under a short path for macOS's Unix sockets.
        let root = tempfile::Builder::new()
            .prefix("cm-pool-")
            .tempdir_in("/tmp")
            .unwrap();
        // Keyboard stacks differ on purpose: returning from the alternate
        // screen must recover the primary screen's flags too.
        std::fs::write(
            root.path().join("app.sh"),
            r"stty -echo
printf '\033[>7u\033[?2004hREADY'
while :; do
  IFS= read -r command || continue
  case $command in
    alt) printf '\033[?1049h\033[?1007h\033[>5uALT_READY';;
    primary) printf '\033[<u\033[?1007l\033[?1049lPRIMARY_READY';;
    wait) printf WAITING; IFS= read -r update < control; printf '\033[<u\033[?1007l\033[?1049lPRIMARY_READY';;
    check) printf CHECK;;
  esac
done
",
        )
        .unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(root.path().join("control"))
                .status()
                .unwrap()
                .success()
        );
        let daemon = Command::new(env!("CARGO_BIN_EXE_miao-server"))
            .arg("pty-daemon")
            .env("HOME", root.path())
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pool = Self { root, daemon };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pool.socket().exists() {
            assert!(Instant::now() < deadline, "pool did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        pool
    }

    fn socket(&self) -> PathBuf {
        self.root.path().join("run/captain-miao/pty-pool.sock")
    }

    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let _: VersionHeader = receive(&stream);
        stream
    }

    fn attach(&self, create: bool) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stream = self.connect();
            send(
                &stream,
                &ConnectHeader::Attach(AttachHeader {
                    name_template: "mode-test".into(),
                    name: "mode-test".into(),
                    local_tty_size: TtySize {
                        rows: 24,
                        cols: 80,
                        xpixel: 0,
                        ypixel: 0,
                    },
                    local_env: vec![("TERM".into(), "xterm-256color".into())],
                    ttl_secs: None,
                    cmd: create.then(|| "/bin/sh app.sh".into()),
                    dir: Some(self.root.path().to_string_lossy().into_owned()),
                    start_cmd: None,
                }),
            );
            let reply: AttachReplyHeader = receive(&stream);
            if reply.status == AttachStatus::Busy && !create {
                assert!(
                    Instant::now() < deadline,
                    "previous attach did not release the session"
                );
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            assert!(
                matches!(
                    reply.status,
                    AttachStatus::Created { .. } | AttachStatus::Attached { .. }
                ),
                "{:?}",
                reply.status
            );
            return stream;
        }
    }

    fn detach(&self) {
        let stream = self.connect();
        send(
            &stream,
            &ConnectHeader::Detach(DetachRequest {
                sessions: vec!["mode-test".into()],
            }),
        );
        let _: DetachReply = receive(&stream);
    }

    fn input_fixture(&self, agent: &str) {
        let tool = |name: &str| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .filter(|dir| dir.join(name).is_file())
                .find_map(|dir| dir.canonicalize().ok().map(|dir| dir.join(name)))
                .expect("tool on test PATH")
        };
        // GNU stty's raw mode leaves IEXTEN set; macOS still interprets Ctrl+V
        // as literal-next with that flag, even when canonical mode is off.
        std::fs::write(
            self.root.path().join("app.sh"),
            format!(
                "{} raw -echo -iexten\nprintf READY\nexec {}\n",
                shell_words::quote(&tool("stty").to_string_lossy()),
                shell_words::quote(&tool("cat").to_string_lossy())
            ),
        )
        .unwrap();
        #[derive(Serialize)]
        struct State<'a> {
            agent: &'a str,
            launcher_pid: u32,
            cwd: &'a str,
            status: cm_core::state::SessionStatus,
            updated_at: u64,
            pool_session: &'a str,
        }
        let path = self
            .root
            .path()
            .join("state/captain-miao/sessions")
            .join(format!("{}.json", std::process::id()));
        cm_core::state::create_dir_all_private(path.parent().unwrap()).unwrap();
        cm_core::state::write_json_atomic(
            &path,
            &State {
                agent,
                launcher_pid: std::process::id(),
                cwd: "/work/repo",
                status: cm_core::state::SessionStatus::Idle,
                updated_at: 0,
                pool_session: "mode-test",
            },
        )
        .unwrap();
        assert!(cm_core::state::read_json::<cm_core::state::LauncherState>(&path).is_some());
    }

    fn clipboard(&self) -> UnixListener {
        UnixListener::bind(self.root.path().join("run/captain-miao/clipboard.sock")).unwrap()
    }

    fn images(&self) -> PathBuf {
        self.root
            .path()
            .join("run/captain-miao/launchers")
            .join(format!("{}-images", std::process::id()))
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(stream) = UnixStream::connect(self.socket()) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            if rmp_serde::from_read::<_, VersionHeader>(&stream).is_ok() {
                let _ = ConnectHeader::Kill(KillRequest {
                    sessions: vec!["mode-test".into()],
                })
                .serialize(&mut rmp_serde::Serializer::new(&stream).with_struct_map());
                let _ = rmp_serde::from_read::<_, KillReply>(&stream);
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn read_until(stream: &mut UnixStream, marker: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no application response");
        let mut kind = [0];
        stream.read_exact(&mut kind).unwrap();
        if kind[0] == 1 {
            continue;
        }
        assert_eq!(
            kind[0],
            0,
            "session exited unexpectedly: {}",
            String::from_utf8_lossy(&output)
        );
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let length = u32::from_le_bytes(length) as usize;
        assert!(length < 65536);
        let start = output.len();
        output.resize(start + length, 0);
        stream.read_exact(&mut output[start..]).unwrap();
        if output.windows(marker.len()).any(|part| part == marker) {
            return output;
        }
    }
}

#[test]
fn reattach_restores_an_active_alternate_screen_before_live_output() {
    let pool = Pool::new();
    let mut first = pool.attach(true);
    let startup = read_until(&mut first, b"READY");
    first.write_all(b"alt\n").unwrap();
    let entered = read_until(&mut first, b"ALT_READY");
    assert!(
        entered.windows(8).any(|part| part == b"\x1b[?1049h"),
        "fixture must enter the alternate screen"
    );
    pool.detach();
    drop(first);

    let mut second = pool.attach(false);
    second.write_all(b"check\n").unwrap();
    let output = read_until(&mut second, b"CHECK");
    for mode in [b"\x1b[?1049h".as_slice(), b"\x1b[?1007h", b"\x1b[>5u"] {
        assert!(
            output.windows(mode.len()).any(|part| part == mode),
            "reattach did not restore {mode:?} before live output: {output:?}"
        );
    }

    let mut expected = cm_core::terminal_modes::TerminalModes::default();
    expected.process(&startup);
    expected.process(&entered);
    let mut restored = cm_core::terminal_modes::TerminalModes::default();
    restored.process(&output);
    assert_eq!(
        restored.restore_buffer(),
        expected.restore_buffer(),
        "the new terminal must recover both keyboard stacks and the selected screen"
    );

    // The application leaves its overlay while no client is connected.
    second.write_all(b"wait\n").unwrap();
    read_until(&mut second, b"WAITING");
    pool.detach();
    drop(second);
    std::fs::OpenOptions::new()
        .write(true)
        .open(pool.root.path().join("control"))
        .unwrap()
        .write_all(b"primary\n")
        .unwrap();
    let mut third = pool.attach(false);
    third.write_all(b"check\n").unwrap();
    let output = read_until(&mut third, b"CHECK");
    expected.process(b"\x1b[<u\x1b[?1007l\x1b[?1049l");
    let mut restored = cm_core::terminal_modes::TerminalModes::default();
    restored.process(&output);
    assert_eq!(
        restored.restore_buffer(),
        expected.restore_buffer(),
        "detached output must update the mode state before the next response"
    );
}

#[test]
fn codex_ctrl_v_fetches_distinct_images_and_survives_reattach() {
    let pool = Pool::new();
    pool.input_fixture("codex");
    let clipboard = pool.clipboard();
    let serve = std::thread::spawn(move || {
        for image in [b"first image".as_slice(), b"second image"] {
            let (mut stream, _) = clipboard.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 13];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"v1 image png\n");
            stream
                .write_all(format!("v1 image png\n{}\n", image.len()).as_bytes())
                .unwrap();
            stream.write_all(image).unwrap();
            stream.write_all(b"0\n").unwrap();
        }
    });
    let mut client = pool.attach(true);
    read_until(&mut client, b"READY");
    client.write_all(b"draft \x16 after").unwrap();
    let first = read_until(&mut client, b" after");
    let first = String::from_utf8(first).unwrap();
    let path = first
        .strip_prefix("draft \x1b[200~file://")
        .unwrap()
        .strip_suffix("\x1b[201~ after")
        .unwrap();
    let first_path = PathBuf::from(path);
    assert_eq!(std::fs::read(&first_path).unwrap(), b"first image");
    assert_eq!(
        std::fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(pool.images())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    pool.detach();
    drop(client);
    let mut client = pool.attach(false);
    // Consume the restored terminal modes before checking the transformed input.
    client.write_all(b"SYNC").unwrap();
    read_until(&mut client, b"SYNC");
    client.write_all(b"\x1b[118:").unwrap();
    client.write_all(b"86;5u after").unwrap();
    let second = read_until(&mut client, b" after");
    let second = String::from_utf8(second).unwrap();
    let second_path = PathBuf::from(
        second
            .strip_prefix("\x1b[200~file://")
            .unwrap()
            .strip_suffix("\x1b[201~ after")
            .unwrap(),
    );
    assert_ne!(first_path, second_path);
    assert_eq!(std::fs::read(first_path).unwrap(), b"first image");
    assert_eq!(std::fs::read(second_path).unwrap(), b"second image");
    serve.join().unwrap();
}

#[test]
fn other_agents_and_bracketed_text_keep_their_input_without_a_clipboard_read() {
    for agent in ["claude", "codex"] {
        let pool = Pool::new();
        pool.input_fixture(agent);
        let clipboard = pool.clipboard();
        clipboard.set_nonblocking(true).unwrap();
        let mut client = pool.attach(true);
        read_until(&mut client, b"READY");
        let bytes = if agent == "claude" {
            b"\x16\x1b[118;5u!".as_slice()
        } else {
            b"\x1b[200~literal \x16 \x1b[118;5u\x1b[201~!"
        };
        client.write_all(bytes).unwrap();
        assert_eq!(read_until(&mut client, b"!"), bytes);
        assert_eq!(
            clipboard.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        client.write_all(b"\x1b").unwrap();
        assert_eq!(
            read_until(&mut client, b"\x1b"),
            b"\x1b",
            "standalone Escape must flush"
        );
    }
}

#[test]
fn detach_cancels_a_stalled_clipboard_download_and_removes_the_partial_file() {
    let pool = Pool::new();
    pool.input_fixture("codex");
    let clipboard = pool.clipboard();
    let mut client = pool.attach(true);
    read_until(&mut client, b"READY");
    client.write_all(b"\x16").unwrap();
    let (mut fetch, _) = clipboard.accept().unwrap();
    fetch
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = [0; 13];
    fetch.read_exact(&mut request).unwrap();
    fetch.write_all(b"v1 image png\n100\npartial").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pool.images().exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = Instant::now();
    pool.detach();
    drop(client);
    let mut client = pool.attach(false);
    client.write_all(b"SYNC").unwrap();
    read_until(&mut client, b"SYNC");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "detach waited for the clipboard deadline"
    );
    assert_eq!(std::fs::read_dir(pool.images()).unwrap().count(), 0);
}

#[test]
fn a_missing_clipboard_bridge_preserves_codex_ctrl_v() {
    let pool = Pool::new();
    pool.input_fixture("codex");
    let mut client = pool.attach(true);
    read_until(&mut client, b"READY");
    client.write_all(b"\x16done").unwrap();
    assert_eq!(read_until(&mut client, b"done"), b"\x16done");
    assert!(!pool.images().exists());
}
