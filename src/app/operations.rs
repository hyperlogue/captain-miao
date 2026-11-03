//! Background operations return owned results; only the UI applies them.
//!
//! Workers never borrow App. A bounded set of operation keys prevents repeated
//! input from creating unlimited work. Dropping the dashboard aborts async work;
//! each transport/subprocess must also enforce its own cancellation contract.
use std::{collections::HashMap, future::Future, time::Duration};

use super::{
    App,
    notifications::{Level, NotificationId},
};

type Apply = Box<dyn FnOnce(&mut App) + Send>;
struct Pending {
    notice: Option<NotificationId>,
    task: tokio::task::JoinHandle<()>,
    reply: tokio::sync::oneshot::Receiver<anyhow::Result<Apply>>,
    success: Option<String>,
    description: String,
}

#[derive(Default)]
pub(super) struct Operations {
    pending: HashMap<String, Pending>,
}

impl Drop for Operations {
    fn drop(&mut self) {
        for pending in self.pending.values() {
            pending.task.abort();
        }
    }
}

impl Operations {
    pub(super) fn contains(&self, key: &str) -> bool {
        self.pending.contains_key(key)
    }

    pub(super) fn has_capacity(&self) -> bool {
        self.pending.len() < 64
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl App {
    /// A success message is appropriate only when the result has no visible UI
    /// consequence. Errors always surface. The key identifies duplicate work.
    pub(super) fn start_operation<T: Send + 'static>(
        &mut self,
        key: String,
        description: String,
        success: Option<String>,
        future: impl Future<Output = anyhow::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        if self.operations.contains(&key) {
            return;
        }
        if !self.operations.has_capacity() {
            self.notify(
                Level::Warning,
                "Too many operations running; try again when one finishes".into(),
            );
            return;
        }
        let notice =
            (!description.is_empty()).then(|| self.notify(Level::Progress, description.clone()));
        let (tx, reply) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let result = future
                .await
                .map(|value| Box::new(move |app: &mut App| apply(app, value)) as Apply);
            let _ = tx.send(result);
        });
        self.operations.pending.insert(
            key,
            Pending {
                notice,
                task,
                reply,
                success,
                description,
            },
        );
    }

    pub(super) fn start_background<T: Send + 'static>(
        &mut self,
        key: String,
        future: impl Future<Output = anyhow::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        if self.operations.contains(&key) {
            return;
        }
        self.start_operation(key.clone(), String::new(), None, future, apply);
    }

    pub(super) fn poll_operations(&mut self) -> bool {
        let mut ready = Vec::new();
        for (key, pending) in &mut self.operations.pending {
            match pending.reply.try_recv() {
                Ok(result) => ready.push((key.clone(), result)),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => ready.push((
                    key.clone(),
                    Err(anyhow::anyhow!("Operation worker stopped")),
                )),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        let changed = !ready.is_empty();
        for (key, result) in ready {
            let pending = self.operations.pending.remove(&key).unwrap();
            match result {
                Ok(apply) => {
                    apply(self);
                    if let Some(notice) = pending.notice {
                        if let Some(message) = pending.success {
                            self.update_notification(notice, Level::Success, message);
                        } else {
                            self.notifications.remove(notice);
                            if self.status_msg.as_deref() == Some(pending.description.as_str()) {
                                self.status_msg = None;
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(notice) = pending.notice {
                        self.update_notification(
                            notice,
                            Level::Error,
                            format!("{}: {error}", pending.description),
                        );
                    } else {
                        tracing::debug!("Background operation failed: {error}");
                    }
                }
            }
        }
        changed
    }
}

/// Bound read-only work and terminal commands. Mutation transports provide
/// their own outcome-aware deadlines rather than claiming a timeout undoes them.
pub(super) async fn within<T>(
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| anyhow::anyhow!("Operation timed out; its outcome is unknown. Check the host or terminal before retrying"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{Backend, test_support},
        protocol::{ClientFrame, ServerFrame},
        state::HostId,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test(start_paused = true)]
    async fn directory_picker_handles_escape_while_host_reply_is_held() {
        let mut app = App::new();
        let host = HostId("slow-host".into());
        let (remote, mut peer) = test_support::connected(host.clone());
        app.backends.push(Backend::Remote(remote));
        app.extra_prefs.host_order = Some(vec![host.0.clone()]);
        app.handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT));
        assert!(app.picker.is_some());
        let (request, reply) = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .unwrap();
        let ClientFrame::ListRecentDirs { req_id } = request else {
            panic!("expected directory request")
        };
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.picker.is_none());
        reply
            .send(ServerFrame::RecentDirs {
                req_id,
                cwds: vec!["~/project".into()],
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(app.poll_operations());
        assert!(
            app.picker.is_none(),
            "late reply must not reopen the picker"
        );
        assert_eq!(app.recent_dirs_cache[&host], ["~/project"]);
        assert!(app.operations.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn flag_writes_are_ordered_and_coalesce_to_the_latest_value() {
        let mut app = App::new();
        let host = HostId("slow-host".into());
        let (remote, mut peer) = test_support::connected(host.clone());
        app.backends.push(Backend::Remote(remote));
        let key = (host, 123);
        app.update_flags(key.clone(), super::super::Cursor::HoldIndex, |f| {
            f.follow_up = true
        });
        app.persist_flags([key.clone()]);
        let (request, first) = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .unwrap();
        let ClientFrame::SetSessionFlags { req_id, flags, .. } = request else {
            panic!("expected flags")
        };
        assert!(flags.follow_up);
        app.update_flags(key.clone(), super::super::Cursor::HoldIndex, |f| {
            f.follow_up = false
        });
        app.persist_flags([key.clone()]);
        assert!(peer.is_empty(), "do not overtake an outstanding write");
        first
            .send(ServerFrame::FlagsSet { req_id, ok: true })
            .unwrap();
        tokio::task::yield_now().await;
        app.poll_operations();
        let (request, second) = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .unwrap();
        let ClientFrame::SetSessionFlags { req_id, flags, .. } = request else {
            panic!("expected flags")
        };
        assert!(!flags.follow_up);
        second
            .send(ServerFrame::FlagsSet { req_id, ok: true })
            .unwrap();
        tokio::task::yield_now().await;
        app.poll_operations();
        assert!(app.pending_flags.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn silent_host_releases_the_operation_at_its_deadline() {
        let mut app = App::new();
        let host = HostId("silent-host".into());
        let (remote, mut peer) = test_support::connected(host.clone());
        app.backends.push(Backend::Remote(remote));
        assert!(app.host_recent_dirs(&host).is_empty());
        let (_, reply) = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert!(app.poll_operations());
        assert!(app.operations.is_empty());
        assert!(reply.is_closed());
        assert!(
            !app.recent_dirs_cache.contains_key(&host),
            "failure must not poison the cache"
        );
    }
}
