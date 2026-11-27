//! Sessions on a host's Tower daemon, behind [`crate::tower::FLAG`].
//!
//! With the flag set and an SSH host opted in, its new sessions run under the Tower
//! installed there. Under pooled-localhost (`[launcher] pooled`) this
//! machine is such a host too: its link runs the same CLI through a local
//! login shell instead of ssh, so the dashboard here and one reaching this
//! machine over ssh share one daemon and see each other's sessions. When Tower is disabled for a host, sessions an earlier run started there
//! stay listed and operable, but no daemon is started and nothing new runs
//! on Tower. The dashboard speaks Tower's protocol on the daemon's own
//! socket, forwarded over the host's shared ssh connection like
//! `miao-server`'s (reached directly under pooled-localhost): one connection
//! follows the session records and opens, stops and tags sessions. Daemons
//! predating `requests.concurrent.v1` pause events while handling a call. Only
//! an attach window runs `tower-cli attach`. Nothing here starts a daemon
//! except [`TowerLink::ready`], which settles which Tower the host runs first,
//! and starts the one this dashboard carries only when none of it is running
//! (see [`DEPLOY_REL`]). The socket's path comes from the daemon's report, or
//! for a daemon that predates it, from the login shell's environment.
//!
//! `miao-server` keeps serving everything else for the host: its sessions
//! (marked for a restart), resume candidates, directory queries, vitals and
//! VCS. Rows from both are merged by [`super::RemoteBackend`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tower_client::{Connection, Error};
use tower_protocol::AgentRecord;

use super::{
    ConnLog, KillOutcome, LaunchPlan, OpenSpec, REMOTE_OUTPUT_CAP, capped_output, host_text_safe,
    login_shell_quote_arg, login_shell_safe, ssh_common_opts,
};
use crate::state::{self, HostId, LauncherState, SessionFlags, SessionKey};

/// Tower's command-line client, found on the host's login `PATH`.
const CLI: &str = "tower-cli";

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How many times a new forward's local socket is looked for.
const FORWARD_ATTEMPTS: u32 = 8;
const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// A stream that lasted this long was healthy, so its loss retries promptly.
const HEALTHY: Duration = Duration::from_secs(30);
/// How often a host is asked again, with the flag unset, after its Tower
/// was found absent or idle.
const RETRY_DORMANT: Duration = Duration::from_secs(600);

/// How a link reaches its host's shell.
enum Via {
    /// `ssh target`, sharing the connection `miao-server` uses.
    Ssh { target: String, opts: Vec<String> },
    /// `/bin/sh` on this machine: pooled-localhost.
    Local,
}

/// One host's Tower: its live sessions, and how to reach its CLI.
pub(crate) struct TowerLink {
    pub(super) enabled: bool,
    host: HostId,
    via: Via,
    /// Live sessions by Tower id.
    rows: Mutex<HashMap<String, LauncherState>>,
    /// The event stream has delivered its first snapshot and is still up.
    mirrored: AtomicBool,
    dirty: Arc<AtomicBool>,
    log: Arc<ConnLog>,
    task: OnceLock<tokio::task::AbortHandle>,
    stopped: AtomicBool,
    /// The build whose deploy last failed, and when, so a host that refuses
    /// it is not sent tens of megabytes on every reconnect.
    refused: Mutex<Option<(String, Instant)>>,
    /// The host's `tower-cli`, once [`Self::ready`] found its Tower running.
    cli: Mutex<Option<String>>,
    /// Held while [`Self::ready`] inspects or starts the host's Tower, so a
    /// launch waits for an upload already under way instead of racing it.
    preparing: tokio::sync::Mutex<()>,
    /// The daemon's socket on the host, once known.
    socket: Mutex<Option<String>>,
    /// Serializes connection setup and teardown for this host's forward.
    endpoint: tokio::sync::Mutex<Option<Endpoint>>,
    #[cfg(test)]
    probe_socket: OnceLock<PathBuf>,
    generation: AtomicU64,
}

/// The local socket a link connects to: the daemon's own under
/// pooled-localhost, or a forward of it.
struct Endpoint {
    connection: Option<Arc<Connection>>,
    local: PathBuf,
    remote: String,
    generation: u64,
    /// The `-N` child that asked for the forward; see
    /// [`super::forward_unix_socket`].
    _forward: Option<tokio::process::Child>,
}

/// Why a CLI call failed. `unreachable` means the command may never have
/// reached Tower.
#[derive(Debug)]
struct Failure {
    unreachable: bool,
    /// Tower's error code, for a call the daemon refused.
    code: Option<String>,
    message: String,
}

impl Failure {
    fn call(method: &str, error: Error) -> Self {
        if let Error::Rpc(rpc) = error {
            return Self {
                unreachable: false,
                message: format!("{}: {}", rpc.data.code, rpc.message),
                code: Some(rpc.data.code),
            };
        }
        Self {
            unreachable: false,
            code: None,
            message: format!("`{method}` failed: {error}"),
        }
    }

    fn unreachable(message: impl Into<String>) -> Self {
        Self {
            unreachable: true,
            code: None,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl TowerLink {
    pub(crate) fn new(
        host: HostId,
        target: &str,
        ssh_options: &[String],
        enabled: bool,
        dirty: Arc<AtomicBool>,
        log: Arc<ConnLog>,
    ) -> Arc<Self> {
        Self::with(
            host,
            Via::Ssh {
                target: target.to_string(),
                opts: ssh_common_opts(&state::ssh_control_path(target), ssh_options),
            },
            enabled,
            dirty,
            log,
        )
    }

    /// The link for this machine's own Tower, under pooled-localhost.
    pub(crate) fn local(host: HostId, dirty: Arc<AtomicBool>, log: Arc<ConnLog>) -> Arc<Self> {
        Self::with(host, Via::Local, crate::tower::enabled(), dirty, log)
    }

    fn with(
        host: HostId,
        via: Via,
        enabled: bool,
        dirty: Arc<AtomicBool>,
        log: Arc<ConnLog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            host,
            via,
            rows: Mutex::default(),
            mirrored: AtomicBool::new(false),
            dirty,
            log,
            task: OnceLock::new(),
            stopped: AtomicBool::new(false),
            refused: Mutex::default(),
            cli: Mutex::default(),
            preparing: tokio::sync::Mutex::default(),
            socket: Mutex::default(),
            endpoint: tokio::sync::Mutex::default(),
            #[cfg(test)]
            probe_socket: OnceLock::new(),
            generation: AtomicU64::new(0),
        })
    }

    /// Start following the host's sessions until [`Self::stop`].
    pub(crate) fn start(self: &Arc<Self>) {
        let task = tokio::spawn(follow(Arc::downgrade(self)));
        let _ = self.task.set(task.abort_handle());
    }

    pub(crate) fn stop(self: &Arc<Self>) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(task) = self.task.get() {
            task.abort();
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let link = self.clone();
            runtime.spawn(async move {
                let mut slot = link.endpoint.lock().await;
                link.clear_endpoint(&mut slot).await;
            });
        }
    }

    pub(crate) fn rows(&self) -> Vec<LauncherState> {
        self.rows.lock().unwrap().values().cloned().collect()
    }

    /// Whether the host's sessions are known: the stream is up and has
    /// delivered its snapshot.
    pub(crate) fn reported(&self) -> bool {
        self.mirrored.load(Ordering::Relaxed)
    }

