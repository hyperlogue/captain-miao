//! Dashboard-owned launches, from an immediate placeholder to a host-owned row.
//!
//! A placeholder has no pid and never enters `App::sessions`. Reservations and
//! window creation run in separate workers; `poll` records the token before it
//! starts a window, so even an immediate attach-exit report can be retained.
//! Host + token is the only reconciliation key. Directory, agent and arrival
//! order cannot distinguish two launches requested in the same directory.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::AgentControl;
use crate::backend::{LaunchPlan, RemoteBackend};
use crate::state::{DetachReport, HostId, LauncherState};
use crate::terminal::{SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget};

use super::FlagKey;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Placement captured at the keystroke, shared by synchronous restart/resume
/// and background new-session launches. Later preference changes cannot move
/// an already requested session to another layout or directory.
pub(super) struct WindowLaunch {
    pub local_token: String,
    pub host: HostId,
    pub cwd: String,
    pub home: String,
    pub target: SpawnTarget,
    pub title: String,
}

impl WindowLaunch {
    pub fn prepare(&self, plan: LaunchPlan) -> (String, SpawnSpec) {
        let mut argv = plan.argv().to_vec();
        let (token, cwd) = match plan {
            LaunchPlan::SpawnLocal { .. } => {
                argv.extend(["--launch-id".into(), self.local_token.clone()]);
                (
                    self.local_token.clone(),
                    cm_core::paths::expand_home(&self.cwd, &self.home),
                )
            }
            LaunchPlan::AttachRemote { session_name, .. } => {
                // The local attach window starts at home; the remote cwd is
                // interpreted only by its host. Use the same exit reporting
                // and held-error wrapper as attaching an existing session.
                let exe = crate::backend::reporter_exe();
                argv = crate::backend::report_on_exit_argv(
                    argv,
                    exe.as_deref(),
                    &self.host.0,
                    &session_name,
                );
                (session_name, self.home.clone())
            }
        };
        (
            token,
            SpawnSpec {
                cwd,
                target: self.target.clone(),
                command: SpawnCommand::Exec(argv),
                title: Some(self.title.clone()),
                // The launcher/wrapper holds failures itself. Terminal hold
                // can leave an untracked local shell after an SSH disconnect.
                hold: false,
                take_focus: false,
                stack: true,
            },
        )
    }
}

pub(super) struct Request {
    pub agent: AgentControl,
    pub window: WindowLaunch,
    /// A removed/replaced backend must not open a window from its stale reply.
    /// `None` denotes the direct-local backend, including in isolated tests.
    pub owner: Option<Arc<RemoteBackend>>,
}

type Reply<T> = tokio::sync::oneshot::Receiver<anyhow::Result<T>>;

fn work<T: Send + 'static>(
    future: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> Reply<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = tx.send(future.await);
    });
    rx
}

fn ready<T>(rx: &mut Reply<T>) -> Option<anyhow::Result<T>> {
    match rx.try_recv() {
        Ok(result) => Some(result),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            Some(Err(anyhow::anyhow!("session launch worker stopped")))
        }
    }
}

enum Phase {
    Opening(Reply<LaunchPlan>),
    Spawning(Reply<SpawnResult>),
    Waiting(Instant),
    Failed,
}

pub(super) struct Pending {
    pub request: Request,
    token: Option<String>,
    phase: Phase,
    focus: bool,
    reports: Vec<DetachReport>,
}

impl Pending {
    fn matches(&self, session: &LauncherState) -> bool {
        session.host == self.request.window.host
            && self
                .token
                .as_deref()
                .is_some_and(|token| session.binding_token() == Some(token))
    }
}

pub(super) struct Finished {
    pub host: HostId,
    pub token: Option<String>,
    pub result: anyhow::Result<SpawnResult>,
    pub reports: Vec<DetachReport>,
}

#[derive(Default)]
pub(super) struct Progress {
    pub finished: Vec<Finished>,
    pub changed: bool,
}

pub(super) enum Report {
    Deferred,
    Closed,
    Other,
}

#[derive(Default)]
pub(super) struct Launches {
    pending: Vec<Pending>,
}

impl Launches {
    pub fn start(
        &mut self,
        request: Request,
        open: impl Future<Output = anyhow::Result<LaunchPlan>> + Send + 'static,
    ) {
        self.cancel_focus();
        self.pending.push(Pending {
            request,
            token: None,
            phase: Phase::Opening(work(open)),
            focus: true,
            reports: Vec::new(),
        });
    }

