//! Wake/network events are hints to recheck transport health, never proof that
//! sessions died. Kept separate from connection setup so provisioning is not
//! cancelled halfway through a server installation.
use std::{
    future::Future,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Wake,
    Network,
}

static EVENTS: LazyLock<watch::Sender<Event>> = LazyLock::new(|| watch::channel(Event::Network).0);

pub(super) fn subscribe(ssh: bool) -> watch::Receiver<Event> {
    if ssh {
        start_monitor();
        EVENTS.subscribe()
    } else {
        static LOCAL: LazyLock<watch::Sender<Event>> =
            LazyLock::new(|| watch::channel(Event::Network).0);
        LOCAL.subscribe()
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn start_monitor() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        std::thread::spawn(|| {
            unsafe extern "C" {
                fn cm_watch_recovery(callback: extern "C" fn(i32)) -> i32;
            }
            extern "C" fn signal(reason: i32) {
                EVENTS.send_replace(if reason == 1 {
                    Event::Wake
                } else {
                    Event::Network
                });
            }
            // The native function owns its CF run loop and registrations for this
            // process-lifetime thread. The callback captures no borrowed state.
            let status = unsafe { cm_watch_recovery(signal) };
            tracing::warn!("macOS recovery monitor stopped ({status})");
        });
    });
}

#[cfg(any(not(target_os = "macos"), test))]
fn start_monitor() {}

pub(super) async fn changed(events: &mut watch::Receiver<Event>) -> Event {
    if events.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
    *events.borrow_and_update()
}

/// Probe the actual daemon channel, including the forwarded socket, rather
/// than merely proving that a new SSH channel can reach the host. Share request
/// IDs with normal RPCs and never keep the owning backend alive.
pub(super) async fn daemon_responsive(
    requests: &tokio::sync::mpsc::WeakUnboundedSender<super::PendingRequest>,
    ids: &std::sync::atomic::AtomicU64,
) -> bool {
    let Some(requests) = requests.upgrade() else {
        return false;
    };
    let req_id = ids.fetch_add(1, Ordering::Relaxed);
    let (reply, rx) = tokio::sync::oneshot::channel();
    if requests
        .send(super::PendingRequest {
            req_id,
            frame: super::ClientFrame::GetVitals { req_id },
            reply,
        })
        .is_err()
    {
        return false;
    }
    drop(requests);
    matches!(
        tokio::time::timeout(super::ssh::WAKE_CHECK_TIMEOUT, rx).await,
        Ok(Ok(_))
    )
}

pub(super) enum Outcome<T> {
    Finished(T),
    EndedDuringProbe(T),
    Stalled,
}

