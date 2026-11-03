use super::*;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[tokio::test(start_paused = true)]
async fn persistence_failure_retries_without_canceling_the_adapter() {
    let (publisher, updates) = SessionPublisher::channel();
    let (commands, _control) = mpsc::unbounded_channel();
    let (ready, running) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let (writes, mut written) = mpsc::unbounded_channel();
    let mut row = LauncherState::for_test(AgentKind::Claude, SessionStatus::Idle);
    let mut observed = row.clone();
    observed.name = Some("retained observation".into());
    let task: SessionTask = Box::pin(async move {
        assert!(publisher.write(&observed).await.is_err());
        ready.send(()).unwrap();
        finished.await.unwrap();
        Ok(SessionExit { code: 0 })
    });
    let driver = tokio::spawn(async move {
        let mut attempt = 0;
        drive(
            task,
            updates,
            commands,
            &mut row,
            std::future::pending(),
            |state| {
                attempt += 1;
                writes.send((attempt, state.name.clone())).unwrap();
                if attempt == 1 {
                    anyhow::bail!("state store unavailable")
                }
                Ok(())
            },
        )
        .await
    });
    running.await.unwrap();
    assert!(!driver.is_finished());
    assert_eq!(written.recv().await.unwrap().0, 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let (attempt, name) = written.recv().await.unwrap();
    assert_eq!(attempt, 2);
    assert_eq!(name.as_deref(), Some("retained observation"));
    assert!(!driver.is_finished());
    finish.send(()).unwrap();
    assert_eq!(driver.await.unwrap().unwrap().code, 0);
}

#[tokio::test]
async fn stop_is_a_command_and_cleanup_failure_keeps_the_task_alive() {
    let (publisher, updates) = SessionPublisher::channel();
    let (commands, control) = mpsc::unbounded_channel();
    let retry = commands.clone();
    let (stop, stopped) = oneshot::channel();
    let (rejected, failed) = oneshot::channel();
    let mut row = LauncherState::for_test(AgentKind::Claude, SessionStatus::Active);
    let mut observed = row.clone();
    let task: SessionTask = Box::pin(async move {
        let mut control = SessionCommands(control);
        control.stop().await;
        observed.set_last_error(Some("cleanup can be retried".into()));
        publisher.write(&observed).await.unwrap();
        rejected.send(()).unwrap();
        control.stop().await;
        Ok(SessionExit { code: 0 })
    });
    let driver = tokio::spawn(async move {
        drive(
            task,
            updates,
            commands,
            &mut row,
            async {
                stopped.await.unwrap();
            },
            |_| Ok(()),
        )
        .await
    });
    stop.send(()).unwrap();
    failed.await.unwrap();
    assert!(
        !driver.is_finished(),
        "requesting stop must not drop the adapter"
    );
    retry.send(SessionCommand::Stop).unwrap();
    assert_eq!(driver.await.unwrap().unwrap().code, 0);
}
