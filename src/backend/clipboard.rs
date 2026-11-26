//! Revocable local clipboard grants. SSH forwards only to a grant's private
//! relay, never directly to the shared clipboard service. Retiring the last
//! grant closes streams locally; SSH cleanup is housekeeping, not authorization.
use super::*;
use std::net::Shutdown;
use std::os::unix::net::UnixStream as StdStream;
use std::sync::Weak;
use tokio::net::UnixListener;

#[derive(Default)]
struct Channels {
    closed: bool,
    streams: Vec<StdStream>,
}

struct Relay {
    directory: tempfile::TempDir,
    channels: Arc<Mutex<Channels>>,
    task: tokio::task::AbortHandle,
}

impl Relay {
    fn start(parent: &Path, service: PathBuf) -> std::io::Result<Self> {
        ssh::prepare_socket_dir(parent)?;
        // A new random directory for every grant, including after a dashboard
        // restart. A surviving old SSH forward must never regain access.
        let directory = tempfile::Builder::new()
            .prefix("clip-")
            .rand_bytes(16)
            .tempdir_in(parent)?;
        let path = directory.path().join("s");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let channels = Arc::new(Mutex::new(Channels::default()));
        let active = channels.clone();
        let task = tokio::spawn(async move {
            // The clipboard service itself handles one image at a time. Keep
            // this relay bounded too: no task or image buffer per queued peer.
            while let Ok((client, _)) = listener.accept().await {
                let exchange = async {
                    let mut client = track(client, &active)?;
                    let mut server = track(UnixStream::connect(&service).await?, &active)?;
                    tokio::io::copy_bidirectional(&mut client, &mut server).await
                };
                let _ = tokio::time::timeout(Duration::from_secs(300), exchange).await;
                active.lock().unwrap().streams.clear();
            }
        });
        Ok(Self {
            directory,
            channels,
            task: task.abort_handle(),
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join("s")
    }
}

fn track(stream: UnixStream, channels: &Mutex<Channels>) -> std::io::Result<UnixStream> {
    let mut channels = channels.lock().unwrap();
    if channels.closed {
        return Err(std::io::ErrorKind::ConnectionAborted.into());
    }
    let stream = stream.into_std()?;
    channels.streams.push(stream.try_clone()?);
    UnixStream::from_std(stream)
}

impl Drop for Relay {
    fn drop(&mut self) {
        let mut channels = self.channels.lock().unwrap();
        channels.closed = true;
        // shutdown acts now, even if the async task is stalled or not polled.
        for stream in channels.streams.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.task.abort();
        let _ = std::fs::remove_file(self.path());
    }
}

#[derive(Clone)]
struct Dial {
    target: String,
    options: Vec<String>,
    home: String,
}

#[derive(Default)]
struct AccessState {
    retired: bool,
    dial: Option<Dial>,
    grant: Option<(Arc<Endpoint>, Arc<Relay>)>,
}

/// One host row's authorization, shared with its connection task. The task's
/// reference does not prolong the grant after explicit backend retirement.
#[derive(Default)]
pub(super) struct Access(Mutex<AccessState>);

impl Access {
    pub(super) fn configure(&self, target: &str, options: &[String], home: &str) {
        self.0.lock().unwrap().dial = Some(Dial {
            target: target.into(),
            options: options.into(),
            home: home.into(),
        });
    }

    pub(super) async fn connect(&self, log: &ConnLog) {
        if let Err(error) = self.install().await {
            log.error(format!("clipboard unavailable: {error}"));
        } else {
            log.info("offering this machine's clipboard through a revocable local relay");
        }
    }

    async fn install(&self) -> Result<(), String> {
        let dial = {
            let state = self.0.lock().unwrap();
            if state.retired {
                return Err("host retired".into());
            }
            state
                .dial
                .clone()
                .ok_or("the probe did not report the host's home")?
        };
        let options = ssh::control_options(&dial.target, &dial.options).await?;
        let remote = cm_core::clipboard::paths::remote_socket_for_home(&dial.home);
        let endpoint = {
            let mut endpoints = ENDPOINTS.lock().unwrap();
            let key = (options.clone(), remote);
            if let Some(endpoint) = endpoints.get(&key) {
                endpoint.clone()
            } else {
                let endpoint = Arc::new(
                    Endpoint::new(&dial.target, options, &dial.home, &state::ssh_sock_dir())
                        .map_err(|e| e.to_string())?,
                );
                endpoints.insert(key, endpoint.clone());
                endpoint
            }
        };
        let mut live = endpoint.live.lock().await;
        let path = self.acquire(
            &endpoint,
            &mut live,
            &state::ssh_sock_dir(),
            cm_core::clipboard::paths::local_socket_path(),
        )?;
        // Serialize by remote listener, not the whole -R specification. A late
        // cancel for an older destination must not remove a replacement's socket.
        endpoint.cancel(&mut live).await?;
        if live.relay.strong_count() == 0 {
            return Err("host retired".into());
        }
        let mut prep = detached("ssh");
        prep.args(&endpoint.options)
            // Reuse exactly this master, and fail closed if it has disappeared.
            .args(["-oControlMaster=no", "-oProxyCommand=false", "-T"])
            .arg(&endpoint.target)
            .arg(login_shell_safe(CLIPBOARD_PREP_SCRIPT));
        if !bounded_status(prep, CLIPBOARD_PREP_TIMEOUT).await {
            return Err("could not prepare the host's clipboard socket".into());
        }
        if live.relay.strong_count() == 0 {
            return Err("host retired".into());
        }
        let forward = clipboard_forward(&dial.home, &path);
        // Record before awaiting: cancellation or a timeout may leave an add
        // installed, so the next attempt must cancel it before rebinding.
        endpoint.remember(&mut live, Some(forward.clone()))?;
        ssh::forward(&endpoint.target, &endpoint.options, &forward, true).await
    }

    fn acquire(
        &self,
        endpoint: &Arc<Endpoint>,
        live: &mut Live,
        parent: &Path,
        service: PathBuf,
    ) -> Result<PathBuf, String> {
        let mut state = self.0.lock().unwrap();
        if state.retired {
            return Err("host retired".into());
        }
        if let Some((old, relay)) = &state.grant
            && Arc::ptr_eq(old, endpoint)
        {
            return Ok(relay.path());
        }
        let relay = match live.relay.upgrade() {
            Some(relay) => relay,
            None => Arc::new(Relay::start(parent, service).map_err(|e| e.to_string())?),
        };
        live.relay = Arc::downgrade(&relay);
        let path = relay.path();
        let previous = state.grant.replace((endpoint.clone(), relay));
        drop(state);
        retire_grant(previous);
        Ok(path)
    }

    pub(super) fn revoke(&self) {
        let grant = {
            let mut state = self.0.lock().unwrap();
            state.retired = true;
            state.grant.take()
        };
        retire_grant(grant);
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        self.revoke();
    }
}

fn retire_grant(grant: Option<(Arc<Endpoint>, Arc<Relay>)>) {
    if let Some((endpoint, relay)) = grant {
        drop(relay); // Local revocation is synchronous, before any SSH operation.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                endpoint.cleanup().await;
            });
        }
    }
}

