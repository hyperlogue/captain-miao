//! Live user forwards on the host's existing SSH master. Port edits do not
//! replace the session mirror, daemon tunnel, or attach connections.
//!
//! One async lock serializes edits, reconnects and cleanup. A failed edit keeps
//! the previous desired set and attempts to restore its listeners; the status
//! map records restoration failures instead of claiming a rollback succeeded.

use super::*;
use crate::ssh_forward::{Forward, Rule};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Off,
    Waiting,
    Listening,
    Failed(String),
}

#[async_trait::async_trait]
trait Control: Send + Sync {
    async fn execute(&self, forward: &Forward, add: bool) -> Result<(), String>;
}

struct Owner {
    generation: u64,
    host: HostId,
    active: bool,
    retiring: Arc<AtomicBool>,
}
type Ownership = Arc<tokio::sync::Mutex<Option<Owner>>>;
static OWNERS: LazyLock<Mutex<HashMap<(String, Forward), Ownership>>> =
    LazyLock::new(Mutex::default);
static GENERATION: AtomicU64 = AtomicU64::new(1);

struct SshControl {
    target: String,
    host: HostId,
    generation: u64,
    retiring: Arc<AtomicBool>,
    options: Vec<String>,
    endpoint: tokio::sync::OnceCell<Vec<String>>,
}

#[async_trait::async_trait]
impl Control for SshControl {
    async fn execute(&self, f: &Forward, add: bool) -> Result<(), String> {
        let options = self
            .endpoint
            .get_or_try_init(|| ssh::control_options(&self.target, &self.options))
            .await?;
        let ownership = OWNERS
            .lock()
            .unwrap()
            .entry((self.target.clone(), f.clone()))
            .or_default()
            .clone();
        let mut owner = ownership.lock().await;
        if let Some(current) = &*owner {
            // Old backend cleanup can finish after its replacement connects.
            // It must never cancel a listener the replacement has taken over.
            if current.generation > self.generation {
                return if add {
                    Err("SSH connection was replaced; retry on the new connection".into())
                } else {
                    Ok(())
                };
            }
            if current.active
                && current.host != self.host
                && !current.retiring.load(Ordering::Relaxed)
            {
                return Err("This listener is managed by another host entry".into());
            }
        }
        // Reserve ownership before the request: a timed-out add may already
        // have installed its listener, so an older backend must not touch it.
        *owner = Some(Owner {
            generation: self.generation,
            host: self.host.clone(),
            active: true,
            retiring: self.retiring.clone(),
        });
        let mut cmd = detached("ssh");
        cmd.args(options)
            .arg("-O")
            .arg(if add { "forward" } else { "cancel" })
            .arg(&f.flag)
            .arg(&f.spec)
            .arg(&self.target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(MUX_CONTROL_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) => {
                let result = control_result(
                    out.status.success(),
                    &String::from_utf8_lossy(&out.stderr),
                    add,
                );
                if result.is_ok() {
                    owner.as_mut().unwrap().active = add;
                }
                result
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("SSH forwarding request timed out; listener state is uncertain".into()),
        }
    }
}

/// OpenSSH's mux cancel path exits zero even after a refused request. Its
/// diagnostic distinguishes a failure from a listener already absent.
pub(super) fn control_result(success: bool, stderr: &str, add: bool) -> Result<(), String> {
    let error = stderr.trim();
    if !add && error.contains("port not forwarded") {
        return Ok(());
    }
    let refused = error.contains("forwarding request failed")
        || error.contains("Master refused")
        || error.contains("cancel forward request failed");
    if success && !refused {
        return Ok(());
    }
    Err(if error.is_empty() {
        "SSH forwarding command failed".into()
    } else {
        error.into()
    })
}

#[derive(Default)]
struct Live {
    online: bool,
    retired: bool,
    active: HashSet<Forward>,
    // Includes uncertain requests: a timeout may have installed a listener.
    requested: HashSet<Forward>,
}

pub(crate) struct Manager {
    control: Arc<dyn Control>,
    closed: Arc<AtomicBool>,
    wanted: Mutex<Vec<Forward>>,
    live: tokio::sync::Mutex<Live>,
    statuses: Mutex<HashMap<Forward, Status>>,
    dirty: Arc<AtomicBool>,
    log: Arc<ConnLog>,
}

fn enabled(rules: &[Rule]) -> Vec<Forward> {
    rules
        .iter()
        .filter(|r| !r.disabled)
        .map(|r| r.forward.clone())
        .collect()
}

impl Manager {
    pub(super) fn new(
        host: &HostId,
        target: &str,
        options: &[String],
        rules: &[Rule],
        dirty: Arc<AtomicBool>,
        log: Arc<ConnLog>,
    ) -> Arc<Self> {
        let closed = Arc::new(AtomicBool::new(false));
        Arc::new(Self {
            control: Arc::new(SshControl {
                target: target.into(),
                host: host.clone(),
                generation: GENERATION.fetch_add(1, Ordering::Relaxed),
                retiring: closed.clone(),
                options: ssh_common_opts(&state::ssh_control_path(target), options),
                endpoint: Default::default(),
            }),
            closed,
            wanted: Mutex::new(enabled(rules)),
            live: Default::default(),
            statuses: Default::default(),
            dirty,
            log,
        })
    }

