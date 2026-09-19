//! Shared transport for Pi and omp's generated extensions. Each adapter owns
//! its event table, extra payload fields and Rust-side status interpretation.
//! The extension only captures facts and forwards them to the launcher.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::{common, synth_home};
use crate::state::HookEvent;

pub(super) struct Extension {
    pub agent: &'static str,
    pub events: &'static [(&'static str, HookEvent)],
    pub extra_fields: &'static [(&'static str, &'static str)],
}

impl Extension {
    /// Shared per agent, since the socket arrives through CAPTAIN_MIAO_SOCK.
    /// Pi's loader requires `.ts`; a per-launch file would evade runtime cleanup.
    pub fn path(&self) -> PathBuf {
        crate::state::state_dir().join(format!("{}-extension.ts", self.agent))
    }

    pub fn command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        let source = std::fs::read_to_string(settings_path)
            .with_context(|| format!("reading {} hook extension", self.agent))?;
        let extension = self.path();
        let parent = extension
            .parent()
            .expect("extension lives in the state directory");
        crate::state::create_dir_all_private(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        synth_home::write_if_changed(&extension, &source)?;

        let mut cmd = common::agent_command(self.agent, cwd, shim_dir)?;
        cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
        // Both agents interpret trailing positionals as prompts. The working
        // directory belongs on the process, never in the forwarded argv.
        cmd.arg("-e").arg(extension).args(extra_args);
        Ok(cmd)
    }

    /// Plain JavaScript accepted by both Pi's TypeScript loader and omp's Bun.
    /// Only our executable and static adapter definitions are interpolated.
    pub fn source(&self, miao_exe: &str) -> String {
        let exe = serde_json::to_string(miao_exe).expect("a string is valid JSON");
        let agent = self.agent;
        let table = self
            .events
            .iter()
            .map(|(native, forwarded)| format!("  [\"{native}\", \"{}\"],\n", forwarded.as_kebab()))
            .collect::<String>();
        let extra = self
            .extra_fields
            .iter()
            .map(|(name, value)| format!("      {name}: {value},\n"))
            .collect::<String>();
        format!(
            r#"// captain-miao's {agent} session forwarder — GENERATED. Edits are overwritten.
// Loaded with `{agent} -e`. The launcher socket arrives as $CAPTAIN_MIAO_SOCK.
// Event interpretation belongs to captain-miao; this extension only forwards facts.
import {{ spawn }} from "node:child_process";

const MIAO = {exe};
const FORWARD = [
{table}];

export default function (pi) {{
  for (const [name, forwarded] of FORWARD) {{
    // An unavailable event must not prevent the others from being registered.
    try {{
      pi.on(name, (event, ctx) => send(forwarded, pi, event, ctx));
    }} catch {{}}
  }}
}}

function send(forwarded, pi, event, ctx) {{
  let body = "{{}}";
  try {{
    body = JSON.stringify({{
      session_id: ctx?.sessionManager?.getSessionId?.(),
      session_title: pi?.getSessionName?.(),
      cwd: ctx?.cwd,
      tool_name: event?.toolName,
      prompt: event?.prompt,
      is_error: event?.isError,
{extra}      // Round fractional estimates; an absent estimate serializes as null.
      context_tokens: Math.round(ctx?.getContextUsage?.()?.tokens),
      model: ctx?.model?.id,
    }});
  }} catch {{
    // A throwing getter must not cost the event, which is carried in argv.
  }}
  // Resolve without a value, even on failure: handlers may return patches.
  // Awaiting the child also preserves the host's order of awaited events.
  return new Promise((resolve) => {{
    let child;
    try {{
      child = spawn(MIAO, ["hook", "--agent", "{agent}", forwarded], {{
        stdio: ["pipe", "ignore", "ignore"],
      }});
    }} catch {{
      resolve();
      return;
    }}
    child.on("error", () => resolve());
    child.on("close", () => resolve());
    child.stdin.on("error", () => {{}});
    child.stdin.end(body);
  }});
}}
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::{LauncherState, SessionStatus};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn generated_extensions_preserve_metadata_and_distinct_turn_outcomes() {
        for agent in [AgentControl::Pi, AgentControl::Omp] {
            let source = agent.hooks_settings_json("/unused.sock");
            let messages = super::super::forwarder_test::run(
                &source,
                &format!("{}_extension", agent.cli_subcommand()),
            );
            assert_eq!(messages.len(), 4);
            let mut state = LauncherState::for_test(agent, SessionStatus::Idle);
            for ((event, body), expected) in messages.into_iter().zip([
                HookEvent::PromptSubmit,
                HookEvent::PostToolUseFailure,
                HookEvent::PostToolUse,
                HookEvent::Stop,
            ]) {
                let msg = agent.parse_hook_payload(event, &body).unwrap();
                assert_eq!(msg.event, expected);
                assert_eq!(msg.session_id.as_deref(), Some("root"));
                assert_eq!(msg.cwd.as_deref(), Some("/project"));
                assert_eq!(msg.session_title.as_deref(), Some("Current title"));
                assert_eq!(msg.context_tokens, Some(1235));
                assert_eq!(msg.model.as_deref(), Some("test-model"));
                if expected == HookEvent::PostToolUseFailure {
                    assert_eq!(msg.tool_name.as_deref(), Some("bash"));
                }
                agent.dispatch_hook(&mut state, msg);
                assert_eq!(
                    state.status,
                    if expected == HookEvent::Stop {
                        SessionStatus::Idle
                    } else {
                        SessionStatus::Active
                    }
                );
            }
            assert_eq!(state.last_prompt.as_deref(), Some("Do the work"));
        }
    }

    #[test]
    fn launch_installs_extension_and_preserves_arguments() {
        const TEST: &str =
            "agents::pi_extension::tests::launch_installs_extension_and_preserves_arguments";
        if std::env::var_os("CM_TEST_PI_EXTENSION").is_none() {
            let root = std::env::temp_dir().join(format!("miao-extension-{}", std::process::id()));
            crate::state::create_dir_all_private(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("CM_TEST_PI_EXTENSION", "1")
                .env("XDG_STATE_HOME", &root)
                .output()
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // A real executable available on both supported platforms; no Pi or
        // omp installation is needed to exercise command construction.
        let extension = Extension {
            agent: "sh",
            events: &[],
            extra_fields: &[],
        };
        let root = crate::state::state_dir();
        crate::state::create_dir_all_private(&root).unwrap();
        let settings = root.join("settings.json");
        let socket = root.join("launcher.sock");
        let source = extension.source("/path/to/miao");
        std::fs::write(&settings, &source).unwrap();
        let extra = ["--resume".into(), "saved-session".into()];
        let build = || {
            extension
                .command(&root.to_string_lossy(), &socket, &settings, &extra, None)
                .unwrap()
        };
        let command = build();
        let command = command.as_std();
        let path = extension.path();
        assert_eq!(path.file_name().unwrap(), "sh-extension.ts");
        assert_eq!(command.get_current_dir(), Some(root.as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-e"),
                path.as_os_str(),
                std::ffi::OsStr::new("--resume"),
                std::ffi::OsStr::new("saved-session"),
            ]
        );
        assert!(command.get_envs().any(|(key, value)| key == "CAPTAIN_MIAO_SOCK" && value == Some(socket.as_os_str())));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let _ = build();
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            metadata.ino(),
            "unchanged extensions must not be replaced"
        );
        std::fs::write(&settings, "// updated extension\n").unwrap();
        let _ = build();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "// updated extension\n"
        );
    }
}
