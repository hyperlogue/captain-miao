use super::{CodexConfig, monitor::Observation, transport};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

pub(crate) struct Relay {
    pub(crate) events: mpsc::Receiver<Observation>,
    input_paused: watch::Sender<bool>,
    pause_ack: watch::Receiver<InputState>,
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub(crate) async fn start(config: &CodexConfig, path: &Path) -> Result<Self> {
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
        let task = tokio::spawn(async move {
            let mut gate = InputGate::default();
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
                let result = async {
                    let mut client = tokio::time::timeout(transport::DEADLINE,
                        tokio_tungstenite::accept_async_with_config(stream, Some(transport::limits())))
                        .await??;
                    let mut server = transport::connect(&upstream).await?;
                    loop {
                        tokio::select! {
                            biased;
                            changed = pause_rx.changed() => {
                                if changed.is_err() { break; }
                                gate.acknowledge(*pause_rx.borrow_and_update(), &ack_tx);
                            }
                            frame = client.next(), if !*pause_rx.borrow() => {
                                let Some(frame) = frame else { break };
                                let frame = frame?;
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                                    // Requests are reduced before crossing the channel;
                                    // environment/configuration never enters monitor state.
                                    gate.request(&value);
                                    if let Some(request) = Observation::client(&value)
                                        && tx.send(request).await.is_err() { break; }
                                }
                                let closed = frame.is_close();
                                server.send(frame).await?;
                                if closed { break; }
                            }
                            frame = server.next() => {
                                let Some(frame) = frame else { break };
                                let frame = frame?;
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                                    // Settle after enqueueing metadata: a pause ACK
                                    // must never overtake selection of its thread.
                                    if observable(&value) && tx.send(Observation::Server(value.clone())).await.is_err() { break; }
                                    gate.response(&value);
                                    gate.acknowledge(*pause_rx.borrow(), &ack_tx);
                                }
                                let closed = frame.is_close();
                                client.send(frame).await?;
                                if closed { break; }
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }.await;
                if result.is_err() {
                    // Transport errors can contain endpoint/authentication data.
                    tracing::debug!("Codex relay connection ended");
                }
                gate.disconnected();
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
        })
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

/// Input is fenced, but lost replies leave these runtimes unaccounted for.
/// Preserve their identities so the supervisor can prove they no longer run
/// after a daemon restart without guessing from the currently displayed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnsettledThreads(HashSet<UnsettledThread>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum UnsettledThread {
    Known(String),
    Unknown,
    InternalCreation,
}

impl UnsettledThreads {
    pub(super) fn only_internal_creations(&self) -> bool {
        self.0.len() == 1 && self.0.contains(&UnsettledThread::InternalCreation)
    }

    pub(super) fn may_be_running(&self, loaded: &HashSet<String>) -> bool {
        self.0.iter().any(|thread| match thread {
            UnsettledThread::Known(id) => loaded.contains(id),
            UnsettledThread::Unknown | UnsettledThread::InternalCreation => !loaded.is_empty(),
        })
    }
}

impl std::fmt::Display for UnsettledThreads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.iter().any(|thread| !matches!(thread, UnsettledThread::Known(_))) {
            "Codex lost a thread-creation reply; cleanup cannot confirm which thread was created"
        } else {
            "Codex disconnected with an unresolved thread operation; reconnect that thread before retrying cleanup"
        })
    }
}
impl std::error::Error for UnsettledThreads {}

/// Only request identities cross this gate; prompts/configuration stay on the
/// original stream. A successful lifecycle reply reestablishes which thread
/// the TUI controls after a lost connection.
#[derive(Default)]
struct InputGate {
    pending: HashMap<String, PendingOperation>,
    uncertain: HashSet<UnsettledThread>,
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
            } else if matches!(method, "thread/start" | "thread/fork") {
                UnsettledThread::Unknown
            } else {
                value["params"]["threadId"]
                    .as_str()
                    .map(|id| UnsettledThread::Known(id.to_owned()))
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
    fn response(&mut self, value: &Value) {
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
            self.uncertain
                .remove(&UnsettledThread::Known(thread.to_owned()));
        }
    }
    fn disconnected(&mut self) {
        self.uncertain
            .extend(self.pending.drain().map(|(_, operation)| operation.thread));
    }
    fn acknowledge(&self, paused: bool, ack: &watch::Sender<InputState>) {
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
        ack.send_if_modified(|previous| {
            if *previous == state {
                false
            } else {
                *previous = state;
                true
            }
        });
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
        let _ = std::fs::remove_file(&self.path);
    }
}
