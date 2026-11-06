use super::*;
use tokio::io::AsyncReadExt;

async fn wake_preserves_responsive_daemon(reply_delay: Duration, supports_vitals: bool) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let (_backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("wake-test".into()),
    );
    let (events, mut wake) = tokio::sync::watch::channel(recovery::Event::Network);
    let serving = serve(
        client,
        None,
        MirrorCells {
            mirror: &shared.mirror,
            presumed_dead: &shared.presumed_dead,
            presumed_attached: &shared.presumed_attached,
            dirty: &shared.dirty,
            mirrored: &shared.mirrored,
            server_version: &shared.server_version,
        },
        &mut requests,
    );
    let connection =
        recovery::watch_connection(serving, &mut wake, &shared.recovery_active, || {
            recovery::daemon_responsive(&shared.health_requests, &shared.next_req_id)
        });
    let conversation = async {
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Hello { .. })
        ));
        write_frame(
            &mut peer,
            &ServerFrame::Welcome {
                server_version: "test".into(),
                protocol: PROTOCOL_VERSION,
                host: "wake-test".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Subscribe)
        ));
        events.send_replace(recovery::Event::Wake);
        loop {
            let request = read_frame::<_, ClientFrame>(&mut peer)
                .await
                .unwrap()
                .expect("wake must preserve the existing daemon channel");
            assert!(shared.recovery_active.load(Ordering::Relaxed));
            let response = match request {
                ClientFrame::GetVitals { req_id } if supports_vitals => ServerFrame::Vitals {
                    req_id,
                    vitals: HostVitals::default(),
                },
                // Compatible older daemons ignore GetVitals but answer v4 RPCs.
                ClientFrame::GetVitals { .. } => continue,
                ClientFrame::ListRecentDirs { req_id } => ServerFrame::RecentDirs {
                    req_id,
                    cwds: Vec::new(),
                },
                other => panic!("unexpected wake check: {other:?}"),
            };
            tokio::time::sleep(reply_delay).await;
            // Network notifications can follow wake while Wi-Fi/VPN resumes.
            events.send_replace(recovery::Event::Network);
            write_frame(&mut peer, &response).await.unwrap();
            break;
        }
        while shared.recovery_active.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = connection => panic!("wake discarded a responsive daemon connection"),
        result = tokio::time::timeout(Duration::from_secs(20), conversation) => {
            result.expect("waiting for wake probe reply and recovery completion");
        }
    }
}

#[tokio::test]
async fn wake_preserves_a_connection_that_answers_after_one_second() {
    wake_preserves_responsive_daemon(Duration::from_millis(1500), true).await;
}

#[tokio::test]
async fn wake_preserves_a_compatible_daemon_without_vitals() {
    wake_preserves_responsive_daemon(Duration::ZERO, false).await;
}

#[tokio::test(start_paused = true)]
async fn a_silent_handshake_has_a_deadline() {
    let (client, _peer) = UnixStream::pair().unwrap();
    let (_backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("test".into()),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(16),
        serve(
            client,
            None,
            MirrorCells {
                mirror: &shared.mirror,
                presumed_dead: &shared.presumed_dead,
                presumed_attached: &shared.presumed_attached,
                dirty: &shared.dirty,
                mirrored: &shared.mirrored,
                server_version: &shared.server_version,
            },
            &mut requests,
        ),
    )
    .await;
    assert!(
        matches!(outcome, Ok(ServeOutcome::HandshakeFailed(_))),
        "silent handshake must expire: {outcome:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn exited_tunnel_is_reaped_while_protocol_stays_connected() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut tunnel = detached("true")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = tunnel.stdout.take().unwrap();
    let pid = tunnel.id().unwrap();
    let (_backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("review".into()),
    );
    write_frame(
        &mut peer,
        &ServerFrame::Welcome {
            server_version: "test".into(),
            protocol: PROTOCOL_VERSION,
            host: "review".into(),
        },
    )
    .await
    .unwrap();
    let connection = serve(
        client,
        Some(tunnel),
        MirrorCells {
            mirror: &shared.mirror,
            presumed_dead: &shared.presumed_dead,
            presumed_attached: &shared.presumed_attached,
            dirty: &shared.dirty,
            mirrored: &shared.mirrored,
            server_version: &shared.server_version,
        },
        &mut requests,
    );
    tokio::pin!(connection);
    let reaped = async {
        output.read_to_end(&mut Vec::new()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    };
    tokio::select! {
        outcome = &mut connection => panic!("child exit ended the protocol: {outcome:?}"),
        result = reaped => result.expect("connected backend retains an unreaped tunnel process"),
    }
}

#[tokio::test]
async fn dropping_backend_cancels_a_silent_handshake() {
    let dir = scratch_home("review-handshake");
    let sock = dir.join("control.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let backend = RemoteBackend::connect(Transport::LocalSocket(sock), HostId("review".into()));
    let (mut peer, _) = listener.accept().await.unwrap();
    assert!(matches!(
        read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
        Some(ClientFrame::Hello { .. })
    ));
    drop(backend);
    let closed = tokio::time::timeout(Duration::from_secs(2), peer.read_u8()).await;
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        matches!(closed, Ok(Err(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof),
        "removed backend keeps its connection alive while awaiting Welcome"
    );
}

#[tokio::test]
async fn expired_queued_mutation_is_not_sent_after_connecting() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let (backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("review".into()),
    );
    let reply = backend
        .request_within(Duration::from_millis(10), |req_id| {
            ClientFrame::ForgetRecentDir {
                req_id,
                cwd: "~/example".into(),
            }
        })
        .await;
    assert!(
        reply.is_none(),
        "the caller timed out before a connection existed"
    );
    let connection = serve(
        client,
        None,
        MirrorCells {
            mirror: &shared.mirror,
            presumed_dead: &shared.presumed_dead,
            presumed_attached: &shared.presumed_attached,
            dirty: &shared.dirty,
            mirrored: &shared.mirrored,
            server_version: &shared.server_version,
        },
        &mut requests,
    );
    tokio::pin!(connection);
    let peer_check = async {
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Hello { .. })
        ));
        write_frame(
            &mut peer,
            &ServerFrame::Welcome {
                server_version: "test".into(),
                protocol: PROTOCOL_VERSION,
                host: "review".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Subscribe)
        ));
        tokio::time::timeout(
            Duration::from_millis(100),
            read_frame::<_, ClientFrame>(&mut peer),
        )
        .await
    };
    let received = tokio::select! {
        outcome = &mut connection => panic!("unexpected connection outcome: {outcome:?}"),
        received = peer_check => received,
    };
    assert!(
        received.is_err(),
        "expired mutation still sent to the server: {received:?}"
    );
}
