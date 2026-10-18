//! Offline compatibility check: real Codex, isolated home, no credentials or
//! inference requests. An MCP initialize gate holds a real lifecycle handler.
use super::*;
use std::process::Stdio;
use tokio::process::Command;

const WATCHDOG: Duration = Duration::from_secs(75);

async fn wait_for(mut ready: impl FnMut() -> bool, phase: &str) {
    tokio::time::timeout(WATCHDOG, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("waiting for {phase}"));
}

#[tokio::test]
#[ignore = "requires installed Codex and Node; isolated home and no inference API"]
async fn real_codex_serializes_recovery_after_disconnected_startup() {
    const TEST: &str = "agents::codex::app_server::recovery_live_tests::real_codex_serializes_recovery_after_disconnected_startup";
    if std::env::var_os("CM_TEST_CODEX_RECOVERY_LIVE").is_none() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path())
            .env("CODEX_HOME", root.path().join("codex"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("CM_TEST_CODEX_RECOVERY_LIVE", "1")
            .kill_on_drop(true)
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
    let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let home = root.join("codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
model = "test-model"
model_provider = "fixture"
thread_unload_delay_secs = 0
approval_policy = "never"
sandbox_mode = "workspace-write"
[features]
goals = true
[model_providers.fixture]
name = "Offline fixture"
base_url = "http://127.0.0.1:1/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
"#,
    )
    .unwrap();
    let mcp = root.join("blocked-mcp.cjs");
    std::fs::write(&mcp, r#"
const fs = require('node:fs');
const rl = require('node:readline').createInterface({ input: process.stdin });
const [ready, release] = process.argv.slice(2);
function reply(id, result) { process.stdout.write(JSON.stringify({jsonrpc:'2.0',id,result})+'\n'); }
rl.on('line', line => {
  const message = JSON.parse(line);
  if (message.method === 'initialize') {
    fs.writeFileSync(ready, 'ready');
    const timer = setInterval(() => {
      if (!fs.existsSync(release)) return;
      clearInterval(timer);
      reply(message.id, {protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'blocked',version:'1'}});
    }, 10);
  } else if (message.method === 'tools/list') reply(message.id, {tools:[]});
});
rl.on('close', () => process.exit(0));
"#).unwrap();
    let socket = root.join("server.sock");
    let spawn_server = |log: &str| {
        Command::new("codex")
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", socket.display()))
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join(log)).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let mut server = spawn_server("server.log");
    wait_for(|| socket.exists(), "isolated Codex socket").await;
    let config = CodexConfig {
        endpoint: format!("unix://{}", socket.display()),
        ..Default::default()
    };
    let mut observer = transport::Client::connect(&socket).await.unwrap();
    assert!(
        observer.ordered_thread_reads,
        "Codex does not advertise a supported protocol version"
    );
    let thread = observer
        .request("thread/start", json!({"cwd":root}))
        .await
        .unwrap()["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    observer.request("thread/goal/set", json!({"threadId":thread,"objective":"Keep the offline fixture resumable","status":"paused"})).await.unwrap();
    observer
        .request("thread/unsubscribe", json!({"threadId":thread}))
        .await
        .unwrap();
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let loaded = observer
                .request("thread/loaded/list", json!({}))
                .await
                .unwrap();
            if !loaded["data"].as_array().unwrap().contains(&json!(thread)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("waiting for initial thread unload");
    for known in [true, false] {
        let ready = root.join(format!("ready-{known}"));
        let release = root.join(format!("release-{known}"));
        let mut relay = Relay::start(&config, &root.join("relay.sock"))
            .await
            .unwrap();
        let mut tui = transport::Client::connect(&root.join("relay.sock"))
            .await
            .unwrap();
        let mut params = json!({"cwd":root,"config":{"mcp_servers.blocked":{
            "command":"node", "args":[mcp, ready, release], "required":true, "startup_timeout_sec":60
        }}});
        if known {
            params["threadId"] = json!(thread);
            params["excludeTurns"] = json!(true);
        }
        let mut requester = tokio::spawn(async move {
            tui.request(
                if known {
                    "thread/resume"
                } else {
                    "thread/start"
                },
                params,
            )
            .await
        });
        tokio::select! {
            _ = wait_for(|| ready.exists(), "MCP initialize gate") => {}
            result = &mut requester => panic!("lifecycle request ended before MCP became ready: {result:?}"),
        }
        requester.abort();
        let _ = requester.await;
        tokio::time::timeout(WATCHDOG, async {
            while !matches!(
                relay.events.recv().await,
                Some(monitor::Observation::Disconnected)
            ) {}
        })
        .await
        .expect("waiting for disconnected relay receipt");
        let paused = relay.pause_input(true).await;
        assert!(paused.is_err());
        // The inventory is deliberately independent of the blocked handler.
        let loaded = observer
            .request("thread/loaded/list", json!({}))
            .await
            .unwrap();
        assert!(
            loaded["data"].as_array().unwrap().is_empty(),
            "startup must still be blocked"
        );
        let mut row =
            LauncherState::for_test(AgentControl::Codex, crate::state::SessionStatus::Starting);
        row.session_id = Some(thread.clone());
        let cleanup = stop_after_quiescence(&config, &row, None, paused, true);
        tokio::pin!(cleanup);
        let early = tokio::time::timeout(Duration::from_millis(150), &mut cleanup).await;
        // Always release the fixture before any assertion can unwind.
        std::fs::write(&release, "release").unwrap();
        if known {
            assert!(
                early.is_err(),
                "cleanup overtook the real thread/resume handler: {early:?}"
            );
            tokio::time::timeout(WATCHDOG, &mut cleanup)
                .await
                .expect("waiting for ordered recovery")
                .unwrap();
        } else {
            assert!(
                matches!(early, Ok(Err(_))),
                "empty inventory must not settle an unknown creation"
            );
            // Model an upgrade that replaces the daemon on the same endpoint.
            // Keep the relay's lost creation unresolved across process exit.
            server.kill().await.unwrap();
            server.wait().await.unwrap();
            drop(observer);
            let _ = std::fs::remove_file(&socket);
            server = spawn_server("replacement.log");
            wait_for(|| socket.exists(), "replacement Codex socket").await;
            observer = transport::Client::connect(&socket).await.unwrap();
            assert!(
                loaded_threads(&mut observer).await.unwrap().is_empty(),
                "a replacement daemon must start without loaded runtimes"
            );
            for explicit_kill in [false, true] {
                stop_after_quiescence(
                    &config,
                    &row,
                    None,
                    relay.pause_input(true).await,
                    explicit_kill,
                )
                .await
                .expect("old daemon exit must allow both Restart and Kill cleanup");
            }
            let resumed = observer
                .request(
                    "thread/resume",
                    json!({"threadId":thread,"excludeTurns":true}),
                )
                .await
                .unwrap();
            assert_eq!(
                resumed["thread"]["id"], thread,
                "replacement must resume the saved conversation"
            );
            stop(&config, &row, None).await.unwrap();
            println!("Real Codex: daemon replacement, cleanup and saved-thread resume verified");
        }
        drop(relay);
        println!(
            "Real Codex: {} recovery ordering verified",
            if known {
                "known thread"
            } else {
                "unknown creation"
            }
        );
    }
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