#[derive(Default)]
struct Live {
    relay: Weak<Relay>,
    requested: Option<Forward>,
}

struct Endpoint {
    target: String,
    options: Vec<String>,
    live: tokio::sync::Mutex<Live>,
    record: PathBuf,
}

// Keep uncertain cancellations so a later grant retries them. Host labels are
// not identities here: aliases of the same master/listener share authorization
// until the last row revokes it.
type EndpointKey = (Vec<String>, String);
static ENDPOINTS: LazyLock<Mutex<HashMap<EndpointKey, Arc<Endpoint>>>> =
    LazyLock::new(Mutex::default);

impl Endpoint {
    fn new(target: &str, options: Vec<String>, home: &str, parent: &Path) -> std::io::Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        ssh::prepare_socket_dir(parent)?;
        let remote = cm_core::clipboard::paths::remote_socket_for_home(home);
        // Connection details are hashed only in memory. The record's filename
        // is a digest and its contents are solely a random relay directory name.
        let key = serde_json::to_vec(&(&options, remote))?;
        let digest: String = Sha256::digest(key)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let record = parent.join(format!("clipboard-{digest}.json"));
        let requested = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&record)
        {
            Ok(file) => {
                let mut data = Vec::new();
                file.take(1024).read_to_end(&mut data)?;
                let name: Option<String> = serde_json::from_slice(&data)?;
                match name {
                    Some(name) if relay_name(&name) => {
                        Some(clipboard_forward(home, &parent.join(name).join("s")))
                    }
                    Some(_) => return Err(std::io::ErrorKind::InvalidData.into()),
                    None => None,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Upgrade from the old direct-to-service bridge. Never install
                // a second destination while the master still knows this one.
                Some(clipboard_forward(
                    home,
                    &cm_core::clipboard::paths::local_socket_path(),
                ))
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            target: target.into(),
            options,
            record,
            live: tokio::sync::Mutex::new(Live {
                requested,
                ..Default::default()
            }),
        })
    }

    fn remember(&self, live: &mut Live, forward: Option<Forward>) -> Result<(), String> {
        use std::io::Write;
        let write = || -> std::io::Result<()> {
            // Store no target, home directory or absolute socket path. Rebuild
            // the spec from the current probe and private socket root on restart.
            let name = forward
                .as_ref()
                .map(|forward| {
                    let local = Path::new(forward.spec.rsplit(':').next().unwrap_or_default());
                    let name = local
                        .parent()
                        .and_then(Path::file_name)
                        .and_then(|s| s.to_str());
                    match name {
                        Some(name)
                            if relay_name(name) && local.file_name().is_some_and(|s| s == "s") =>
                        {
                            Ok(name)
                        }
                        _ => Err(std::io::Error::from(std::io::ErrorKind::InvalidData)),
                    }
                })
                .transpose()?;
            let mut temporary = tempfile::NamedTempFile::new_in(self.record.parent().unwrap())?;
            temporary.write_all(&serde_json::to_vec(&name)?)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&self.record).map_err(|e| e.error)?;
            Ok(())
        };
        write().map_err(|e| format!("could not record clipboard forwarding: {e}"))?;
        live.requested = forward;
        Ok(())
    }

    async fn cancel(&self, live: &mut Live) -> Result<(), String> {
        if let Some(forward) = &live.requested {
            ssh::forward(&self.target, &self.options, forward, false).await?;
            self.remember(live, None)?;
        }
        Ok(())
    }

    async fn cleanup(&self) {
        let mut live = self.live.lock().await;
        // A replacement or another live alias still needs this listener.
        if live.relay.strong_count() == 0
            && let Err(error) = self.cancel(&mut live).await
        {
            tracing::warn!("clipboard forward cleanup failed: {error}");
        }
    }
}