    /// The Tower id of the session `key` names.
    pub(crate) fn id(&self, key: &SessionKey) -> Option<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|(_, row)| &row.key() == key)
            .map(|(id, _)| id.clone())
    }

    /// Whether `name`, a row's pool session, is one of this Tower's.
    pub(crate) fn owns(&self, name: &str) -> bool {
        self.rows.lock().unwrap().contains_key(name)
    }

    /// The argv for a window attached to session `id`.
    pub(crate) fn attach_argv(&self, id: &str) -> Vec<String> {
        let command = remote_command(self.cli().as_deref(), &cli_args(&["attach", id]));
        match &self.via {
            Via::Ssh { target, opts } => {
                let mut argv = vec!["ssh".to_string(), "-t".to_string()];
                argv.extend(opts.iter().cloned());
                argv.extend([target.clone(), command]);
                argv
            }
            Via::Local => vec!["/bin/sh".into(), "-c".into(), command],
        }
    }

    /// Reserve a session for `spec`; it starts when its window attaches, so
    /// the agent meets the real terminal.
    pub(crate) async fn open(&self, spec: OpenSpec) -> anyhow::Result<LaunchPlan> {
        self.ready()
            .await
            .map_err(|reason| anyhow::anyhow!("Tower on {}: {reason}", self.host))?;
        let harness = crate::tower::harness(spec.agent)
            .ok_or_else(|| anyhow::anyhow!(crate::agent::UNKNOWN_AGENT_REFUSAL))?;
        let args = spec
            .worktree
            .as_deref()
            .and_then(|name| spec.agent.worktree_args(Some(name)))
            .unwrap_or_default();
        let params = serde_json::json!({
            "harness": harness.id(),
            "cwd": spec.cwd,
            "args": args,
            "resume": spec.resume.map(|(id, fork)| serde_json::json!({
                "native_session_id": id,
                "fork": fork,
            })),
            "terminal": {"defer": true},
        });
        let opened = self
            .rpc("session.open", params)
            .await
            .map_err(|failure| anyhow::anyhow!("Tower on {}: {failure}", self.host))?;
        let id = opened
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Tower on {} returned no session id", self.host))?
            .to_string();
        self.log.info(format!("Tower reserved session {id}"));
        Ok(LaunchPlan::AttachRemote {
            argv: self.attach_argv(&id),
            session_name: id,
        })
    }

    pub(crate) async fn kill(&self, id: &str) -> KillOutcome {
        match self
            .rpc("session.kill", serde_json::json!({"id": id}))
            .await
        {
            Ok(_) => KillOutcome::Signalled,
            Err(failure) if failure.unreachable => KillOutcome::Unreachable,
            Err(failure) if failure.code.as_deref() == Some("not_found") => {
                KillOutcome::AlreadyGone
            }
            Err(failure) => KillOutcome::Failed(failure.message),
        }
    }

    pub(crate) async fn set_flags(&self, id: &str, flags: SessionFlags) -> bool {
        let pin = crate::tower::PINNED;
        let (add, remove): (&[&str], &[&str]) = if flags.pinned {
            (&[pin], &[])
        } else {
            (&[], &[pin])
        };
        let tagged = self
            .rpc(
                "session.tags.patch",
                serde_json::json!({"id": id, "add": add, "remove": remove}),
            )
            .await
            .inspect_err(|failure| {
                self.log
                    .error(format!("Tower could not tag {id}: {failure}"))
            })
            .is_ok();
        // Tower alone raises attention, so only a cleared follow-up is sent.
        let acknowledged = flags.follow_up
            || self
                .rpc("session.attention.clear", serde_json::json!({"id": id}))
                .await
                .inspect_err(|failure| {
                    self.log
                        .error(format!("Tower could not clear {id}'s attention: {failure}"))
                })
                .is_ok();
        tagged && acknowledged
    }

    /// Repeat an unsent request once. After delivery, only idempotent
    /// methods can be retried: session.open reserves a new session.
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Failure> {
        let idempotent = matches!(
            method,
            "session.kill" | "session.tags.patch" | "session.attention.clear"
        );
        let mut retried = false;
        loop {
            let (generation, connection) = match self.connection().await {
                Ok(connected) => connected,
                Err(failure) if failure.unreachable && !retried => {
                    retried = true;
                    continue;
                }
                Err(failure) => return Err(failure),
            };
            let handle = connection.handle();
            drop(connection);
            match handle.call(method, &params).await {
                Ok(result) => {
                    return serde_json::to_value(result).map_err(|e| Failure {
                        unreachable: false,
                        code: None,
                        message: format!("`{method}` answered unreadably: {e}"),
                    });
                }
                Err(error @ (Error::NotSent(_) | Error::Disconnected(_))) => {
                    self.release(generation).await;
                    if !retried && (matches!(error, Error::NotSent(_)) || idempotent) {
                        retried = true;
                        continue;
                    }
                    return Err(Failure::unreachable(format!("`{method}` failed: {error}")));
                }
                Err(error) => return Err(Failure::call(method, error)),
            }
        }
    }

    async fn connection(&self) -> Result<(u64, Arc<Connection>), Failure> {
        let mut slot = self.endpoint.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(Failure::unreachable("Tower link has stopped"));
        }
        if slot.is_none() {
            *slot = Some(self.open_endpoint().await?);
        }
        let endpoint = slot.as_mut().expect("endpoint just opened");
        if let Some(connection) = &endpoint.connection {
            return Ok((endpoint.generation, connection.clone()));
        }
        match Connection::connect_at(&endpoint.local, Default::default()).await {
            Ok(connection) => {
                let connection = Arc::new(connection);
                endpoint.connection = Some(connection.clone());
                Ok((endpoint.generation, connection))
            }
            Err(error) => {
                self.clear_endpoint(&mut slot).await;
                Err(match error {
                    Error::NotSent(_) | Error::Disconnected(_) => {
                        Failure::unreachable(format!("cannot reach Tower's socket: {error}"))
                    }
                    other => Failure::call("hello", other),
                })
            }
        }
    }

    async fn open_endpoint(&self) -> Result<Endpoint, Failure> {
        let remote = self.socket_path().await.map_err(Failure::unreachable)?;
        let (local, forward) = match &self.via {
            Via::Local => (PathBuf::from(&remote), None),
            Via::Ssh { target, opts } => {
                let local = state::tower_forward_sock(&self.host.0);
                let forward =
                    super::forward_unix_socket(opts, target, &local, &remote, "tower-forward")
                        .await
                        .ok_or_else(|| Failure::unreachable("could not forward Tower's socket"))?;
                // The master binds the local socket shortly after the request.
                for delay in super::connect_retry_delays(FORWARD_ATTEMPTS) {
                    if local.exists() {
                        break;
                    }
                    tokio::time::sleep(delay).await;
                }
                (local, Some(forward))
            }
        };
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Endpoint {
            connection: None,
            local,
            remote,
            generation,
            _forward: forward,
        })
    }

    async fn release(&self, generation: u64) {
        let mut slot = self.endpoint.lock().await;
        if slot
            .as_ref()
            .is_some_and(|endpoint| endpoint.generation == generation)
        {
            self.clear_endpoint(&mut slot).await;
        }
    }

    // Keep the slot locked through cancellation: every generation forwards
    // onto the same local path, so an old cancel must finish before rebinding.
    async fn clear_endpoint(&self, slot: &mut Option<Endpoint>) {
        self.socket.lock().unwrap().take();
        if let Some(mut endpoint) = slot.take() {
            endpoint.connection.take();
            let cancel = self.cancel(&endpoint);
            if let Some(cancel) = cancel {
                cancel.await;
            }
        }
    }

    /// Cancelling `endpoint`'s forward, when it has one.
    fn cancel(&self, endpoint: &Endpoint) -> Option<impl Future<Output = ()> + use<>> {
        let Via::Ssh { target, opts } = &self.via else {
            return None;
        };
        let (opts, target) = (opts.clone(), target.clone());
        let (local, remote) = (endpoint.local.clone(), endpoint.remote.clone());
        Some(async move {
            super::cancel_unix_forward(&opts, &target, &local, &remote).await;
            let _ = std::fs::remove_file(&local);
        })
    }

    /// The daemon's socket on the host: as it last reported, or as the
    /// login shell's environment places it.
    async fn socket_path(&self) -> Result<String, String> {
        if let Some(socket) = self.socket.lock().unwrap().clone() {
            return Ok(socket);
        }
        let socket = self.probe().await?.socket();
        Ok(self.socket.lock().unwrap().get_or_insert(socket).clone())
    }

    /// The host's `tower-cli`, once its Tower is known to be running.
    fn cli(&self) -> Option<String> {
        self.cli.lock().unwrap().clone()
    }

    /// Settle which Tower the host runs, starting this dashboard's when none
    /// of it is: find a working `tower-cli` (login `PATH`, then
    /// [`DEPLOY_REL`]), uploading this dashboard's build when there is none;
    /// ask it about the daemon; use a daemon that speaks this build's
    /// protocol; upload and start this build's when no daemon runs. Anything
    /// else is left to the user. The terminal pool is the daemon's business:
    /// it starts the one beside its own binary. Returns the CLI to use.
    pub(crate) async fn ready(&self) -> Result<String, String> {
        let _preparing = self.preparing.lock().await;
        if let Some(cli) = self.cli() {
            return Ok(cli);
        }
        let cli = self.prepare().await?;
        self.log.info(format!("using Tower through {cli}"));
        *self.cli.lock().unwrap() = Some(cli.clone());
        Ok(cli)
    }

    /// Forget the CLI [`Self::ready`] settled, so the next call looks again.
    fn forget(&self) {
        self.cli.lock().unwrap().take();
    }

    async fn prepare(&self) -> Result<String, String> {
        let probe = self.probe().await?;
        let bundles = Bundles::for_platform(&probe.platform);
        let deployed = format!("{}/{DEPLOY_REL}/current/{CLI}", probe.home);
        let (cli, installed) = match &probe.cli {
            Some(cli) => (cli.clone(), false),
            None => {
                self.install(&bundles, &probe.current)
                    .await
                    .map_err(|reason| format!("the host has no working {CLI}, and {reason}"))?;
                (deployed.clone(), true)
            }
        };
        match self.status(&cli).await {
            Ok(info) => compatible(&info).map(|()| cli),
            Err(reason) if probe.daemon => Err(format!(
                "a Tower daemon is running, but `{cli} daemon status` failed \
                 ({reason}); this needs manual care on the host"
            )),
            Err(_) => {
                if !installed {
                    self.install(&bundles, &probe.current)
                        .await
                        .map_err(|reason| {
                            format!("Tower is not running on the host, and {reason}")
                        })?;
                }
                self.launch(&deployed).await?;
                Ok(deployed)
            }
        }
    }

    /// What the host has: its platform, the deployed build, a working CLI,
    /// and whether a Tower daemon runs under this account.
    async fn probe(&self) -> Result<Probe, String> {
        let (status, stdout, stderr) = self.shell(login_shell_safe(&probe_script()), None).await?;
        if !status.success() {
            return Err(format!("probing the host failed: {}", stderr.trim()));
        }
        let probe = Probe::parse(&stdout)
            .ok_or_else(|| "probing the host printed no report".to_string())?;
        self.socket.lock().unwrap().get_or_insert(probe.socket());
        Ok(probe)
    }

    /// The daemon's report, through `cli`, without starting it.
    async fn status(&self, cli: &str) -> Result<tower_protocol::DaemonInfo, String> {
        let mut command = self.shell_command(remote_command(
            Some(cli),
            &cli_args(&["--json", "daemon", "status"]),
        ));
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run {}: {error}", self.runner()))?;
        let (status, stdout, stderr) =
            tokio::time::timeout(RPC_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
                .await
                .map_err(|_| format!("`{CLI} daemon status` timed out"))?
                .map_err(|error| format!("{} failed: {error}", self.runner()))?;
        if !status.success() {
            let message = host_text_safe(stderr.trim());
            return Err(if message.is_empty() {
                format!("rc={:?}", status.code())
            } else {
                message
            });
        }
        let info: tower_protocol::DaemonInfo = json_in(&stdout)
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| format!("`{CLI} daemon status` printed no report"))?;
        if let Some(socket) = &info.socket {
            *self.socket.lock().unwrap() = Some(socket.clone());
        }
        Ok(info)
    }

    /// Start the deployed daemon, detached, from the login shell so its
    /// sessions get that environment, and wait until it answers.
    async fn launch(&self, cli: &str) -> Result<(), String> {
        self.log.info("starting Tower on the host");
        let line = format!(
            "{} {}",
            login_shell_safe(&launch_script()),
            login_shell_quote_arg(LAUNCH_COMMAND)
        );
        let (status, _, stderr) = self.shell(line, None).await?;
        if !status.success() {
            return Err(format!(
                "starting Tower failed: {}",
                host_text_safe(stderr.trim())
            ));
        }
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            let last = match self.status(cli).await {
                Ok(info) => return compatible(&info),
                Err(reason) => reason,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "Tower did not come up within {}s: {last}",
                    LAUNCH_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(LAUNCH_POLL).await;
        }
    }

    /// Make the deployed build this dashboard's, uploading it unless
    /// `current` already names it.
    async fn install(&self, bundles: &Bundles, current: &str) -> Result<(), String> {
        if bundles.0.is_empty() {
            return Err("this dashboard carries no Tower for the host".into());
        }
        if bundles.0.iter().any(|(_, digest, _)| digest == current) {
            return Ok(());
        }
        let key: String = bundles
            .0
            .iter()
            .map(|(_, digest, _)| digest.as_str())
            .collect();
        if let Some((refused, at)) = &*self.refused.lock().unwrap()
            && *refused == key
            && at.elapsed() < DEPLOY_COOLDOWN
        {
            return Err("the last deploy of its Tower failed; retrying later".into());
        }
        let mut reasons = Vec::new();
        for (target, digest, bundle) in &bundles.0 {
            self.log
                .info(format!("deploying Tower {digest} for {target}"));
            match self.upload(digest, bundle).await {
                Ok(()) => {
                    self.log.info(format!("deployed Tower {digest} ({target})"));
                    return Ok(());
                }
                Err(reason) => reasons.push(format!("{target}: {reason}")),
            }
        }
        *self.refused.lock().unwrap() = Some((key, Instant::now()));
        Err(format!(
            "deploying its Tower failed: {}",
            reasons.join("; ")
        ))
    }

    async fn upload(
        &self,
        digest: &str,
        bundle: &'static crate::server_payload::TowerPayload,
    ) -> Result<(), String> {
        let binaries = tokio::task::spawn_blocking(move || bundle.decompress())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        for (i, (binary, bytes)) in binaries.into_iter().enumerate() {
            let (status, _, stderr) = self
                .shell(
                    login_shell_safe(&stage_script(digest, binary, i == 0)),
                    Some(bytes),
                )
                .await?;
            if !status.success() {
                return Err(format!("sending {binary}: {}", stderr.trim()));
            }
        }
        let (status, _, stderr) = self
            .shell(login_shell_safe(&publish_script(digest)), None)
            .await?;
        if !status.success() {
            return Err(format!("the host cannot run it: {}", stderr.trim()));
        }
        Ok(())
    }

    /// Hand `line` to the host's shell, feeding it `stdin`.
    async fn shell(
        &self,
        line: String,
        stdin: Option<Vec<u8>>,
    ) -> Result<(std::process::ExitStatus, String, String), String> {
        let mut child = self
            .shell_command(line)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not run {}: {error}", self.runner()))?;
        // Written from a task while the output drains: one task doing both
        // deadlocks once a pipe fills.
        let writer = match (child.stdin.take(), stdin) {
            (Some(mut pipe), Some(bytes)) => Some(tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                pipe.write_all(&bytes).await?;
                pipe.shutdown().await
            })),
            _ => None,
        };
        let output = tokio::time::timeout(DEPLOY_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| format!("timed out after {}s", DEPLOY_TIMEOUT.as_secs()))?
            .map_err(|error| format!("{} failed: {error}", self.runner()))?;
        if let Some(writer) = writer
            && output.0.success()
            && !matches!(writer.await, Ok(Ok(())))
        {
            return Err("the upload was cut short".into());
        }
        Ok(output)
    }

    /// A command handing `line` to the host's shell.
    fn shell_command(&self, line: String) -> Command {
        let mut command = match &self.via {
            Via::Ssh { target, opts } => {
                let mut command = Command::new("ssh");
                command.args(opts).arg("-T").arg(target).arg(line);
                command
            }
            Via::Local => {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(line);
                command
            }
        };
        #[cfg(test)]
        if let Some(socket) = self.probe_socket.get() {
            command.env("TOWER_SOCKET", socket);
        }
        command.kill_on_drop(true);
        command
    }

    /// What runs the host's commands, for error messages.
    fn runner(&self) -> &'static str {
        match self.via {
            Via::Ssh { .. } => "ssh",
            Via::Local => "the shell",
        }
    }

    /// Fold one event into the rows.
    fn apply(&self, event: Event, since: &mut HashMap<String, u64>) {
        let mut rows = self.rows.lock().unwrap();
        match event {
            Event::Snapshot(records) => {
                rows.clear();
                for record in records {
                    if let Some(row) = self.row(record, since) {
                        rows.insert(row.0, row.1);
                    }
                }
                self.mirrored.store(true, Ordering::Relaxed);
            }
            Event::Removed(id) => {
                rows.remove(&id);
                since.remove(&id);
            }
            Event::Record(record) => {
                let id = record.id.clone();
                match self.row(*record, since) {
                    Some((id, row)) => rows.insert(id, row),
                    None => rows.remove(&id),
                };
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The row for a record, or `None` for one the dashboard does not list:
    /// an ended session, or one Tower observes but does not run.
    fn row(
        &self,
        record: AgentRecord,
        since: &mut HashMap<String, u64>,
    ) -> Option<(String, LauncherState)> {
        if record.ownership != "owned" || matches!(record.status.as_str(), "exited" | "lost") {
            since.remove(&record.id);
            return None;
        }
        let (id, attached) = (record.id.clone(), record.attach_count > 0);
        // The record's paths are already relative to the host's home.
        let mut row = crate::tower::row(record, "", since);
        row.launcher_pid = launcher_key(&id);
        row.pool_session = Some(id.clone());
        row.attached = Some(attached);
        row.host = self.host.clone();
        Some((id, row))
    }

    fn lost(&self) {
        let was = self.mirrored.swap(false, Ordering::Relaxed);
        let mut rows = self.rows.lock().unwrap();
        if was || !rows.is_empty() {
            rows.clear();
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

/// Whether `key` names a Tower session; see [`launcher_key`].
pub(crate) fn is_tower_key(key: &SessionKey) -> bool {
    key.as_str()
        .parse::<u32>()
        .is_ok_and(|pid| pid >= 0x8000_0000)
}

/// A stand-in launcher pid for a Tower session, which the dashboard keys
/// rows by. The high bit keeps it clear of every real pid, so it cannot
/// collide with a `miao-server` row on the same host.
fn launcher_key(id: &str) -> u32 {
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let hash = id.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    0x8000_0000 | hash
}

/// The CLI's arguments. With the flag unset the dashboard only follows
/// sessions Tower already runs, so it must never start a daemon.
/// The CLI never starts a daemon: [`TowerLink::ready`] decides whether one
/// may be started, and which build.
fn cli_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    std::iter::once("--no-start")
        .chain(args.iter().copied())
        .collect()
}

/// The remote command running `cli args` in the account's login shell, where
/// a user's `PATH` and environment are complete. Before [`TowerLink::ready`]
/// settles a CLI, as when Tower is not enabled for the host, the deployed
/// build wins over one on `PATH`.
fn remote_command(cli: Option<&str>, args: &[&str]) -> String {
    let program = cli.map_or_else(|| "\"$CM_TOWER_CLI\"".to_string(), login_shell_quote_arg);
    let command = std::iter::once(format!("exec {program}"))
        .chain(args.iter().map(|arg| login_shell_quote_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    let script = match cli {
        Some(_) => "exec \"${SHELL:-/bin/sh}\" -l -c \"$0\"".to_string(),
        None => format!(
            "c=\"$HOME/{DEPLOY_REL}/current/{CLI}\"; [ -x \"$c\" ] || c={CLI}; \
             export CM_TOWER_CLI=\"$c\"; exec \"${{SHELL:-/bin/sh}}\" -l -c \"$0\""
        ),
    };
    format!(
        "{} {}",
        login_shell_safe(&script),
        login_shell_quote_arg(&command)
    )
}

// =============================================================================
// Deploying the Tower this dashboard carries
// =============================================================================

/// Where deployed Tower builds live on a host, relative to `$HOME`: one
/// directory per build, named by its digest, and `current` linking to the one
/// in use. A build is never replaced in place, so a daemon already running
/// from an older one keeps its files.
const DEPLOY_REL: &str = ".cache/captain-miao/tower";

const DEPLOY_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a failed deploy of one build is not retried.
const DEPLOY_COOLDOWN: Duration = Duration::from_secs(300);

/// How long a Tower this dashboard started has to answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_POLL: Duration = Duration::from_millis(250);

/// What a host reports, one `key=value` line each: its platform, `$HOME`,
/// the build `current` names, the first `tower-cli` that runs (login `PATH`,
/// then the deployed one), whether this account runs a Tower daemon, and the
/// login shell's `TOWER_SOCKET` and `XDG_STATE_HOME`, which place the
/// daemon's socket. A `PATH` entry may be a wrapper around a build that is gone, so a
/// CLI counts only when `--version` succeeds. The process pattern brackets a
/// letter so it never matches this script's own command line.
fn probe_script() -> String {
    format!(
        "echo platform=$(uname -sm); echo home=$HOME; \
         echo current=$(readlink \"$HOME/{DEPLOY_REL}/current\" 2>/dev/null); \
         cli=; for c in \"$(\"${{SHELL:-/bin/sh}}\" -l -c \"command -v {CLI}\" </dev/null 2>/dev/null | tail -n 1)\" \
           \"$HOME/{DEPLOY_REL}/current/{CLI}\"; do \
           case \"$c\" in /*) ;; *) continue;; esac; \
           if [ -x \"$c\" ] && \"$c\" --version </dev/null >/dev/null 2>&1; then cli=$c; break; fi; \
         done; echo cli=$cli; \
         u=$(id -u); \
         pgrep -u \"$u\" -f \"^[^ ]*[t]ower( |$)\" >/dev/null 2>&1 && echo daemon=1; \
         \"${{SHELL:-/bin/sh}}\" -l -c env </dev/null 2>/dev/null \
           | sed -n -e \"s/^TOWER_SOCKET=/tower_socket=/p\" -e \"s/^XDG_STATE_HOME=/state_home=/p\"; \
         true"
    )
}

/// What [`probe_script`] reported.
#[derive(Debug, Default, PartialEq)]
struct Probe {
    platform: String,
    home: String,
    current: String,
    cli: Option<String>,
    daemon: bool,
    tower_socket: Option<String>,
    state_home: Option<String>,
}

impl Probe {
    /// The report in `output`, skipping anything a login script printed.
    fn parse(output: &str) -> Option<Self> {
        let mut probe = Self::default();
        let mut seen = false;
        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key {
                "platform" => {
                    seen = true;
                    probe.platform = value;
                }
                "home" => probe.home = value,
                "current" => probe.current = value,
                "cli" => probe.cli = Some(value).filter(|cli| !cli.is_empty()),
                "daemon" => probe.daemon = value == "1",
                "tower_socket" => probe.tower_socket = Some(value),
                "state_home" => probe.state_home = Some(value),
                _ => {}
            }
        }
        (seen && !probe.home.is_empty()).then_some(probe)
    }

    /// Where Tower puts its daemon's socket in that environment, as
    /// `tower_pal::Paths::discover` does: `TOWER_SOCKET`, else under the
    /// state directory. A relative value is ignored, as Tower ignores it.
    /// Retain this fallback until all hosts report the socket in daemon status.
    fn socket(&self) -> String {
        let absolute = |value: &Option<String>| value.clone().filter(|v| v.starts_with('/'));
        absolute(&self.tower_socket).unwrap_or_else(|| {
            let state =
                absolute(&self.state_home).unwrap_or_else(|| format!("{}/.local/state", self.home));
            format!("{state}/tower/run/towerd.sock")
        })
    }
}

/// The Tower builds this dashboard carries for a platform, best first.
struct Bundles(
    Vec<(
        &'static str,
        String,
        &'static crate::server_payload::TowerPayload,
    )>,
);

impl Bundles {
    fn for_platform(platform: &str) -> Self {
        Self(
            crate::server_payload::target_candidates(platform)
                .iter()
                .filter_map(|target| {
                    crate::server_payload::tower_bundle(target)
                        .map(|(digest, bundle)| (*target, digest, bundle))
                })
                .collect(),
        )
    }
}

/// Whether a running daemon is one this dashboard can drive: it speaks this
/// build's protocol major.
fn compatible(info: &tower_protocol::DaemonInfo) -> Result<(), String> {
    let theirs = info.protocol_version.major;
    let ours = tower_protocol::PROTOCOL_VERSION.major;
    if theirs != ours {
        return Err(format!(
            "the host's Tower {} speaks protocol {theirs}, not {ours}; \
             this needs manual care on the host",
            info.version
        ));
    }
    Ok(())
}

/// The deployed build's daemon, detaching itself. It starts the terminal
/// pool beside its own binary when a session first needs one. Runs in the
/// login shell with `CM_TOWER_DIR` naming the build.
const LAUNCH_COMMAND: &str = "exec \"$CM_TOWER_DIR/tower\" run --detach";

/// Run [`LAUNCH_COMMAND`], passed as `$0`, in the account's login shell.
fn launch_script() -> String {
    format!(
        "set -e; export CM_TOWER_DIR=\"$HOME/{DEPLOY_REL}/current\"; \
         exec \"${{SHELL:-/bin/sh}}\" -l -c \"$0\" </dev/null"
    )
}

/// Stream one binary into the build's staging directory. The first binary
/// clears whatever an interrupted attempt left there.
fn stage_script(digest: &str, binary: &str, first: bool) -> String {
    let clear = if first { "rm -rf \"$d\"; " } else { "" };
    format!(
        "set -e; d=\"$HOME/{DEPLOY_REL}/{digest}.incoming\"; {clear}mkdir -p \"$d\"; \
         cat > \"$d/{binary}\"; chmod 0755 \"$d/{binary}\""
    )
}

/// Check the host can run the staged build, publish it, point `current` at
/// it, and remove builds no process runs from.
fn publish_script(digest: &str) -> String {
    format!(
        "set -e; r=\"$HOME/{DEPLOY_REL}\"; d=\"$r/{digest}\"; \
         \"$d.incoming/{CLI}\" --version; \
         rm -rf \"$d\"; mv \"$d.incoming\" \"$d\"; ln -sfn {digest} \"$r/current\"; \
         for o in \"$r\"/*; do \
           case \"$o\" in \"$d\"|\"$r/current\") continue;; esac; \
           if ls -l /proc/[0-9]*/exe 2>/dev/null | grep -qF \"$o/\"; then continue; fi; \
           rm -rf \"$o\"; \
         done"
    )
}

/// The JSON value in `output`, skipping anything a login script printed
/// before it.
fn json_in(output: &str) -> Option<serde_json::Value> {
    let start = output.find(['{', '['])?;
    serde_json::Deserializer::from_str(&output[start..])
        .into_iter()
        .next()?
        .ok()
}

enum Event {
    Snapshot(Vec<AgentRecord>),
    Record(Box<AgentRecord>),
    Removed(String),
}

/// One event notification, as JSON, or `None` for one that is not a
/// session event.
fn event(line: &[u8]) -> Option<Event> {
    #[derive(serde::Deserialize)]
    struct Line {
        method: String,
        params: serde_json::Value,
    }
    let Line { method, params } = serde_json::from_slice(line).ok()?;
    if method == "snapshot" {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            agents: Vec<AgentRecord>,
        }
        return Some(Event::Snapshot(
            serde_json::from_value::<Snapshot>(params).ok()?.agents,
        ));
    }
    #[derive(serde::Deserialize)]
    struct Params {
        id: String,
        #[serde(default)]
        record: Option<AgentRecord>,
    }
    let Params { id, record } = serde_json::from_value(params).ok()?;
    match record {
        _ if method == "agent.removed" => Some(Event::Removed(id)),
        Some(record) => Some(Event::Record(Box::new(record))),
        None => None,
    }
}

/// Follow the host's session records for as long as the link lives,
/// re-subscribing after each loss.
async fn follow(link: std::sync::Weak<TowerLink>) {
    let mut backoff = RETRY_INITIAL;
    loop {
        let Some(this) = link.upgrade() else { return };
        let started = Instant::now();
        // With the flag unset, a host without Tower or without a running
        // daemon is the normal case, not worth a line in the log.
        let enabled = this.enabled;
        if enabled {
            this.log.info("following Tower sessions on its socket");
        }
        let outcome = match enabled {
            true => match this.ready().await {
                Ok(_) => follow_once(&this).await,
                Err(reason) => Err(reason),
            },
            false => follow_once(&this).await,
        };
        // The daemon may be gone with the stream; look again before reuse.
        this.forget();
        let was_up = this.reported();
        if enabled || was_up {
            match outcome {
                Ok(()) => this.log.error("the Tower event stream ended"),
                Err(reason) => this.log.error(format!("Tower: {reason}")),
            }
        }
        this.lost();
        drop(this);
        if started.elapsed() >= HEALTHY {
            backoff = RETRY_INITIAL;
        }
        let wait = if enabled || was_up {
            backoff
        } else {
            RETRY_DORMANT
        };
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

/// Subscribe to the host's session records and fold them into the rows
/// until the connection fails.
async fn follow_once(link: &TowerLink) -> Result<(), String> {
    let (generation, connection) = link.connection().await.map_err(|failure| failure.message)?;
    let result = follow_events(link, &connection).await;
    drop(connection);
    link.release(generation).await;
    result
}

async fn follow_events(link: &TowerLink, connection: &Connection) -> Result<(), String> {
    let (subscribed, mut events) = connection
        .subscribe(&tower_protocol::SubscribeParams {
            topics: vec!["agents".into()],
            approval_id: None,
        })
        .await
        .map_err(|error| format!("subscribing failed: {error}"))?;
    let mut since = HashMap::new();
    link.apply(Event::Snapshot(subscribed.agents), &mut since);
    loop {
        let notification = events
            .next()
            .await
            .map_err(|error| format!("the event stream failed: {error}"))?;
        let line = serde_json::to_vec(&notification).map_err(|e| e.to_string())?;
        if let Some(event) = event(&line) {
            link.apply(event, &mut since);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: &str, ownership: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "display_name": null, "node": "local", "harness": "claude",
            "mode": null, "ownership": ownership, "trust": "strong",
            "native_session_id": "c-1", "pid": 7, "pid_start_time": 1,
            "cwd": "~/work", "status": status, "status_detail": {"last_prompt": "hi"},
            "caps": {"attach": true, "kill": true, "message": "none",
                     "message_reason": null, "wake_when_idle": false,
                     "reply_path": false, "steer": false},
            "pool_session": "p", "attach_count": 1, "sources": [],
            "registered_at": 1, "updated_at": 2000, "last_seen": 1,
            "revision": 1, "tags": [], "needs_attention": true, "sync_state": "synced",
        })
    }

    fn line(method: &str, params: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"method": method, "params": params})).unwrap()
    }

    /// A stand-in Tower daemon on a private socket. It answers `hello`,
    /// `subscribe` (with `snapshot`, then pushes `pushed`), `session.open`
    /// (`ag-new`) and acknowledges other calls, except: `session.kill`
    /// refuses with `not_found`, and the first call named in `hangup` on a
    /// connection that has already answered a call closes that connection
    /// unanswered. Every method it receives is recorded.
    struct MockTower {
        socket: std::path::PathBuf,
        calls: Arc<Mutex<Vec<String>>>,
        delayed_open: Arc<Mutex<Option<DelayedOpen>>>,
        snapshots: Arc<tokio::sync::Semaphore>,
        _dir: tempfile::TempDir,
    }

    struct DelayedOpen {
        event: serde_json::Value,
        release: tokio::sync::oneshot::Receiver<()>,
    }

    impl MockTower {
        fn start(
            snapshot: Vec<serde_json::Value>,
            pushed: Vec<serde_json::Value>,
            hangup: &'static [&'static str],
        ) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("towerd.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let delayed_open = Arc::new(Mutex::new(None::<DelayedOpen>));
            let snapshots = Arc::new(tokio::sync::Semaphore::new(1));
            let delay = delayed_open.clone();
            let permits = snapshots.clone();
            let recorded = calls.clone();
            let hung = Arc::new(Mutex::new(std::collections::HashSet::new()));
            tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (snapshot, pushed) = (snapshot.clone(), pushed.clone());
                    let (delay, permits) = (delay.clone(), permits.clone());
                    let (recorded, hung) = (recorded.clone(), hung.clone());
                    tokio::spawn(async move {
                        let mut reader = tokio::io::BufReader::new(stream);
                        let mut answered = 0;
                        while let Ok(Some(frame)) =
                            tower_protocol::framing::read_frame(&mut reader).await
                        {
                            let request: serde_json::Value =
                                serde_json::from_slice(&frame).unwrap();
                            let method = request["method"].as_str().unwrap().to_string();
                            recorded.lock().unwrap().push(method.clone());
                            if answered > 0
                                && hangup.contains(&method.as_str())
                                && hung.lock().unwrap().insert(method.clone())
                            {
                                return;
                            }
                            let _snapshot = if method == "subscribe" {
                                Some(permits.acquire().await.unwrap())
                            } else {
                                None
                            };
                            if method == "session.open" {
                                let held = delay.lock().unwrap().take();
                                if let Some(DelayedOpen { event, release }) = held {
                                    tower_protocol::framing::write_frame(reader.get_mut(), &event)
                                        .await
                                        .unwrap();
                                    release.await.unwrap();
                                }
                            }
                            let body = match method.as_str() {
                                "hello" => serde_json::json!({"result": {
                                    "protocol_version": tower_protocol::PROTOCOL_VERSION,
                                    "capabilities": [],
                                }}),
                                "subscribe" => serde_json::json!({"result": {
                                    "subscription_id": "s", "cursor": 1, "agents": snapshot,
                                }}),
                                "session.open" => serde_json::json!({"result": {
                                    "id": "ag-new", "mode": null,
                                }}),
                                "session.kill" => serde_json::json!({"error": {
                                    "code": -32000, "message": "Session not found",
                                    "data": {"code": "not_found"},
                                }}),
                                _ => serde_json::json!({"result": {}}),
                            };
                            let mut reply = body;
                            reply["jsonrpc"] = "2.0".into();
                            reply["id"] = request["id"].clone();
                            let write = reader.get_mut();
                            tower_protocol::framing::write_frame(write, &reply)
                                .await
                                .unwrap();
                            if method == "fixture.close_after_reply" {
                                return;
                            }
                            answered += 1;
                            if method == "subscribe" {
                                for event in &pushed {
                                    tower_protocol::framing::write_frame(write, event)
                                        .await
                                        .unwrap();
                                }
                            }
                        }
                    });
                }
            });
            Self {
                socket,
                calls,
                delayed_open,
                snapshots,
                _dir: dir,
            }
        }

        /// A local link that reaches this daemon, its Tower already settled.
        fn link(&self) -> Arc<TowerLink> {
            let link = TowerLink::with(
                HostId("desk".into()),
                Via::Local,
                false,
                Arc::default(),
                Arc::default(),
            );
            link.probe_socket.set(self.socket.clone()).unwrap();
            *link.socket.lock().unwrap() = Some(self.socket.to_string_lossy().into_owned());
            *link.cli.lock().unwrap() = Some("tower-cli".into());
            link
        }

        fn calls(&self, method: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == method)
                .count()
        }
    }

    /// Wait, within a watchdog well past any real delay, for `done`.
    async fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(40);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn the_socket_feeds_the_rows_and_answers_calls() {
        let tower = MockTower::start(
            vec![record("ag-1", "idle", "owned")],
            vec![serde_json::json!({"jsonrpc": "2.0", "method": "agent.hook",
                "params": {"seq": 2, "id": "ag-2", "record": record("ag-2", "working", "owned")}})],
            &[],
        );
        let link = tower.link();
        let follower = tokio::spawn({
            let link = link.clone();
            async move { follow_once(&link).await }
        });
        until("the snapshot and the pushed record", || {
            link.reported() && link.rows().len() == 2
        })
        .await;

        let spec = OpenSpec {
            agent: crate::agent::AgentKind::Claude,
            cwd: "~/work".into(),
            resume: None,
            worktree: None,
        };
        match link.open(spec).await.unwrap() {
            LaunchPlan::AttachRemote { session_name, .. } => assert_eq!(session_name, "ag-new"),
            LaunchPlan::SpawnLocal { .. } => panic!("a Tower session attaches"),
        }
        assert!(matches!(link.kill("ag-9").await, KillOutcome::AlreadyGone));
        assert!(
            link.set_flags("ag-1", SessionFlags::default()).await,
            "tagging and acknowledging succeed"
        );
        // The mock advertises no concurrency capability.
        assert_eq!(tower.calls("hello"), 1);
        follower.abort();
    }

    #[tokio::test]
    async fn a_dead_connection_is_replaced_and_only_safe_calls_repeat() {
        let tower = MockTower::start(vec![], vec![], &["session.kill", "session.open"]);
        let link = tower.link();
        link.rpc("session.attention.clear", serde_json::json!({"id": "ag-1"}))
            .await
            .unwrap();
        // The connection dies under a call that is safe to repeat: it is
        // repeated once on a new connection.
        assert!(matches!(link.kill("ag-1").await, KillOutcome::AlreadyGone));
        assert_eq!(tower.calls("session.kill"), 2);
        // One that reserves a session is not repeated.
        let failure = link
            .rpc("session.open", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(failure.unreachable, "{failure}");
        assert_eq!(tower.calls("session.open"), 1);
        // The next call reconnects.
        link.rpc("session.open", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(tower.calls("hello"), 3);
    }

    #[tokio::test]
    async fn an_event_updates_rows_before_the_open_reply() {
        let tower = MockTower::start(vec![], vec![], &[]);
        let link = tower.link();
        let follower = tokio::spawn({
            let link = link.clone();
            async move { follow_once(&link).await }
        });
        until("subscription snapshot", || link.reported()).await;
        let (release, wait) = tokio::sync::oneshot::channel();
        let event = serde_json::json!({"jsonrpc":"2.0","method":"agent.changed","params":{
            "id":"ag-pending","seq":2,"record":record("ag-pending","working","owned")}});
        *tower.delayed_open.lock().unwrap() = Some(DelayedOpen {
            event,
            release: wait,
        });
        let open = tokio::spawn({
            let link = link.clone();
            async move { link.rpc("session.open", serde_json::json!({})).await }
        });
        until("row delivered while session.open awaits its reply", || {
            link.owns("ag-pending")
        })
        .await;
        assert_eq!(tower.calls("hello"), 1);
        release.send(()).unwrap();
        assert_eq!(open.await.unwrap().unwrap()["id"], "ag-new");
        follower.abort();
    }

    #[tokio::test]
    async fn an_unsent_open_reconnects_and_reserves_once() {
        let tower = MockTower::start(vec![], vec![], &[]);
        let link = tower.link();
        let (_, first) = link.connection().await.unwrap();
        link.rpc("fixture.close_after_reply", serde_json::json!({}))
            .await
            .unwrap();
        first.closed().await;
        link.rpc("session.open", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(tower.calls("session.open"), 1);
        assert_eq!(tower.calls("hello"), 2);
    }

    #[tokio::test]
    async fn follower_clears_rows_and_reloads_after_disconnect() {
        let tower = MockTower::start(vec![record("ag-1", "idle", "owned")], vec![], &[]);
        let link = tower.link();
        link.start();
        until("first snapshot", || link.owns("ag-1")).await;
        let hold = tower.snapshots.acquire().await.unwrap();
        link.rpc("fixture.close_after_reply", serde_json::json!({}))
            .await
            .unwrap();
        until("reconnected follower awaiting snapshot", || {
            tower.calls("subscribe") == 2
        })
        .await;
        assert!(!link.reported());
        assert!(link.rows().is_empty());
        drop(hold);
        until("replacement snapshot", || link.owns("ag-1")).await;
        assert_eq!(tower.calls("hello"), 2);
        link.stop();
    }

    #[tokio::test]
    async fn an_absent_daemon_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let link = TowerLink::local(HostId("desk".into()), Arc::default(), Arc::default());
        link.probe_socket
            .set(dir.path().join("absent.sock"))
            .unwrap();
        *link.socket.lock().unwrap() = Some(
            dir.path()
                .join("absent.sock")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(matches!(link.kill("ag-1").await, KillOutcome::Unreachable));
        // A failed connection forgets the socket; name it again.
        *link.socket.lock().unwrap() = Some(
            dir.path()
                .join("absent.sock")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(follow_once(&link).await.is_err());
    }

    fn link() -> Arc<TowerLink> {
        TowerLink::new(
            HostId("box".into()),
            "box",
            &[],
            false,
            Arc::default(),
            Arc::default(),
        )
    }

    #[test]
    fn events_keep_the_hosts_running_sessions() {
        let link = link();
        let mut since = HashMap::new();
        let mut feed = |bytes: Vec<u8>| link.apply(event(&bytes).unwrap(), &mut since);
        feed(line(
            "snapshot",
            serde_json::json!({"subscription_id": "s", "cursor": 1, "agents": [
                record("ag-1", "idle", "owned"),
                record("ag-2", "exited", "owned"),
                record("ag-3", "idle", "registered"),
            ]}),
        ));
        assert!(link.reported());
        let rows = link.rows();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.binding_token(), Some("ag-1"));
        assert_eq!(row.host, HostId("box".into()));
        assert_eq!(row.cwd, "~/work");
        assert_eq!(row.attached, Some(true));
        assert!(row.flags.is_some_and(|flags| flags.follow_up));
        assert_eq!(link.id(&row.key()).as_deref(), Some("ag-1"));
        assert!(link.owns("ag-1") && !link.owns("ag-2"));

        feed(line(
            "agent.hook",
            serde_json::json!({"seq": 2, "id": "ag-4", "record": record("ag-4", "working", "owned")}),
        ));
        assert_eq!(link.rows().len(), 2);
        feed(line(
            "agent.exit",
            serde_json::json!({"seq": 3, "id": "ag-4", "record": record("ag-4", "exited", "owned")}),
        ));
        feed(line(
            "agent.removed",
            serde_json::json!({"seq": 4, "id": "ag-1"}),
        ));
        assert!(link.rows().is_empty());
        assert!(event(b"Welcome to the host\n").is_none());
    }

    #[test]
    fn the_cli_never_starts_a_daemon() {
        assert_eq!(cli_args(&["events"]), ["--no-start", "events"]);
    }

    #[test]
    fn tower_keys_never_collide_with_a_real_pid() {
        let key = launcher_key("ag-1");
        assert!(is_tower_key(&SessionKey::from_launcher_pid(key)));
        assert!(!is_tower_key(&SessionKey::from_launcher_pid(4_194_304)));
        assert_eq!(key, launcher_key("ag-1"));
        assert_ne!(key, launcher_key("ag-2"));
    }

    fn executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Run `script` as ssh would hand it to the host's login shell.
    fn on_host(
        home: &std::path::Path,
        bin: &std::path::Path,
        script: &str,
        stdin: &[u8],
    ) -> std::process::Output {
        use std::io::Write;
        // A login shell that runs its `-c` command.
        let shell = bin.join("login");
        executable(
            &shell,
            r#"[ "$1" = -l ] && [ "$2" = -c ] && exec /bin/sh -c "$3""#,
        );
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .env("HOME", home)
            .env("SHELL", &shell)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn the_cli_runs_in_the_login_shell_with_its_arguments_intact() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        // A CLI on PATH that prints each argument on its own line.
        executable(&bin.join("tower-cli"), r#"for arg; do echo "$arg"; done"#);
        let params = r#"{"cwd":"~/it's \ $HOME"}"#;
        let run = || {
            on_host(
                &home,
                &bin,
                &remote_command(None, &["rpc", "session.open", params]),
                b"",
            )
        };
        let out = run();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("rpc\nsession.open\n{params}\n"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A deployed build wins over the one on PATH.
        executable(
            &home.join(DEPLOY_REL).join("current/tower-cli"),
            r#"echo deployed "$1""#,
        );
        assert_eq!(String::from_utf8_lossy(&run().stdout), "deployed rpc\n");
        // A CLI the link settled on runs as named, whatever else is there.
        let settled = dir.path().join("it's here/tower-cli");
        executable(&settled, r#"echo settled "$1""#);
        let out = on_host(
            &home,
            &bin,
            &remote_command(Some(settled.to_str().unwrap()), &["rpc"]),
            b"",
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "settled rpc\n");
        assert_eq!(
            json_in("motd\n{\"id\":\"ag-1\"}\n"),
            Some(serde_json::json!({"id": "ag-1"}))
        );
    }

    /// Pooled-localhost attaches to this machine's Tower the way ssh attaches
    /// to a host's: the same CLI through the account's login shell.
    #[test]
    fn a_local_link_runs_the_cli_in_this_machines_login_shell() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        executable(&bin.join("tower-cli"), r#"for arg; do echo "$arg"; done"#);
        let shell = bin.join("login");
        executable(
            &shell,
            r#"[ "$1" = -l ] && [ "$2" = -c ] && exec /bin/sh -c "$3""#,
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let link = TowerLink::local(HostId("desk".into()), Arc::default(), Arc::default());

        let argv = link.attach_argv("ag-1");
        assert_eq!(argv[..2], ["/bin/sh", "-c"]);
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("HOME", &home)
            .env("SHELL", &shell)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            cli_args(&["attach", "ag-1"]).join("\n") + "\n"
        );
    }

    #[test]
    fn a_deploy_publishes_a_new_build_beside_the_ones_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        let root = home.join(DEPLOY_REL);
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::create_dir_all(root.join("gone.incoming")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let stage = |digest: &str, binary: &str, first: bool, body: &str| {
            let out = on_host(
                &home,
                &bin,
                &login_shell_safe(&stage_script(digest, binary, first)),
                format!("#!/bin/sh\n{body}\n").as_bytes(),
            );
            assert!(out.status.success(), "{out:?}");
        };
        stage("d1", "tower-cli", true, "echo tower-cli 0.1.0");
        stage("d1", "tower", false, "exit 0");
        assert_eq!(probe(&home, &bin).current, "");
        let out = on_host(&home, &bin, &login_shell_safe(&publish_script("d1")), b"");
        assert!(out.status.success(), "{out:?}");
        assert!(root.join("d1/tower-cli").is_file() && root.join("d1/tower").is_file());
        assert!(!root.join("d1.incoming").exists());
        // Builds no process runs from are removed; `current` names the new one.
        assert!(!root.join("old").exists() && !root.join("gone.incoming").exists());
        assert_eq!(probe(&home, &bin).current, "d1");

        // A build the host cannot run is never published.
        stage("d2", "tower-cli", true, "exit 1");
        let out = on_host(&home, &bin, &login_shell_safe(&publish_script("d2")), b"");
        assert!(!out.status.success());
        assert!(!root.join("d2").exists());
        assert_eq!(probe(&home, &bin).current, "d1");
    }

    fn probe(home: &std::path::Path, bin: &std::path::Path) -> Probe {
        let out = on_host(home, bin, &login_shell_safe(&probe_script()), b"");
        assert!(out.status.success(), "{out:?}");
        Probe::parse(&String::from_utf8_lossy(&out.stdout)).expect("a probe report")
    }

    /// The socket is placed from the login shell's environment, as Tower
    /// places it: `TOWER_SOCKET` first, then the state directory.
    #[test]
    fn the_probe_places_the_socket_as_tower_would() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let expected = match std::env::var("XDG_STATE_HOME") {
            Ok(state) if state.starts_with('/') => format!("{state}/tower/run/towerd.sock"),
            _ => format!("{}/.local/state/tower/run/towerd.sock", home.display()),
        };
        assert_eq!(probe(&home, &bin).socket(), expected);

        let named =
            Probe::parse("platform=Linux\nhome=/h\ntower_socket=/run/t.sock\nstate_home=/s\n")
                .unwrap();
        assert_eq!(named.socket(), "/run/t.sock");
        let relative = Probe::parse("platform=Linux\nhome=/h\nstate_home=s\n").unwrap();
        assert_eq!(relative.socket(), "/h/.local/state/tower/run/towerd.sock");
    }

    /// A `PATH` wrapper around a build that is gone is no CLI, so the
    /// deployed one is found instead; a working one on `PATH` comes first.
    #[test]
    fn the_probe_finds_a_cli_that_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let found = probe(&home, &bin);
        assert_eq!(found.cli, None);
        assert_eq!(found.home, home.to_str().unwrap());
        assert!(!found.platform.is_empty());

        executable(&bin.join("tower-cli"), "exit 127");
        assert_eq!(probe(&home, &bin).cli, None);
        let deployed = home.join(DEPLOY_REL).join("current/tower-cli");
        executable(&deployed, "exit 0");
        assert_eq!(
            probe(&home, &bin).cli.as_deref(),
            deployed.to_str(),
            "the deployed CLI stands in for a broken one on PATH"
        );
        executable(&bin.join("tower-cli"), "exit 0");
        assert_eq!(
            probe(&home, &bin).cli.as_deref(),
            bin.join("tower-cli").to_str()
        );
    }

    /// The probe sees this account's Tower daemon by its program name.
    #[test]
    fn the_probe_sees_a_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        let build = dir.path().join("build");
        // A process whose command line names a deployed build's daemon.
        let mut running: Vec<_> = ["tower"]
            .into_iter()
            .map(|name| {
                std::process::Command::new("bash")
                    .args(["-c", r#"exec -a "$0" bash -c "sleep 30; true""#])
                    .arg(build.join(name))
                    .spawn()
                    .unwrap()
            })
            .collect();
        // They are seen once `exec` has renamed them; allow far longer.
        let deadline = Instant::now() + Duration::from_secs(10);
        let found = loop {
            let found = probe(&home, &bin);
            if found.daemon || Instant::now() >= deadline {
                break found;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        for child in &mut running {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(found.daemon, "the probe never saw the daemon: {found:?}");
    }

    #[test]
    fn a_probe_report_survives_a_chatty_login() {
        let probe = Probe::parse(
            "Welcome\nplatform=Linux x86_64\nhome=/h\ncurrent=\ncli=/h/bin/tower-cli\ndaemon=1\n\
             state_home=/h/state\n",
        )
        .unwrap();
        assert_eq!(
            probe,
            Probe {
                platform: "Linux x86_64".into(),
                home: "/h".into(),
                current: String::new(),
                cli: Some("/h/bin/tower-cli".into()),
                daemon: true,
                tower_socket: None,
                state_home: Some("/h/state".into()),
            }
        );
        assert_eq!(probe.socket(), "/h/state/tower/run/towerd.sock");
        assert!(Probe::parse("motd only\n").is_none());
    }

    fn info(protocol: u16) -> tower_protocol::DaemonInfo {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "protocol_version": {"major": protocol, "minor": 0},
            "pid": 7, "host_id": "h", "node_name": "n", "mode": "local",
            "pool": {"running": false, "proto_version": null},
            "allgather": {"connected": false, "policy_revision": null},
            "startup": "detached",
        }))
        .unwrap()
    }

    /// A daemon on this build's protocol is used whether or not its pool
    /// has started yet.
    #[test]
    fn only_a_daemon_speaking_this_builds_protocol_is_used() {
        let ours = tower_protocol::PROTOCOL_VERSION.major;
        assert!(compatible(&info(ours)).is_ok());
        let error = compatible(&info(ours + 1)).unwrap_err();
        assert!(
            error.contains("speaks protocol") && error.contains("manual care"),
            "{error}"
        );
    }

    /// Only the deployed daemon starts, in the login shell, asked to detach.
    #[test]
    fn a_launch_starts_the_deployed_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        let current = home.join(DEPLOY_REL).join("current");
        let log = dir.path().join("log");
        for name in ["tower-terminal-pool", "tower"] {
            // A pool there is the daemon's to start, never the launch's.
            executable(
                &current.join(name),
                &format!(r#"echo "{name} $*" >> "{}""#, log.display()),
            );
        }
        let line = format!(
            "{} {}",
            login_shell_safe(&launch_script()),
            login_shell_quote_arg(LAUNCH_COMMAND)
        );
        let out = on_host(&home, &bin, &line, b"");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "tower run --detach\n"
        );
    }
}
