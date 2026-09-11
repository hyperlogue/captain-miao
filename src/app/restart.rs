//! Restart bookkeeping stays alive in the event loop while host cleanup runs
//! independently. A session belongs to at most one pending restart, and batch
//! totals count completions rather than submitted requests.
use super::{KillWindow, RestartSpec};
use crate::backend::{BindingPresence, KillOutcome};
use crate::state::{HostId, SessionKey};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Clone)]
pub(super) struct Job {
    batch: u64,
    pub(super) replacement_token: Option<String>,
    pub(super) launched_at: Option<Instant>,
    replacement_seen: bool,
    pub(super) reuse_replacement: bool,
    pub(super) spec: RestartSpec,
    pub(super) window: Option<KillWindow>,
}
impl Job {
    fn has_retry_budget(&self) -> bool {
        self.spec.agent != crate::agent::AgentControl::Codex
            || self
                .launched_at
                .is_some_and(|at| at.elapsed() < cm_core::agents::codex::RESTART_RETRY_WINDOW)
    }
}
pub(super) struct Completion {
    pub(super) job: Job,
    pub(super) outcome: KillOutcome,
}
struct Batch {
    total: usize,
    finished: usize,
    succeeded: usize,
    error: Option<String>,
}

pub(super) struct Restarts {
    queued: VecDeque<Job>,
    pending: HashSet<(HostId, SessionKey)>,
    pending_replacements: HashMap<(HostId, SessionKey), (String, bool)>,
    failed: HashMap<(HostId, SessionKey), Job>,
    batches: HashMap<u64, Batch>,
    next_batch: u64,
    tx: mpsc::UnboundedSender<Completion>,
    rx: mpsc::UnboundedReceiver<Completion>,
}
impl Default for Restarts {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            queued: VecDeque::new(),
            pending: HashSet::new(),
            pending_replacements: HashMap::new(),
            failed: HashMap::new(),
            batches: HashMap::new(),
            next_batch: 0,
            tx,
            rx,
        }
    }
}
impl Restarts {
    pub(super) fn enqueue(
        &mut self,
        specs: Vec<RestartSpec>,
        mut presence: impl FnMut(&HostId, &str) -> BindingPresence,
    ) -> usize {
        self.next_batch += 1;
        let batch = self.next_batch;
        let mut count = 0;
        for spec in specs {
            let key = (spec.host.clone(), spec.key.clone());
            if self.pending.contains(&key) {
                continue;
            }
            let mut job = Job {
                batch,
                spec,
                window: None,
                replacement_token: None,
                launched_at: None,
                replacement_seen: false,
                reuse_replacement: false,
            };
            if let Some(previous) = self.failed.get(&key) {
                let status = presence(&key.0, previous.replacement_token.as_deref().unwrap());
                match status {
                    BindingPresence::Present(crate::state::SessionStatus::FailedToStart) => {}
                    BindingPresence::Missing if previous.replacement_seen => {}
                    BindingPresence::Present(_) if previous.has_retry_budget() => {
                        job = previous.clone();
                        job.batch = batch;
                        job.reuse_replacement = true;
                    }
                    _ => continue,
                }
                self.failed.remove(&key);
            }
            self.pending.insert(key);
            self.queued.push_back(job);
            count += 1;
        }
        if count != 0 {
            self.batches.insert(
                batch,
                Batch {
                    total: count,
                    finished: 0,
                    succeeded: 0,
                    error: None,
                },
            );
        }
        count
    }
    pub(super) fn next(&mut self) -> Option<Job> {
        while let Some(job) = self.queued.pop_front() {
            // Enqueue can precede submission by several frames or other opens.
            if job.reuse_replacement && !job.has_retry_budget() {
                self.finish(job, KillOutcome::Failed("The existing replacement is near its wait deadline; let it finish before retrying restart".into()));
            } else {
                return Some(job);
            }
        }
        None
    }

    pub(super) fn observe_failed(
        &mut self,
        mut presence: impl FnMut(&HostId, &str) -> BindingPresence,
    ) {
        self.failed.retain(|(host, _), job| {
            match presence(host, job.replacement_token.as_deref().unwrap()) {
                BindingPresence::Present(_) => {
                    job.replacement_seen = true;
                    true
                }
                BindingPresence::Missing => !job.replacement_seen,
                BindingPresence::Unknown => true,
            }
        });
    }

