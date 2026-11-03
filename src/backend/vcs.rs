//! Short-lived SSH agent access for Git, independent of the host's shared master.

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

// A session channel makes sshd create its agent socket. Keeping stdin open
// holds that socket for exactly one RPC, including network Git preparation.
const AGENT_SESSION_SCRIPT: &str = "echo \"$HOME\"; echo \"$SSH_AUTH_SOCK\"; exec cat >/dev/null";

impl RemoteBackend {
    pub(crate) async fn prepare_push_within(
        &self,
        within: Duration,
        cwd: String,
    ) -> Result<Box<cm_core::vcs::PreparedVcsPush>, String> {
        let plan = prepared_reply(self.prepare_vcs_within(within, cwd, true).await)?;
        plan.into_prepared_push().map(Box::new)
    }

    pub(crate) async fn prepare_pull_within(
        &self,
        within: Duration,
        cwd: String,
    ) -> Result<Box<cm_core::vcs::PreparedVcsPull>, String> {
        let plan = prepared_reply(self.prepare_vcs_within(within, cwd, false).await)?;
        plan.into_prepared_pull().map(Box::new)
    }

    pub(crate) async fn execute_push_within(
        &self,
        within: Duration,
        cwd: String,
        prepared: Box<cm_core::vcs::PreparedVcsPush>,
    ) -> Result<String, String> {
        command_reply(
            self.run_vcs_within(within, cwd, Box::new((*prepared).into()))
                .await,
        )
    }

    pub(crate) async fn execute_pull_within(
        &self,
        within: Duration,
        cwd: String,
        prepared: Box<cm_core::vcs::PreparedVcsPull>,
    ) -> Result<String, String> {
        command_reply(
            self.run_vcs_within(within, cwd, Box::new((*prepared).into()))
                .await,
        )
    }

    async fn prepare_vcs_within(
        &self,
        within: Duration,
        cwd: String,
        push: bool,
    ) -> Result<Option<ServerFrame>, String> {
        // Push preparation only inspects local refs; pull preparation fetches.
        self.vcs_request_within(within, !push, |req_id, ssh_auth_sock| {
            ClientFrame::PrepareVcs {
                req_id,
                cwd,
                push,
                ssh_auth_sock,
            }
        })
        .await
    }

    async fn run_vcs_within(
        &self,
        within: Duration,
        cwd: String,
        plan: Box<cm_core::protocol::VcsPlan>,
    ) -> Result<Option<ServerFrame>, String> {
        self.vcs_request_within(within, true, |req_id, ssh_auth_sock| ClientFrame::RunVcs {
            req_id,
            cwd,
            plan,
            ssh_auth_sock,
        })
        .await
    }

    async fn vcs_request_within(
        &self,
        within: Duration,
        network: bool,
        make: impl FnOnce(u64, Option<String>) -> ClientFrame,
    ) -> Result<Option<ServerFrame>, String> {
        let Some(target) = self.attach_target.as_deref().filter(|_| network) else {
            return Ok(self.request_within(within, |id| make(id, None)).await);
        };
        // Resolve each request so ssh_config edits take effect without a host
        // reconnect. Disabled forwarding needs no additional network hop.
        let options = ssh::session_options(&self.ssh_options, false);
        let started = Instant::now();
        let session = tokio::time::timeout(within, async {
            if !agent_forwarding_may_be_enabled(target, &options).await? {
                return Ok::<_, String>(None);
            }
            let mut command = detached("ssh");
            command
                .args(&options)
                .arg(target)
                .arg(login_shell_safe(AGENT_SESSION_SCRIPT));
            let mut session = AgentSession::start(command).await?;
            if session.socket.is_none() {
                // Like an ordinary SSH login, no forwarded agent is not an
                // authentication error: Git may use HTTPS or host credentials.
                // Do not hold an unused connection through the Git request.
                session.close().await;
                Ok(None)
            } else {
                Ok(Some(session))
            }
        })
        .await
        .unwrap_or_else(|_| Err("Git's SSH agent connection timed out".into()))
        .inspect_err(|error| self.log.error(error))?;
        // Configuration, connection setup and the RPC share one deadline.
        // Cancellation drops and kills whichever SSH child is active.
        let remaining = within.saturating_sub(started.elapsed());
        let Some(mut session) = session else {
            return Ok(self.request_within(remaining, |id| make(id, None)).await);
        };
        self.log.info("SSH agent forwarding opened for Git");
        let reply = if remaining.is_zero() {
            None
        } else {
            self.request_during_agent_session(remaining, &mut session, make)
                .await
        };
        session.close().await;
        self.log.info("SSH agent forwarding closed after Git");
        Ok(reply)
    }

