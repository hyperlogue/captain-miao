//! Compatibility check against an installed Codex and real direnv. The model
//! is a loopback Responses fixture; no account, credentials or inference API.
use super::environment::*;
use super::{CodexConfig, Relay, transport};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const WATCHDOG: Duration = Duration::from_secs(75);

struct Client {
    socket: transport::Socket,
    events: VecDeque<Value>,
    id: u64,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let mut client = Self {
            socket: transport::connect(path).await.unwrap(),
            events: VecDeque::new(),
            id: 0,
        };
        client.request("initialize", json!({"clientInfo":{"name":"miao_direnv_test","version":"1"},"capabilities":{"experimentalApi":true}})).await;
        client
            .socket
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await
            .unwrap();
        client
    }

    async fn receive(&mut self) -> Value {
        loop {
            let frame = tokio::time::timeout(WATCHDOG, self.socket.next())
                .await
                .expect("waiting for real Codex frame")
                .unwrap()
                .unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.socket
            .send(Message::Text(
                json!({"id":self.id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap_or_else(|error| panic!("sending {method}: {error}"));
        loop {
            let response = self.receive().await;
            if response["id"] == self.id && response.get("method").is_none() {
                assert!(response.get("error").is_none(), "{method}: {response}");
                return response["result"].clone();
            }
            self.events.push_back(response);
        }
    }

    async fn completed_command(&mut self, thread: &str) {
        let mut command_seen = false;
        loop {
            let event = match self.events.pop_front() {
                Some(event) => event,
                None => self.receive().await,
            };
            if event["params"]["threadId"] != thread {
                continue;
            }
            if event["method"] == "item/completed"
                && event["params"]["item"]["type"] == "commandExecution"
            {
                let item = &event["params"]["item"];
                assert_eq!(
                    item["exitCode"], 0,
                    "project binary/environment check failed: {item}"
                );
                assert!(
                    item["aggregatedOutput"]
                        .as_str()
                        .unwrap_or("")
                        .contains("verified"),
                    "missing environment assertion result: {item}"
                );
                command_seen = true;
            }
            if event["method"] == "turn/completed" {
                assert!(
                    command_seen,
                    "turn completed without executing the requested command: {event}"
                );
                break;
            }
        }
    }
}

async fn model_fixture() -> (u16, mpsc::Sender<Vec<Value>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, mut rx) = mpsc::channel::<Vec<Value>>(8);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let post = line.starts_with("POST ");
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).await.unwrap();
            let response = if post {
                let events = rx.recv().await.unwrap();
                let body: String = events
                    .into_iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
            };
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
    });
    (port, tx, task)
}

async fn enqueue_tool(tx: &mpsc::Sender<Vec<Value>>, command: &str, workdir: &Path, login: bool) {
    let mut args =
        json!({"cmd":command,"workdir":workdir,"yield_time_ms":1000,"max_output_tokens":1000});
    if !login {
        args["login"] = json!(false);
    }
    for item in [
        json!({"type":"function_call","name":"exec_command","call_id":"probe-call","arguments":args.to_string()}),
        json!({"type":"message","role":"assistant","id":"probe-message","content":[{"type":"output_text","text":"Done."}]}),
    ] {
        tx.send(vec![
            json!({"type":"response.created","response":{"id":"probe-response"}}),
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":"probe-response","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}),
        ]).await.unwrap();
    }
}

async fn start_server(root: &Path) -> tokio::process::Child {
    let _ = std::fs::remove_file(root.join("server.sock"));
    let mut server = Command::new("codex")
        .args(["app-server", "--listen"])
        .arg(format!("unix://{}", root.join("server.sock").display()))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(root.join("server.log")).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                server.try_wait().unwrap().is_none(),
                "isolated Codex exited during startup"
            );
            if root.join("server.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("waiting for isolated Codex socket");
    server
}

