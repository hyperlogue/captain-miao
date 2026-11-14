//! Tower as the backend for this machine's sessions, behind the
//! [`FLAG`] environment variable.
//!
//! With the flag set, `miao launch` supervises its agent with Tower's
//! launcher, agent hooks reach that launcher through [`helper`], and the
//! dashboard lists, stops and flags sessions from the records Tower keeps
//! below [`root`]. No Tower daemon or terminal pool is involved: each window
//! is still its own launcher.
//!
//! The dashboard keys a session by its launcher's pid, as it always has, so
//! everything above [`crate::backend::Backend`] is unchanged. Sessions of
//! both launchers are always listed and controlled; the flag only decides
//! which launcher new sessions use, and so which rows need a restart.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use tower_agents::Harness;
use tower_agents::runtime::{AgentDetail, AgentSpec, Embedding, NativeSession};
use tower_launcher::local::{self, Local, Session};
use tower_protocol::AgentRecord;

use crate::agent::AgentKind;
use crate::backend::{LaunchPlan, OpenSpec};
use crate::state::{self, HostId, LauncherState, SessionFlags, SessionKey, SessionStatus};
use cm_core::agents::codex::CodexMode;
use cm_core::terminal::WindowId;

/// Set to anything but empty or `0` to run local sessions on Tower.
pub(crate) const FLAG: &str = "CAPTAIN_MIAO_TOWER";

/// Marks a `miao launch` argv for Tower. A window does not inherit the
/// dashboard's environment, so the choice travels in the command.
const LAUNCH: &str = "--tower";
/// The agent's own id of the conversation to continue.
const RESUME: &str = "--tower-resume";
/// Continue [`RESUME`] as a new conversation.
const FORK: &str = "--tower-fork";

/// The tags that hold a session's dashboard flags.
pub(crate) const PINNED: &str = "pinned";
pub(crate) const FOLLOW_UP: &str = "follow-up";

pub(crate) fn enabled() -> bool {
    std::env::var(FLAG).is_ok_and(|value| !value.is_empty() && value != "0")
}

/// Whether a `miao launch` argv asks for Tower, by flag or by argument.
pub(crate) fn requested(args: &[String]) -> bool {
    enabled() || args.iter().any(|arg| arg == LAUNCH)
}

/// Where Tower keeps its records, inside captain-miao's own state.
fn root() -> PathBuf {
    state::state_dir().join("tower")
}

fn sessions_dir() -> PathBuf {
    root().join("sessions")
}

/// Whether Tower has ever recorded a session here. Everything that reads
/// its records asks this first, so a machine that never enabled Tower pays
/// one `stat`.
pub(crate) fn has_records() -> bool {
    sessions_dir().is_dir()
}

fn local() -> io::Result<Local> {
    let codex = cm_core::config::read_codex().unwrap_or_default();
    Local::new(Embedding {
        helper: std::env::current_exe()?.to_string_lossy().into_owned(),
        state: root().to_string_lossy().into_owned(),
        codex_endpoint: (codex.mode == CodexMode::AppServer)
            .then(|| codex.socket_path().ok())
            .flatten()
            .map(|path| format!("unix://{}", path.display())),
    })
}

/// Answers the commands Tower's launcher and its agents send to this
/// executable. `None` means the arguments are an ordinary `miao` command.
pub(crate) fn helper(args: &[String]) -> Option<io::Result<()>> {
    // Tower's commands take no options. `miao hook <event> --agent=<name>`
    // has the same number of arguments as Tower's `hook <harness> <event>`.
    if args.iter().any(|arg| arg.starts_with('-')) {
        return None;
    }
    local::helper(args)
}

pub(crate) fn harness(agent: AgentKind) -> Option<Harness> {
    Harness::parse(agent.cli_subcommand())
}

fn agent(harness: Harness) -> AgentKind {
    AgentKind::ALL
        .iter()
        .copied()
        .find(|agent| self::harness(*agent) == Some(harness))
        .unwrap_or(AgentKind::Unknown)
}

fn mode(agent: AgentKind) -> Option<String> {
    (agent == AgentKind::Codex).then(|| {
        match cm_core::config::read_codex().unwrap_or_default().mode {
            CodexMode::Native => "native",
            CodexMode::AppServer => "app-server",
        }
        .to_string()
    })
}