    async fn request_during_agent_session(
        &self,
        within: Duration,
        session: &mut AgentSession,
        make: impl FnOnce(u64, Option<String>) -> ClientFrame,
    ) -> Option<ServerFrame> {
        // The RPC still goes to the persistent daemon: confirmed plans retain
        // its instance identity, and its environment never acquires the agent.
        let socket = session.socket.clone();
        tokio::select! {
            reply = self.request_within(within, |id| make(id, socket)) => reply,
            _ = session.child.wait() => {
                self.log.error("Git's SSH agent connection ended before the host replied");
                None
            }
        }
    }
}

fn prepared_reply(
    reply: Result<Option<ServerFrame>, String>,
) -> Result<Box<cm_core::protocol::VcsPlan>, String> {
    match reply? {
        Some(ServerFrame::VcsPrepared {
            plan: Some(plan),
            error: None,
            ..
        }) => Ok(plan),
        Some(ServerFrame::VcsPrepared {
            error: Some(error), ..
        }) => Err(error),
        _ => {
            Err("host did not prepare the command; check its connection and server version".into())
        }
    }
}

fn command_reply(reply: Result<Option<ServerFrame>, String>) -> Result<String, String> {
    match reply? {
        Some(ServerFrame::VcsCommandDone {
            ok: true, message, ..
        }) => Ok(message),
        Some(ServerFrame::VcsCommandDone {
            ok: false, message, ..
        }) => Err(message),
        _ => Err("no answer from the host; outcome unknown — verify before retrying".into()),
    }
}

/// Ask OpenSSH, rather than parsing ssh_config ourselves: Include, Match,
/// IdentityAgent and ForwardAgent socket paths must keep SSH's own semantics.
/// A socket path in `ssh -G` hides the boolean (even with -a), so only "no"
/// proves forwarding is disabled. The actual session supplies the final answer.
async fn agent_forwarding_may_be_enabled(target: &str, options: &[String]) -> Result<bool, String> {
    let child = detached("ssh")
        .arg("-G")
        .args(options)
        .arg(target)
        .arg(login_shell_safe(AGENT_SESSION_SCRIPT))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not resolve Git's SSH configuration: {error}"))?;
    let (status, stdout, stderr) =
        tokio::time::timeout(MUX_CONTROL_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| "Resolving Git's SSH configuration timed out".to_string())?
            .map_err(|error| format!("Could not resolve Git's SSH configuration: {error}"))?;
    if !status.success() {
        return Err(format!(
            "Could not resolve Git's SSH configuration: {}",
            stderr.trim()
        ));
    }
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("forwardagent "))
        .filter(|value| !value.is_empty())
        .map(|value| value != "no")
        .ok_or_else(|| "SSH did not report its agent forwarding setting".into())
}

struct AgentSession {
    child: tokio::process::Child,
    // Child::wait closes child.stdin. Keep this separately while monitoring it.
    stdin: Option<tokio::process::ChildStdin>,
    socket: Option<String>,
}

impl AgentSession {
    async fn start(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Could not start Git's SSH agent connection".to_string())?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout.take(4096));
        let mut home = String::new();
        let mut socket = String::new();
        let read = tokio::time::timeout(MUX_CONTROL_TIMEOUT, async {
            reader.read_line(&mut home).await?;
            reader.read_line(&mut socket).await
        })
        .await;
        if !matches!(read, Ok(Ok(n)) if n > 0 && socket.ends_with('\n')) {
            return Err("SSH did not report Git's forwarded agent before the deadline".into());
        }
        let home = home.trim_end_matches(['\r', '\n']);
        let socket = socket.trim_end_matches(['\r', '\n']);
        if !Path::new(home).is_absolute()
            || (!socket.is_empty() && !Path::new(socket).is_absolute())
        {
            return Err("SSH reported an invalid home or agent socket path".into());
        }
        Ok(Self {
            child,
            stdin,
            socket: (!socket.is_empty()).then(|| cm_core::paths::collapse_home(socket, home)),
        })
    }