    /// Tests drive the same worker transitions with held replies and a scripted
    /// terminal. Production supplies the serialized terminal spawn function.
    pub fn poll<F>(
        &mut self,
        can_spawn: impl Fn(&Request) -> bool,
        spawn: impl Fn(SpawnSpec) -> F,
        now: Instant,
    ) -> Progress
    where
        F: Future<Output = anyhow::Result<SpawnResult>> + Send + 'static,
    {
        let mut progress = Progress::default();
        for pending in &mut self.pending {
            let result = match &mut pending.phase {
                Phase::Opening(rx) => match ready(rx) {
                    None => continue,
                    Some(Ok(plan)) if can_spawn(&pending.request) => {
                        let (token, spec) = pending.request.window.prepare(plan);
                        pending.token = Some(token);
                        pending.phase = Phase::Spawning(work(spawn(spec)));
                        progress.changed = true;
                        continue;
                    }
                    Some(Ok(_)) => Err(anyhow::anyhow!("launch host was disconnected or changed")),
                    Some(Err(error)) => Err(error),
                },
                Phase::Spawning(rx) => match ready(rx) {
                    None => continue,
                    Some(Ok(result)) if result.window.is_some() => Ok(result),
                    Some(Ok(_)) => Err(anyhow::anyhow!("terminal did not report a window id")),
                    Some(Err(error)) => Err(error),
                },
                Phase::Waiting(_) | Phase::Failed => continue,
            };
            pending.phase = if result.is_ok() {
                Phase::Waiting(now)
            } else {
                Phase::Failed
            };
            progress.changed = true;
            progress.finished.push(Finished {
                host: pending.request.window.host.clone(),
                token: pending.token.clone(),
                result,
                reports: std::mem::take(&mut pending.reports),
            });
        }
        self.pending.retain(|p| !matches!(p.phase, Phase::Failed));
        progress
    }

    pub fn visible<'a>(&'a self, sessions: &'a [LauncherState]) -> Vec<&'a Pending> {
        self.pending
            .iter()
            .filter(|p| {
                !matches!(p.phase, Phase::Waiting(_)) || !sessions.iter().any(|s| p.matches(s))
            })
            .collect()
    }

    /// A pushed row can beat the spawn reply. Until its window id is recorded,
    /// retain the placeholder and keep that row out of selection/actions:
    /// Enter would otherwise see a detached session and attach a second time.
    pub fn awaiting_window(&self, session: &LauncherState) -> bool {
        self.pending
            .iter()
            .any(|p| matches!(p.phase, Phase::Spawning(_)) && p.matches(session))
    }

    /// Consume confirmations only after the window result has been recorded.
    /// A row can arrive first; the placeholder stays until its window is bound.
    pub fn reconcile(
        &mut self,
        sessions: &[LauncherState],
        now: Instant,
    ) -> (Option<FlagKey>, Vec<HostId>) {
        let mut focus = None;
        let mut expired = Vec::new();
        self.pending.retain(|pending| {
            let Phase::Waiting(since) = pending.phase else {
                return true;
            };
            if let Some(session) = sessions.iter().find(|s| pending.matches(s)) {
                if pending.focus {
                    focus = Some(super::flag_key(session));
                }
                return false;
            }
            if now.saturating_duration_since(since) >= CONFIRM_TIMEOUT {
                expired.push(pending.request.window.host.clone());
                return false;
            }
            true
        });
        (focus, expired)
    }

    pub fn cancel_focus(&mut self) {
        for pending in &mut self.pending {
            pending.focus = false;
        }
    }

    /// A window can exit before its spawn command reports the id. Keep that
    /// report until the caller can bind the window and apply normal close policy.
    pub fn defer_report(&mut self, report: &DetachReport) -> Report {
        let Some(pending) = self.pending.iter_mut().find(|p| {
            p.request.window.host.0 == report.host
                && p.token.as_deref() == Some(report.token.as_str())
        }) else {
            return Report::Other;
        };
        if matches!(pending.phase, Phase::Spawning(_)) {
            pending.reports.push(report.clone());
            Report::Deferred
        } else {
            self.pending.retain(|p| {
                p.request.window.host.0 != report.host
                    || p.token.as_deref() != Some(report.token.as_str())
            });
            Report::Closed
        }
    }

    /// Workers that can still create windows or return their binding.
    #[cfg(test)]
    pub fn working(&self) -> bool {
        self.pending
            .iter()
            .any(|p| matches!(p.phase, Phase::Opening(_) | Phase::Spawning(_)))
    }

