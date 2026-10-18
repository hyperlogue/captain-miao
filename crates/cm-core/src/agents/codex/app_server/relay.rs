use super::{CodexConfig, monitor::Observation, transport};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;

pub(crate) struct Relay {
    pub(crate) events: mpsc::Receiver<Observation>,
    input_paused: watch::Sender<bool>,
    pause_ack: watch::Receiver<InputState>,
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
    environment: Option<Arc<Mutex<super::Environments>>>,
    upstream: PathBuf,
    maintenance: tokio::task::JoinHandle<()>,
}

impl Relay {
    #[cfg(test)]
    pub(crate) async fn start(config: &CodexConfig, path: &Path) -> Result<Self> {
        Self::start_with_environment(config, path, None).await
    }

    pub(crate) async fn start_with_environment(
        config: &CodexConfig,
        path: &Path,
        environment: Option<super::Environments>,
    ) -> Result<Self> {
        let environment = environment.map(|env| Arc::new(Mutex::new(env)));
        let upstream = config.socket_path()?;
        // Fail before launching the TUI, with the ordinary FailedToStart row.
        // No fallback may change the host's selected execution mode.
        let _ = transport::Client::connect(&upstream).await?;
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).context("binding Codex TUI relay")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let mut listener = super::listener::Listener::new(listener);
        let (tx, events) = mpsc::channel(128);
        let (input_paused, mut pause_rx) = watch::channel(false);
        let (ack_tx, pause_ack) = watch::channel(InputState::Running);
        let ack_tx = GateAck::new(ack_tx);
        let maintenance = {
            let environment = environment.clone();
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let Some(environment) = environment else {
                    return;
                };
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let root = environment.lock().await.root();
                    super::Environments::collect(&root, &upstream).await;
                }
            })
        };
        let task_environment = environment.clone();
        let task_upstream = upstream.clone();
        let task = tokio::spawn(async move {
            let environment = task_environment;
            let upstream = task_upstream;
            let mut gate = InputGate::default();
            // Extra clients (the in-session resume picker) outlive one session
            // connection. Aborting this task drops the set and stops them too.
            let mut extras = JoinSet::new();
            loop {
                gate.acknowledge(*pause_rx.borrow(), &ack_tx);
                let accepted = tokio::select! {
                    biased;
                    changed = pause_rx.changed() => {
                        if changed.is_err() { break; }
                        gate.acknowledge(*pause_rx.borrow_and_update(), &ack_tx);
                        continue;
                    }
                    accepted = listener.accept() => accepted,
                };
                let Ok(stream) = accepted else {
                    break;
                };
                while extras.try_join_next().is_some() {}
                let result = async {
                    let mut client = tokio::time::timeout(transport::DEADLINE,
                        tokio_tungstenite::accept_async_with_config(stream, Some(transport::limits())))
                        .await??;
                    let mut server = transport::connect(&upstream).await?;
                    let daemon = server.get_ref().peer_cred().ok().and_then(|cred| cred.pid());
                    let mut accept_extras = true;
                    let mut environment_requests = HashSet::new();
                    let mut peer_closed = false;
                    let result = async {
                    loop {
                        tokio::select! {
                            biased;
                            changed = pause_rx.changed() => {
                                if changed.is_err() { break; }
                                gate.acknowledge(*pause_rx.borrow_and_update(), &ack_tx);
                            }
                            // EOF before a new accept, so a reconnect stays the
                            // observed session. Accept before server frames, so a
                            // busy turn cannot starve the picker handshake.
                            frame = client.next(), if !*pause_rx.borrow() => {
                                let Some(frame) = frame else { break };
                                let mut frame = frame?;
                                // Keep upstream open long enough to acknowledge
                                // preceding mutations, even on a graceful close.
                                if frame.is_close() { break; }
                                match prepare_environment(&mut frame, environment.as_ref(), &upstream, &mut pause_rx, daemon).await {
                                    Ok(Some(id)) => { environment_requests.insert(id); }
                                    Ok(None) => {}
                                    Err(reply) => {
                                        client.send(reply).await?;
                                        gate.acknowledge(*pause_rx.borrow(), &ack_tx);
                                        continue;
                                    }
                                }
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                                    // Requests are reduced before crossing the channel;
                                    // environment/configuration never enters monitor state.
                                    gate.request(&value);
                                    if let Some(request) = Observation::client(&value)
                                        && tx.send(request).await.is_err() { break; }
                                }
                                server.send(frame).await?;
                            }
                            accepted = listener.accept(), if accept_extras => {
                                match accepted {
                                    Ok(stream) => {
                                        let upstream = upstream.clone();
                                        let mut pause_rx = pause_rx.clone();
                                        let environment = environment.clone();
                                        let ack = ack_tx.fork();
                                        extras.spawn(async move {
                                            if forward_unobserved(stream, &upstream, &mut pause_rx, environment, ack).await.is_err() {
                                                tracing::debug!("Codex relay connection ended");
                                            }
                                        });
                                    }
                                    Err(_) => {
                                        // The message can name the socket path.
                                        accept_extras = false;
                                        tracing::debug!(
                                            "Codex relay stopped accepting extra connections"
                                        );
                                    }
                                }
                            }
                            frame = server.next() => {
                                let Some(frame) = frame else { peer_closed = true; break };
                                let frame = frame.inspect_err(|error| {
                                    peer_closed = upstream_closed(error);
                                })?;
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                                    settle_environment(&value, &mut environment_requests, environment.as_ref(), daemon).await;
                                    // Settle after enqueueing metadata: a pause ACK
                                    // must never overtake selection of its thread.
                                    if observable(&value) && tx.send(Observation::Server(value.clone())).await.is_err() { break; }
                                    gate.response(&value, daemon);
                                    gate.acknowledge(*pause_rx.borrow(), &ack_tx);
                                }
                                let closed = frame.is_close();
                                peer_closed |= closed;
                                client.send(frame).await?;
                                if closed { break; }
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                    }.await;
                    // A local TUI disconnect must not let a new control socket
                    // overtake bytes still unread on this upstream socket.
                    // Codex's transport enqueues preceding JSON messages before
                    // replying to Ping. Drain late replies while obtaining that
                    // receipt; failure retains uncertainty until daemon exit.
                    let received = peer_closed || gate.pending.is_empty() || receive_pending(
                        &mut server, &mut gate, daemon, Some(&tx), &pause_rx, &ack_tx,
                        &mut environment_requests, environment.as_ref(),
                    ).await;
                    gate.disconnected(daemon, received);
                    result
                }.await;
                if result.is_err() {
                    // Transport errors can contain endpoint/authentication data.
                    tracing::debug!("Codex relay connection ended");
                }
                gate.acknowledge(*pause_rx.borrow(), &ack_tx);
                if tx.send(Observation::Disconnected).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            events,
            input_paused,
            pause_ack,
            path: path.to_owned(),
            task,
            environment,
            upstream,
            maintenance,
        })
    }
    pub(crate) async fn cleanup_environment(&mut self) {
        self.task.abort();
        self.maintenance.abort();
        let _ = (&mut self.task).await;
        let _ = (&mut self.maintenance).await;
        if let Some(environment) = self.environment.take() {
            environment.lock().await.finish(&self.upstream).await;
        }
    }

    /// Fence new input and wait for already forwarded thread/turn operations.
    /// The owned future lets the supervisor drain observations concurrently.
    /// A lost response is uncertainty, never evidence that no work started.
    pub(crate) fn pause_input(
        &self,
        paused: bool,
    ) -> futures_util::future::BoxFuture<'static, Result<()>> {
        let sent = self
            .input_paused
            .send(paused)
            .context("Codex relay stopped");
        let mut ack = self.pause_ack.clone();
        Box::pin(async move {
            sent?;
            let state = tokio::time::timeout(
                transport::DEADLINE,
                ack.wait_for(|state| {
                    if paused {
                        *state != InputState::Running
                    } else {
                        *state == InputState::Running
                    }
                }),
            )
            .await
            .context("Codex relay still has pending requests; retry cleanup after they settle")?
            .context("Codex relay stopped")?;
            if let InputState::Uncertain(threads) = &*state {
                return Err(threads.clone().into());
            }
            Ok(())
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
enum InputState {
    Running,
    Paused,
    Uncertain(UnsettledThreads),
}

fn upstream_closed(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
    matches!(
        error,
        Error::ConnectionClosed
            | Error::AlreadyClosed
            | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)
    ) || matches!(error, Error::Io(error) if matches!(error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected))
}

/// Each websocket has independent request IDs. Its fence joins the primary
/// connection's fence without letting picker replies retarget the dashboard.
struct GateAck {
    id: usize,
    states: Arc<std::sync::Mutex<GateStates>>,
    tx: watch::Sender<InputState>,
    retain: bool,
}

struct GateStates {
    next: usize,
    states: HashMap<usize, InputState>,
}

impl GateAck {
    fn new(tx: watch::Sender<InputState>) -> Self {
        Self {
            id: 0,
            states: Arc::new(std::sync::Mutex::new(GateStates {
                next: 1,
                states: [(0, InputState::Running)].into(),
            })),
            tx,
            retain: false,
        }
    }

    fn fork(&self) -> Self {
        let id = {
            let mut states = self.states.lock().unwrap();
            let id = states.next;
            states.next += 1;
            id
        };
        let ack = Self {
            id,
            states: Arc::clone(&self.states),
            tx: self.tx.clone(),
            retain: false,
        };
        ack.update(Some(InputState::Running));
        ack
    }

    fn update(&self, state: Option<InputState>) {
        let mut states = self.states.lock().unwrap();
        if let Some(state) = state {
            states.states.insert(self.id, state);
        } else {
            states.states.remove(&self.id);
        }
        let state = if states.states.values().any(|s| *s == InputState::Running) {
            InputState::Running
        } else {
            let uncertain: HashSet<_> = states
                .states
                .values()
                .filter_map(|s| match s {
                    InputState::Uncertain(threads) => Some(&threads.0),
                    _ => None,
                })
                .flatten()
                .cloned()
                .collect();
            if uncertain.is_empty() {
                InputState::Paused
            } else {
                InputState::Uncertain(UnsettledThreads(uncertain))
            }
        };
        self.tx.send_if_modified(|previous| {
            if *previous == state {
                false
            } else {
                *previous = state;
                true
            }
        });
    }

    fn finish(&mut self, gate: &InputGate) {
        if !gate.uncertain.is_empty() {
            gate.acknowledge(true, self);
            self.retain = true;
        }
    }
}

impl Drop for GateAck {
    fn drop(&mut self) {
        if !self.retain {
            self.update(None);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_pending(
    server: &mut transport::Socket,
    gate: &mut InputGate,
    daemon: Option<i32>,
    tx: Option<&mpsc::Sender<Observation>>,
    paused: &watch::Receiver<bool>,
    ack: &GateAck,
    environment_requests: &mut HashSet<String>,
    environment: Option<&Arc<Mutex<super::Environments>>>,
) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_RECEIPT: AtomicU64 = AtomicU64::new(0);
    let receipt = format!(
        "miao-cleanup-{}-{}",
        std::process::id(),
        NEXT_RECEIPT.fetch_add(1, Ordering::Relaxed)
    );
    tokio::time::timeout(transport::DEADLINE, async {
        server
            .send(Message::Ping(receipt.as_bytes().to_vec().into()))
            .await?;
        while let Some(frame) = server.next().await {
            if frame.as_ref().is_err_and(upstream_closed) {
                return Ok(true);
            }
            match frame? {
                Message::Pong(payload) if payload.as_ref() == receipt.as_bytes() => {
                    return Ok(true);
                }
                Message::Close(_) => return Ok(true),
                Message::Ping(_) => server.flush().await?,
                Message::Text(text) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        settle_environment(&value, environment_requests, environment, daemon).await;
                        if observable(&value)
                            && let Some(tx) = tx
                        {
                            let _ = tx.send(Observation::Server(value.clone())).await;
                        }
                        gate.response(&value, daemon);
                        gate.acknowledge(*paused.borrow(), ack);
                    }
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(true)
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

/// Input is fenced, but lost replies leave these runtimes unaccounted for.
/// Preserve their identities so the supervisor can prove they no longer run
/// after a daemon restart without guessing from the currently displayed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnsettledThreads(HashSet<UnsettledOperation>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UnsettledOperation {
    thread: UnsettledThread,
    daemon: Option<i32>,
    received: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum UnsettledThread {
    Known(String),
    // RPC completion only enqueues a Core operation; a thread read does not
    // establish that Core has processed it (for example manual compaction).
    Queued(String),
    Unknown,
    InternalCreation,
}

impl UnsettledThreads {
    pub(super) fn only_internal_creations(&self) -> bool {
        !self.0.is_empty()
            && self
                .0
                .iter()
                .all(|op| op.thread == UnsettledThread::InternalCreation)
    }

    pub(super) fn threads_to_fence(&self, daemon: Option<i32>) -> Result<HashSet<String>> {
        let mut threads = HashSet::new();
        for op in &self.0 {
            // A reused PID delays recovery, never falsely proves process death.
            if op.daemon.is_some_and(|pid| {
                pid > 0
                    && unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }) {
                continue;
            }
            if op.received
                && op.daemon.is_some()
                && op.daemon == daemon
                && let UnsettledThread::Known(id) = &op.thread
            {
                threads.insert(id.clone());
            } else {
                return Err(self.clone().into());
            }
        }
        Ok(threads)
    }
}

impl std::fmt::Display for UnsettledThreads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.iter().any(|op| matches!(op.thread, UnsettledThread::Unknown | UnsettledThread::InternalCreation)) {
            "Codex lost a thread-creation reply; cleanup cannot confirm which thread was created"
        } else {
            "Codex disconnected with an unresolved thread operation; reconnect that thread before retrying cleanup"
        })
    }
}
impl std::error::Error for UnsettledThreads {}

/// Codex's `/resume` picker opens a second websocket to the same `--remote`
/// socket while the session TUI stays connected. Give it its own upstream
/// connection and fence it during cleanup, but do not observe it: it repeats
/// the session connection's request ids, and its close is not a disconnect.
async fn forward_unobserved(
    stream: UnixStream,
    upstream: &std::path::Path,
    pause_rx: &mut watch::Receiver<bool>,
    environment: Option<Arc<Mutex<super::Environments>>>,
    mut ack: GateAck,
) -> Result<()> {
    let mut client = tokio::time::timeout(
        transport::DEADLINE,
        tokio_tungstenite::accept_async_with_config(stream, Some(transport::limits())),
    )
    .await??;
    let mut server = transport::connect(upstream).await?;
    let daemon = server
        .get_ref()
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid());
    let mut environment_requests = HashSet::new();
    let mut gate = InputGate::default();
    let mut peer_closed = false;
    gate.acknowledge(*pause_rx.borrow(), &ack);
    let result = async {
    loop {
        tokio::select! {
            biased;
            changed = pause_rx.changed() => {
                if changed.is_err() { break; }
                gate.acknowledge(*pause_rx.borrow_and_update(), &ack);
            }
            frame = client.next(), if !*pause_rx.borrow() => {
                let Some(frame) = frame else { break };
                let mut frame = frame?;
                if frame.is_close() { break; }
                match prepare_environment(&mut frame, environment.as_ref(), upstream, pause_rx, daemon).await {
                    Ok(Some(id)) => { environment_requests.insert(id); }
                    Ok(None) => {}
                    Err(reply) => { client.send(reply).await?; continue; }
                }
                if let Message::Text(text) = &frame && let Ok(value) = serde_json::from_str::<Value>(text) {
                    gate.request(&value);
                }
                server.send(frame).await?;
            }
            frame = server.next() => {
                let Some(frame) = frame else { peer_closed = true; break };
                let frame = frame.inspect_err(|error| {
                    peer_closed = upstream_closed(error);
                })?;
                if let Message::Text(text) = &frame
                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                    settle_environment(&value, &mut environment_requests, environment.as_ref(), daemon).await;
                    gate.response(&value, daemon);
                    gate.acknowledge(*pause_rx.borrow(), &ack);
                }
                let closed = frame.is_close();
                peer_closed |= closed;
                client.send(frame).await?;
                if closed { break; }
            }
        }
    }
    Ok::<_, anyhow::Error>(())
    }.await;
    let received = peer_closed
        || gate.pending.is_empty()
        || receive_pending(
            &mut server,
            &mut gate,
            daemon,
            None,
            pause_rx,
            &ack,
            &mut environment_requests,
            environment.as_ref(),
        )
        .await;
    gate.disconnected(daemon, received);
    ack.finish(&gate);
    result
}

async fn prepare_environment(
    frame: &mut Message,
    environment: Option<&Arc<Mutex<super::Environments>>>,
    upstream: &Path,
    pause_rx: &mut watch::Receiver<bool>,
    daemon: Option<i32>,
) -> std::result::Result<Option<String>, Message> {
    let (Some(environment), Message::Text(text)) = (environment, &*frame) else {
        return Ok(None);
    };
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    if !super::environment::needs_environment(&value) {
        return Ok(None);
    }
    let result = tokio::select! {
        biased;
        _ = pause_rx.wait_for(|paused| *paused) => Err(anyhow::anyhow!("session cleanup is in progress; retry after cleanup")),
        result = async { environment.lock().await.request(&mut value, upstream, daemon).await } => result,
    };
    let applied = match result {
        Ok(applied) => applied,
        Err(error) => return Err(Message::Text(serde_json::json!({
            "id":value["id"], "error":{"code":-32000,"message":format!("miao direnv setup failed: {error:#}")}
        }).to_string().into())),
    };
    *frame = Message::Text(value.to_string().into());
    Ok(applied.then(|| value["id"].to_string()))
}

async fn settle_environment(
    value: &Value,
    pending: &mut HashSet<String>,
    environment: Option<&Arc<Mutex<super::Environments>>>,
    daemon: Option<i32>,
) {
    if value.get("method").is_none()
        && let Some(id) = value.get("id")
        && pending.remove(&id.to_string())
        && let Some(environment) = environment
    {
        environment.lock().await.response(value, daemon);
    }
}

/// Only request identities cross this gate; prompts/configuration stay on the
/// original stream. A successful lifecycle reply reestablishes which thread
/// the TUI controls after a lost connection.
#[derive(Default)]
struct InputGate {
    pending: HashMap<String, PendingOperation>,
    uncertain: HashSet<UnsettledOperation>,
}
struct PendingOperation {
    selects_thread: bool,
    thread: UnsettledThread,
}
impl InputGate {
    fn request(&mut self, value: &Value) {
        let Some(method) = value["method"].as_str() else {
            return;
        };
        // Reads cannot start work. Only mutations fence cleanup or leave
        // uncertain execution behind when their replies are lost.
        let selects_thread = matches!(method, "thread/start" | "thread/resume" | "thread/fork");
        if !selects_thread
            && !matches!(
                method,
                "turn/start"
                    | "turn/steer"
                    | "thread/goal/set"
                    | "thread/goal/clear"
                    | "thread/rollback"
                    | "thread/compact/start"
            )
        {
            return;
        }
        if let Some(id) = value.get("id") {
            let thread = if method == "thread/start"
                && value["params"]["ephemeral"] == true
                && value["params"]["threadSource"] == "system"
            {
                // Codex's recap/title helpers submit their prompt only after
                // receiving this creation reply. Keep this narrow: ephemeral
                // user threads, forks and lost turn replies are still strict.
                UnsettledThread::InternalCreation
            } else if matches!(method, "thread/start" | "thread/fork")
                || (method == "thread/resume"
                    && (!value["params"]["path"].is_null()
                        || !value["params"]["history"].is_null()))
            {
                UnsettledThread::Unknown
            } else {
                value["params"]["threadId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(|id| {
                        if matches!(method, "thread/compact/start" | "thread/rollback") {
                            UnsettledThread::Queued(id.to_owned())
                        } else {
                            UnsettledThread::Known(id.to_owned())
                        }
                    })
                    .unwrap_or(UnsettledThread::Unknown)
            };
            self.pending.insert(
                id.to_string(),
                PendingOperation {
                    selects_thread,
                    thread,
                },
            );
        }
    }
    fn response(&mut self, value: &Value, daemon: Option<i32>) {
        if value.get("method").is_some() {
            return;
        }
        let Some(id) = value.get("id") else {
            return;
        };
        if self
            .pending
            .remove(&id.to_string())
            .is_some_and(|operation| operation.selects_thread)
            && value["result"]["thread"]["parentThreadId"].is_null()
            && let Some(thread) = value["result"]["thread"]["id"].as_str()
        {
            // Reconnecting B cannot settle lost work on A. A thread created
            // without any returned identity cannot be guessed from a new one.
            self.uncertain.retain(|op| {
                !(op.received
                    && op.daemon.is_some()
                    && op.daemon == daemon
                    && op.thread == UnsettledThread::Known(thread.to_owned()))
            });
        }
    }
    fn disconnected(&mut self, daemon: Option<i32>, received: bool) {
        self.uncertain.extend(
            self.pending
                .drain()
                .map(|(_, operation)| UnsettledOperation {
                    thread: operation.thread,
                    daemon,
                    received,
                }),
        );
    }
    fn acknowledge(&self, paused: bool, ack: &GateAck) {
        // Inventory can settle a lost reply only after every newer forwarded
        // operation has replied too; otherwise a resume could still load work
        // just after the inventory reported it absent.
        let state = if !paused || !self.pending.is_empty() {
            InputState::Running
        } else if !self.uncertain.is_empty() {
            InputState::Uncertain(UnsettledThreads(self.uncertain.clone()))
        } else {
            InputState::Paused
        };
        ack.update(Some(state));
    }
}

fn observable(value: &Value) -> bool {
    match value["method"].as_str() {
        None => {
            value
                .get("result")
                .is_some_and(|v| v.get("thread").is_some())
                || value.get("error").is_some()
        }
        Some(method) => {
            method.starts_with("thread/")
                || method.starts_with("turn/")
                || matches!(
                    method,
                    "item/started" | "item/completed" | "serverRequest/resolved" | "error"
                )
                || method.ends_with("/requestApproval")
                || method.ends_with("/requestUserInput")
                || method == "mcpServer/elicitation/request"
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        self.maintenance.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recovery_requires_receipt_identity_and_the_original_daemon() {
        let pid = std::process::id() as i32;
        for (method, params, received, daemon, recovers) in [
            (
                "turn/start",
                json!({"threadId":"known"}),
                true,
                Some(pid),
                true,
            ),
            (
                "turn/start",
                json!({"threadId":"known"}),
                false,
                Some(pid),
                false,
            ),
            ("turn/start", json!({"threadId":"known"}), true, None, false),
            (
                "thread/resume",
                json!({"threadId":"known","path":"/other/history"}),
                true,
                Some(pid),
                false,
            ),
            (
                "thread/resume",
                json!({"threadId":"known","history":[]}),
                true,
                Some(pid),
                false,
            ),
            (
                "thread/resume",
                json!({"threadId":""}),
                true,
                Some(pid),
                false,
            ),
            (
                "thread/compact/start",
                json!({"threadId":"known"}),
                true,
                Some(pid),
                false,
            ),
            ("thread/start", json!({}), true, Some(pid), false),
        ] {
            let mut gate = InputGate::default();
            gate.request(&json!({"id":1,"method":method,"params":params}));
            gate.disconnected(daemon, received);
            let lost = UnsettledThreads(gate.uncertain);
            assert_eq!(
                lost.threads_to_fence(Some(pid)).is_ok(),
                recovers,
                "{method}: {params}, received={received}, daemon={daemon:?}"
            );
            assert!(
                lost.threads_to_fence(None).is_err(),
                "a new peer of unknown identity cannot settle an old request"
            );
        }
    }

    #[test]
    fn only_process_exit_settles_unidentified_creation() {
        let mut daemon = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = daemon.id() as i32;
        let mut gate = InputGate::default();
        gate.request(&json!({"id":1,"method":"thread/start","params":{}}));
        gate.disconnected(Some(pid), false);
        let lost = UnsettledThreads(gate.uncertain);
        let before = lost.threads_to_fence(Some(std::process::id() as i32));
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        assert!(
            before.is_err(),
            "an empty replacement daemon is not proof that the original stopped"
        );
        assert!(
            lost.threads_to_fence(Some(std::process::id() as i32))
                .unwrap()
                .is_empty()
        );
    }
}