    /// Scheduling, not awaiting, is the interface: neither a slow host nor a
    /// batch of such hosts can borrow the dashboard through its RPC wait.
    pub(super) fn wait_for_cleanup(
        &mut self,
        job: Job,
        task: tokio::task::JoinHandle<KillOutcome>,
    ) {
        self.track_replacement(&job);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let outcome = task.await.unwrap_or_else(|error| {
                KillOutcome::Failed(format!("Cleanup worker failed: {error}"))
            });
            let _ = tx.send(Completion { job, outcome });
        });
    }
    pub(super) fn finish(&mut self, job: Job, outcome: KillOutcome) {
        self.track_replacement(&job);
        let _ = self.tx.send(Completion { job, outcome });
    }
    fn track_replacement(&mut self, job: &Job) {
        if let Some(token) = &job.replacement_token {
            self.pending_replacements
                .entry((job.spec.host.clone(), job.spec.key.clone()))
                .or_insert_with(|| (token.clone(), job.replacement_seen));
        }
    }

    /// Positive observations are cheap to take from the dashboard's existing
    /// rows. Keep them here while the job itself belongs to a cleanup worker.
    pub(super) fn observe_pending(&mut self, rows: &[crate::state::LauncherState]) {
        for ((host, _), (token, seen)) in &mut self.pending_replacements {
            *seen |= rows
                .iter()
                .any(|row| &row.host == host && row.binding_token() == Some(token.as_str()));
        }
    }

    pub(super) fn try_recv(&mut self) -> Option<Completion> {
        self.rx.try_recv().ok()
    }

    pub(super) fn settle(&mut self, result: &Completion) -> (String, bool) {
        self.pending
            .remove(&(result.job.spec.host.clone(), result.job.spec.key.clone()));
        let seen = self
            .pending_replacements
            .remove(&(result.job.spec.host.clone(), result.job.spec.key.clone()))
            .is_some_and(|(_, seen)| seen);
        if matches!(
            result.outcome,
            KillOutcome::Failed(_) | KillOutcome::Unreachable
        ) && result.job.replacement_token.is_some()
        {
            let mut job = result.job.clone();
            job.replacement_seen |= seen;
            self.failed
                .insert((job.spec.host.clone(), job.spec.key.clone()), job);
        }
        let batch = self
            .batches
            .get_mut(&result.job.batch)
            .expect("pending restart batch");
        batch.finished += 1;
        match &result.outcome {
            KillOutcome::Signalled | KillOutcome::AlreadyGone => batch.succeeded += 1,
            KillOutcome::Failed(error) => batch.error = Some(error.clone()),
            KillOutcome::Unreachable => {
                batch.error = Some(format!("{} did not answer", result.job.spec.host.0))
            }
        }
        let failed = batch.error.is_some();
        let message = if batch.finished == batch.total {
            match &batch.error {
                None => format!(
                    "Restarted {} {}",
                    batch.succeeded,
                    super::plural_sessions(batch.succeeded)
                ),
                Some(error) => format!(
                    "Restarted {}/{} sessions: {error}",
                    batch.succeeded, batch.total
                ),
            }
        } else {
            format!(
                "Restarting sessions: {}/{} finished",
                batch.finished, batch.total
            )
        };
        if batch.finished == batch.total {
            self.batches.remove(&result.job.batch);
        }
        (message, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionFlags;
    use super::*;
    use crate::agent::AgentControl;

    fn spec(pid: u32) -> RestartSpec {
        RestartSpec {
            agent: AgentControl::Codex,
            host: HostId("test-host".into()),
            key: SessionKey::from_launcher_pid(pid),
            window_id: None,
            window_pid: None,
            cwd: "/work".into(),
            session_id: format!("thread-{pid}"),
            flags: SessionFlags::default(),
            kill_old: true,
        }
    }

    #[tokio::test]
    async fn pending_cleanup_keeps_restart_queue_responsive_and_prevents_duplicates() {
        let mut restarts = Restarts::default();
        assert_eq!(
            restarts.enqueue(vec![spec(1), spec(2)], |_, _| BindingPresence::Missing),
            2
        );
        let first = restarts.next().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        restarts.wait_for_cleanup(
            first,
            tokio::spawn(async move {
                wait.await.unwrap();
                KillOutcome::Signalled
            }),
        );
        assert!(restarts.try_recv().is_none());
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            0,
            "a second keystroke must not start another replacement"
        );
        let second = restarts
            .next()
            .expect("another restart can advance while cleanup waits");
        restarts.finish(second, KillOutcome::Failed("cleanup refused".into()));
        let failed = restarts.try_recv().unwrap();
        let (message, error) = restarts.settle(&failed);
        assert!(error);
        assert_eq!(message, "Restarting sessions: 1/2 finished");
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            0
        );
        release.send(()).unwrap();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(result) = restarts.try_recv() {
                    break result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (message, error) = restarts.settle(&finished);
        assert!(error);
        assert_eq!(message, "Restarted 1/2 sessions: cleanup refused");
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            1,
            "completion releases the session for a later restart"
        );
    }
    #[tokio::test]
    async fn failed_cleanup_reuses_its_waiting_replacement_on_retry() {
        let mut restarts = Restarts::default();
        restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing);
        let mut job = restarts.next().unwrap();
        job.replacement_token = Some("waiting-replacement".into());
        job.launched_at = Some(Instant::now());
        restarts.finish(job, KillOutcome::Failed("cleanup refused".into()));
        let result = restarts.try_recv().unwrap();
        restarts.settle(&result);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            0,
            "an unseen replacement might still attach"
        );
        let mut replacement = crate::state::LauncherState::for_test(
            AgentControl::Codex,
            crate::state::SessionStatus::Starting,
        );
        replacement.host = HostId("test-host".into());
        replacement.launch_id = Some("waiting-replacement".into());
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Present(
                replacement.status.clone()
            )),
            1
        );
        let job = restarts.next().unwrap();
        assert!(
            job.reuse_replacement,
            "retry cleanup without opening a competing replacement"
        );
        restarts.finish(job, KillOutcome::Failed("still refused".into()));
        let result = restarts.try_recv().unwrap();
        restarts.settle(&result);
        replacement.status = crate::state::SessionStatus::FailedToStart;
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Present(
                replacement.status.clone()
            )),
            1
        );
        assert!(
            !restarts.next().unwrap().reuse_replacement,
            "a replacement that can no longer attach permits a fresh launch"
        );
    }
    fn fail_with_replacement(restarts: &mut Restarts, launched_at: Instant) {
        restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing);
        let mut job = restarts.next().unwrap();
        job.replacement_token = Some("waiting-replacement".into());
        job.launched_at = Some(launched_at);
        restarts.finish(job, KillOutcome::Failed("cleanup refused".into()));
        let result = restarts.try_recv().unwrap();
        restarts.settle(&result);
    }

    #[tokio::test]
    async fn retry_preserves_the_full_cleanup_budget_and_rechecks_after_queueing() {
        let mut restarts = Restarts::default();
        fail_with_replacement(&mut restarts, Instant::now());
        let waiting =
            |_: &HostId, _: &str| BindingPresence::Present(crate::state::SessionStatus::Starting);
        assert_eq!(restarts.enqueue(vec![spec(1)], waiting), 1);
        // Model the queue consuming the remaining retry window before launch.
        restarts.queued.front_mut().unwrap().launched_at = Some(
            Instant::now()
                - cm_core::agents::codex::RESTART_RETRY_WINDOW
                - std::time::Duration::from_secs(1),
        );
        assert!(
            restarts.next().is_none(),
            "never submit cleanup after its remaining handoff budget expires"
        );
        let result = restarts.try_recv().unwrap();
        assert!(matches!(result.outcome, KillOutcome::Failed(_)));
        restarts.settle(&result);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], waiting),
            0,
            "retry must not reset the original launch time"
        );
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Present(
                crate::state::SessionStatus::FailedToStart
            )),
            1
        );
        assert!(!restarts.next().unwrap().reuse_replacement);
    }

    #[tokio::test]
    async fn confirmed_replacement_removal_releases_a_failed_restart() {
        let mut restarts = Restarts::default();
        fail_with_replacement(&mut restarts, Instant::now());
        restarts.observe_failed(|_, _| BindingPresence::Missing);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            0,
            "initial absence is not a confirmed removal"
        );
        restarts
            .observe_failed(|_, _| BindingPresence::Present(crate::state::SessionStatus::Starting));
        restarts.observe_failed(|_, _| BindingPresence::Unknown);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Unknown),
            0,
            "disconnect must retain restart ownership"
        );
        restarts.observe_failed(|_, _| BindingPresence::Missing);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            1
        );
        assert!(!restarts.next().unwrap().reuse_replacement);
    }
    #[tokio::test]
    async fn replacement_observed_before_cleanup_failure_can_be_retired() {
        let mut restarts = Restarts::default();
        restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing);
        let mut job = restarts.next().unwrap();
        job.replacement_token = Some("waiting-replacement".into());
        job.launched_at = Some(Instant::now());
        let (release, wait) = tokio::sync::oneshot::channel();
        restarts.wait_for_cleanup(
            job,
            tokio::spawn(async move {
                wait.await.unwrap();
                KillOutcome::Failed("cleanup refused".into())
            }),
        );
        let mut row = crate::state::LauncherState::for_test(
            AgentControl::Codex,
            crate::state::SessionStatus::Starting,
        );
        row.host = HostId("test-host".into());
        row.launch_id = Some("waiting-replacement".into());
        restarts.observe_pending(&[row]);
        restarts.observe_pending(&[]); // It disappeared while the RPC was pending.
        release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(result) = restarts.try_recv() {
                    break result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        restarts.settle(&result);
        restarts.observe_failed(|_, _| BindingPresence::Missing);
        assert_eq!(
            restarts.enqueue(vec![spec(1)], |_, _| BindingPresence::Missing),
            1
        );
    }
}