    /// Wait through confirmation on quit so the genuine launcher's pid can be
    /// persisted with its window binding. Confirmation has a bounded deadline.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl super::App {
    pub(super) fn reconcile_launches(&mut self, now: Instant) -> bool {
        let before = self.launches.pending.len();
        let (focus, expired) = self.launches.reconcile(&self.sessions, now);
        if let Some(key) = focus
            && let Some(index) = self
                .visible_sessions()
                .iter()
                .position(|s| super::matches_key(s, &key))
        {
            self.table_state.select(Some(index));
        }
        for host in expired {
            self.set_status(
                format!(
                    "Session launch on {} has not appeared. Check its window and host connection before retrying.",
                    host.0
                ),
                true,
            );
        }
        before != self.launches.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionStatus;
    use crate::terminal::{TabId, WindowId};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn request(host: &str, local_token: &str) -> Request {
        Request {
            agent: AgentControl::Codex,
            window: WindowLaunch {
                local_token: local_token.into(),
                host: HostId(host.into()),
                cwd: "~/project".into(),
                home: "/work/local".into(),
                target: SpawnTarget::SharedStackTab,
                title: "Codex: project".into(),
            },
            owner: None,
        }
    }

    fn plan(token: &str) -> LaunchPlan {
        LaunchPlan::AttachRemote {
            session_name: token.into(),
            argv: vec!["remote-client".into(), "attach".into(), token.into()],
        }
    }

    fn row(host: &str, token: &str, pid: u32) -> LauncherState {
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Starting);
        row.host = HostId(host.into());
        row.launcher_pid = pid;
        row.pool_session = Some(token.into());
        row.cwd = "~/project".into();
        row
    }

    fn window() -> SpawnResult {
        SpawnResult {
            window: Some(WindowId::from(42)),
            tab: Some(TabId::from(7)),
        }
    }