fn relay_name(name: &str) -> bool {
    name.len() == 21
        && name.starts_with("clip-")
        && name[5..].bytes().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cm-relay-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn endpoint(options: Vec<String>, root: &Path) -> Arc<Endpoint> {
        Arc::new(Endpoint {
            target: "example.invalid".into(),
            options,
            live: Default::default(),
            record: root.join("record.json"),
        })
    }

    async fn grant(access: &Access, endpoint: &Arc<Endpoint>, root: &Path) -> PathBuf {
        access
            .acquire(
                endpoint,
                &mut *endpoint.live.lock().await,
                root,
                root.join("service"),
            )
            .unwrap()
    }

    async fn closed(stream: &mut UnixStream) {
        let result = tokio::time::timeout(Duration::from_secs(2), stream.read_u8())
            .await
            .expect("revocation must close active streams");
        assert!(result.is_err(), "revoked connection still carries data");
    }

    #[tokio::test]
    async fn revocation_closes_both_directions_and_reenable_uses_a_fresh_path() {
        let root = root();
        let service = UnixListener::bind(root.path().join("service")).unwrap();
        let first = Access::default();
        let endpoint = endpoint(vec![], root.path());
        let old_path = grant(&first, &endpoint, root.path()).await;
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut client = UnixStream::connect(&old_path).await.unwrap();
        client.write_all(b"request").await.unwrap();
        let (mut server, _) = service.accept().await.unwrap();
        let mut request = [0; 7];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"request");
        server.write_all(b"image").await.unwrap();
        let mut image = [0; 5];
        client.read_exact(&mut image).await.unwrap();
        assert_eq!(&image, b"image");
        first.revoke();
        // No task polling is required to remove the local authorization path.
        assert!(!old_path.exists());
        closed(&mut client).await;
        closed(&mut server).await;
        assert!(UnixStream::connect(&old_path).await.is_err());

        let replacement = Access::default();
        let new_path = grant(&replacement, &endpoint, root.path()).await;
        assert_ne!(old_path, new_path);
        endpoint.cleanup().await; // Late cleanup of the previous grant.
        let mut client = UnixStream::connect(&new_path).await.unwrap();
        client.write_u8(42).await.unwrap();
        let (mut server, _) = service.accept().await.unwrap();
        assert_eq!(server.read_u8().await.unwrap(), 42);
        replacement.revoke();
    }

    #[tokio::test]
    async fn independent_hosts_are_isolated_and_aliases_share_until_the_last_revocation() {
        let root = root();
        let service = UnixListener::bind(root.path().join("service")).unwrap();
        let a = Access::default();
        let alias = Access::default();
        let b = Access::default();
        let endpoint_a = endpoint(vec![], root.path());
        let endpoint_b = endpoint(vec![], root.path());
        let path_a = grant(&a, &endpoint_a, root.path()).await;
        assert_eq!(grant(&alias, &endpoint_a, root.path()).await, path_a);
        let path_b = grant(&b, &endpoint_b, root.path()).await;
        a.revoke();
        assert!(
            path_a.exists(),
            "another row still explicitly grants access"
        );
        alias.revoke();
        assert!(!path_a.exists());
        let mut client = UnixStream::connect(&path_b).await.unwrap();
        client.write_u8(7).await.unwrap();
        let (mut server, _) = service.accept().await.unwrap();
        assert_eq!(server.read_u8().await.unwrap(), 7);
        b.revoke();
    }

    #[tokio::test]
    async fn retiring_a_backend_revokes_even_while_workers_keep_it_alive() {
        let root = root();
        let transport = Transport::Ssh {
            target: "example.invalid".into(),
            local_sock: root.path().join("rpc"),
            options: vec![],
            forwards: vec![],
            clipboard: true,
            tower: false,
        };
        let (backend, shared, _requests) = RemoteBackend::build(&transport, HostId("test".into()));
        let access = shared.clipboard.as_ref().unwrap();
        let endpoint = endpoint(vec![], root.path());
        let path = grant(access, &endpoint, root.path()).await;
        let worker = backend.clone();
        backend.retire();
        assert!(!path.exists());
        // A setup that finishes resolving SSH after retirement cannot resurrect
        // the relay, even if it still holds the task's Arc<Access>.
        access.configure("example.invalid", &[], "/remote");
        assert!(
            access
                .acquire(
                    &endpoint,
                    &mut *endpoint.live.lock().await,
                    root.path(),
                    root.path().join("service")
                )
                .is_err()
        );
        drop(worker);
    }

    #[test]
    fn uncertain_forward_survives_dashboard_restart_without_recording_identity() {
        let root = root();
        let options = vec![
            "-S".into(),
            root.path().join("master").display().to_string(),
        ];
        let first =
            Endpoint::new("example.invalid", options.clone(), "/remote", root.path()).unwrap();
        let mut live = first.live.try_lock().unwrap();
        assert_eq!(
            live.requested,
            Some(clipboard_forward(
                "/remote",
                &cm_core::clipboard::paths::local_socket_path()
            )),
            "cancel the legacy bridge on upgrade"
        );
        let forward = clipboard_forward("/remote", &root.path().join("clip-abcdefghijklmnop/s"));
        first.remember(&mut live, Some(forward.clone())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.record).unwrap(),
            "\"clip-abcdefghijklmnop\""
        );
        drop(live);
        drop(first);
        let restarted =
            Endpoint::new("example.invalid", options.clone(), "/remote", root.path()).unwrap();
        let mut live = restarted.live.try_lock().unwrap();
        assert_eq!(live.requested, Some(forward));
        restarted.remember(&mut live, None).unwrap();
        drop(live);
        drop(restarted);
        let clean = Endpoint::new("example.invalid", options, "/remote", root.path()).unwrap();
        assert!(clean.live.try_lock().unwrap().requested.is_none());
        assert_eq!(
            std::fs::metadata(&clean.record)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn failed_cancellation_is_retained_and_late_cleanup_preserves_replacements() {
        let root = root();
        let listener = UnixListener::bind(root.path().join("mux-2222.sock")).unwrap();
        let config = root.path().join("config");
        std::fs::write(&config, "Host *\n Port 2222\n").unwrap();
        let refuse = Arc::new(AtomicBool::new(true));
        let refusals = refuse.clone();
        let calls = Arc::new(AtomicU64::new(0));
        let requests = calls.clone();
        let mut peers = tokio::task::JoinSet::new();
        peers.spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                while let Ok(length) = stream.read_u32().await {
                    assert!(length <= 4096);
                    let mut frame = vec![0; length as usize];
                    stream.read_exact(&mut frame).await.unwrap();
                    let word = |i| u32::from_be_bytes(frame[i..i + 4].try_into().unwrap());
                    let mut response: Vec<u8> = match word(0) {
                        1 => vec![1_u32, 4],
                        0x10000004 => vec![0x80000005, word(4), std::process::id()],
                        0x10000007 => {
                            requests.fetch_add(1, Ordering::Relaxed);
                            vec![
                                if refusals.load(Ordering::Relaxed) {
                                    0x80000003
                                } else {
                                    0x80000001
                                },
                                word(4),
                            ]
                        }
                        kind => panic!("unexpected mux operation {kind:x}"),
                    }
                    .into_iter()
                    .flat_map(u32::to_be_bytes)
                    .collect();
                    if word(0) == 0x10000007 && refusals.load(Ordering::Relaxed) {
                        let message = b"deliberate refusal";
                        response.extend((message.len() as u32).to_be_bytes());
                        response.extend(message);
                    }
                    stream.write_u32(response.len() as u32).await.unwrap();
                    stream.write_all(&response).await.unwrap();
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let options = ssh_common_opts(
                &root.path().join("default"),
                &[
                    "-F".into(),
                    config.to_string_lossy().into_owned(),
                    format!("-oControlPath={}/mux-%p.sock", root.path().display()),
                ],
            );
            let endpoint = endpoint(
                ssh::control_options("example.invalid", &options)
                    .await
                    .unwrap(),
                root.path(),
            );
            let old = clipboard_forward("/remote", &root.path().join("old"));
            endpoint.live.lock().await.requested = Some(old.clone());
            // Cleanup keeps the original, concrete master after config edits.
            std::fs::write(&config, "Host *\n Port 3333\n").unwrap();
            endpoint.cleanup().await;
            assert_eq!(endpoint.live.lock().await.requested, Some(old.clone()));
            let replacement = Access::default();
            let path = grant(&replacement, &endpoint, root.path()).await;
            let mut live = endpoint.live.lock().await;
            assert!(endpoint.cancel(&mut live).await.is_err());
            assert_eq!(live.requested, Some(old));
            refuse.store(false, Ordering::Relaxed);
            endpoint.cancel(&mut live).await.unwrap();
            assert!(live.requested.is_none());
            let new = clipboard_forward("/remote", &path);
            live.requested = Some(new.clone());
            drop(live);
            endpoint.cleanup().await;
            assert_eq!(
                calls.load(Ordering::Relaxed),
                3,
                "old cleanup cannot cancel a replacement"
            );
            assert_eq!(endpoint.live.lock().await.requested, Some(new));
            replacement.revoke();
            endpoint.cleanup().await;
            assert_eq!(calls.load(Ordering::Relaxed), 4);
            assert!(endpoint.live.lock().await.requested.is_none());
        })
        .await
        .unwrap();
    }
}
