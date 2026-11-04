//! SSH connection policies, private socket directories and consistent master paths.
use super::*;

pub(super) const WAKE_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

const CONNECTION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

// Rows sharing a target must not race a failed health check against another
// row's replacement master. Check again after the preceding reset finishes.
static MASTER_CHECKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Retry policy for one backend. Startup preserves existing attach windows;
/// manual reconnect replaces their transport, and retries test SSH itself.
pub(super) struct Reconnect {
    check: bool,
    reset: bool,
    check_timeout: Duration,
}

impl Reconnect {
    pub(super) fn new(fresh: bool) -> Self {
        Self {
            check: false,
            reset: fresh,
            check_timeout: CONNECTION_CHECK_TIMEOUT,
        }
    }

    pub(super) fn after_wake(&mut self) {
        self.check = true;
        self.check_timeout = WAKE_CHECK_TIMEOUT;
    }

    pub(super) async fn prepare(
        &mut self,
        target: &str,
        options: &[String],
        log: &ConnLog,
    ) -> Result<(), String> {
        let lock = MASTER_CHECKS
            .lock()
            .unwrap()
            .entry(target.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.check && !self.reset {
            // `-O check` only asks the local master for its PID. A wedged
            // connection can answer that while every remote channel stalls.
            log.info("checking SSH responsiveness before retrying");
            let timeout = std::mem::replace(&mut self.check_timeout, CONNECTION_CHECK_TIMEOUT);
            if !responsive(target, options, timeout).await {
                log.error("SSH did not answer; replacing the shared connection");
                self.reset = true;
            }
        }
        if self.reset {
            reset_master(target, options).await?;
            // Retain the reset on timeout/refusal: the next attempt must not
            // silently rejoin the master the user asked us to replace.
            self.reset = false;
            log.info("previous SSH connection retired; dialing a fresh connection");
        }
        self.check = true;
        self.check_timeout = CONNECTION_CHECK_TIMEOUT;
        Ok(())
    }
}

pub(super) async fn responsive(target: &str, options: &[String], within: Duration) -> bool {
    let mut command = detached("ssh");
    command.args(options).arg(target).arg("true");
    bounded_status(command, within).await
}

async fn reset_master(target: &str, options: &[String]) -> Result<(), String> {
    let Some(endpoint) = resolve_control_options(target, options).await? else {
        // Explicit ControlPath=none already makes every dial independent.
        return Ok(());
    };
    let result = async {
        let child = detached("ssh")
            .args(endpoint)
            .args(["-O", "exit", target])
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| error.to_string())?;
        let (status, _, stderr) = capped_output(child, REMOTE_OUTPUT_CAP)
            .await
            .map_err(|error| error.to_string())?;
        if status.success()
            || (stderr.contains("Control socket connect(")
                && (stderr.contains("No such file or directory")
                    || stderr.contains("Connection refused")))
        {
            Ok(())
        } else {
            Err(format!(
                "Could not retire SSH connection: {}",
                stderr.trim()
            ))
        }
    };
    tokio::time::timeout(MUX_CONTROL_TIMEOUT, result)
        .await
        .map_err(|_| "Retiring SSH connection timed out; retrying before dialing".to_string())?
}

/// Fresh Git/work-tab connection, with forwarding decided by OpenSSH config.
/// Keep it independent of the non-forwarding dashboard master and any external
/// master, and end it with its owning command instead of persisting it.
/// Retain authentication/routing settings but override modes that would detach
/// this child, share a connection, or replace its session command. ssh's short
/// flags override even preceding -o settings, so strip them as well.
pub(super) fn session_options(options: &[String], tty: bool) -> Vec<String> {
    let mut opts: Vec<String> = [
        "ControlPath=none",
        "ControlMaster=no",
        "ControlPersist=no",
        "ForkAfterAuthentication=no",
        "StdinNull=no",
        "SessionType=default",
        "RemoteCommand=none",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "Tunnel=no",
    ]
    .into_iter()
    .flat_map(|option| ["-o".into(), option.into()])
    .collect();
    // Git cannot prompt on a detached child. Work tabs retain the existing
    // ability to override BatchMode through Advanced SSH options.
    if !tty {
        opts.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    let mut rest = options.iter();
    while let Some(arg) = rest.next() {
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            opts.push(arg.clone());
            continue;
        };
        let mut kept = String::from("-");
        for (index, flag) in flags.char_indices() {
            if "BbceEFIiJlmopPQSLRDOWw".contains(flag) {
                let value = &flags[index + flag.len_utf8()..];
                let separate = value.is_empty().then(|| rest.next()).flatten();
                if !"SOWwLRD".contains(flag) {
                    kept.push(flag);
                    kept.push_str(value);
                    opts.push(kept.clone());
                    if let Some(value) = separate {
                        opts.push(value.clone());
                    }
                    kept.clear();
                }
                break;
            }
            if !"fnNMtTs".contains(flag) {
                kept.push(flag);
            }
        }
        if kept.len() > 1 {
            opts.push(kept);
        }
    }
    // The short switches also win over conflicting command-line -o options.
    opts.extend([
        "-S".into(),
        "none".into(),
        if tty { "-t".into() } else { "-T".into() },
    ]);
    // Connection defaults remain overridable. Never supply ForwardAgent here:
    // OpenSSH owns its precedence, including -A/-a and agent socket paths.
    opts.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ]);
    opts
}

