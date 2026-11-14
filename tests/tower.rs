//! `miao launch` on Tower, run as the real binary against private state
//! roots: the launcher records the session, the agent's hooks reach it through
//! the same binary, and a stop ends both.

use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tower_agents::runtime::Embedding;
use tower_launcher::local::{Local, Session};

// Stands in for Claude Code: reports through the helper its hooks would call.
const AGENT: &str = r#"#!/bin/sh
printf '%s\n' "$@" > claude.args
payload='{"session_id":"native-claude","model":"fixture-model","root_confirmed":true}'
report() { printf '%s' "$payload" | "$TOWER_HELPER" hook claude-code "$1"; }
report SessionStart
last=
while :; do
  phase=$(cat phase 2>/dev/null || :)
  if [ "$phase" != "$last" ]; then
    case $phase in work) report UserPromptSubmit;; idle) report Stop;; esac
    last=$phase
  fi
  sleep 0.03
done
"#;

/// Longer than any launcher step, so a miss means a lost update rather than a
/// slow machine; the panic names the state being awaited.
const WATCHDOG: Duration = Duration::from_secs(60);

fn await_session(local: &Local, phase: &str, ready: impl Fn(&Session) -> bool) -> Session {
    let deadline = Instant::now() + WATCHDOG;
    loop {
        let sessions = local.sessions().unwrap();
        if let Some(session) = sessions.iter().find(|session| ready(session)) {
            return session.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out awaiting {phase}: {sessions:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Ends the launcher's process group when the test does, so a failed
/// assertion leaves no agent behind.
struct Group(std::process::Child);
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: signals the group this test created with `process_group(0)`.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_tower_launch_tracks_its_agent_and_stops_on_request() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let (state, bin, work) = (root.join("state"), root.join("bin"), root.join("work"));
    for dir in [&bin, &work] {
        std::fs::create_dir(dir).unwrap();
    }
    let agent = bin.join("claude");
    write(&agent, AGENT);
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();

    let miao = env!("CARGO_BIN_EXE_miao");
    let launcher = Command::new(miao)
        .args(["launch", "claude"])
        .arg(&work)
        .args(["--tower", "--launch-id", "L1-1", "--verbose"])
        .env_clear()
        .env("HOME", &root)
        .env("XDG_STATE_HOME", &state)
        // The launch gate only asks whether a supported terminal is present.
        .env("KITTY_PID", "1")
        .env("TERM", "xterm-kitty")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut launcher = Group(launcher);

    let tower = state.join("captain-miao/tower");
    let local = Local::new(Embedding {
        helper: miao.into(),
        state: tower.to_string_lossy().into_owned(),
        codex_endpoint: None,
    })
    .unwrap();

    let started = await_session(&local, "agent start", |session| {
        session.live
            && session
                .state
                .native
                .as_ref()
                .is_some_and(|native| native.id == "native-claude")
    });
    assert_eq!(started.plan.id, "L1-1");
    assert_eq!(started.state.launcher_pid, launcher.0.id());
    let args = std::fs::read_to_string(work.join("claude.args")).unwrap();
    assert!(
        args.ends_with("--verbose\n") && !args.contains("--tower") && !args.contains("--launch-id"),
        "the agent receives its own arguments and none of the launch's: {args:?}"
    );
    let window: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tower.join("sessions/L1-1/window.json")).unwrap())
            .unwrap();
    assert_eq!(window["terminfo"], "xterm-kitty");

    write(&work.join("phase"), "work");
    await_session(&local, "working", |session| {
        session.state.status == "working"
    });
    write(&work.join("phase"), "idle");
    await_session(&local, "idle", |session| session.state.status == "idle");

    local.stop("L1-1").unwrap();
    let deadline = Instant::now() + WATCHDOG;
    while launcher.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the launcher outlived its stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !tower.join("sessions/L1-1").exists(),
        "an ended session leaves no record for the dashboard to list"
    );
}
