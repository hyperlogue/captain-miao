//! Snapshot ownership is independent of the launcher's lifetime: loaded Codex
//! threads (and their subagents) can keep using a file after its launcher exits.
//! A lease fences creation against collection; inventory proves eventual disuse.
use super::{environment::persist, transport};
use crate::state;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct References {
    endpoint: PathBuf,
    threads: BTreeSet<String>,
    pending_daemons: BTreeSet<i32>,
}

pub(super) struct Store {
    dir: PathBuf,
    _lease: File,
    references: References,
    pending: BTreeMap<i32, usize>,
}

impl Store {
    pub(super) fn new(root: &Path, endpoint: &Path) -> Result<Self> {
        state::create_dir_all_private(root)?;
        let dir = tempfile::Builder::new().prefix("lease-").tempdir_in(root)?;
        let lease = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.path().join("lease"))?;
        lease.lock()?;
        let store = Self {
            dir: dir.keep(),
            _lease: lease,
            references: References {
                endpoint: endpoint.to_owned(),
                threads: BTreeSet::new(),
                pending_daemons: BTreeSet::new(),
            },
            pending: BTreeMap::new(),
        };
        store.save()?;
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        state::write_json_atomic(&self.dir.join("references.json"), &self.references)
    }

    pub(super) fn prepare(&mut self, script: &str, daemon: Option<i32>) -> Result<Value> {
        // Persist uncertainty BEFORE a request can reach the daemon. A crash
        // or lost reply must never make a running thread look unreferenced.
        let daemon = daemon.filter(|pid| *pid > 0).unwrap_or(0);
        *self.pending.entry(daemon).or_default() += 1;
        self.references.pending_daemons.insert(daemon);
        self.save()?;
        persist(&self.dir, script)
    }

    pub(super) fn response(&mut self, response: &Value, daemon: Option<i32>) -> Result<()> {
        if let Some(id) = response["result"]["thread"]["id"].as_str() {
            self.references.threads.insert(id.into());
        } else if !response["error"]["code"].is_i64() {
            // A malformed success cannot settle ownership safely.
            return Ok(());
        }
        let daemon = daemon.filter(|pid| *pid > 0).unwrap_or(0);
        if let Some(pending) = self.pending.get_mut(&daemon) {
            *pending = pending.saturating_sub(1);
            if *pending == 0 {
                self.references.pending_daemons.remove(&daemon);
            }
        }
        self.save()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit the descriptor before exec
        // closes it. Explicit unlock releases that shared open-file lock too.
        let _ = self._lease.unlock();
    }
}