pub(super) async fn forward(
    target: &str,
    options: &[String],
    forward: &Forward,
    add: bool,
) -> Result<(), String> {
    let result = async {
        let child = detached("ssh")
            .args(options)
            .args(["-O", if add { "forward" } else { "cancel" }])
            .arg(&forward.flag)
            .arg(&forward.spec)
            .arg(target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| error.to_string())?;
        let (status, _, stderr) = capped_output(child, REMOTE_OUTPUT_CAP)
            .await
            .map_err(|error| error.to_string())?;
        forwards::control_result(status.success(), &stderr, add)
    };
    tokio::time::timeout(MUX_CONTROL_TIMEOUT, result)
        .await
        .map_err(|_| "SSH forwarding request timed out; listener state is uncertain".to_string())?
}

/// Resolve with the dial's full configuration before discarding options that
/// could add unrelated forwards to a mux control command. ControlPath tokens
/// depend on HostName/User/Port (and potentially ProxyJump), not just the text
/// of ControlPath itself.
pub(super) async fn control_options(
    target: &str,
    options: &[String],
) -> Result<Vec<String>, String> {
    resolve_control_options(target, options)
        .await?
        .ok_or_else(|| "SSH multiplexing is disabled; no control socket is available".to_string())
}

async fn resolve_control_options(
    target: &str,
    options: &[String],
) -> Result<Option<Vec<String>>, String> {
    let child = detached("ssh")
        .arg("-G")
        .args(options)
        .arg(target)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not resolve SSH control socket: {error}"))?;
    let (status, stdout, stderr) =
        tokio::time::timeout(MUX_CONTROL_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| "Resolving SSH control socket timed out".to_string())?
            .map_err(|error| format!("Could not resolve SSH control socket: {error}"))?;
    if !status.success() {
        return Err(format!(
            "Could not resolve SSH control socket: {}",
            stderr.trim()
        ));
    }
    let Some(path) = stdout
        .lines()
        .find_map(|line| line.strip_prefix("controlpath "))
        .filter(|path| !path.is_empty() && *path != "none")
    else {
        return Ok(None);
    };
    // The path has already been expanded. Preserve literal percent signs when
    // OpenSSH processes the concrete -S value a second time.
    Ok(Some(vec![
        "-F".into(),
        "/dev/null".into(),
        "-S".into(),
        path.replace('%', "%%"),
    ]))
}