    async fn finish(launches: &mut Launches) -> Vec<Finished> {
        let mut finished = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while launches.working() {
                finished.extend(
                    launches
                        .poll(|_| true, |_| async { Ok(window()) }, Instant::now())
                        .finished,
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("launch workers did not finish");
        finished
    }

    #[tokio::test]
    async fn placeholder_precedes_reservation_and_handoff_handles_an_early_row() {
        let mut launches = Launches::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        launches.start(request("remote", "L1"), async { rx.await.unwrap() });
        assert_eq!(launches.visible(&[]).len(), 1);
        assert!(launches.working());
        assert!(
            launches
                .poll(
                    |_| true,
                    |_| async { panic!("reservation is still pending") },
                    Instant::now()
                )
                .finished
                .is_empty()
        );

        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(AtomicBool::new(false));
        assert!(tx.send(Ok(plan("pool-a"))).is_ok());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::Relaxed) {
                launches.poll(
                    |_| true,
                    |_| {
                        let gate = gate.clone();
                        let started = started.clone();
                        async move {
                            started.store(true, Ordering::Relaxed);
                            gate.notified().await;
                            Ok(window())
                        }
                    },
                    Instant::now(),
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Same directory, different host/token: neither is this launch.
        let unrelated = [row("other", "pool-a", 1), row("remote", "pool-b", 2)];
        assert_eq!(launches.visible(&unrelated).len(), 1);
        let actual = [row("remote", "pool-a", 3)];
        assert_eq!(launches.visible(&actual).len(), 1);
        assert!(launches.awaiting_window(&actual[0]));
        assert!(launches.reconcile(&actual, Instant::now()).0.is_none());
        assert!(launches.working(), "quit must wait for the window result");

        gate.notify_one();
        let finished = finish(&mut launches).await;
        assert_eq!(finished.len(), 1);
        assert!(!launches.awaiting_window(&actual[0]));
        assert!(launches.visible(&actual).is_empty());
        assert_eq!(finished[0].token.as_deref(), Some("pool-a"));
        assert_eq!(
            finished[0].result.as_ref().unwrap().window,
            Some(WindowId::from(42))
        );
        assert_eq!(
            launches.reconcile(&actual, Instant::now()).0,
            Some((HostId("remote".into()), 3))
        );
        assert!(launches.reconcile(&actual, Instant::now()).0.is_none());
        assert!(!launches.working());
        assert!(launches.is_empty(), "confirmed bindings allow quit");
    }

    #[tokio::test]
    async fn repeated_launches_reconcile_independently_and_only_latest_can_select() {
        let mut launches = Launches::default();
        launches.start(request("remote", "L1"), async { Ok(plan("pool-a")) });
        launches.start(request("remote", "L2"), async { Ok(plan("pool-b")) });
        assert_eq!(launches.visible(&[]).len(), 2);
        assert_eq!(finish(&mut launches).await.len(), 2);
        assert!(!launches.working());
        assert!(
            !launches.is_empty(),
            "quit must wait to persist real bindings"
        );
        let first = row("remote", "pool-a", 1);
        assert!(launches.reconcile(&[first], Instant::now()).0.is_none());
        assert_eq!(launches.visible(&[]).len(), 1);
        let second = row("remote", "pool-b", 2);
        assert_eq!(
            launches.reconcile(&[second], Instant::now()).0,
            Some((HostId("remote".into()), 2))
        );
        assert!(launches.visible(&[]).is_empty());
    }

    #[tokio::test]
    async fn navigation_and_late_confirmation_never_steal_selection() {
        let mut launches = Launches::default();
        launches.start(request("remote", "L1"), async { Ok(plan("pool-a")) });
        finish(&mut launches).await;
        launches.cancel_focus();
        assert!(
            launches
                .reconcile(&[row("remote", "pool-a", 1)], Instant::now())
                .0
                .is_none()
        );

        launches.start(request("remote", "L2"), async { Ok(plan("pool-b")) });
        finish(&mut launches).await;
        let now = Instant::now();
        assert!(launches.reconcile(&[], now).1.is_empty());
        let (focus, expired) = launches.reconcile(&[], now + CONFIRM_TIMEOUT);
        assert!(focus.is_none());
        assert_eq!(expired, vec![HostId("remote".into())]);
        assert!(launches.visible(&[]).is_empty());
        assert!(launches.is_empty(), "confirmation timeout also allows quit");
        assert!(
            launches
                .reconcile(&[row("remote", "pool-b", 2)], now + CONFIRM_TIMEOUT)
                .0
                .is_none()
        );
    }

    #[tokio::test]
    async fn early_attach_exit_is_retained_until_window_binding_can_be_recorded() {
        let mut launches = Launches::default();
        launches.start(request("remote", "L1"), async { Ok(plan("pool-a")) });
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(AtomicBool::new(false));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::Relaxed) {
                launches.poll(
                    |_| true,
                    |_| {
                        let gate = gate.clone();
                        let started = started.clone();
                        async move {
                            started.store(true, Ordering::Relaxed);
                            gate.notified().await;
                            Ok(window())
                        }
                    },
                    Instant::now(),
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let report = DetachReport {
            host: "remote".into(),
            token: "pool-a".into(),
            status: Some(255),
            held_secs: Some(0),
        };
        assert!(matches!(launches.defer_report(&report), Report::Deferred));
        gate.notify_one();
        let finished = finish(&mut launches).await;
        assert_eq!(finished[0].reports, vec![report.clone()]);
        assert!(matches!(launches.defer_report(&report), Report::Closed));
        assert!(launches.visible(&[]).is_empty());
    }

    #[tokio::test]
    async fn failures_and_worker_panics_remove_placeholders_without_retrying() {
        for failure in ["open", "spawn", "missing-id", "panic", "changed-host"] {
            let mut launches = Launches::default();
            launches.start(request("remote", "L1"), async move {
                if failure == "open" {
                    anyhow::bail!("reservation refused");
                }
                if failure == "panic" {
                    panic!("scripted worker failure");
                }
                Ok(plan("pool-a"))
            });
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let calls = calls.clone();
                    let result = launches.poll(
                        |_| failure != "changed-host",
                        move |_| {
                            calls.fetch_add(1, Ordering::Relaxed);
                            async move {
                                match failure {
                                    "spawn" => anyhow::bail!("terminal refused"),
                                    "missing-id" => Ok(SpawnResult {
                                        window: None,
                                        tab: None,
                                    }),
                                    _ => panic!("spawn must not run"),
                                }
                            }
                        },
                        Instant::now(),
                    );
                    if !result.finished.is_empty() {
                        break result.finished;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(finished.len(), 1, "{failure}");
            assert!(finished[0].result.is_err(), "{failure}");
            assert!(launches.visible(&[]).is_empty(), "{failure}");
            assert!(!launches.working(), "{failure}");
            assert_eq!(
                calls.load(Ordering::Relaxed),
                usize::from(matches!(failure, "spawn" | "missing-id"))
            );
        }
    }

    #[test]
    fn local_and_pooled_window_plans_preserve_launch_and_attach_contracts() {
        let launch = request("remote", "L1").window;
        let (token, spec) = launch.prepare(plan("pool-a"));
        assert_eq!(token, "pool-a");
        assert_eq!(spec.cwd, "/work/local");
        assert!(!spec.hold && !spec.take_focus && spec.stack);
        let SpawnCommand::Exec(argv) = spec.command else {
            panic!("expected exec")
        };
        assert_eq!(
            &argv[argv.len() - 3..],
            ["remote-client", "attach", "pool-a"]
        );
        assert!(argv.iter().any(|arg| arg.contains("trap 'r 129' HUP")));

        let (token, spec) = launch.prepare(LaunchPlan::SpawnLocal {
            argv: vec![
                "miao".into(),
                "launch".into(),
                "codex".into(),
                "~/project".into(),
            ],
        });
        assert_eq!(token, "L1");
        assert_eq!(spec.cwd, "/work/local/project");
        let SpawnCommand::Exec(argv) = spec.command else {
            panic!("expected exec")
        };
        assert_eq!(&argv[argv.len() - 2..], ["--launch-id", "L1"]);
    }
}