fn daemon_exited(pid: i32) -> bool {
    // Signal zero observes liveness without signaling anything. Unknown PIDs,
    // permission errors and PID reuse all retain files. No /proc dependency.
    pid > 0
        && unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Only this version's leased directories are collectible. Older untracked
/// snapshots have no endpoint or thread ownership and must be retained.
pub(super) async fn collect(root: &Path, endpoint: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry.file_name().to_string_lossy().starts_with("lease-")
        {
            continue;
        }
        let dir = entry.path();
        let Ok(lease) = File::open(dir.join("lease")) else {
            continue;
        };
        if lease.try_lock().is_err() {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join("references.json")) else {
            continue;
        };
        let Ok(references) = serde_json::from_slice::<References>(&bytes) else {
            continue;
        };
        if references.endpoint == endpoint {
            candidates.push((dir, lease, references));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut client = transport::Client::connect(endpoint).await?;
    let loaded = super::loaded_threads(&mut client).await?;
    let mut referenced = loaded.clone();
    // A subagent inherits its parent's shell configuration. Follow the whole
    // ancestry, including saved parents that have already unloaded.
    let mut pending: Vec<_> = loaded.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        let result = client
            .request(
                "thread/read",
                serde_json::json!({"threadId":id,"includeTurns":false}),
            )
            .await?;
        let parent = result["thread"]
            .get("parentThreadId")
            .context("Codex inventory omitted thread ancestry")?;
        if let Some(parent) = parent.as_str() {
            if referenced.insert(parent.into()) {
                pending.push(parent.into());
            }
        } else {
            anyhow::ensure!(parent.is_null(), "invalid Codex thread ancestry");
        }
    }
    for (dir, _lease, references) in candidates {
        if references.threads.iter().any(|id| referenced.contains(id))
            || references
                .pending_daemons
                .iter()
                .any(|pid| !daemon_exited(*pid))
        {
            continue;
        }
        // The lease stays locked through deletion. A live launcher never
        // reuses an old lease directory; new requests always get their own.
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::Message;

    async fn inventory(listener: UnixListener) {
        // Two sweeps: a loaded subagent, then a completely unloaded daemon.
        for loaded in [true, false] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/loaded/list" => {
                        json!({"data":if loaded {vec!["child"]} else {vec![]},"nextCursor":null})
                    }
                    "thread/read" => {
                        json!({"thread":{"parentThreadId":if request["params"]["threadId"] == "child" {json!("parent")} else {Value::Null}}})
                    }
                    method => panic!("unexpected {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn collection_protects_live_launchers_loaded_descendants_and_lost_replies() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let endpoint = root.path().join("server.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        let active = Store::new(root.path(), &endpoint).unwrap();
        let active_dir = active.dir.clone();
        let mut parent = Store::new(root.path(), &endpoint).unwrap();
        parent
            .prepare("parent-secret", Some(std::process::id() as i32))
            .unwrap();
        parent
            .response(
                &json!({"result":{"thread":{"id":"parent"}}}),
                Some(std::process::id() as i32),
            )
            .unwrap();
        let parent_dir = parent.dir.clone();
        let mut unused = Store::new(root.path(), &endpoint).unwrap();
        unused
            .prepare("unused-secret", Some(std::process::id() as i32))
            .unwrap();
        unused
            .response(
                &json!({"result":{"thread":{"id":"unloaded"}}}),
                Some(std::process::id() as i32),
            )
            .unwrap();
        let unused_dir = unused.dir.clone();
        let mut uncertain = Store::new(root.path(), &endpoint).unwrap();
        uncertain
            .prepare("uncertain-secret", Some(std::process::id() as i32))
            .unwrap();
        let uncertain_dir = uncertain.dir.clone();
        let mut exited = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited.id() as i32;
        exited.wait().unwrap();
        let mut abandoned = Store::new(root.path(), &endpoint).unwrap();
        abandoned
            .prepare("abandoned-secret", Some(exited_pid))
            .unwrap();
        let abandoned_dir = abandoned.dir.clone();
        drop(abandoned);
        let legacy = root.path().join("untracked-old-snapshot");
        std::fs::create_dir(&legacy).unwrap();
        drop((parent, unused, uncertain));
        let server = tokio::spawn(inventory(listener));
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            collect(root.path(), &endpoint).await.unwrap();
            assert!(!unused_dir.exists());
            assert!(
                !abandoned_dir.exists(),
                "the daemon owning the unanswered request has exited"
            );
            assert!(parent_dir.exists());
            assert!(uncertain_dir.exists());
            assert!(active_dir.exists());
            collect(root.path(), &endpoint).await.unwrap();
            assert!(!parent_dir.exists());
            assert!(
                uncertain_dir.exists(),
                "an empty inventory cannot settle an in-flight creation"
            );
            assert!(active_dir.exists());
            assert!(legacy.exists());
            server.await.unwrap();
        })
        .await
        .expect("waiting for two environment inventory sweeps");
    }

    #[tokio::test]
    async fn unreachable_inventory_never_proves_a_snapshot_unused() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let endpoint = root.path().join("missing.sock");
        let mut store = Store::new(root.path(), &endpoint).unwrap();
        store.prepare("private", None).unwrap();
        let dir = store.dir.clone();
        // A live lease skips inventory entirely, even during request preparation.
        collect(root.path(), &endpoint).await.unwrap();
        // dup shares the same open-file description, just like a descriptor
        // inherited by a child between fork and exec. Store drop must unlock
        // even while that duplicate remains open.
        let inherited = store._lease.try_clone().unwrap();
        drop(store);
        assert!(collect(root.path(), &endpoint).await.is_err());
        assert!(dir.join("references.json").exists());
        drop(inherited);
    }
}