/// The launch argv for `spec`: the same verb the older launcher uses, marked
/// for Tower.
pub(crate) fn open_plan(spec: &OpenSpec, home: &str) -> anyhow::Result<LaunchPlan> {
    if harness(spec.agent).is_none() {
        anyhow::bail!(crate::agent::UNKNOWN_AGENT_REFUSAL);
    }
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| "miao".into())
        .to_string_lossy()
        .into_owned();
    let mut argv = vec![
        exe,
        "launch".to_string(),
        spec.agent.cli_subcommand().to_string(),
        cm_core::paths::expand_home(&spec.cwd, home),
        LAUNCH.to_string(),
    ];
    if let Some((conversation, fork)) = &spec.resume {
        argv.extend([RESUME.to_string(), conversation.clone()]);
        if *fork {
            argv.push(FORK.to_string());
        }
    }
    if let Some(name) = &spec.worktree
        && let Some(args) = spec.agent.worktree_args(Some(name))
    {
        argv.extend(args);
    }
    Ok(LaunchPlan::SpawnLocal { argv })
}

/// What a `miao launch` argv asks Tower for, with Tower's own arguments
/// removed from what the agent receives.
#[derive(Debug, PartialEq)]
struct Request {
    id: Option<String>,
    cwd: String,
    resume: Option<String>,
    fork: bool,
    args: Vec<String>,
}

fn parse(args: Vec<String>) -> Request {
    let (id, args) = cm_core::cli::take_launch_id(args);
    let mut request = Request {
        id,
        cwd: String::new(),
        resume: None,
        fork: false,
        args: Vec::new(),
    };
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            LAUNCH => {}
            FORK => request.fork = true,
            RESUME => request.resume = args.next(),
            _ => rest.push(arg),
        }
    }
    (request.cwd, request.args) = cm_core::cli::split_cwd(rest);
    request
}

/// What the launcher knows about its window, kept beside Tower's records
/// because Tower has no notion of one.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Window {
    #[serde(default)]
    terminal: Option<String>,
    #[serde(default)]
    terminfo: Option<String>,
    #[serde(default)]
    window_id: Option<WindowId>,
}

fn window_path(id: &str) -> PathBuf {
    sessions_dir().join(id).join("window.json")
}

/// Runs `agent` in this terminal under Tower's launcher until it exits, and
/// exits with its status.
pub(crate) async fn launch(agent: AgentKind, args: Vec<String>) -> anyhow::Result<()> {
    cm_core::logging::init_tracing("launcher");
    let request = parse(args);
    let harness = harness(agent).ok_or_else(|| anyhow::anyhow!("Tower cannot run {agent:?}"))?;
    if request.id.is_none() && cm_core::terminal::is_unbindable_ghostty() {
        anyhow::bail!(
            "Ghostty exports no per-surface id, so a session started by hand here could not be \
             bound to its window. Start it from the dashboard instead."
        );
    }
    let window = Window {
        terminal: cm_core::terminal::current_terminal_identity(),
        terminfo: std::env::var("TERM")
            .ok()
            .map(|term| term.trim().to_string())
            .filter(|term| !term.is_empty()),
        // A dashboard launch is bound by its id; only a session started by
        // hand has to name its own window.
        window_id: request
            .id
            .is_none()
            .then(cm_core::terminal::current_window)
            .flatten(),
    };
    let id = request
        .id
        .clone()
        .unwrap_or_else(|| format!("M{}", std::process::id()));
    let cwd = std::fs::canonicalize(&request.cwd)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(request.cwd.clone());
    let mode = mode(agent);

    let prepared = {
        let (id, mode) = (id.clone(), mode.clone());
        tokio::task::spawn_blocking(move || -> io::Result<PathBuf> {
            let local = local()?;
            let (resume, native_home) = match &request.resume {
                // Tower's own lookup knows which home holds the conversation.
                // One it cannot find may still be known to the agent.
                Some(native) => match local.conversation(harness, mode.as_deref(), native) {
                    Ok(found) => (Some(found.native), found.namespace),
                    Err(error) => {
                        tracing::warn!("Tower did not find conversation {native}: {error}");
                        let native = NativeSession {
                            id: native.clone(),
                            resume_ref: None,
                        };
                        (Some(native), None)
                    }
                },
                None => (None, None),
            };
            // An ended session that had this id leaves records behind.
            match local.forget(&id) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let plan = local.prepare(
                &id,
                AgentSpec {
                    harness,
                    cwd,
                    args: request.args,
                    mode,
                    fork: request.fork && resume.is_some(),
                    resume,
                    native_home,
                },
            )?;
            state::write_json_atomic(&window_path(&id), &window).map_err(io::Error::other)?;
            Ok(plan)
        })
        .await?
    };
    let code = match prepared {
        Ok(plan) => local::run(&plan).await,
        Err(error) => Err(error),
    };
    // The session is over: the dashboard lists live sessions only, and the
    // agent's own history is what a later resume reads.
    let _ = tokio::task::spawn_blocking(move || local().and_then(|local| local.forget(&id))).await;
    match code {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            // The window closes with this process, taking the reason with it.
            eprintln!("Could not start the session: {error}\n\nPress Enter to close.");
            let _ = io::stdin().read_line(&mut String::new());
            std::process::exit(1)
        }
    }
}

