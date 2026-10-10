use super::*;
use futures_util::{SinkExt, StreamExt};
use std::os::unix::fs::PermissionsExt;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn snapshot_restores_path_and_values_without_reinitializing_child_shells() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join("project-only");
    std::fs::write(&executable, "#!/bin/sh\nprintf project-tool").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let value = "quote'\n$(exit 49) `exit 50` \\";
    let vars = BTreeMap::from([
        ("PATH".into(), Some(bin.to_str().unwrap().into())),
        ("PROJECT_SECRET".into(), Some(value.into())),
        ("HTTP_PROXY".into(), Some("project-proxy".into())),
        ("REMOVED".into(), None),
    ]);
    let settings = persist(root.path(), &script(&vars)).unwrap();
    assert!(!settings.to_string().contains(value));
    let hook = settings["set"]["BASH_ENV"].as_str().unwrap();
    assert_eq!(
        std::fs::metadata(hook).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(Path::new(hook).parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let bash = find_in_path("bash").unwrap();
    let command = format!(
        "test \"$(project-only)\" = project-tool && test \"$PROJECT_SECRET\" = {} && test -z \"${{REMOVED+x}}\" && test \"$CODEX_THREAD_ID\" = runtime-thread && test \"$HTTP_PROXY\" = runtime-proxy && export PATH=deliberate-change && {} --noprofile --norc -c 'test \"$PATH\" = deliberate-change'",
        shell_quote(value),
        shell_quote(&bash.to_string_lossy())
    );
    for login in [false, true] {
        let output = std::process::Command::new(&bash)
            .arg(if login { "-lc" } else { "-c" })
            .arg(&command)
            .env_clear()
            .env("BASH_ENV", hook)
            .env("SHLVL", "1")
            .env("REMOVED", "profile-value")
            .env("CODEX_THREAD_ID", "runtime-thread")
            .env("CODEX_NETWORK_PROXY_ACTIVE", "1")
            .env("HTTP_PROXY", "runtime-proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "login={login}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn environment_records_do_not_turn_into_shell_code_or_capture_runtime_identity() {
    let vars = parse(b"PATH=/bin\0GOOD=two=equals\0CODEX_THREAD_ID=launcher\0BASH_ENV=untrusted\0BAD-NAME=x\0BASH_FUNC_f%%=() { :; }\0").unwrap();
    assert_eq!(
        vars,
        BTreeMap::from([
            ("PATH".into(), "/bin".into()),
            ("GOOD".into(), "two=equals".into())
        ])
    );
    assert!(parse(b"PATH=\xff\0").is_err());
    assert!(parse(b"unprinted-sensitive-output").is_err());
    assert!(
        !parse(b"unprinted-sensitive-output")
            .unwrap_err()
            .to_string()
            .contains("unprinted-sensitive-output")
    );
}

#[test]
fn zsh_snapshot_runs_once_for_login_and_nonlogin_commands() {
    let Some(zsh) = find_in_path("zsh") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let variables = BTreeMap::from([
        ("PATH".into(), Some("project-path".into())),
        ("PROJECT".into(), Some("captured".into())),
    ]);
    let settings = persist(root.path(), &script(&variables)).unwrap();
    for login in [false, true] {
        let output = std::process::Command::new(&zsh)
            .arg(if login { "-lc" } else { "-c" })
            .arg(format!("test \"$PATH\" = project-path && test \"$PROJECT\" = captured && test -z \"${{ZDOTDIR+x}}\" && export PATH=deliberate-change && {} -c 'test \"$PATH\" = deliberate-change'", shell_quote(&zsh.to_string_lossy())))
            .env_clear().env("ZDOTDIR", settings["set"]["ZDOTDIR"].as_str().unwrap())
            .output().unwrap();
        assert!(
            output.status.success(),
            "zsh login={login}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

async fn receive(socket: &mut transport::Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("waiting for relay frame")
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

async fn send(socket: &mut transport::Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn initialize(listener: &UnixListener) -> transport::Socket {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let request = receive(&mut socket).await;
    assert_eq!(request["method"], "initialize");
    send(&mut socket, json!({"id":request["id"], "result":{}})).await;
    assert_eq!(receive(&mut socket).await["method"], "initialized");
    socket
}

async fn config_read(listener: &UnixListener) {
    let mut socket = initialize(listener).await;
    let read = receive(&mut socket).await;
    assert_eq!(read["method"], "config/read");
    send(&mut socket, json!({"id":read["id"],"result":{"config":{}}})).await;
}

#[tokio::test]
async fn relay_applies_snapshots_to_start_resume_and_fork_and_preserves_other_config() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let other = cwd.join("other");
    std::fs::create_dir(&other).unwrap();
    let upstream = cwd.join("upstream.sock");
    let listener = UnixListener::bind(&upstream).unwrap();
    let config = super::super::CodexConfig {
        mode: super::super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", upstream.display()),
    };
    let mut env = Environments::new(&cwd, cwd.join("snapshots"));
    for project in [&cwd, &other] {
        let vars = BTreeMap::from([
            ("PATH".into(), Some(project.to_string_lossy().into_owned())),
            ("PRIVATE_MARKER".into(), Some("only-in-private-file".into())),
        ]);
        env.snapshots.insert(project.clone(), Some(vars));
    }

    let server = tokio::spawn(async move {
        drop(initialize(&listener).await); // preflight
        // New connection simulates the TUI reconnecting after a daemon restart.
        for connection in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            config_read(&listener).await;
            let start = receive(&mut socket).await;
            assert_eq!(
                start["params"]["config"]["shell_environment_policy"]["exclude"],
                json!(["PRIVATE_*"])
            );
            let policy = &start["params"]["config"]["shell_environment_policy"];
            assert_eq!(policy["set"]["PATH"], "wrong");
            let hook =
                std::fs::read_to_string(policy["set"]["BASH_ENV"].as_str().unwrap()).unwrap();
            assert!(hook.contains("export PATH='wrong'"));
            assert!(!hook.contains("only-in-private-file"));
            assert_eq!(
                start["params"]["config"]["features"]["shell_snapshot"],
                false
            );
            assert_eq!(start["params"]["config"]["features"]["futureFeature"], true);
            assert_eq!(start["params"]["config"]["futureSetting"], "preserved");
            assert!(!start.to_string().contains("only-in-private-file"));
            send(
                &mut socket,
                json!({"id":1,"result":{"thread":{"id":"selected"}}}),
            )
            .await;
            let helper = receive(&mut socket).await;
            assert_eq!(
                helper,
                json!({"id":2,"method":"thread/start","params":{"threadSource":"system","ephemeral":true}})
            );
            send(&mut socket, json!({"id":2,"result":{}})).await;
            let mut metadata = initialize(&listener).await;
            let read = receive(&mut metadata).await;
            assert_eq!(read["method"], "thread/read");
            send(
                &mut metadata,
                json!({"id":read["id"],"result":{"thread":{"cwd":other}}}),
            )
            .await;
            drop(metadata);
            config_read(&listener).await;
            let fork = receive(&mut socket).await;
            assert_eq!(
                fork["method"],
                if connection == 0 {
                    "thread/fork"
                } else {
                    "thread/resume"
                }
            );
            let hook = std::fs::read_to_string(
                fork["params"]["config"]["shell_environment_policy"]["set"]["BASH_ENV"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert!(hook.contains(&format!(
                "export PATH={}",
                shell_quote(other.to_str().unwrap())
            )));
            send(
                &mut socket,
                json!({"id":3,"result":{"thread":{"id":"other"}}}),
            )
            .await;
            assert_eq!(
                receive(&mut socket).await,
                json!({"id":4,"method":"thread/shellCommand","params":{"threadId":"other","command":"project-only | cat"}})
            );
            send(&mut socket, json!({"id":4,"result":{}})).await;
            socket.close(None).await.unwrap();
        }
    });
    let relay =
        super::super::Relay::start_with_environment(&config, &cwd.join("relay.sock"), Some(env))
            .await
            .unwrap();
    for connection in 0..2 {
        let mut client = transport::connect(&cwd.join("relay.sock")).await.unwrap();
        send(
            &mut client,
            json!({"id":1,"method":"thread/start","params":{"config":{
                "futureSetting":"preserved","features":{"futureFeature":true,"shell_snapshot":true},
                "shell_environment_policy.set.PATH":"wrong",
                "shell_environment_policy.exclude":["PRIVATE_*"]
            }}}),
        )
        .await;
        assert_eq!(receive(&mut client).await["id"], 1);
        send(&mut client, json!({"id":2,"method":"thread/start","params":{"threadSource":"system","ephemeral":true}})).await;
        receive(&mut client).await;
        send(&mut client, json!({"id":3,"method":if connection == 0 {"thread/fork"} else {"thread/resume"},"params":{"threadId":"other"}})).await;
        assert_eq!(receive(&mut client).await["id"], 3);
        send(&mut client, json!({"id":4,"method":"thread/shellCommand","params":{"threadId":"other","command":"project-only | cat"}})).await;
        receive(&mut client).await;
        assert!(client.next().await.unwrap().unwrap().is_close());
    }
    server.await.unwrap();
    drop(relay);
}

#[tokio::test]
async fn no_envrc_leaves_requests_unchanged() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let mut env = Environments::new(root.path(), root.path().join("snapshots"));
    let mut request =
        json!({"id":1,"method":"thread/start","params":{"config":{"allow_login_shell":true}}});
    let original = request.clone();
    env.request(&mut request, Path::new("unused.sock"), None)
        .await
        .unwrap();
    assert_eq!(request, original);
    assert!(!env.root.exists());
}

#[tokio::test]
async fn cleanup_cancels_pending_environment_lookup_before_forwarding_a_resume() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let upstream = root.path().join("upstream.sock");
    let listener = UnixListener::bind(&upstream).unwrap();
    let config = super::super::CodexConfig {
        mode: super::super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", upstream.display()),
    };
    let (ready, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        drop(initialize(&listener).await);
        let (stream, _) = listener.accept().await.unwrap();
        let mut main = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut metadata = initialize(&listener).await;
        assert_eq!(receive(&mut metadata).await["method"], "thread/read");
        ready.send(()).unwrap();
        // Never answer the metadata lookup. No lifecycle request should reach
        // this socket while miao is still preparing its environment.
        assert!(
            main.next()
                .await
                .is_none_or(|frame| frame.is_err() || frame.unwrap().is_close())
        );
    });
    let env = Environments::new(root.path(), root.path().join("snapshots"));
    let mut relay = super::super::Relay::start_with_environment(
        &config,
        &root.path().join("relay.sock"),
        Some(env),
    )
    .await
    .unwrap();
    let mut client = transport::connect(&root.path().join("relay.sock"))
        .await
        .unwrap();
    send(
        &mut client,
        json!({"id":9,"method":"thread/resume","params":{"threadId":"saved"}}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), received)
        .await
        .expect("waiting for metadata lookup")
        .unwrap();
    relay.pause_input(true).await.unwrap();
    let reply = receive(&mut client).await;
    assert_eq!(reply["id"], 9);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cleanup")
    );
    assert!(
        relay.events.try_recv().is_err(),
        "an unforwarded request must not enter monitor state"
    );
    drop(relay);
    server.await.unwrap();
}

#[tokio::test]
async fn capture_checks_approval_and_keeps_failed_environment_output_private() {
    const TEST: &str = "agents::codex::app_server::environment::tests::capture_checks_approval_and_keeps_failed_environment_output_private";
    if std::env::var_os("CM_TEST_DIRENV_CAPTURE").is_none() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("direnv");
        std::fs::write(&fake, "#!/bin/sh\ncase $1 in\nstatus) if [ -f denied ]; then printf '{\"state\":{\"foundRC\":{\"allowed\":1}}}'; else printf '{\"state\":{\"foundRC\":{\"allowed\":0}}}'; fi;;\nexec) if [ ! -f .envrc ]; then shift 2; exec \"$@\"; fi; if [ -f failed ]; then echo private-output >&2; exit 1; fi; shift 2; export CAPTURED_PROJECT=yes; unset REMOVED; exec \"$@\";;\nesac\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .env("XDG_STATE_HOME", root.path())
            .env("XDG_RUNTIME_DIR", root.path())
            .env("REMOVED", "parent")
            .env("UNRELATED_CREDENTIAL", "unchanged-launcher-secret")
            .env("CM_TEST_DIRENV_CAPTURE", "1")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join(".envrc"), "synthetic fixture").unwrap();
    std::fs::write(project.join("denied"), "").unwrap();
    let error = Environments::prepare(project.to_str().unwrap())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("direnv allow"));
    std::fs::remove_file(project.join("denied")).unwrap();
    std::fs::write(project.join("failed"), "").unwrap();
    let error = Environments::prepare(project.to_str().unwrap())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("capture failed"));
    assert!(!format!("{error:#}").contains("private-output"));
    std::fs::remove_file(project.join("failed")).unwrap();
    let mut environments = Environments::prepare(project.to_str().unwrap())
        .await
        .unwrap();
    let settings = environments.snapshot(&project).await.unwrap().unwrap();
    let script = script(&settings);
    assert!(!script.contains("unchanged-launcher-secret"));
    assert!(script.contains("export CAPTURED_PROJECT='yes'"));
    assert!(script.contains("unset REMOVED"));
    // Capture is once per project per launcher, including subsequent resumes.
    std::fs::write(project.join("failed"), "").unwrap();
    assert_eq!(
        environments.snapshot(&project).await.unwrap(),
        Some(settings)
    );
}

#[tokio::test]
#[ignore = "requires installed direnv; uses isolated HOME and XDG roots"]
async fn real_direnv_capture_recovers_changes_from_an_already_activated_project() {
    const TEST: &str = "agents::codex::app_server::environment::tests::real_direnv_capture_recovers_changes_from_an_already_activated_project";
    if std::env::var_os("CM_TEST_DIRENV_ACTIVATED").is_some() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(std::env::var("PROJECT").unwrap(), "activated");
        assert!(std::env::var_os("DIRENV_DIFF").is_some());
        let delta = capture(&cwd).await.unwrap().unwrap();
        assert_eq!(delta["PROJECT"].as_deref(), Some("activated"));
        assert_eq!(delta["REMOVED"], None);
        assert!(!delta.contains_key("UNCHANGED_LAUNCHER"));
        assert!(!delta.keys().any(|name| name.starts_with("DIRENV_")));
        return;
    }
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join(".envrc"),
        "export PROJECT=activated\nunset REMOVED\nPATH_add bin\n",
    )
    .unwrap();
    let clean = |cmd: &mut Command| {
        cmd.env_clear()
            .env("HOME", root.path())
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"));
    };
    let mut allow = Command::new("direnv");
    clean(&mut allow);
    assert!(
        allow
            .arg("allow")
            .arg(&project)
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    let mut child = Command::new("direnv");
    clean(&mut child);
    let output = tokio::time::timeout(
        Duration::from_secs(150),
        child
            .arg("exec")
            .arg(&project)
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .current_dir(&project)
            .env("CM_TEST_DIRENV_ACTIVATED", "1")
            .env("UNCHANGED_LAUNCHER", "unrelated")
            .env("REMOVED", "baseline")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("waiting for activated direnv capture child")
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
