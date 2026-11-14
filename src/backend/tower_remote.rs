//! Sessions on an ssh host's Tower daemon, behind [`crate::tower::FLAG`].
//!
//! With the flag set, a remote host's new sessions run under the Tower
//! installed there. With it unset, sessions an earlier run started there
//! stay listed and operable, but no daemon is started and nothing new runs
//! on Tower. The dashboard drives that host's `tower-cli` over the
//! host's shared ssh connection: `tower-cli events` streams the daemon's
//! session records, `tower-cli rpc` opens, stops and tags sessions, and an
//! attach window runs `tower-cli attach`. The CLI starts the daemon when it
//! is not running, so nothing is provisioned here; Tower must be installed
//! on the host, on its login shell's `PATH`.
//!
//! `miao-server` keeps serving everything else for the host: its sessions
//! (marked for a restart), resume candidates, directory queries, vitals and
//! VCS. Rows from both are merged by [`super::RemoteBackend`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tower_protocol::AgentRecord;

use super::{
    ConnLog, KillOutcome, LaunchPlan, OpenSpec, REMOTE_OUTPUT_CAP, capped_output, host_text_safe,
    login_shell_quote_arg, login_shell_safe, ssh_common_opts,
};
use crate::state::{self, HostId, LauncherState, SessionFlags, SessionKey};

/// Tower's command-line client, found on the host's login `PATH`.
const CLI: &str = "tower-cli";

/// The largest event line accepted. The first one carries every record.
const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// A stream that lasted this long was healthy, so its loss retries promptly.
const HEALTHY: Duration = Duration::from_secs(30);
/// How often a host is asked again, with the flag unset, after its Tower
/// was found absent or idle.
const RETRY_DORMANT: Duration = Duration::from_secs(600);

/// One host's Tower: its live sessions, and how to reach its CLI.
pub(crate) struct TowerLink {
    host: HostId,
    target: String,
    /// The host's ssh options, sharing the connection `miao-server` uses.
    opts: Vec<String>,
    /// Live sessions by Tower id.
    rows: Mutex<HashMap<String, LauncherState>>,
    /// The event stream has delivered its first snapshot and is still up.
    mirrored: AtomicBool,
    dirty: Arc<AtomicBool>,
    log: Arc<ConnLog>,
    task: OnceLock<tokio::task::AbortHandle>,
}