fn status(status: &str, phase: Option<&str>) -> SessionStatus {
    match status {
        "starting" => SessionStatus::Starting,
        "compacting" => SessionStatus::Compacting,
        "waiting_approval" => SessionStatus::WaitingForApproval,
        "waiting_input" => SessionStatus::WaitingForDecision,
        "idle" if phase == Some("background") => SessionStatus::BackgroundActive,
        "idle" => SessionStatus::Idle,
        // `working`, `waiting_tool` and anything a newer launcher adds.
        _ => SessionStatus::Active,
    }
}

/// When each session was first seen working. Tower records the last change,
/// not the start of the turn.
static WORKING_SINCE: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);

fn live(local: &Local) -> io::Result<Vec<Session>> {
    Ok(local
        .sessions()?
        .into_iter()
        .filter(|session| session.live)
        .collect())
}

/// This machine's Tower sessions, as the rows the dashboard already shows.
pub(crate) fn rows(home: &str) -> Vec<LauncherState> {
    if !has_records() {
        return Vec::new();
    }
    let (sessions, records) = match local().and_then(|local| Ok((live(&local)?, local.records()?)))
    {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!("could not read Tower sessions: {error}");
            return Vec::new();
        }
    };
    let mut since = WORKING_SINCE.lock().unwrap();
    since.retain(|id, _| sessions.iter().any(|session| &session.plan.id == id));
    records
        .into_iter()
        .filter_map(|record| {
            let session = sessions.iter().find(|s| s.plan.id == record.id)?;
            let window: Window = std::fs::read(window_path(&record.id))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            let mut row = row(record, home, &mut since);
            row.launcher_pid = session.state.launcher_pid;
            // The id the dashboard minted for the launch, which binds the row
            // to the window it opened.
            row.launch_id = Some(session.plan.id.clone());
            (row.window_id, row.terminal, row.terminfo) =
                (window.window_id, window.terminal, window.terminfo);
            Some(row)
        })
        .collect()
}

