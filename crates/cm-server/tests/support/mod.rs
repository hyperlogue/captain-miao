//! Private process and protocol drivers for session_lifecycle. Conditions and
//! RPC arrivals synchronize the scenario; deadlines only bound a broken test.
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cm_core::protocol::{ClientFrame, PROTOCOL_VERSION, ServerFrame, read_frame, write_frame};
use cm_core::state::{LauncherState, SessionKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;

pub const WAIT: Duration = Duration::from_secs(15);

pub async fn until(label: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

pub struct Host {
    root: tempfile::TempDir,
    launchers: Vec<Child>,
}

impl Host {
    pub fn new() -> Self {
        // Short, atomically reserved paths also fit macOS's Unix socket limit.
        let root = tempfile::Builder::new()
            .prefix("cm-life-")
            .tempdir_in("/tmp")
            .unwrap();
        for dir in [
            "bin",
            "run",
            "state",
            "cache",
            "codex",
            "native/captain-miao",
            "app/captain-miao",
        ] {
            cm_core::state::create_dir_all_private(&root.path().join(dir)).unwrap();
        }
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'codex-cli 0.0.0'; exit 0; fi
relay=
while [ "$#" -gt 0 ]; do
  case "$1" in --remote) shift; relay=$1;; esac
  shift
done
export CM_LIFECYCLE_RELAY="$relay"
exec {} --exact codex_fixture --nocapture
"#,
            shell_words::quote(&std::env::current_exe().unwrap().to_string_lossy())
        );
        let bin = root.path().join("bin/codex");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = Self {
            root,
            launchers: vec![],
        };
        std::fs::write(
            host.root.path().join("app/captain-miao/config.toml"),
            format!(
                "[codex]\nmode = \"app-server\"\nendpoint = \"unix://{}\"\n",
                host.codex_socket().display()
            ),
        )
        .unwrap();
        host
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_miao-server"));
        let mut paths = vec![self.root.path().join("bin")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.root.path().join("app"))
            .env("CODEX_HOME", self.root.path().join("codex"))
            .env("TERM", "dumb")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    pub fn start(&self) {
        assert!(
            self.command()
                .args(["daemon", "ensure"])
                .status()
                .unwrap()
                .success(),
            "private daemon failed to start"
        );
    }

    pub fn restart(&self) {
        assert!(
            self.command()
                .args(["daemon", "stop", "--force"])
                .status()
                .unwrap()
                .success()
        );
        self.start();
    }

    pub fn socket(&self) -> PathBuf {
        self.root.path().join("run/captain-miao/server.sock")
    }
    pub fn codex_socket(&self) -> PathBuf {
        self.root.path().join("codex.sock")
    }

    pub fn launch(&mut self, id: &str, app_server: bool) -> u32 {
        let cwd = self.root.path().join(id);
        cm_core::state::create_dir_all_private(&cwd).unwrap();
        let mut command = self.command();
        command
            .args(["launch", "codex"])
            .arg(&cwd)
            .args(["resume", id])
            .env("CM_LIFECYCLE_THREAD", id)
            .env(
                "XDG_CONFIG_HOME",
                self.root
                    .path()
                    .join(if app_server { "app" } else { "native" }),
            );
        let log = std::fs::File::create(self.root.path().join(format!("{id}.log"))).unwrap();
        command.stderr(log.try_clone().unwrap()).stdout(log);
        let child = command.spawn().unwrap();
        let pid = child.id();
        self.launchers.push(child);
        pid
    }

    pub fn assert_alive(&mut self, pids: &[u32]) {
        for pid in pids {
            assert!(
                self.launchers
                    .iter_mut()
                    .find(|child| child.id() == *pid)
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "launcher {pid} ended unexpectedly"
            );
        }
    }

    pub async fn wait_exited(&mut self, pid: u32) {
        let child = self
            .launchers
            .iter_mut()
            .find(|child| child.id() == pid)
            .unwrap();
        until("launcher exit", || child.try_wait().unwrap().is_some()).await;
    }

    pub fn launcher_sockets_gone(&self) -> bool {
        let dir = self.root.path().join("run/captain-miao/launchers");
        self.launchers.iter().all(|child| {
            !dir.join(format!("{}.sock", child.id())).exists()
                && !dir.join(format!("{}-codex.sock", child.id())).exists()
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if std::thread::panicking() {
            for id in ["alpha", "beta", "native"] {
                if let Ok(log) = std::fs::read_to_string(self.root.path().join(format!("{id}.log")))
                {
                    eprintln!("fixture {id}: {log}");
                }
            }
        }
        // On assertion failure, reap only children belonging to our launchers.
        // Fixtures have no grandchildren or model/tool processes.
        for child in &mut self.launchers {
            if child.try_wait().ok().flatten().is_some() {
                continue;
            }
            let file = self
                .root
                .path()
                .join(format!("state/captain-miao/sessions/{}.json", child.id()));
            if let Ok(bytes) = std::fs::read(file)
                && let Ok(row) = serde_json::from_slice::<LauncherState>(&bytes)
                && row.launcher_pid == child.id()
                && let Some(pid) = row.child_pid
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.command().args(["daemon", "stop", "--force"]).status();
    }
}

pub struct Peer {
    socket: UnixStream,
    rows: HashMap<SessionKey, LauncherState>,
    replies: HashMap<u64, ServerFrame>,
    fragment: Vec<u8>,
}

impl Peer {
    pub async fn connect(path: PathBuf) -> Self {
        let socket = tokio::time::timeout(WAIT, async {
            loop {
                if let Ok(socket) = UnixStream::connect(&path).await {
                    break socket;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("host did not bind");
        let mut peer = Self {
            socket,
            rows: HashMap::new(),
            replies: HashMap::new(),
            fragment: vec![],
        };
        peer.send(ClientFrame::Hello {
            client_version: "lifecycle-test".into(),
            protocol: PROTOCOL_VERSION,
        })
        .await;
        peer.next().await;
        peer.send(ClientFrame::Subscribe).await;
        peer.next().await;
        peer
    }

    pub async fn send(&mut self, frame: ClientFrame) {
        write_frame(&mut self.socket, &frame).await.unwrap();
    }

    async fn next(&mut self) {
        let frame = tokio::time::timeout(WAIT, read_frame::<_, ServerFrame>(&mut self.socket))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "host stopped responding; rows: {:?}",
                    self.rows
                        .values()
                        .map(|s| (&s.session_id, &s.status, &s.last_error))
                        .collect::<Vec<_>>()
                )
            })
            .expect("host read failed")
            .expect("host disconnected");
        if let Some(id) = frame.req_id() {
            self.replies.insert(id, frame);
            return;
        }
        match frame {
            ServerFrame::Snapshot { sessions } => {
                self.rows = sessions.into_iter().map(|s| (s.key(), s)).collect()
            }
            ServerFrame::Delta { state } => {
                self.rows.insert(state.key(), *state);
            }
            ServerFrame::Removed { key } => {
                self.rows.remove(&key);
            }
            _ => {}
        }
    }

    pub async fn rows_until(
        &mut self,
        ready: impl Fn(&HashMap<SessionKey, LauncherState>) -> bool,
    ) {
        tokio::time::timeout(WAIT, async {
            while !ready(&self.rows) {
                self.next().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "session state did not converge: {:?}",
                self.rows
                    .values()
                    .map(|s| (&s.session_id, &s.status, &s.last_error))
                    .collect::<Vec<_>>()
            )
        });
    }

    pub fn row(&self, id: &str) -> &LauncherState {
        self.rows
            .values()
            .find(|s| s.session_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("missing thread {id}"))
    }

    pub async fn reply(&mut self, id: u64) -> ServerFrame {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(reply) = self.replies.remove(&id) {
                    return reply;
                }
                self.next().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("request {id} was not answered"))
    }

    pub async fn fragmented_check(&mut self, req_id: u64) {
        use tokio::io::AsyncWriteExt;
        let mut packet = Vec::new();
        write_frame(
            &mut packet,
            &ClientFrame::CheckDir {
                req_id,
                path: "/tmp".into(),
            },
        )
        .await
        .unwrap();
        self.socket.write_all(&packet[..2]).await.unwrap();
        self.fragment = packet[2..].to_vec();
    }

    pub async fn finish_fragment(&mut self) {
        use tokio::io::AsyncWriteExt;
        self.socket
            .write_all(&std::mem::take(&mut self.fragment))
            .await
            .unwrap();
    }
}

#[derive(Clone)]
enum Cleanup {
    Hold(Arc<Notify>),
    Reject,
    Missing,
}

#[derive(Default)]
struct Script {
    cleanup: Mutex<HashMap<String, Cleanup>>,
    requests: Mutex<Vec<(String, String)>>,
    active: Mutex<HashSet<String>>,
}

pub struct CodexServer {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
    events: broadcast::Sender<Value>,
    script: Arc<Script>,
}

impl CodexServer {
    pub async fn start(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let (events, _) = broadcast::channel(128);
        let script = Arc::new(Script::default());
        let task = {
            let script = script.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            connections.spawn(serve_codex(stream, events.subscribe(), script.clone()));
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => { result.unwrap(); }
                    }
                }
            })
        };
        Self {
            path,
            task,
            events,
            script,
        }
    }

    pub async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn rename(&self, id: &str, name: &str) {
        self.events
            .send(
                json!({"method":"thread/name/updated","params":{"threadId":id,"threadName":name}}),
            )
            .unwrap();
    }
    pub fn helpers(&self) {
        self.events
            .send(json!({"method":"fixture/helpers"}))
            .unwrap();
    }
    pub fn activate(&self, id: &str) {
        self.script.active.lock().unwrap().insert(id.into());
        self.events.send(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":format!("turn-{id}")}}})).unwrap();
    }
    pub fn hold_cleanup(&self, id: &str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.script
            .cleanup
            .lock()
            .unwrap()
            .insert(id.into(), Cleanup::Hold(gate.clone()));
        gate
    }
    pub fn reject_cleanup(&self, id: &str) {
        self.script
            .cleanup
            .lock()
            .unwrap()
            .insert(id.into(), Cleanup::Reject);
    }
    pub fn missing_cleanup(&self, id: &str) {
        self.script
            .cleanup
            .lock()
            .unwrap()
            .insert(id.into(), Cleanup::Missing);
    }
    pub fn allow_cleanup(&self, id: &str) {
        self.script.cleanup.lock().unwrap().remove(id);
    }
    pub fn requests(&self) -> Vec<(String, String)> {
        self.script.requests.lock().unwrap().clone()
    }
    pub async fn wait_for(&self, method: &str, id: &str) {
        until("Codex RPC arrival", || {
            self.requests().iter().any(|(m, t)| m == method && t == id)
        })
        .await;
    }
}

impl Drop for CodexServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_codex(
    stream: UnixStream,
    mut events: broadcast::Receiver<Value>,
    script: Arc<Script>,
) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let mut thread = None;
    loop {
        let response = tokio::select! {
            event = events.recv() => {
                let Ok(event) = event else { return };
                if thread.is_none() { continue; }
                event
            }
            message = ws.next() => {
                let Some(Ok(Message::Text(text))) = message else { return };
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                let id = request["params"]["threadId"].as_str().unwrap_or("");
                script.requests.lock().unwrap().push((method.into(), id.into()));
                let result = match method {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/resume" => {
                        thread = Some(id.to_string());
                        json!({"thread":{
                            "id":id,"name":format!("Title {id}"),"preview":format!("Prompt {id}"),
                            "source":"cli","status":{"type":"idle"},"turns":[{
                                "id":"previous-turn","status":"completed","items":[{
                                    "type":"userMessage","content":[{"type":"text","text":format!("Prompt {id}")}]
                                }]
                            }]
                        }})
                    }
                    "thread/start" => {
                        for message in [
                            json!({"id":request["id"],"result":{"thread":{"id":"helper","ephemeral":true,"threadSource":"system","name":"Catch-up","preview":"Internal prompt","status":{"type":"idle"}}}}),
                            json!({"method":"item/completed","params":{"threadId":"helper","item":{"type":"userMessage","content":[{"type":"text","text":"Internal prompt"}]}}}),
                            json!({"method":"thread/closed","params":{"threadId":"helper"}}),
                            json!({"method":"thread/name/updated","params":{"threadId":thread,"threadName":format!("After helper {}", thread.as_deref().unwrap())}}),
                        ] {
                            if ws.send(Message::Text(message.to_string().into())).await.is_err() { return; }
                        }
                        continue;
                    }
                    "thread/goal/get" => {
                        if script.active.lock().unwrap().contains(id) { json!({"goal":{"status":"active"}}) } else { json!({"goal":null}) }
                    }
                    "thread/goal/set" => {
                        assert_eq!(request["params"]["status"], "paused");
                        json!({})
                    }
                    "thread/turns/list" => {
                        if script.active.lock().unwrap().contains(id) { json!({"data":[{"id":format!("turn-{id}"),"status":"inProgress"}]}) } else { json!({"data":[]}) }
                    }
                    "turn/interrupt" => {
                        assert_eq!(request["params"]["turnId"], format!("turn-{id}"));
                        script.active.lock().unwrap().remove(id);
                        json!({})
                    }
                    "thread/backgroundTerminals/clean" => {
                        let cleanup = script.cleanup.lock().unwrap().get(id).cloned();
                        let error = match cleanup {
                            Some(Cleanup::Hold(gate)) => { gate.notified().await; Some((-32603, "cleanup rejected".to_string())) }
                            Some(Cleanup::Reject) => Some((-32603, "cleanup rejected".to_string())),
                            Some(Cleanup::Missing) => Some((-32600, format!("thread not found: {id}"))),
                            None => None,
                        };
                        if let Some((code, message)) = error {
                            if ws.send(Message::Text(json!({"id":request["id"],"error":{"code":code,"message":message}}).to_string().into())).await.is_err() { return; }
                            continue;
                        }
                        json!({})
                    }
                    "thread/loaded/list" => {
                        // Emulate older daemons without this optional inventory,
                        // exercising the explicit missing-thread fallback.
                        if ws.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"method not found"}}).to_string().into())).await.is_err() { return; }
                        continue;
                    }
                    other => panic!("unexpected Codex request: {other}"),
                };
                json!({"id":request["id"],"result":result})
            }
        };
        if ws
            .send(Message::Text(response.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}
