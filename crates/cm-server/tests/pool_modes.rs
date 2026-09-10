#![cfg(feature = "pty-pool")]

//! A real private pool daemon and a tiny terminal application. No agent login,
//! user's pool, or terminal emulator is involved: inspect the actual bytes a
//! newly attached terminal receives before the application's next response.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
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
    root: PathBuf,
    daemon: Child,
}

impl Pool {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cm-mode-pool-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Keyboard stacks differ on purpose: returning from the alternate
        // screen must recover the primary screen's flags too.
        std::fs::write(
            root.join("app.sh"),
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
                .arg(root.join("control"))
                .status()
                .unwrap()
                .success()
        );
        let daemon = Command::new(env!("CARGO_BIN_EXE_miao-server"))
            .arg("pty-daemon")
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
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
        self.root.join("run/captain-miao/pty-pool.sock")
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
                    dir: Some(self.root.to_string_lossy().into_owned()),
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
}

impl Drop for Pool {
    fn drop(&mut self) {
        if self.socket().exists() {
            let stream = self.connect();
            send(
                &stream,
                &ConnectHeader::Kill(KillRequest {
                    sessions: vec!["mode-test".into()],
                }),
            );
            let _: KillReply = receive(&stream);
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
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
        assert_eq!(kind[0], 0, "session exited unexpectedly");
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
        .open(pool.root.join("control"))
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
