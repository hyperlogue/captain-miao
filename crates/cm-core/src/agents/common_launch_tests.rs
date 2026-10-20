//! Launch through the shared command builder with private process environments.
use super::agent_command;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use tokio::process::Command;

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

async fn isolated(test: &str, real_direnv: bool) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("CM_TEST_PARENT_DIRENV") {
        return Some(PathBuf::from(root));
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    symlink("/bin/sh", bin.join("sh")).unwrap();
    if real_direnv {
        for name in ["direnv", "bash"] {
            symlink(
                super::super::find_in_path(name)
                    .unwrap_or_else(|| panic!("{name} must be installed")),
                bin.join(name),
            )
            .unwrap();
        }
    } else {
        executable(
            &bin.join("direnv"),
            r#"#!/bin/sh
case "$1" in
    status)
        while IFS= read -r line; do printf '%s\n' "$line"; done < "$CM_TEST_PARENT_DIRENV/status.json"
        printf 'private-status-stderr\n' >&2
        [ ! -f "$CM_TEST_PARENT_DIRENV/status-failed" ]
        ;;
    exec)
        cd "$2" || exit 1
        . "$CM_TEST_PARENT_DIRENV/work/.envrc"
        shift 2
        exec "$@"
        ;;
    *) exit 1 ;;
esac
"#,
        );
    }
    // Real approval files and all global state belong to this child, never
    // the developer's HOME or the concurrently running agent sessions.
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env("CM_TEST_PARENT_DIRENV", &root)
        .env("PATH", &bin)
        .env("HOME", &home);
    for kind in ["CONFIG", "DATA", "STATE", "CACHE"] {
        child.env(format!("XDG_{kind}_HOME"), root.join(kind));
    }
    child.env("XDG_RUNTIME_DIR", root.join("run"));
    if real_direnv {
        child.arg("--ignored");
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        child.kill_on_drop(true).output(),
    )
    .await
    .expect("waiting for isolated direnv launch and approval checks")
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

fn project(root: &Path) -> PathBuf {
    let project = root.join("work/nested/project ' with spaces");
    std::fs::create_dir_all(&project).unwrap();
    let bin = root.join("project-bin");
    std::fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("r3"),
        "#!/bin/sh\nprintf '%s\\n' \"$PROJECT_VALUE\"\n",
    );
    std::fs::write(
        root.join("work/.envrc"),
        "export PATH=\"$CM_TEST_PARENT_DIRENV/project-bin:$PATH\"\nexport PROJECT_VALUE=parent\n",
    )
    .unwrap();
    project
}

async fn project_value(project: &Path) -> Vec<u8> {
    let output = agent_command("sh", project.to_str().unwrap(), None)
        .unwrap()
        .args(["-c", "r3"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "project binary must be available in {}: {}",
        project.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn launch_error(project: &Path) -> String {
    let error = agent_command("sh", project.to_str().unwrap(), None).unwrap_err();
    let error = format!("{error:#}");
    assert!(!error.contains("private-status"), "{error}");
    error
}

#[tokio::test]
async fn parent_direnv_launch_checks_approval_without_exposing_output() {
    const TEST: &str = "agents::common::launch_tests::parent_direnv_launch_checks_approval_without_exposing_output";
    let Some(root) = isolated(TEST, false).await else {
        return;
    };
    let project = project(&root);
    let approved = "{\"state\":{\"foundRC\":{\"allowed\":0}}}\n";
    std::fs::write(root.join("status.json"), approved).unwrap();
    assert_eq!(project_value(&project).await, b"parent\n");

    for allowed in [1, 2] {
        std::fs::write(
            root.join("status.json"),
            format!(
                "{{\"state\":{{\"foundRC\":{{\"allowed\":{allowed},\"path\":\"private-status-path\"}}}}}}\n"
            ),
        )
        .unwrap();
        let error = launch_error(&project);
        assert!(error.contains("direnv allow"), "{error}");
    }
    for malformed in [
        "private-status-invalid-json\n",
        "{}\n",
        "{\"state\":{\"foundRC\":null}}\n",
        "{\"state\":{\"foundRC\":{\"allowed\":\"private-status-value\"}}}\n",
        "{\"state\":{\"foundRC\":{\"allowed\":3}}}\n",
    ] {
        std::fs::write(root.join("status.json"), malformed).unwrap();
        assert!(launch_error(&project).contains("approval"));
    }
    // A nonzero status must not become permission, even with approved JSON.
    std::fs::write(root.join("status.json"), approved).unwrap();
    std::fs::write(root.join("status-failed"), "").unwrap();
    assert!(launch_error(&project).contains("approval"));
    std::fs::remove_file(root.join("status-failed")).unwrap();

    let direnv = root.join("bin/direnv");
    std::fs::set_permissions(&direnv, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(launch_error(&project).contains("approval"));
    std::fs::remove_file(&direnv).unwrap();
    assert!(launch_error(&project).contains("direnv is not on PATH"));

    std::fs::remove_file(root.join("work/.envrc")).unwrap();
    let output = agent_command("sh", project.to_str().unwrap(), None)
        .unwrap()
        .args(["-c", "printf ordinary"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"ordinary");
}

async fn direnv(action: &str, path: &Path) {
    let output = Command::new("direnv")
        .arg(action)
        .arg(path)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{action} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires installed direnv and bash; uses isolated HOME and XDG roots"]
async fn real_direnv_launch_uses_nearest_approved_parent() {
    const TEST: &str =
        "agents::common::launch_tests::real_direnv_launch_uses_nearest_approved_parent";
    let Some(root) = isolated(TEST, true).await else {
        return;
    };
    let project = project(&root);
    assert!(launch_error(&project).contains("direnv allow"));
    direnv("allow", &root.join("work")).await;
    assert_eq!(project_value(&project).await, b"parent\n");

    // Follow the real working directory, not the symlink's unrelated parents.
    let link = root.join("linked project");
    symlink(&project, &link).unwrap();
    assert_eq!(project_value(&link).await, b"parent\n");

    // A nearer blocked file must shadow the approved parent without executing
    // either the blocked file or the requested agent.
    let local = project.join(".envrc");
    let script = "printf touched > \"$CM_TEST_PARENT_DIRENV/executed\"\nexport PATH=\"$CM_TEST_PARENT_DIRENV/project-bin:$PATH\"\nexport PROJECT_VALUE=local\n";
    std::fs::write(&local, script).unwrap();
    assert!(launch_error(&project).contains("direnv allow"));
    assert!(!root.join("executed").exists());
    direnv("allow", &project).await;
    assert_eq!(project_value(&project).await, b"local\n");
    std::fs::remove_file(root.join("executed")).unwrap();

    std::fs::write(&local, format!("{script}# changed after approval\n")).unwrap();
    assert!(launch_error(&project).contains("direnv allow"));
    assert!(!root.join("executed").exists());
    direnv("deny", &project).await;
    assert!(launch_error(&project).contains("denied"));
    std::fs::remove_file(&local).unwrap();
    assert_eq!(project_value(&project).await, b"parent\n");
}