    pub(crate) fn status(&self, f: &Forward) -> Status {
        self.statuses
            .lock()
            .unwrap()
            .get(f)
            .cloned()
            .unwrap_or_else(|| {
                if self.wanted.lock().unwrap().contains(f) {
                    Status::Waiting
                } else {
                    Status::Off
                }
            })
    }

    fn report(&self, f: &Forward, status: Status) {
        if let Status::Failed(error) = &status {
            self.log.error(format!("{f}: {error}"));
        }
        self.statuses.lock().unwrap().insert(f.clone(), status);
        self.dirty.store(true, Ordering::Relaxed);
    }

    async fn add(&self, live: &mut Live, f: &Forward) -> Result<(), String> {
        if let Err(error) = f.validate() {
            self.report(f, Status::Failed(error.clone()));
            return Err(error);
        }
        live.requested.insert(f.clone());
        match self.control.execute(f, true).await {
            Ok(()) => {
                live.active.insert(f.clone());
                self.report(f, Status::Listening);
                Ok(())
            }
            Err(error) => {
                self.report(f, Status::Failed(error.clone()));
                Err(format!("{f}: {error}"))
            }
        }
    }

    async fn remove(&self, live: &mut Live, f: &Forward) -> Result<(), String> {
        match self.control.execute(f, false).await {
            Ok(()) => {
                live.active.remove(f);
                live.requested.remove(f);
                self.report(f, Status::Off);
                Ok(())
            }
            Err(error) => {
                live.active.remove(f); // a lost acknowledgement is not proof it still listens
                self.report(f, Status::Failed(error.clone()));
                Err(format!("{f}: {error}"))
            }
        }
    }