/// A Tower session record as a dashboard row, apart from what only its
/// launcher or window knows. `since` remembers when each session began the
/// turn it is working on; `home` is the session host's home directory.
pub(crate) fn row(
    record: AgentRecord,
    home: &str,
    since: &mut HashMap<String, u64>,
) -> LauncherState {
    let detail: AgentDetail = record
        .status_detail
        .and_then(|detail| serde_json::from_value(serde_json::to_value(detail).ok()?).ok())
        .unwrap_or_default();
    let status = status(&record.status, detail.phase.as_deref());
    // Tower counts milliseconds; the dashboard counts seconds.
    let updated_at = record.updated_at / 1000;
    let active_since = if status == SessionStatus::Active {
        Some(*since.entry(record.id.clone()).or_insert(updated_at))
    } else {
        since.remove(&record.id);
        None
    };
    // Every path leaving a backend is host-canonical.
    let tidy = |text: String| match home.len() {
        0 | 1 => text,
        _ => text.replace(home, "~"),
    };
    let flags = SessionFlags {
        pinned: record.tags.contains(PINNED),
        follow_up: record.tags.contains(FOLLOW_UP),
    };
    LauncherState {
        agent: Harness::parse(&record.harness).map_or(AgentKind::Unknown, agent),
        codex_mode: if record.mode.as_deref() == Some("app-server") {
            CodexMode::AppServer
        } else {
            CodexMode::Native
        },
        codex_connected: None,
        codex_control: false,
        launcher_pid: 0,
        session_id: record.native_session_id,
        child_session_ids: Vec::new(),
        window_id: None,
        tab_id: None,
        cwd: cm_core::paths::collapse_home(&record.cwd, home),
        status,
        background_commands: detail.background_commands.into_iter().map(tidy).collect(),
        last_tool: detail.last_tool,
        updated_at,
        active_since,
        last_prompt: detail.last_prompt,
        child_pid: record.pid,
        last_error_at: detail.error.as_ref().map(|_| updated_at),
        last_error: detail.error.map(tidy),
        cleanup: None,
        context_tokens: detail.context_tokens,
        context_window: None,
        model: detail.model,
        effort: detail.effort,
        name: record.display_name,
        first_prompt: None,
        pool_session: None,
        launch_id: None,
        terminal: None,
        terminfo: None,
        alt_screen: false,
        kitty_keyboard: false,
        flags: Some(flags),
        attached: None,
        host: HostId::local(),
        needs_restart: false,
    }
}

/// The Tower session whose launcher `key` names.
fn find(local: &Local, key: &SessionKey) -> io::Result<Option<String>> {
    Ok(live(local)?
        .into_iter()
        .find(|session| session.state.launcher_pid.to_string() == key.0)
        .map(|session| session.plan.id))
}

/// Whether Tower still runs the session `key` names.
pub(crate) fn runs(key: &SessionKey) -> bool {
    has_records() && local().is_ok_and(|local| matches!(find(&local, key), Ok(Some(_))))
}

