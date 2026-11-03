//! One ordered writer for dashboard-owned files. Repeated writes to the same
//! file coalesce while an earlier write is running. The UI never waits on disk.
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

type Write = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;
#[derive(Default)]
struct Queue {
    pending: BTreeMap<String, Write>,
    active: Option<(String, Instant)>,
    errors: Vec<String>,
    stop: bool,
}

pub(super) struct Persistence {
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

impl Persistence {
    pub(super) fn new() -> Self {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker = queue.clone();
        std::thread::spawn(move || {
            loop {
                let (lock, wake) = &*worker;
                let mut queue = lock.lock().unwrap();
                while queue.pending.is_empty() && !queue.stop {
                    queue = wake.wait(queue).unwrap();
                }
                if queue.pending.is_empty() && queue.stop {
                    break;
                }
                let (key, write) = queue.pending.pop_first().unwrap();
                queue.active = Some((key.clone(), Instant::now()));
                drop(queue);
                let result = write();
                let mut queue = lock.lock().unwrap();
                queue.active = None;
                if let Err(error) = result {
                    queue.errors.push(format!("Could not save {key}: {error}"));
                }
            }
        });
        Self { queue }
    }

    pub(super) fn submit(&self, key: String, write: Write) {
        let mut queue = self.queue.0.lock().unwrap();
        queue.pending.insert(key, write);
        self.queue.1.notify_one();
    }

    pub(super) fn idle(&self) -> bool {
        let queue = self.queue.0.lock().unwrap();
        queue.pending.is_empty() && queue.active.is_none()
    }

    pub(super) fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut self.queue.0.lock().unwrap().errors)
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        self.queue.0.lock().unwrap().stop = true;
        self.queue.1.notify_one();
    }
}

impl super::App {
    pub(super) fn persist(
        &self,
        key: impl Into<String>,
        write: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        if let Some(writer) = &self.persistence {
            writer.submit(key.into(), Box::new(write));
        } else if let Err(error) = write() {
            tracing::warn!("Could not save dashboard state: {error}");
        }
    }

    pub(super) fn persist_json<T: serde::Serialize + Send + 'static>(
        &self,
        path: std::path::PathBuf,
        value: T,
    ) {
        let key = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.persist(key, move || crate::state::write_json_atomic(&path, &value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn a_slow_write_does_not_block_submission_and_later_values_coalesce() {
        let writer = Persistence::new();
        let (started, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (written, results) = mpsc::channel();
        writer.submit(
            "first".into(),
            Box::new(move || {
                started.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(10))
                    .expect("release first write");
                Ok(())
            }),
        );
        ready
            .recv_timeout(Duration::from_secs(10))
            .expect("first write started");
        let old = written.clone();
        writer.submit(
            "settings".into(),
            Box::new(move || {
                old.send(1).unwrap();
                Ok(())
            }),
        );
        writer.submit(
            "settings".into(),
            Box::new(move || {
                written.send(2).unwrap();
                Ok(())
            }),
        );
        assert!(!writer.idle());
        release.send(()).unwrap();
        assert_eq!(
            results
                .recv_timeout(Duration::from_secs(10))
                .expect("latest write"),
            2
        );
        assert!(results.try_recv().is_err());
    }
}
