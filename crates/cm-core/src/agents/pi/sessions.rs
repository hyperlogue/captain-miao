//! Pi's resume picker metadata follows SessionManager.listAll: header identity,
//! first user text and the latest session_info name, independent of tree position.

use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::common;
use crate::agent::{AgentControl, ResumeCandidate};

pub(super) fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let directories = if let Some(root) = env_path("PI_CODING_AGENT_SESSION_DIR") {
        vec![root]
    } else {
        let Some(agent_dir) = env_path("PI_CODING_AGENT_DIR")
            .or_else(|| dirs::home_dir().map(|home| home.join(".pi/agent")))
        else {
            return Ok(Vec::new());
        };
        common::read_subdirs(&agent_dir.join("sessions"))
    };
    let mut files = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_file()
                && let Ok(mtime) = metadata.modified()
            {
                files.push((path, mtime));
            }
        }
    }
    // Apply the limit to valid candidates, so a damaged newest file cannot
    // displace a usable conversation. Only those candidates' bodies are read.
    Ok(common::newest_first(files, usize::MAX)
        .into_iter()
        .filter_map(|(path, mtime)| read_candidate(&path, mtime))
        .take(limit)
        .collect())
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name).filter(|value| !value.is_empty())?;
    Some(PathBuf::from(crate::paths::expand_home(
        &value.to_string_lossy(),
        &crate::paths::host_home(),
    )))
}

// Image and tool-result records can be huge. Skip oversized records in chunks
// while continuing to find later names; never allocate an entire transcript.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