/// Asks the launcher `key` names to end its agent. `None` means Tower runs
/// no such session. Blocks for the launcher's reply.
pub(crate) fn stop(key: &SessionKey) -> Option<io::Result<()>> {
    if !has_records() {
        return None;
    }
    let local = match local() {
        Ok(local) => local,
        Err(error) => return Some(Err(error)),
    };
    match find(&local, key) {
        Ok(Some(id)) => Some(local.stop(&id)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// Stores `flags` as tags on the session `key` names. `None` means Tower
/// runs no such session.
pub(crate) fn set_flags(key: &SessionKey, flags: SessionFlags) -> Option<bool> {
    if !has_records() {
        return None;
    }
    let local = local().ok()?;
    let id = find(&local, key).ok()??;
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    for (tag, set) in [(PINNED, flags.pinned), (FOLLOW_UP, flags.follow_up)] {
        if set { &mut add } else { &mut remove }.push(tag.to_string());
    }
    Some(
        local
            .patch_tags(&id, &add, &remove, false)
            .inspect_err(|error| tracing::warn!("could not store session flags: {error}"))
            .is_ok(),
    )
}

/// Reports every change below Tower's session records through `changed`.
pub(crate) fn watch(
    changed: impl Fn() + Send + 'static,
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::Watcher;
    let dir = sessions_dir();
    state::create_dir_all_private(&dir)
        .map_err(|error| notify::Error::generic(&error.to_string()))?;
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            changed();
        }
    })?;
    watcher.watch(&dir, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_maps_to_a_harness_and_back() {
        for &kind in AgentKind::ALL {
            let harness = harness(kind).unwrap_or_else(|| panic!("{kind:?} has no harness"));
            assert_eq!(agent(harness), kind);
        }
    }

    #[test]
    fn the_launch_argv_carries_resume_and_fork_apart_from_agent_arguments() {
        let plan = open_plan(
            &OpenSpec {
                agent: AgentKind::Claude,
                cwd: "~/work".into(),
                resume: Some(("abc".into(), true)),
                worktree: None,
            },
            "/home/miao",
        )
        .unwrap();
        let mut argv = plan.argv()[3..].to_vec();
        argv.extend(["--launch-id".into(), "L1-2".into(), "--verbose".into()]);
        assert_eq!(
            parse(argv),
            Request {
                id: Some("L1-2".into()),
                cwd: "/home/miao/work".into(),
                resume: Some("abc".into()),
                fork: true,
                args: vec!["--verbose".into()],
            }
        );
    }

    #[test]
    fn a_launch_without_a_directory_runs_in_the_current_one() {
        let request = parse(vec![LAUNCH.into(), "--model".into(), "x".into()]);
        assert_eq!(request.cwd, ".");
        assert_eq!(request.args, ["--model", "x"]);
        assert_eq!(request.id, None);
    }

    fn record(status: &str, updated_at: u64) -> AgentRecord {
        serde_json::from_value(serde_json::json!({
            "id": "L7-1", "display_name": "Fix the build", "node": "local",
            "harness": "codex", "mode": "app-server", "ownership": "owned",
            "trust": "strong", "native_session_id": "thread-1", "pid": 4243,
            "pid_start_time": 2, "cwd": "/home/miao/work", "status": status,
            "status_detail": {
                "last_tool": "Bash", "model": "m", "context_tokens": 1200,
                "error": "failed in /home/miao/work", "last_prompt": "fix it",
                "background_commands": ["/home/miao/bin/serve"],
            },
            "caps": {"attach": false, "kill": true, "message": "none",
                     "message_reason": null, "wake_when_idle": false,
                     "reply_path": false, "steer": false},
            "pool_session": null, "attach_count": 0, "sources": [],
            "registered_at": 1, "updated_at": updated_at, "last_seen": 1,
            "revision": 9, "tags": ["pinned"], "sync_state": "synced",
        }))
        .unwrap()
    }

    #[test]
    fn a_tower_record_becomes_a_row() {
        let mut since = HashMap::new();
        let row = row(record("working", 5_000_999), "/home/miao", &mut since);
        assert_eq!(
            (row.agent, row.codex_mode),
            (AgentKind::Codex, CodexMode::AppServer)
        );
        assert_eq!(row.session_id.as_deref(), Some("thread-1"));
        assert_eq!(
            (row.cwd.as_str(), row.name.as_deref()),
            ("~/work", Some("Fix the build"))
        );
        assert_eq!(row.background_commands, ["~/bin/serve"]);
        assert_eq!(row.last_error.as_deref(), Some("failed in ~/work"));
        assert_eq!(row.last_prompt.as_deref(), Some("fix it"));
        assert_eq!((row.updated_at, row.active_since), (5_000, Some(5_000)));
        assert_eq!(row.child_pid, Some(4243));
        assert_eq!(
            row.flags,
            Some(SessionFlags {
                pinned: true,
                follow_up: false
            })
        );
        assert!(!row.needs_restart);

        // A later event in the same turn keeps the turn's start.
        let later = super::row(record("working", 9_000_000), "/home/miao", &mut since);
        assert_eq!((later.updated_at, later.active_since), (9_000, Some(5_000)));
        let idle = super::row(record("idle", 9_500_000), "/home/miao", &mut since);
        assert_eq!(
            (idle.status, idle.active_since),
            (SessionStatus::Idle, None)
        );
        assert!(since.is_empty());
    }

    #[test]
    fn only_tower_commands_reach_the_helper() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert!(helper(&args(&["hook", "Stop", "--agent=codex"])).is_none());
        assert!(helper(&args(&["hook", "Stop"])).is_none());
        assert!(helper(&args(&["launch", "claude", "/work"])).is_none());
    }

    #[test]
    fn tower_statuses_map_to_dashboard_statuses() {
        for (tower, phase, want) in [
            ("starting", None, SessionStatus::Starting),
            ("working", None, SessionStatus::Active),
            ("waiting_tool", None, SessionStatus::Active),
            ("compacting", None, SessionStatus::Compacting),
            ("idle", None, SessionStatus::Idle),
            ("idle", Some("background"), SessionStatus::BackgroundActive),
            ("waiting_approval", None, SessionStatus::WaitingForApproval),
            ("waiting_input", None, SessionStatus::WaitingForDecision),
        ] {
            assert_eq!(status(tower, phase), want, "{tower} {phase:?}");
        }
    }
}
