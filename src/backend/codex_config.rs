//! Host-owned Codex policy. Reads are asynchronous while the hosts panel is
//! visible; old servers time out explicitly instead of masquerading as native.
use super::*;
use cm_core::agents::codex::CodexConfig;
use tokio::time::Instant;

#[derive(Default)]
pub(super) struct ConfigCell {
    value: Option<Result<CodexConfig, String>>,
    pending: bool,
    last_poll: Option<Instant>,
    revision: u64,
}

impl Backend {
    pub(crate) fn codex_config(&self) -> Option<Result<CodexConfig, String>> {
        match self {
            Self::Local(_) => Some(cm_core::config::read_codex().map_err(|e| e.to_string())),
            Self::Remote(remote) => remote.codex_config.lock().unwrap().value.clone(),
        }
    }

    pub(crate) fn poll_codex_config(&self) {
        let Self::Remote(remote) = self else { return };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let revision = {
            let mut cell = remote.codex_config.lock().unwrap();
            if cell.pending
                || cell
                    .last_poll
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
            {
                return;
            }
            cell.pending = true;
            cell.last_poll = Some(Instant::now());
            cell.revision
        };
        let remote = remote.clone();
        tokio::spawn(async move {
            let result = decode(
                remote
                    .request_within(Duration::from_secs(3), |req_id| {
                        ClientFrame::GetCodexConfig { req_id }
                    })
                    .await,
            );
            let mut cell = remote.codex_config.lock().unwrap();
            if cell.revision == revision {
                cell.value = Some(result);
            }
            cell.pending = false;
            remote.dirty.store(true, Ordering::Relaxed);
        });
    }

    pub(crate) async fn set_codex_config(
        &self,
        config: CodexConfig,
    ) -> Result<CodexConfig, String> {
        match self {
            Self::Local(_) => {
                cm_core::config::write_codex(&config).map_err(|e| e.to_string())?;
                cm_core::config::read_codex().map_err(|e| e.to_string())
            }
            Self::Remote(remote) => {
                remote.codex_config.lock().unwrap().revision += 1;
                let result = decode(
                    remote
                        .request_within(Duration::from_secs(3), |req_id| {
                            ClientFrame::SetCodexConfig { req_id, config }
                        })
                        .await,
                );
                remote.codex_config.lock().unwrap().value = Some(result.clone());
                result
            }
        }
    }
}

fn decode(frame: Option<ServerFrame>) -> Result<CodexConfig, String> {
    match frame {
        Some(ServerFrame::CodexConfig {
            config: Some(config),
            error: None,
            ..
        }) => Ok(config),
        Some(ServerFrame::CodexConfig {
            error: Some(error), ..
        }) => Err(error),
        _ => Err("Codex settings unavailable; connect or upgrade this host's miao-server".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn host_settings_round_trip_and_old_servers_time_out() {
        let transport = Transport::LocalSocket(std::path::PathBuf::from("/unused.sock"));
        let (remote, _shared, mut requests) =
            RemoteBackend::build(&transport, HostId("test-host".into()));
        let backend = Backend::Remote(remote.clone());
        backend.poll_codex_config();
        let read = requests.recv().await.unwrap();
        assert!(matches!(read.frame, ClientFrame::GetCodexConfig { .. }));
        let config = CodexConfig {
            mode: cm_core::agents::codex::CodexMode::AppServer,
            endpoint: "unix://~/control.sock".into(),
        };
        read.reply
            .send(ServerFrame::CodexConfig {
                req_id: read.req_id,
                config: Some(config.clone()),
                error: None,
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(backend.codex_config(), Some(Ok(config.clone())));
        let task = tokio::spawn(async move { backend.set_codex_config(config).await });
        let write = requests.recv().await.unwrap();
        let ClientFrame::SetCodexConfig { config, .. } = write.frame else {
            panic!("expected host write")
        };
        assert_eq!(config.endpoint, "unix://~/control.sock");
        write
            .reply
            .send(ServerFrame::CodexConfig {
                req_id: write.req_id,
                config: Some(config.clone()),
                error: None,
            })
            .unwrap();
        assert_eq!(task.await.unwrap(), Ok(config));

        tokio::time::advance(Duration::from_secs(3)).await;
        let backend = Backend::Remote(remote);
        backend.poll_codex_config();
        let ignored = requests.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert!(
            backend
                .codex_config()
                .unwrap()
                .unwrap_err()
                .contains("upgrade")
        );
        drop(ignored);
    }
}