    /// Offline edits are configuration changes, pending the next connection.
    /// Online changes are acknowledged transactions. Only touched rules change.
    pub(crate) async fn apply(&self, rules: &[Rule]) -> Result<(), String> {
        let desired = enabled(rules);
        let mut live = self.live.lock().await;
        if self.closed.load(Ordering::Relaxed) {
            return Err("This host connection has closed".into());
        }
        let previous = self.wanted.lock().unwrap().clone();
        if !live.online {
            for f in &previous {
                if !desired.contains(f) {
                    self.report(
                        f,
                        if live.requested.contains(f) {
                            Status::Waiting
                        } else {
                            Status::Off
                        },
                    );
                }
            }
            *self.wanted.lock().unwrap() = desired;
            return Ok(());
        }
        let removed: HashSet<_> = previous
            .iter()
            .chain(&live.requested)
            .filter(|f| !desired.contains(f))
            .cloned()
            .collect();
        let added: Vec<_> = desired
            .iter()
            .filter(|f| !previous.contains(f) || (desired == previous && !live.active.contains(f)))
            .cloned()
            .collect();
        let mut failure = None;
        for f in &removed {
            if live.requested.contains(f)
                && let Err(error) = self.remove(&mut live, f).await
            {
                failure = Some(error);
                break;
            }
        }
        if failure.is_none() {
            for f in &added {
                if let Err(error) = self.add(&mut live, f).await {
                    failure = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            let mut rollback_errors = Vec::new();
            // Also cancel uncertain additions: a lost acknowledgement can leave
            // a listener installed. Cancellation names the complete SSH spec,
            // and the control owns the generation/host check above.
            for f in &added {
                if !previous.contains(f)
                    && live.requested.contains(f)
                    && let Err(e) = self.remove(&mut live, f).await
                {
                    rollback_errors.push(e);
                }
            }
            for f in &removed {
                if previous.contains(f)
                    && !live.active.contains(f)
                    && let Err(e) = self.add(&mut live, f).await
                {
                    rollback_errors.push(e);
                }
            }
            return Err(if rollback_errors.is_empty() {
                error
            } else {
                format!(
                    "{error}; could not restore previous listeners: {}",
                    rollback_errors.join("; ")
                )
            });
        }
        *self.wanted.lock().unwrap() = desired;
        Ok(())
    }

    /// Configuration imports use the same path; failed requests remain visible
    /// and the next reconnect retries the saved configuration.
    pub(crate) fn configure(self: &Arc<Self>, rules: Vec<Rule>) {
        if enabled(&rules) == *self.wanted.lock().unwrap() {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            let mut live = manager.live.lock().await;
            if manager.closed.load(Ordering::Relaxed) {
                return;
            }
            let desired = enabled(&rules);
            *manager.wanted.lock().unwrap() = desired.clone();
            if !live.online {
                return;
            }
            let removed: Vec<_> = live
                .requested
                .iter()
                .filter(|f| !desired.contains(f))
                .cloned()
                .collect();
            for f in removed {
                let _ = manager.remove(&mut live, &f).await;
            }
            for f in desired {
                if !live.active.contains(&f) {
                    let _ = manager.add(&mut live, &f).await;
                }
            }
        });
    }

    pub(super) async fn connected(&self) {
        let mut live = self.live.lock().await;
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let wanted = self.wanted.lock().unwrap().clone();
        // The master may outlive the dashboard or its daemon tunnel. Cancel
        // remembered/requested specs before asking again, as setup_ssh does.
        let stale: HashSet<_> = live.requested.iter().chain(&wanted).cloned().collect();
        for f in &stale {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            // A failed retirement stays remembered and visible, even if the
            // user disabled the rule while disconnected.
            live.requested.insert(f.clone());
            let _ = self.remove(&mut live, f).await;
        }
        live.active.clear();
        live.online = true;
        for f in &wanted {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            let _ = self.add(&mut live, f).await;
        }
    }

    pub(super) async fn disconnected(&self) {
        let mut live = self.live.lock().await;
        live.online = false;
        live.active.clear();
        *self.statuses.lock().unwrap() = live
            .requested
            .iter()
            .map(|f| (f.clone(), Status::Waiting))
            .collect();
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Announce retirement synchronously before a replacement starts dialing.
    /// A rename may reuse the same master under a different host label.
    pub(crate) fn retire(self: &Arc<Self>) {
        self.closed.store(true, Ordering::Relaxed);
        let manager = self.clone();
        tokio::spawn(async move {
            manager.shutdown().await;
        });
    }

    pub(crate) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let mut live = self.live.lock().await;
        // Explicit removal, task drop, and dashboard shutdown can all join
        // retirement. The lock makes them await one cleanup attempt.
        if live.retired {
            return;
        }
        live.online = false;
        // Independent listeners can retire together. A dead mux must cost one
        // timeout on quit, not one timeout for every configured port.
        let mut pending = tokio::task::JoinSet::new();
        for f in live.requested.iter().cloned() {
            let control = self.control.clone();
            pending.spawn(async move {
                let result = control.execute(&f, false).await;
                (f, result)
            });
        }
        while let Some(Ok((f, result))) = pending.join_next().await {
            live.active.remove(&f);
            match result {
                Ok(()) => {
                    live.requested.remove(&f);
                    self.report(&f, Status::Off);
                }
                Err(error) => self.report(&f, Status::Failed(error)),
            }
        }
        live.retired = true;
    }
}

/// Every exit from the connection task retires its listeners, including an
/// early dial failure after the backend was removed from the host list.
pub(super) struct Lifetime(pub Option<Arc<Manager>>);
impl Drop for Lifetime {
    fn drop(&mut self) {
        if let Some(manager) = self.0.take() {
            tokio::spawn(async move {
                manager.shutdown().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StalledControl(Arc<tokio::sync::Notify>, AtomicBool);
    #[async_trait::async_trait]
    impl Control for StalledControl {
        async fn execute(&self, _: &Forward, _: bool) -> Result<(), String> {
            if self.1.swap(true, Ordering::Relaxed) {
                return Ok(());
            }
            self.0.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn stalled_forwards_do_not_block_protocol_or_retirement() {
        let root = scratch_home("independent-forwards");
        let socket = root.join("server.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let transport = Transport::LocalSocket(socket);
        let (backend, mut shared, requests) =
            RemoteBackend::build(&transport, HostId("test".into()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let manager = Arc::new(Manager {
            control: Arc::new(StalledControl(entered.clone(), AtomicBool::new(false))),
            closed: Default::default(),
            wanted: Mutex::new(enabled(&crate::ssh_forward::import("3000").unwrap())),
            live: Default::default(),
            statuses: Default::default(),
            dirty: Default::default(),
            log: Default::default(),
        });
        shared.forwards = Some(manager.clone());
        let task = tokio::spawn(connection_task(transport, shared, requests, false));
        backend.connection_task.set(task.abort_handle()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut peer, _) = listener.accept().await.unwrap();
            assert!(matches!(
                read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
                Some(ClientFrame::Hello { .. })
            ));
            assert_eq!(backend.conn_state(), ConnState::Connecting);
            write_frame(
                &mut peer,
                &ServerFrame::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                    host: "test".into(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
                Some(ClientFrame::Subscribe)
            ));
            entered.notified().await;
            assert_eq!(backend.conn_state(), ConnState::Connected);
            write_frame(&mut peer, &ServerFrame::Snapshot { sessions: vec![] })
                .await
                .unwrap();
            while !backend.mirrored.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
            backend.retire();
            assert!(task.await.unwrap_err().is_cancelled());
            // Forward reconciliation releases its lock when the owner exits.
            let _live = manager.live.lock().await;
        })
        .await
        .expect("optional forward setup blocked the connection lifecycle");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Exercise the real OpenSSH client against a tiny mux peer. No SSH daemon,
    /// authentication, TCP listener, or user connection is involved. This pins
    /// the unusual zero-exit cancellation failure and the actual argv path.
    #[tokio::test]
    async fn real_ssh_mux_acknowledgements_and_replacement_ownership() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The control socket lives under the runtime dir: bind it in a child
        // with private XDG roots, never in a running dashboard's directory.
        // Keep socket paths short even with macOS's long default TMPDIR.
        const CHILD: &str = "CM_TEST_REAL_SSH_MUX";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::Builder::new()
                .prefix("cm-mux-")
                .tempdir_in("/tmp")
                .unwrap();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "backend::forwards::tests::real_ssh_mux_acknowledgements_and_replacement_ownership",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_STATE_HOME", dir.path().join("state"))
                .env("XDG_RUNTIME_DIR", dir.path().join("run"))
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "isolated SSH mux fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let generation = GENERATION.fetch_add(2, Ordering::Relaxed);
        let target = format!("forward-test-{}-{generation}", std::process::id());
        let socket = state::ssh_control_path(&target);
        state::create_dir_all_private(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        struct SocketGuard(PathBuf);
        impl Drop for SocketGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _socket = SocketGuard(socket);
        let refusal = Arc::new(Mutex::new(None::<String>));
        let requests = Arc::new(AtomicU64::new(0));
        let reply_refusal = refusal.clone();
        let count = requests.clone();
        let peer = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                while let Ok(length) = stream.read_u32().await {
                    assert!(length <= 4096);
                    let mut frame = vec![0; length as usize];
                    stream.read_exact(&mut frame).await.unwrap();
                    let word = |i| u32::from_be_bytes(frame[i..i + 4].try_into().unwrap());
                    let mut response = match word(0) {
                        1 => vec![1_u32, 4]
                            .into_iter()
                            .flat_map(u32::to_be_bytes)
                            .collect::<Vec<_>>(),
                        0x10000004 => vec![0x80000005, word(4), std::process::id()]
                            .into_iter()
                            .flat_map(u32::to_be_bytes)
                            .collect(),
                        kind @ (0x10000006 | 0x10000007) => {
                            count.fetch_add(1, Ordering::Relaxed);
                            assert_eq!(word(8), 1, "one local forwarding request");
                            let error = reply_refusal.lock().unwrap().take();
                            let mut bytes = vec![
                                if error.is_some() {
                                    0x80000003_u32
                                } else {
                                    0x80000001
                                },
                                word(4),
                            ]
                            .into_iter()
                            .flat_map(u32::to_be_bytes)
                            .collect::<Vec<_>>();
                            if let Some(error) = error {
                                assert_eq!(kind, 0x10000007);
                                bytes.extend((error.len() as u32).to_be_bytes());
                                bytes.extend(error.as_bytes());
                            }
                            bytes
                        }
                        kind => panic!("Unexpected mux operation {kind:x}"),
                    };
                    stream.write_u32(response.len() as u32).await.unwrap();
                    stream.write_all(&response).await.unwrap();
                    response.clear();
                }
            }
        });
        let control = |generation| SshControl {
            target: target.clone(),
            host: HostId("example".into()),
            generation,
            retiring: Arc::new(AtomicBool::new(false)),
            // The peer is entirely local to this fixture. User and system SSH
            // configuration must not change its route or reject its setup.
            options: ssh_common_opts(
                &state::ssh_control_path(&target),
                &["-F".into(), "/dev/null".into()],
            ),
            endpoint: Default::default(),
        };
        let old = control(generation);
        let rule = crate::ssh_forward::import("3000")
            .unwrap()
            .remove(0)
            .forward;
        old.execute(&rule, true).await.unwrap();
        *refusal.lock().unwrap() = Some("deliberate refusal".into());
        assert!(
            old.execute(&rule, false)
                .await
                .unwrap_err()
                .contains("deliberate refusal")
        );
        *refusal.lock().unwrap() = Some("port not forwarded".into());
        old.execute(&rule, false).await.unwrap();
        old.execute(&rule, true).await.unwrap();
        old.retiring.store(true, Ordering::Relaxed);
        let mut replacement = control(generation + 1);
        replacement.host = HostId("renamed-example".into());
        replacement.execute(&rule, true).await.unwrap();
        let before = requests.load(Ordering::Relaxed);
        old.execute(&rule, false).await.unwrap();
        assert!(old.execute(&rule, true).await.is_err());
        assert_eq!(
            requests.load(Ordering::Relaxed),
            before,
            "stale cleanup must not reach SSH"
        );
        replacement.execute(&rule, false).await.unwrap();
        peer.abort();
        let _ = peer.await;
    }

    #[test]
    fn mux_cancel_checks_the_acknowledgement_not_just_the_exit_status() {
        assert!(control_result(true, "mux_client_forward: forwarding request failed: port not in permitted opens\nmaster cancel forward request failed", false).is_err());
        assert!(
            control_result(
                true,
                "mux_client_forward: forwarding request failed: port not forwarded",
                false
            )
            .is_ok()
        );
        assert!(
            control_result(
                false,
                "mux_client_forward: forwarding request failed: port not forwarded",
                true
            )
            .is_err()
        );
        assert!(control_result(true, "", false).is_ok());
    }

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(Forward, bool)>>,
        rejected: Mutex<HashSet<Forward>>,
        cancel_rejected: Mutex<HashSet<Forward>>,
    }
    #[async_trait::async_trait]
    impl Control for Arc<Fake> {
        async fn execute(&self, f: &Forward, add: bool) -> Result<(), String> {
            self.calls.lock().unwrap().push((f.clone(), add));
            if (add && self.rejected.lock().unwrap().contains(f))
                || (!add && self.cancel_rejected.lock().unwrap().contains(f))
            {
                Err("address already in use".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn edits_touch_only_changed_listeners_and_failure_restores_the_old_rule() {
        let fake = Arc::new(Fake::default());
        let rules = crate::ssh_forward::import("3000 8080").unwrap();
        let manager = Manager {
            control: Arc::new(fake.clone()),
            closed: Arc::new(AtomicBool::new(false)),
            wanted: Mutex::new(enabled(&rules)),
            live: Default::default(),
            statuses: Default::default(),
            dirty: Default::default(),
            log: Default::default(),
        };
        manager.connected().await;
        fake.calls.lock().unwrap().clear();
        let mut changed = rules.clone();
        changed[0].forward.spec = "localhost:3000:localhost:4000".into();
        fake.rejected
            .lock()
            .unwrap()
            .insert(changed[0].forward.clone());
        assert!(manager.apply(&changed).await.is_err());
        assert_eq!(*manager.wanted.lock().unwrap(), enabled(&rules));
        assert_eq!(manager.status(&rules[0].forward), Status::Listening);
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(f, _)| f != &rules[1].forward)
        );
        fake.rejected.lock().unwrap().clear();
        manager.apply(&changed).await.unwrap();
        fake.calls.lock().unwrap().clear();
        changed[1].disabled = true;
        manager.apply(&changed).await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![(rules[1].forward.clone(), false)]
        );
        manager.disconnected().await;
        assert_eq!(manager.status(&changed[0].forward), Status::Waiting);
        fake.calls.lock().unwrap().clear();
        manager.apply(&[]).await.unwrap();
        assert!(fake.calls.lock().unwrap().is_empty());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn rollback_failure_and_reconnect_are_reported_truthfully() {
        let fake = Arc::new(Fake::default());
        let rules = crate::ssh_forward::import("3000").unwrap();
        let manager = Manager {
            control: Arc::new(fake.clone()),
            closed: Arc::new(AtomicBool::new(false)),
            wanted: Mutex::new(enabled(&rules)),
            live: Default::default(),
            statuses: Default::default(),
            dirty: Default::default(),
            log: Default::default(),
        };
        manager.connected().await;
        let changed = crate::ssh_forward::import("-L localhost:3000:localhost:8080").unwrap();
        fake.rejected
            .lock()
            .unwrap()
            .extend([rules[0].forward.clone(), changed[0].forward.clone()]);
        let error = manager.apply(&changed).await.unwrap_err();
        assert!(error.contains("could not restore"));
        assert!(matches!(
            manager.status(&rules[0].forward),
            Status::Failed(_)
        ));
        assert_eq!(*manager.wanted.lock().unwrap(), enabled(&rules));
        // A failed listener can still be disabled; it is not stuck enabled.
        let mut disabled = rules.clone();
        disabled[0].disabled = true;
        manager.apply(&disabled).await.unwrap();
        manager.disconnected().await;
        manager.apply(&rules).await.unwrap();
        fake.rejected.lock().unwrap().clear();
        manager.connected().await;
        assert_eq!(manager.status(&rules[0].forward), Status::Listening);
    }

    #[tokio::test]
    async fn disabling_offline_keeps_failed_cleanup_visible_and_retryable() {
        let fake = Arc::new(Fake::default());
        let rules = crate::ssh_forward::import("3000").unwrap();
        let manager = Manager {
            control: Arc::new(fake.clone()),
            closed: Arc::new(AtomicBool::new(false)),
            wanted: Mutex::new(enabled(&rules)),
            live: Default::default(),
            statuses: Default::default(),
            dirty: Default::default(),
            log: Default::default(),
        };
        manager.connected().await;
        manager.disconnected().await;
        let mut disabled = rules.clone();
        disabled[0].disabled = true;
        manager.apply(&disabled).await.unwrap();
        assert_eq!(manager.status(&rules[0].forward), Status::Waiting);
        fake.cancel_rejected
            .lock()
            .unwrap()
            .insert(rules[0].forward.clone());
        manager.connected().await;
        assert!(matches!(
            manager.status(&rules[0].forward),
            Status::Failed(_)
        ));
        assert!(
            manager
                .live
                .lock()
                .await
                .requested
                .contains(&rules[0].forward)
        );
        assert!(manager.apply(&disabled).await.is_err());
        assert!(manager.live.lock().await.active.is_empty());
        fake.cancel_rejected.lock().unwrap().clear();
        manager.apply(&disabled).await.unwrap();
        assert_eq!(manager.status(&rules[0].forward), Status::Off);
        assert!(manager.live.lock().await.requested.is_empty());
        manager.shutdown().await;
        manager.connected().await;
        assert!(
            !manager.live.lock().await.online,
            "shutdown cannot be undone by a late reconnect"
        );
    }
}
