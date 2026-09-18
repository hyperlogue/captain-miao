use super::*;
use std::os::unix::fs::PermissionsExt;

/// Environment overrides stay inside child processes, including when the Rust
/// suite runs in parallel. Build the real launch command without spawning an
/// agent or loading any user config.
#[test]
fn launch_preserves_config_precedence() {
    if let Some(root) = std::env::var_os("CM_TEST_OPENCODE_CONFIG_ROOT") {
        check_launch(&PathBuf::from(root));
        return;
    }
    let root = std::env::temp_dir().join(format!("cm-opencode-config-{}", std::process::id()));
    crate::state::create_dir_all_private(&root).unwrap();
    for (path, text) in [
        ("bin/opencode", "#!/bin/sh\nexit 0\n"),
        (
            "global/opencode/opencode.json",
            r#"{"model":"global","permission":"allow"}"#,
        ),
        (
            "global/opencode/plugins/user.js",
            "export default async () => ({});",
        ),
        (
            "project/opencode.json",
            r#"{"model":"project","permission":"deny"}"#,
        ),
        (
            "custom/opencode.json",
            r#"{"model":"custom","permission":"ask"}"#,
        ),
        ("custom/plugins/user.js", "export default async () => ({});"),
        // Old releases mirrored globals here. The fix must not reload them.
        (
            "state/captain-miao/opencode-config/opencode.json",
            r#"{"model":"stale","permission":"allow"}"#,
        ),
    ] {
        crate::state::write_private(&root.join(path), text).unwrap();
    }
    std::fs::set_permissions(
        root.join("bin/opencode"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    for case in ["default", "custom", "relative"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "agents::opencode::config_tests::launch_preserves_config_precedence",
                "--nocapture",
            ])
            .current_dir(root.join("project"))
            .env("CM_TEST_OPENCODE_CONFIG_ROOT", &root)
            .env("CM_TEST_OPENCODE_CONFIG_CASE", case)
            .env("XDG_CONFIG_HOME", root.join("global"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("PATH", root.join("bin"))
            .env_remove("OPENCODE_CONFIG_DIR");
        match case {
            "custom" => {
                child.env("OPENCODE_CONFIG_DIR", root.join("custom"));
            }
            "relative" => {
                child.env("OPENCODE_CONFIG_DIR", "../custom");
            }
            _ => {}
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let default = std::fs::read(root.join("default-path")).unwrap();
    let custom = std::fs::read(root.join("custom-path")).unwrap();
    assert_ne!(
        default, custom,
        "concurrent profiles must not rewrite each other's overlays"
    );
    assert_eq!(custom, std::fs::read(root.join("relative-path")).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

fn check_launch(root: &Path) {
    let case = std::env::var("CM_TEST_OPENCODE_CONFIG_CASE").unwrap();
    let settings = root.join("settings.json");
    crate::state::write_private(&settings, &build_hooks_settings("unused")).unwrap();
    let cmd = build_launch_command(
        root.join("project").to_str().unwrap(),
        &root.join("launcher.sock"),
        &settings,
        &[],
        None,
    )
    .unwrap();
    let config = cmd
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == "OPENCODE_CONFIG_DIR")
        .and_then(|(_, value)| value)
        .map(PathBuf::from)
        .unwrap();
    assert!(config.join("plugins/captain-miao.js").is_file());
    let mut effective: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("global/opencode/opencode.json")).unwrap(),
    )
    .unwrap();
    // These fixtures use scalar values: later files win under OpenCode's
    // documented global -> project -> OPENCODE_CONFIG_DIR load order.
    for path in [
        root.join("project/opencode.json"),
        config.join("opencode.json"),
    ] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let next: Value = serde_json::from_str(&text).unwrap();
            effective
                .as_object_mut()
                .unwrap()
                .extend(next.as_object().unwrap().clone());
        }
    }
    if case == "default" {
        assert_eq!(effective["model"], "project");
        assert_eq!(effective["permission"], "deny");
        assert!(
            !config.join("plugins/user.js").exists(),
            "global plugins already load from the global config directory"
        );
    } else {
        assert_eq!(effective["model"], "custom");
        assert_eq!(effective["permission"], "ask");
        assert!(config.join("plugins/user.js").is_file());
    }
    assert!(
        !root
            .join("global/opencode/plugins/captain-miao.js")
            .exists()
    );
    assert!(!root.join("custom/plugins/captain-miao.js").exists());
    crate::state::write_private(&root.join(format!("{case}-path")), config.to_str().unwrap())
        .unwrap();
}