fn assert_marker_absent(root: &Path, marker: &[u8]) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_marker_absent(&path, marker);
        } else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes.windows(marker.len()).any(|window| window == marker),
                "private environment value leaked into Codex file {}",
                path.file_name().unwrap().to_string_lossy()
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires installed Codex, direnv, and a loopback listener; uses isolated XDG roots"]
async fn real_codex_loads_direnv_for_agent_and_manual_commands() {
    const TEST: &str = "agents::codex::app_server::environment_live_tests::real_codex_loads_direnv_for_agent_and_manual_commands";
    if std::env::var_os("CM_TEST_LIVE_DIRENV").is_none() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path())
            .env("CODEX_HOME", root.path().join("codex"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("CM_TEST_LIVE_DIRENV", "1")
            .env("REMOVED", "daemon-value")
            .env("UNRELATED_CREDENTIAL", "unchanged-launcher-credential-7d51")
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
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let codex_home = root.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    let (port, model, model_task) = model_fixture().await;
    std::fs::write(codex_home.join("config.toml"), format!("model = \"test-model\"\nmodel_provider = \"fixture\"\napproval_policy = \"never\"\nsandbox_mode = \"workspace-write\"\n[model_providers.fixture]\nname = \"Loopback test fixture\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nwire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\n[shell_environment_policy]\nignore_default_excludes = false\n[shell_environment_policy.filters]\n\"aws_*\" = \"exclude\"\n[shell_environment_policy.set]\nCONFIG_WINS = \"explicit\"\n")).unwrap();
    for project in ["alpha", "beta"] {
        let cwd = root.join(project);
        std::fs::create_dir_all(cwd.join("bin")).unwrap();
        std::fs::create_dir(cwd.join("subdir")).unwrap();
        let executable = cwd.join("bin/project-only");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf {project}\n")).unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(cwd.join(".envrc"), format!("export PROJECT={project}\nexport PRIVATE_MARKER=private-direnv-value-9b28\nexport PROJECT_SECRET=excluded-direnv-secret-3c42\nexport AWS_CREDENTIAL=excluded-direnv-aws-6a12\nexport CONFIG_WINS=direnv\nunset REMOVED\nPATH_add bin\n")).unwrap();
        assert!(
            Command::new("direnv")
                .arg("allow")
                .arg(&cwd)
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
    }
    let mut server = start_server(&root).await;
    let config = CodexConfig {
        mode: super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", root.join("server.sock").display()),
    };
    let environments = Environments::prepare(root.join("alpha").to_str().unwrap())
        .await
        .unwrap();
    let mut relay =
        Relay::start_with_environment(&config, &root.join("relay.sock"), Some(environments))
            .await
            .unwrap();
    let mut client = Client::connect(&root.join("relay.sock")).await;
    let mut threads = Vec::new();
    for project in ["alpha", "beta"] {
        let result = client
            .request("thread/start", json!({"cwd":root.join(project)}))
            .await;
        threads.push(result["thread"]["id"].as_str().unwrap().to_owned());
        // Once captured, neither execution path may evaluate direnv again.
        std::fs::write(root.join(project).join(".envrc"), "exit 47\n").unwrap();
    }
    for (index, project) in [(0, "alpha"), (1, "beta"), (0, "alpha")] {
        let command = format!(
            "test \"$(project-only | cat)\" = {project} && test \"$PROJECT\" = {project} && test -n \"$PRIVATE_MARKER\" && test -z \"${{REMOVED+x}}${{PROJECT_SECRET+x}}${{AWS_CREDENTIAL+x}}\" && test \"$CONFIG_WINS\" = explicit && test \"$UNRELATED_CREDENTIAL\" = unchanged-launcher-credential-7d51 && printf verified"
        );
        client
            .request(
                "thread/shellCommand",
                json!({"threadId":threads[index],"command":command}),
            )
            .await;
        client.completed_command(&threads[index]).await;
        for login in [true, false] {
            enqueue_tool(&model, &command, &root.join(project).join("subdir"), login).await;
            client.request("turn/start", json!({"threadId":threads[index],"input":[{"type":"text","text":"Run the fixture command."}]})).await;
            client.completed_command(&threads[index]).await;
        }
    }
    let restricted = client
        .request(
            "thread/start",
            json!({"cwd":root.join("alpha"),"config":{
                "shell_environment_policy.inherit":"none",
                "shell_environment_policy.include_only":["ALLOWED"],
                "shell_environment_policy.set.ALLOWED":"explicit"
            }}),
        )
        .await;
    let restricted = restricted["thread"]["id"].as_str().unwrap();
    client.request("thread/shellCommand", json!({"threadId":restricted,"command":"test \"$ALLOWED\" = explicit && test -z \"${PROJECT+x}${PRIVATE_MARKER+x}${UNRELATED_CREDENTIAL+x}\" && printf verified"})).await;
    client.completed_command(restricted).await;
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    drop(client);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            relay.events.recv().await.unwrap(),
            super::monitor::Observation::Disconnected
        ) {}
    })
    .await
    .expect("waiting for the relay to observe daemon shutdown");
    // The same launcher reuses its snapshot on cold resume after a daemon restart.
    server = start_server(&root).await;
    let mut client = Client::connect(&root.join("relay.sock")).await;
    client
        .request("thread/resume", json!({"threadId":threads[0]}))
        .await;
    client.request("thread/shellCommand", json!({"threadId":threads[0],"command":"test \"$(project-only)\" = alpha && printf verified"})).await;
    client.completed_command(&threads[0]).await;
    // Let the relay settle the last lifecycle reply before releasing its lease.
    relay.cleanup_environment().await;
    let snapshot_root = root.join("state/captain-miao/codex-environments");
    for marker in [
        b"unchanged-launcher-credential-7d51".as_slice(),
        b"excluded-direnv-secret-3c42",
        b"excluded-direnv-aws-6a12",
    ] {
        assert_marker_absent(&snapshot_root, marker);
    }
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    drop(client);
    drop(relay);
    // A restarted daemon has no loaded references; the next sweep removes the
    // old launcher's private files without recapturing its now-broken .envrc.
    server = start_server(&root).await;
    super::environment_store::collect(&snapshot_root, &root.join("server.sock"))
        .await
        .unwrap();
    assert_eq!(std::fs::read_dir(&snapshot_root).unwrap().count(), 0);
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    model_task.abort();
    assert_marker_absent(&codex_home, b"private-direnv-value-9b28");
    println!(
        "Real Codex: 11 environment checks passed; exclusions, delta-only capture, and restart cleanup verified"
    );
}