/// Serving stays polled during the health probe, preserving pushed frames and
/// request replies. A failed probe is the only reason to replace the transport.
pub(super) async fn watch_connection<T, P: Future<Output = bool>>(
    serving: impl Future<Output = T>,
    events: &mut watch::Receiver<Event>,
    active: &AtomicBool,
    mut probe: impl FnMut() -> P,
) -> Outcome<T> {
    tokio::pin!(serving);
    loop {
        tokio::select! {
            result = &mut serving => return Outcome::Finished(result),
            _ = changed(events) => {
                active.store(true, Ordering::Relaxed);
                tokio::select! {
                    result = &mut serving => return Outcome::EndedDuringProbe(result),
                    healthy = probe() => {
                        if !healthy { return Outcome::Stalled; }
                        events.borrow_and_update(); // Coalesce network bursts covered by this successful probe.
                        active.store(false, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// Interruptible backoff, separate from draining failed requests.
pub(super) async fn retry_delay(delay: Duration, events: &mut watch::Receiver<Event>) -> bool {
    tokio::select! {
        _ = changed(events) => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn wake_interrupts_a_thirty_second_retry_wait() {
        let (tx, mut events) = watch::channel(Event::Network);
        let task =
            tokio::spawn(async move { retry_delay(Duration::from_secs(30), &mut events).await });
        tokio::task::yield_now().await;
        tx.send_replace(Event::Wake);
        tokio::task::yield_now().await;
        assert!(task.is_finished(), "wake must bypass the reconnect backoff");
        assert!(task.await.unwrap());
    }
    #[tokio::test(start_paused = true)]
    async fn healthy_wake_preserves_serving_and_coalesces_network_changes() {
        use std::sync::Arc;
        use tokio::sync::{mpsc, oneshot};
        let (tx, mut events) = watch::channel(Event::Network);
        let active = Arc::new(AtomicBool::new(false));
        let observed = active.clone();
        let (traffic, mut incoming) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let (started, ready) = oneshot::channel();
        let (answer, reply) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut probe = Some((started, reply));
            watch_connection(
                async move {
                    while let Some(reply) = incoming.recv().await {
                        let _ = reply.send(());
                    }
                },
                &mut events,
                &active,
                move || {
                    let (started, reply) = probe.take().expect("one probe for the event burst");
                    async move {
                        let _ = started.send(());
                        reply.await.unwrap()
                    }
                },
            )
            .await
        });
        tx.send_replace(Event::Wake);
        ready.await.unwrap();
        assert!(observed.load(Ordering::Relaxed));
        let (ack, received) = oneshot::channel();
        traffic.send(ack).unwrap();
        received.await.unwrap(); // The protocol is serviced while the probe waits.
        tx.send_replace(Event::Network);
        answer.send(true).unwrap();
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "healthy connection must remain open");
        assert!(!observed.load(Ordering::Relaxed));
        drop(traffic);
        assert!(matches!(task.await.unwrap(), Outcome::Finished(())));
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_wake_probe_requests_reconnect_after_one_second() {
        use std::sync::Arc;
        let (tx, mut events) = watch::channel(Event::Network);
        let active = Arc::new(AtomicBool::new(false));
        let observed = active.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut started = Some(started);
            watch_connection(
                std::future::pending::<()>(),
                &mut events,
                &active,
                move || {
                    started.take().unwrap().send(()).unwrap();
                    async {
                        tokio::time::sleep(super::super::ssh::WAKE_CHECK_TIMEOUT).await;
                        false
                    }
                },
            )
            .await
        });
        tx.send_replace(Event::Wake);
        ready.await.unwrap();
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(task.await.unwrap(), Outcome::Stalled));
        assert!(
            observed.load(Ordering::Relaxed),
            "progress continues until reconnect succeeds"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_failure_still_waits_for_backoff() {
        let (_tx, mut events) = watch::channel(Event::Network);
        let task =
            tokio::spawn(async move { retry_delay(Duration::from_secs(30), &mut events).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!task.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn ongoing_recovery_does_not_bypass_backoff_without_a_new_event() {
        let (_tx, mut events) = watch::channel(Event::Network);
        let active = AtomicBool::new(true);
        let result = watch_connection(
            async { "handshake failed" },
            &mut events,
            &active,
            || async { panic!("no event, no probe") },
        )
        .await;
        assert!(matches!(result, Outcome::Finished("handshake failed")));
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_probe_uses_shared_request_ids_and_releases_a_silent_waiter() {
        use std::sync::{Arc, atomic::AtomicU64};
        use tokio::sync::mpsc;
        let (tx, mut requests) = mpsc::unbounded_channel();
        let ids = Arc::new(AtomicU64::new(41));
        let weak = tx.downgrade();
        let shared = ids.clone();
        let task = tokio::spawn(async move { daemon_responsive(&weak, &shared).await });
        let request = requests.recv().await.unwrap();
        assert_eq!(request.req_id, 41);
        assert!(matches!(
            request.frame,
            super::super::ClientFrame::GetVitals { req_id: 41 }
        ));
        assert_eq!(ids.fetch_add(1, Ordering::Relaxed), 42);
        tokio::time::advance(super::super::ssh::WAKE_CHECK_TIMEOUT).await;
        assert!(!task.await.unwrap());
        assert!(request.reply.is_closed());
    }
}