fn next_record(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<Option<Value>> {
    loop {
        line.clear();
        let count = reader.take(MAX_RECORD_BYTES).read_until(b'\n', line)?;
        if count == 0 {
            return Ok(None);
        }
        if count as u64 == MAX_RECORD_BYTES && line.last() != Some(&b'\n') {
            reader.skip_until(b'\n')?;
            continue;
        }
        if let Ok(record) = serde_json::from_slice(line) {
            return Ok(Some(record));
        }
    }
}

fn read_candidate(path: &Path, mtime: SystemTime) -> Option<ResumeCandidate> {
    let mut reader = BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = Vec::new();
    let header = next_record(&mut reader, &mut line).ok()??;
    if header["type"] != "session" {
        return None;
    }
    let session_id = header["id"].as_str().filter(|id| !id.trim().is_empty())?;
    let cwd = header["cwd"]
        .as_str()
        .filter(|cwd| !cwd.trim().is_empty())?;
    let mut candidate = ResumeCandidate {
        agent: AgentControl::Pi,
        session_id: session_id.into(),
        cwd: cwd.into(),
        first_prompt: None,
        custom_title: None,
        git_branch: None,
        mtime,
    };
    while let Some(record) = next_record(&mut reader, &mut line).ok()? {
        match record["type"].as_str() {
            Some("session_info") => {
                // Pi names are session-wide: the latest entry wins, including
                // an explicit clear, regardless of the active conversation leaf.
                candidate.custom_title = record["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
            }
            Some("message")
                if candidate.first_prompt.is_none() && record["message"]["role"] == "user" =>
            {
                let content = &record["message"]["content"];
                let text = if let Some(text) = content.as_str() {
                    text.to_owned()
                } else if let Some(parts) = content.as_array() {
                    parts
                        .iter()
                        .filter(|part| part["type"] == "text")
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    continue;
                };
                let text = super::super::collapse_whitespace(&text);
                candidate.first_prompt = (!text.is_empty()).then_some(text);
            }
            _ => {}
        }
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use crate::agent::AgentControl;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn write_session(root: &Path, name: &str, body: &str, seconds: u64) {
        std::fs::create_dir_all(root).unwrap();
        let file = std::fs::File::create(root.join(name)).unwrap();
        use std::io::Write;
        (&file).write_all(body.as_bytes()).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .unwrap();
    }

    #[test]
    fn picker_discovers_pi_sessions_and_honors_storage_overrides() {
        const TEST: &str = "agents::pi::sessions::tests::picker_discovers_pi_sessions_and_honors_storage_overrides";
        let Ok(case) = std::env::var("CM_TEST_PI_SESSIONS") else {
            for case in ["grouped", "flat"] {
                let root = tempfile::tempdir().unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", TEST, "--nocapture"])
                    .env("CM_TEST_PI_SESSIONS", case)
                    .env("PI_CODING_AGENT_DIR", root.path())
                    .env("XDG_STATE_HOME", root.path().join("state"))
                    .env("XDG_CONFIG_HOME", root.path().join("config"))
                    .env("XDG_DATA_HOME", root.path().join("data"))
                    .env_remove("PI_CODING_AGENT_SESSION_DIR");
                if case == "flat" {
                    command.env("PI_CODING_AGENT_SESSION_DIR", root.path().join("custom"));
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };

        let agent = AgentControl::Pi;
        assert!(agent.list_resumable(10).unwrap().is_empty());
        let base = std::path::PathBuf::from(std::env::var_os("PI_CODING_AGENT_DIR").unwrap());
        let root = if case == "flat" {
            base.join("custom")
        } else {
            base.join("sessions/--unrelated-encoded-name--")
        };
        write_session(
            &root,
            "older.jsonl",
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"older\",\"cwd\":\"/project/essays\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"First   draft\\nplease\"}}\n",
                "{\"type\":\"session_info\",\"name\":\"Old title\"}\n",
                "{\"type\":\"message\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"Alternate branch\"}}\n",
                "{\"type\":\"session_info\",\"name\":\"Final essay\"}\n"
            ),
            1,
        );
        // Large image/tool records must not hide a later rename or require an
        // unbounded allocation. Partial writes must not hide earlier metadata.
        let large = "x".repeat(2 * 1024 * 1024);
        write_session(
            &root,
            "newer.jsonl",
            &format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"newer\",\"cwd\":\"/project/site\"}}\n\
                 {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"image\",\"data\":\"{large}\"}}]}}}}\n\
                 {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"Write\"}},{{\"type\":\"text\",\"text\":\" HTML\"}}]}}}}\n\
                 {{\"type\":\"session_info\",\"name\":\"Discarded\"}}\n\
                 {{\"type\":\"session_info\"}}\n\
                 {{\"type\":\"message\""
            ),
            2,
        );
        write_session(
            &root,
            "broken.jsonl",
            "{\"type\":\"session\",\"id\":\"\"}\n",
            3,
        );
        write_session(&root, "unrelated.json", "{}\n", 4);
        std::fs::create_dir(root.join("artifacts.jsonl")).unwrap();
        write_session(&root.join("artifacts.jsonl"), "nested.jsonl", "{}\n", 5);
        if case == "flat" {
            // A custom flat store replaces the default grouped store.
            write_session(
                &base.join("sessions/--other--"),
                "hidden.jsonl",
                "{\"type\":\"session\",\"id\":\"hidden\",\"cwd\":\"/other\"}\n",
                6,
            );
        }
        let found = agent.list_resumable(10).unwrap();
        assert_eq!(
            found.len(),
            2,
            "Pi conversations must appear in miao's picker"
        );
        assert_eq!(found[0].session_id, "newer");
        assert_eq!(found[0].cwd, "/project/site");
        assert_eq!(found[0].first_prompt.as_deref(), Some("Write HTML"));
        assert_eq!(found[0].custom_title, None);
        assert_eq!(found[1].agent, agent);
        assert_eq!(found[1].session_id, "older");
        assert_eq!(found[1].first_prompt.as_deref(), Some("First draft please"));
        assert_eq!(found[1].custom_title.as_deref(), Some("Final essay"));
        assert_eq!(agent.list_resumable(1).unwrap()[0].session_id, "newer");
        assert!(agent.list_resumable(0).unwrap().is_empty());
    }
}