    async fn close(&mut self) {
        drop(self.stdin.take());
        if tokio::time::timeout(Duration::from_secs(1), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(
        options: Vec<String>,
    ) -> (Arc<RemoteBackend>, mpsc::UnboundedReceiver<PendingRequest>) {
        let (backend, _, requests) = RemoteBackend::build(
            &Transport::Ssh {
                target: "example.invalid".into(),
                local_sock: PathBuf::from("/tmp/cm-git-test.sock"),
                options,
                forwards: vec![],
                clipboard: false,
            },
            HostId("git-test".into()),
        );
        (backend, requests)
    }

    fn agent_command(socket: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", AGENT_SESSION_SCRIPT])
            .env("HOME", "/path/to/repo")
            .env("SSH_AUTH_SOCK", socket);
        command
    }

    #[tokio::test]
    async fn preparation_only_opens_ssh_when_network_git_needs_forwarding() {
        // Push preparation must skip even SSH config evaluation (this file
        // deliberately does not exist). Pull with forwarding off must skip
        // connecting: example.invalid cannot resolve.
        for (push, options) in [
            (true, vec!["-F".into(), "/nonexistent/cm-ssh-config".into()]),
            (
                false,
                vec!["-F".into(), "/dev/null".into(), "-oForwardAgent=no".into()],
            ),
        ] {
            let (backend, mut requests) = backend(options);
            let (reply, ()) = tokio::join!(
                backend.prepare_vcs_within(Duration::from_secs(2), "~/project".into(), push),
                async {
                    let request = tokio::time::timeout(Duration::from_secs(3), requests.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(matches!(
                        request.frame,
                        ClientFrame::PrepareVcs {
                            ssh_auth_sock: None,
                            ..
                        }
                    ));
                    request
                        .reply
                        .send(ServerFrame::VcsPrepared {
                            req_id: request.req_id,
                            plan: None,
                            error: Some("test reply".into()),
                        })
                        .unwrap();
                }
            );
            assert!(matches!(reply, Ok(Some(ServerFrame::VcsPrepared { .. }))));
        }
    }

    #[tokio::test]
    async fn invalid_ssh_configuration_does_not_submit_git() {
        let (backend, mut requests) =
            backend(vec!["-F".into(), "/nonexistent/cm-ssh-config".into()]);
        let result = backend
            .prepare_vcs_within(Duration::from_secs(2), "~/project".into(), false)
            .await;
        assert!(
            result
                .unwrap_err()
                .contains("Could not resolve Git's SSH configuration")
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn git_requests_keep_their_own_live_agent_until_the_reply() {
        let (backend, mut requests) = backend(vec![]);
        for socket in ["/path/to/repo/agent-first.sock", "/tmp/agent-second.sock"] {
            let mut session = AgentSession::start(agent_command(socket)).await.unwrap();
            let expected = cm_core::paths::collapse_home(socket, "/path/to/repo");
            let (reply, ()) = tokio::join!(
                backend.request_during_agent_session(
                    Duration::from_secs(1),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                ),
                async {
                    let request = requests.recv().await.unwrap();
                    let ClientFrame::PrepareVcs { ssh_auth_sock, .. } = request.frame else {
                        panic!("expected Git preparation");
                    };
                    assert_eq!(ssh_auth_sock, Some(expected));
                    // Polling Child::wait must leave the session's stdin open.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    assert!(!request.reply.is_closed());
                    request
                        .reply
                        .send(ServerFrame::VcsPrepared {
                            req_id: request.req_id,
                            plan: None,
                            error: Some("test reply".into()),
                        })
                        .unwrap();
                }
            );
            assert!(reply.is_some());
            assert!(session.child.try_wait().unwrap().is_none());
            session.close().await;
            assert!(session.child.try_wait().unwrap().unwrap().success());
        }
    }

    #[tokio::test]
    async fn timeout_and_connection_loss_release_the_agent_session() {
        let (backend, mut requests) = backend(vec![]);
        for disconnect in [false, true] {
            let mut session = AgentSession::start(agent_command("/tmp/agent.sock"))
                .await
                .unwrap();
            let (reply, request) = tokio::join!(
                backend.request_during_agent_session(
                    Duration::from_millis(50),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                ),
                async {
                    let request = requests.recv().await.unwrap();
                    if disconnect {
                        drop(request);
                        None
                    } else {
                        Some(request)
                    }
                }
            );
            assert!(reply.is_none());
            if let Some(request) = request {
                assert!(request.reply.is_closed());
            }
            session.close().await;
            assert!(session.child.try_wait().unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn missing_agent_preserves_host_authentication() {
        let mut session = AgentSession::start(agent_command("")).await.unwrap();
        assert!(session.socket.is_none());
        session.close().await;
        assert!(session.child.try_wait().unwrap().unwrap().success());
        assert!(
            AgentSession::start(agent_command("relative/socket"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancelling_a_git_request_kills_its_agent_connection() {
        let (backend, mut requests) = backend(vec![]);
        let (pid_tx, pid_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut session = AgentSession::start(agent_command("/tmp/agent.sock"))
                .await
                .unwrap();
            pid_tx.send(session.child.id().unwrap()).unwrap();
            backend
                .request_during_agent_session(
                    Duration::from_secs(30),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                )
                .await
        });
        let pid = pid_rx.await.unwrap();
        let request = requests.recv().await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(request.reply.is_closed());
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn work_tabs_and_git_follow_ssh_agent_configuration() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        // Exercise OpenSSH's Include and Host matching, not our own parser.
        let included = root.path().join("included");
        std::fs::write(&config, format!("Include {}\nHost *\n ControlMaster auto\n ControlPersist 600\n ControlPath /tmp/unwanted-master\n", included.display())).unwrap();
        for (setting, override_flags, expected) in [
            (None, vec![], "no"),
            (Some("yes"), vec![], "yes"),
            (Some("no"), vec![], "no"),
            (
                Some("/tmp/cm-selected-agent.sock"),
                vec![],
                "/tmp/cm-selected-agent.sock",
            ),
            (Some("yes"), vec!["-Ca"], "no"),
            (Some("no"), vec!["-CA"], "yes"),
            (Some("yes"), vec!["-oForwardAgent=no"], "no"),
        ] {
            std::fs::write(
                &included,
                setting
                    .map(|value| format!("Host example.invalid\n ForwardAgent {value}\n"))
                    .unwrap_or_default(),
            )
            .unwrap();
            let mut options = vec!["-F".into(), config.to_string_lossy().into_owned()];
            options.extend(override_flags.into_iter().map(str::to_owned));
            let git = ssh::session_options(&options, false);
            assert_eq!(
                agent_forwarding_may_be_enabled("example.invalid", &git)
                    .await
                    .unwrap(),
                expected != "no"
            );
            let ShellPlan::Spawn { argv: work } = Backend::Remote(backend(options.clone()).0)
                .shell_plan("/work/project", None)
                .unwrap()
            else {
                panic!("expected SSH work tab")
            };
            let master = ssh_common_opts(Path::new("/tmp/cm-master.sock"), &options);
            for (kind, argv, forward) in [
                ("work tab", work[1..].to_vec(), expected),
                (
                    "Git",
                    [git, vec!["example.invalid".into()]].concat(),
                    expected,
                ),
                (
                    "master",
                    [master, vec!["example.invalid".into()]].concat(),
                    "no",
                ),
            ] {
                let output = std::process::Command::new("ssh")
                    .arg("-G")
                    .args(argv)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{kind} config evaluation failed");
                let config = String::from_utf8(output.stdout).unwrap();
                // OpenSSH dumps a configured agent path even when -a has
                // disabled forwarding. Its boolean output is reliable only
                // when no path is configured; the final -a still wins.
                let reported = if kind == "master" && setting.is_some_and(|s| s.starts_with('/')) {
                    setting.unwrap()
                } else {
                    forward
                };
                assert!(
                    config
                        .lines()
                        .any(|line| line == format!("forwardagent {reported}")),
                    "{kind} ignored forwarding policy {forward}"
                );
                if kind != "master" {
                    assert!(
                        !config.lines().any(|line| line.starts_with("controlpath ")),
                        "{kind} can join another master"
                    );
                    assert!(config.lines().any(|line| line == "controlmaster false"));
                    assert!(config.lines().any(|line| line == "controlpersist no"));
                    let tty = if kind == "work tab" { "true" } else { "false" };
                    assert!(
                        config
                            .lines()
                            .any(|line| line == format!("requesttty {tty}"))
                    );
                }
            }
        }
    }

    #[test]
    fn ssh_configuration_keeps_git_connections_fresh_and_other_connections_unforwarded() {
        // Exercise OpenSSH's actual precedence, including grouped short flags
        // and arguments that look like flags. No network connection is made.
        let options: Vec<String> = [
            "-vCAfMnNt",
            "-S",
            "/tmp/shared.sock",
            "-oControlPersist=600",
            "-oForwardAgent=no",
            "-o",
            "ForkAfterAuthentication=yes",
            "-o",
            "RemoteCommand=unwanted-command",
            "-oStdinNull=yes",
            "-oSessionType=none",
            "-oLocalForward=3000 localhost:3000",
            "-i",
            "-f",
            "-p2222",
            "-J",
            "jump.example.invalid",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let config = |opts: Vec<String>| {
            let output = std::process::Command::new("ssh")
                .args(["-G", "-F", "/dev/null"])
                .args(opts)
                .arg("example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let fresh_options = ssh::session_options(&options, false);
        assert!(fresh_options.windows(2).any(|pair| pair == ["-i", "-f"]));
        assert!(fresh_options.contains(&"-vCA".into()));
        let fresh = config(fresh_options);
        for expected in [
            "forwardagent yes",
            "controlmaster false",
            "controlpersist no",
            "forkafterauthentication no",
            "stdinnull no",
            "sessiontype default",
            "requesttty false",
            "clearallforwardings yes",
            "port 2222",
            "proxyjump jump.example.invalid",
        ] {
            assert!(
                fresh.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
        assert!(!fresh.lines().any(|line| line.starts_with("controlpath ")));
        assert!(!fresh.lines().any(|line| line.starts_with("remotecommand ")));
        assert!(!fresh.lines().any(|line| line.starts_with("localforward ")));
        let normal = config(ssh_common_opts(
            Path::new("/tmp/cm-control.sock"),
            &["-A".into(), "-oForwardAgent=yes".into()],
        ));
        assert!(normal.lines().any(|line| line == "forwardagent no"));
    }
}