pub(super) fn prepare_socket_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Check and chmod the same inode. Never follow a pre-created symlink or
    // chmod another user's directory under a shared temporary directory.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    if directory.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socket directory is owned by another user",
        ));
    }
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SESSION: u32 = 0x10000002;
    const EXIT: u32 = 0x10000005;

    async fn receive_stdio(stream: &UnixStream) {
        let mut received = 0;
        while received < 3 {
            stream.readable().await.unwrap();
            let result = stream.try_io(tokio::io::Interest::READABLE, || {
                // OpenSSH versions may send descriptors separately or together.
                // Receive and close all passed copies, rather than assuming a
                // number of marker bytes or leaving a sender mid-send on close.
                let mut control = [0usize; 16];
                let mut byte = 0u8;
                let mut iov = libc::iovec {
                    iov_base: (&mut byte as *mut u8).cast(),
                    iov_len: 1,
                };
                // SAFETY: the buffers and iovec remain live for recvmsg; only
                // SCM_RIGHTS descriptors returned in our buffer are closed.
                unsafe {
                    let mut msg: libc::msghdr = std::mem::zeroed();
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control.as_mut_ptr().cast();
                    msg.msg_controllen = std::mem::size_of_val(&control) as _;
                    if libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    assert_eq!(msg.msg_flags & libc::MSG_CTRUNC, 0);
                    let mut count = 0;
                    let mut header = libc::CMSG_FIRSTHDR(&msg);
                    while !header.is_null() {
                        if (*header).cmsg_level == libc::SOL_SOCKET
                            && (*header).cmsg_type == libc::SCM_RIGHTS
                        {
                            let n = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                                / std::mem::size_of::<libc::c_int>();
                            let fds = libc::CMSG_DATA(header).cast::<libc::c_int>();
                            for i in 0..n {
                                libc::close(*fds.add(i));
                            }
                            count += n;
                        }
                        header = libc::CMSG_NXTHDR(&msg, header);
                    }
                    Ok(count)
                }
            });
            match result {
                Ok(count) => {
                    assert!(count > 0);
                    received += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("receiving SSH stdio: {error}"),
            }
        }
        assert_eq!(received, 3);
    }

    // A local OpenSSH mux peer. Use the real ssh client, but no network host,
    // daemon deployment, global state, or timing-dependent shell scripts.
    struct Master {
        _dir: tempfile::TempDir,
        options: Vec<String>,
        requests: mpsc::UnboundedReceiver<(u32, oneshot::Sender<bool>)>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Master {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Master {
        fn new() -> Self {
            let dir = tempfile::tempdir_in("/tmp").unwrap();
            let path = dir.path().join("master");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let options = ssh_common_opts(&path, &["-F".into(), "/dev/null".into()]);
            let (tx, requests) = mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                let mut peers = tokio::task::JoinSet::new();
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let tx = tx.clone();
                    peers.spawn(async move {
                        while let Ok(length) = stream.read_u32().await {
                            assert!(length < 4096);
                            let mut frame = vec![0; length as usize];
                            stream.read_exact(&mut frame).await.unwrap();
                            let word = |i| u32::from_be_bytes(frame[i..i + 4].try_into().unwrap());
                            let kind = word(0);
                            let reply = match kind {
                                1 => vec![1, 4],
                                0x10000004 => vec![0x80000005, word(4), std::process::id()],
                                SESSION | EXIT => {
                                    if kind == SESSION {
                                        receive_stdio(&stream).await;
                                    }
                                    let (release, wait) = oneshot::channel::<bool>();
                                    tx.send((kind, release)).unwrap();
                                    let Ok(accepted) = wait.await else {
                                        return;
                                    };
                                    if !accepted {
                                        let message = b"test refusal";
                                        stream.write_u32(12 + message.len() as u32).await.unwrap();
                                        for word in [0x80000003, word(4), message.len() as u32] {
                                            stream.write_u32(word).await.unwrap();
                                        }
                                        stream.write_all(message).await.unwrap();
                                        return;
                                    }
                                    if kind == EXIT {
                                        vec![0x80000001, word(4)]
                                    } else {
                                        vec![0x80000006, word(4), 1]
                                    }
                                }
                                _ => panic!("unexpected mux request {kind:x}"),
                            };
                            let bytes: Vec<_> =
                                reply.into_iter().flat_map(u32::to_be_bytes).collect();
                            stream.write_u32(bytes.len() as u32).await.unwrap();
                            stream.write_all(&bytes).await.unwrap();
                            if kind == SESSION {
                                stream.write_u32(12).await.unwrap();
                                for word in [0x80000004, 1, 0] {
                                    stream.write_u32(word).await.unwrap();
                                }
                                return;
                            }
                        }
                    });
                }
            });
            Self {
                _dir: dir,
                options,
                requests,
                task,
            }
        }

        async fn next(&mut self, expected: u32) -> oneshot::Sender<bool> {
            let (kind, release) =
                tokio::time::timeout(Duration::from_secs(40), self.requests.recv())
                    .await
                    .expect("awaiting SSH mux operation beyond its production deadline")
                    .unwrap();
            assert_eq!(kind, expected);
            release
        }
    }

    #[tokio::test]
    async fn manual_reconnect_waits_for_master_exit_before_dialing() {
        let mut master = Master::new();
        let options = master.options.clone();
        let task = tokio::spawn(async move {
            Reconnect::new(true)
                .prepare("manual-reconnect.invalid", &options, &ConnLog::default())
                .await
        });
        let exit = master.next(EXIT).await;
        assert!(
            !task.is_finished(),
            "dialing must wait for the old master to acknowledge exit"
        );
        exit.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("awaiting acknowledged SSH retirement")
            .unwrap()
            .unwrap();
        assert!(
            master.requests.try_recv().is_err(),
            "manual reconnect must not check/reuse the old connection"
        );
    }

    #[tokio::test]
    async fn wake_replaces_a_stalled_master_without_the_normal_five_second_check() {
        let mut master = Master::new();
        let options = master.options.clone();
        let task = tokio::spawn(async move {
            let mut reconnect = Reconnect::new(false);
            reconnect.after_wake();
            reconnect
                .prepare("wake-test.invalid", &options, &ConnLog::default())
                .await
        });
        let _held_reply = master.next(SESSION).await;
        // Real subprocess I/O: watchdog is longer than the production deadline,
        // but shorter than the ordinary five-second stale-master check.
        let exit = tokio::time::timeout(
            WAKE_CHECK_TIMEOUT + Duration::from_secs(3),
            master.next(EXIT),
        )
        .await
        .expect("waiting for wake-time SSH retirement");
        exit.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("waiting for retired master")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn retry_replaces_stalled_ssh_but_preserves_a_responsive_master() {
        for stalled in [false, true] {
            let mut master = Master::new();
            let options = master.options.clone();
            let task = tokio::spawn(async move {
                let mut reconnect = Reconnect::new(false);
                reconnect
                    .prepare("retry-test.invalid", &options, &ConnLog::default())
                    .await
                    .unwrap();
                reconnect
                    .prepare("retry-test.invalid", &options, &ConnLog::default())
                    .await
            });
            let session = master.next(SESSION).await;
            if stalled {
                // Local PID checks succeeded, but a remote request never replies.
                // Keep it pending through the real production check deadline.
                let exit = master.next(EXIT).await;
                assert!(!task.is_finished());
                exit.send(true).unwrap();
            } else {
                session.send(true).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .expect("awaiting SSH retry preparation")
                .unwrap()
                .unwrap();
            assert!(master.requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn fresh_connection_accepts_an_absent_or_disabled_master() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        for path in [dir.path().join("absent"), PathBuf::from("none")] {
            let options = ssh_common_opts(&path, &["-F".into(), "/dev/null".into()]);
            Reconnect::new(true)
                .prepare("absent.invalid", &options, &ConnLog::default())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn refused_retirement_is_retried_before_any_remote_command() {
        let mut master = Master::new();
        let options = master.options.clone();
        let task = tokio::spawn(async move {
            let mut reconnect = Reconnect::new(true);
            let error = reconnect
                .prepare("refused.invalid", &options, &ConnLog::default())
                .await
                .unwrap_err();
            assert!(error.contains("test refusal"), "{error}");
            reconnect
                .prepare("refused.invalid", &options, &ConnLog::default())
                .await
        });
        master.next(EXIT).await.send(false).unwrap();
        master.next(EXIT).await.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("awaiting retried SSH retirement")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn controls_use_the_dials_expanded_socket_without_configured_forwards() {
        let root = scratch_home("control-path-context");
        let config = root.join("config");
        std::fs::write(&config, "Host test-target\n HostName example.invalid\n Port 2222\n User test-user\n LocalForward 9000 localhost:9000\n").unwrap();
        let options = ssh_common_opts(
            &root.join("default"),
            &[
                "-F".into(),
                config.to_string_lossy().into_owned(),
                "-oControlPath=/tmp/cm-test-%h-%r-%p-%%".into(),
                "-L3000:localhost:3000".into(),
            ],
        );
        let controls = control_options("test-target", &options).await.unwrap();
        assert_eq!(
            &controls,
            &[
                "-F",
                "/dev/null",
                "-S",
                "/tmp/cm-test-example.invalid-test-user-2222-%%"
            ]
        );
        let output = detached("ssh")
            .arg("-G")
            .args(&controls)
            .arg("test-target")
            .stdout(Stdio::piped())
            .output()
            .await
            .unwrap();
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("controlpath /tmp/cm-test-example.invalid-test-user-2222-%\n"));
        assert!(!output.contains("localforward "));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_directory_rejects_symlinks_and_files() {
        let root = scratch_home("socket-dir-check");
        let actual = root.join("actual");
        std::fs::create_dir(&actual).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&actual, &link).unwrap();
        let file = root.join("file");
        std::fs::write(&file, "fixture").unwrap();
        let symlink_result = prepare_socket_dir(&link);
        let file_result = prepare_socket_dir(&file);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            symlink_result.is_err(),
            "must not follow a pre-created symlink"
        );
        assert!(file_result.is_err(), "must not accept a regular file");
    }

    #[test]
    fn socket_directory_is_private_on_creation_and_upgrade() {
        let root = scratch_home("socket-dir-modes");
        let dir = root.join("sockets");
        prepare_socket_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        prepare_socket_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