/// Why a CLI call failed. `unreachable` means the command may never have
/// reached Tower.
#[derive(Debug)]
struct Failure {
    unreachable: bool,
    message: String,
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
        dirty: Arc<AtomicBool>,
        log: Arc<ConnLog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            target: target.to_string(),
            opts: ssh_common_opts(&state::ssh_control_path(target), ssh_options),
            rows: Mutex::default(),
            mirrored: AtomicBool::new(false),
            dirty,
            log,
            task: OnceLock::new(),
        })
    }

    /// Start following the host's sessions until [`Self::stop`].
    pub(crate) fn start(self: &Arc<Self>) {
        let task = tokio::spawn(follow(Arc::downgrade(self)));
        let _ = self.task.set(task.abort_handle());
    }

    pub(crate) fn stop(&self) {
        if let Some(task) = self.task.get() {
            task.abort();
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
        let mut argv = vec!["ssh".to_string(), "-t".to_string()];
        argv.extend(self.opts.iter().cloned());
        argv.push(self.target.clone());
        argv.push(remote_command(&["attach", id]));
        argv
    }

    /// Reserve a session for `spec`; it starts when its window attaches, so
    /// the agent meets the real terminal.
    pub(crate) async fn open(&self, spec: OpenSpec) -> anyhow::Result<LaunchPlan> {
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
            Err(failure) if failure.message.to_lowercase().contains("not found") => {
                KillOutcome::AlreadyGone
            }
            Err(failure) => KillOutcome::Failed(failure.message),
        }
    }

    pub(crate) async fn set_flags(&self, id: &str, flags: SessionFlags) -> bool {
        let (mut add, mut remove) = (Vec::new(), Vec::new());
        for (tag, set) in [
            (crate::tower::PINNED, flags.pinned),
            (crate::tower::FOLLOW_UP, flags.follow_up),
        ] {
            if set { &mut add } else { &mut remove }.push(tag);
        }
        self.rpc(
            "session.tags.patch",
            serde_json::json!({"id": id, "add": add, "remove": remove}),
        )
        .await
        .inspect_err(|failure| {
            self.log
                .error(format!("Tower could not tag {id}: {failure}"))
        })
        .is_ok()
    }

    /// Call one of the daemon's methods through the host's CLI.
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Failure> {
        let unreachable = |message: String| Failure {
            unreachable: true,
            message,
        };
        let child = self
            .command(&["rpc", method, &params.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| unreachable(format!("could not run ssh: {error}")))?;
        let (status, stdout, stderr) =
            tokio::time::timeout(RPC_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
                .await
                .map_err(|_| unreachable(format!("`{CLI} rpc {method}` timed out")))?
                .map_err(|error| unreachable(format!("ssh failed: {error}")))?;
        if !status.success() {
            let message = stderr.trim().to_string();
            let message = if message.is_empty() {
                format!("`{CLI} rpc {method}` failed (rc={:?})", status.code())
            } else {
                message
            };
            // ssh itself exits 255; the shell reports a missing CLI as 127.
            return Err(Failure {
                unreachable: matches!(status.code(), Some(255 | 127) | None),
                message,
            });
        }
        json_in(&stdout).ok_or_else(|| Failure {
            unreachable: false,
            message: format!("`{CLI} rpc {method}` printed no JSON"),
        })
    }

    /// An ssh command running the host's CLI with `args`.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("ssh");
        command
            .args(&self.opts)
            .arg("-T")
            .arg(&self.target)
            .arg(remote_command(&cli_args(crate::tower::enabled(), args)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
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
fn cli_args<'a>(enabled: bool, args: &[&'a str]) -> Vec<&'a str> {
    let mut all = Vec::with_capacity(args.len() + 1);
    if !enabled {
        all.push("--no-start");
    }
    all.extend_from_slice(args);
    all
}

/// The remote command running `tower-cli args` in the account's login shell,
/// which is where a user's `PATH` is complete.
fn remote_command(args: &[&str]) -> String {
    let command = std::iter::once(format!("exec {CLI}"))
        .chain(args.iter().map(|arg| login_shell_quote_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {}",
        login_shell_safe("exec \"${SHELL:-/bin/sh}\" -l -c \"$0\""),
        login_shell_quote_arg(&command)
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

/// One line of `tower-cli events`, or `None` for a line that is not a
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
        let enabled = crate::tower::enabled();
        if enabled {
            this.log
                .info(format!("following Tower sessions with `{CLI} events`"));
        }
        let outcome = follow_once(&this).await;
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

async fn follow_once(link: &TowerLink) -> Result<(), String> {
    let mut child = link
        .command(&["events", "--topics", "agents"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run ssh: {error}"))?;
    let stdout = child.stdout.take().ok_or("ssh has no stdout")?;
    let stderr = child.stderr.take().ok_or("ssh has no stderr")?;
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.take(4096).read_to_string(&mut text).await;
        text
    });
    let mut reader = BufReader::new(stdout);
    let mut since = HashMap::new();
    let read = loop {
        match read_line(&mut reader).await {
            Ok(Some(line)) => {
                if let Some(event) = event(&line) {
                    link.apply(event, &mut since);
                }
            }
            Ok(None) => break Ok(()),
            Err(error) => break Err(error.to_string()),
        }
    };
    let status = child.wait().await.ok();
    let errors = errors.await.unwrap_or_default();
    let errors = host_text_safe(errors.trim());
    match (read, status) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(status)) if !status.success() && !errors.is_empty() => Err(errors),
        (Ok(()), Some(status)) if !status.success() => {
            Err(format!("`{CLI} events` exited (rc={:?})", status.code()))
        }
        (Ok(()), _) => Ok(()),
    }
}

/// One newline-terminated line, bounded by [`MAX_EVENT_BYTES`].
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let take = end.map_or(bytes.len(), |at| at + 1);
        if line.len() + take > MAX_EVENT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a Tower event exceeds the size limit",
            ));
        }
        line.extend_from_slice(&bytes[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(Some(line));
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
            "revision": 1, "tags": ["follow-up"], "sync_state": "synced",
        })
    }

    fn line(method: &str, params: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"method": method, "params": params})).unwrap()
    }

    fn link() -> Arc<TowerLink> {
        TowerLink::new(
            HostId("box".into()),
            "box",
            &[],
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
    fn without_the_flag_the_cli_never_starts_a_daemon() {
        assert_eq!(cli_args(false, &["events"]), ["--no-start", "events"]);
        assert_eq!(cli_args(true, &["events"]), ["events"]);
    }

    #[test]
    fn tower_keys_never_collide_with_a_real_pid() {
        let key = launcher_key("ag-1");
        assert!(is_tower_key(&SessionKey::from_launcher_pid(key)));
        assert!(!is_tower_key(&SessionKey::from_launcher_pid(4_194_304)));
        assert_eq!(key, launcher_key("ag-1"));
        assert_ne!(key, launcher_key("ag-2"));
    }

    #[test]
    fn the_cli_runs_in_the_login_shell_with_its_arguments_intact() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // A login shell that runs its `-c` command, and a CLI that prints
        // each argument on its own line.
        let shell = script(
            "login",
            r#"[ "$1" = -l ] && [ "$2" = -c ] && exec /bin/sh -c "$3""#,
        );
        script("tower-cli", r#"for arg; do echo "$arg"; done"#);
        let params = r#"{"cwd":"~/it's \ $HOME"}"#;
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &remote_command(&["rpc", "session.open", params])])
            .env("SHELL", &shell)
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("rpc\nsession.open\n{params}\n"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            json_in("motd\n{\"id\":\"ag-1\"}\n"),
            Some(serde_json::json!({"id": "ag-1"}))
        );
    }
}
