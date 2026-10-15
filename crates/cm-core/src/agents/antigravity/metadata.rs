//! Prompt logs are append-only; titles live in a separate SQLite store.
//! The launcher seeds prompts from the log's ends, then reads only new bytes.
//! The host shares one cached title reader across its sessions, including
//! remote subscribers.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::agent::TranscriptStats;

/// Committed log position. Partial JSON lines are retried on the next write;
/// inode changes and truncation discard the old conversation's accumulators.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptCursor {
    offset: u64,
    device: u64,
    inode: u64,
    modified: Option<SystemTime>,
}

#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    content: String,
}

pub(super) fn user_prompt(line: &[u8]) -> Option<String> {
    // Tool output dominates large logs. Do not deserialize its strings.
    if !line
        .windows(b"\"USER_INPUT\"".len())
        .any(|w| w == b"\"USER_INPUT\"")
    {
        return None;
    }
    let step: Step = serde_json::from_slice(line).ok()?;
    if step.r#type != "USER_INPUT" {
        return None;
    }
    let (_, rest) = step.content.split_once("<USER_REQUEST>")?;
    let (prompt, _) = rest.split_once("</USER_REQUEST>")?;
    let prompt = super::super::collapse_whitespace(prompt);
    (!prompt.is_empty()).then_some(prompt)
}

const PROMPT_READ_BLOCK: usize = 8 * 1024;

/// Stop the head read at the first real prompt, then search backward for the
/// latest one. Typical resumes read two small blocks regardless of history
/// size. A long tool-only suffix may require more backward blocks; there is
/// no fixed cutoff that could silently substitute an older prompt.
///
/// Read only the statted length so concurrent appends remain for the cursor.
/// The returned offset excludes any incomplete final JSONL record.
fn read_prompt_ends(file: &mut (impl Read + Seek), len: u64) -> io::Result<(TranscriptStats, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut stats = TranscriptStats::default();
    let mut head_end = 0;
    {
        let mut reader = BufReader::with_capacity(PROMPT_READ_BLOCK, (&mut *file).take(len));
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 || !line.ends_with(b"\n") {
                return Ok((stats, head_end));
            }
            head_end += n as u64;
            if let Some(prompt) = user_prompt(&line) {
                stats.first_prompt = Some(prompt.clone());
                stats.last_prompt = Some(prompt);
                break;
            }
        }
    }

    let mut block = [0; PROMPT_READ_BLOCK];
    let mut position = len;
    let mut committed_end = None;
    let mut reversed_line = Vec::new();
    while position > head_end {
        let size = (position - head_end).min(block.len() as u64) as usize;
        position -= size as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut block[..size])?;
        for (i, &byte) in block[..size].iter().enumerate().rev() {
            if byte == b'\n' {
                if let Some(end) = committed_end {
                    reversed_line.reverse();
                    if let Some(prompt) = user_prompt(&reversed_line) {
                        stats.last_prompt = Some(prompt);
                        return Ok((stats, end));
                    }
                    reversed_line.clear();
                } else {
                    // Discard the unfinished suffix after this last newline.
                    committed_end = Some(position + i as u64 + 1);
                }
            } else if committed_end.is_some() {
                reversed_line.push(byte);
            }
        }
    }
    // The first line after the opening prompt has no preceding delimiter in
    // the searched range: head_end is already just past that delimiter.
    reversed_line.reverse();
    if let Some(prompt) = user_prompt(&reversed_line) {
        stats.last_prompt = Some(prompt);
    }
    Ok((stats, committed_end.unwrap_or(head_end)))
}

pub fn read_transcript_stats(path: &Path, prior: Option<&TranscriptStats>) -> TranscriptStats {
    let mut stats = prior.cloned().unwrap_or_default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return stats;
    };
    let Ok(meta) = file.metadata() else {
        return stats;
    };
    let cursor = stats.antigravity_cursor.as_ref();
    let reset = cursor.is_none_or(|c| {
        c.device != meta.dev()
            || c.inode != meta.ino()
            || meta.len() < c.offset
            || (meta.len() == c.offset && meta.modified().ok() != c.modified)
    });
    let offset = if reset {
        let Ok((seed, offset)) = read_prompt_ends(&mut file, meta.len()) else {
            return TranscriptStats::default();
        };
        stats = seed;
        offset
    } else {
        let mut offset = stats.antigravity_cursor.as_ref().unwrap().offset;
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return stats;
        }
        let mut reader = BufReader::new(file.take(meta.len() - offset));
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(n) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if n == 0 || !line.ends_with(b"\n") {
                break;
            }
            offset += n as u64;
            if let Some(prompt) = user_prompt(&line) {
                stats.first_prompt.get_or_insert_with(|| prompt.clone());
                stats.last_prompt = Some(prompt);
            }
        }
        offset
    };
    stats.antigravity_cursor = Some(PromptCursor {
        offset,
        device: meta.dev(),
        inode: meta.ino(),
        modified: meta.modified().ok(),
    });
    stats
}

pub fn title_watch_paths() -> Vec<PathBuf> {
    let Some(root) = super::cli_dir() else {
        return vec![];
    };
    vec![
        root.join("conversation_summaries.db"),
        root.join("conversation_summaries.db-wal"),
    ]
}

pub fn title_store_mtimes() -> (Option<SystemTime>, Option<SystemTime>) {
    let paths = title_watch_paths();
    let mtime = |index| {
        paths
            .get(index)
            .and_then(|p| std::fs::metadata(p).ok()?.modified().ok())
    };
    (mtime(0), mtime(1))
}

pub fn read_titles(ids: &[String]) -> HashMap<String, String> {
    super::cli_dir()
        .map(|root| read_titles_at(&root, ids))
        .unwrap_or_default()
}

pub(super) fn read_titles_at(root: &Path, ids: &[String]) -> HashMap<String, String> {
    let read = || -> rusqlite::Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = Connection::open_with_flags(
            root.join("conversation_summaries.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Never wait behind the agent's writer on a dashboard reload.
        conn.busy_timeout(Duration::ZERO)?;
        let mut stmt =
            conn.prepare("SELECT title FROM conversation_summaries WHERE conversation_id = ?1")?;
        Ok(ids
            .iter()
            .filter_map(|id| {
                let title: String = stmt.query_row([id], |row| row.get(0)).ok()?;
                (!title.trim().is_empty()).then(|| (id.clone(), title))
            })
            .collect())
    };
    read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn prompt(text: &str) -> String {
        serde_json::json!({"type":"USER_INPUT", "content":format!(
            "<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>private metadata</ADDITIONAL_METADATA>"
        )}).to_string() + "\n"
    }

    #[test]
    fn startup_reads_only_head_and_tail_blocks_with_large_history_between() {
        struct CountReads<'a> {
            reader: Cursor<&'a [u8]>,
            bytes: usize,
        }
        impl Read for CountReads<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let n = self.reader.read(buffer)?;
                self.bytes += n;
                Ok(n)
            }
        }
        impl Seek for CountReads<'_> {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.reader.seek(position)
            }
        }

        let body = format!(
            "{}{}\n{}",
            prompt("opening"),
            "x".repeat(4 * 1024 * 1024),
            prompt("latest")
        );
        let mut reader = CountReads {
            reader: Cursor::new(body.as_bytes()),
            bytes: 0,
        };
        let (stats, offset) = read_prompt_ends(&mut reader, body.len() as u64).unwrap();
        assert_eq!(stats.first_prompt.as_deref(), Some("opening"));
        assert_eq!(stats.last_prompt.as_deref(), Some("latest"));
        assert_eq!(offset, body.len() as u64);
        assert_eq!(
            reader.bytes,
            2 * PROMPT_READ_BLOCK,
            "startup must skip the history between the prompts"
        );
    }

    #[test]
    fn backward_search_crosses_blocks_and_skips_synthetic_and_partial_records() {
        let latest = "雪".repeat(PROMPT_READ_BLOCK);
        for tail_len in [
            0,
            1,
            PROMPT_READ_BLOCK - 1,
            PROMPT_READ_BLOCK,
            PROMPT_READ_BLOCK + 1,
            3 * PROMPT_READ_BLOCK,
        ] {
            let mut body = prompt("opening");
            body.push_str(&prompt(&latest));
            body.push_str(
                &serde_json::json!({"type":"TOOL_RESULT", "content":"x".repeat(tail_len)})
                    .to_string(),
            );
            body.push_str("\ninvalid\n{\"type\":\"USER_INPUT\",\"content\":\"CHECKPOINT\"}\n");
            let committed = body.len() as u64;
            body.push_str(prompt("not committed yet").trim_end());
            let (stats, offset) =
                read_prompt_ends(&mut Cursor::new(body.as_bytes()), body.len() as u64).unwrap();
            assert_eq!(stats.first_prompt.as_deref(), Some("opening"));
            assert_eq!(
                stats.last_prompt.as_deref(),
                Some(latest.as_str()),
                "tail length {tail_len}"
            );
            assert_eq!(offset, committed);
        }
    }

    #[test]
    fn startup_cursor_retries_uncommitted_input_even_without_an_opening_prompt() {
        for prefix in [
            String::new(),
            prompt("opening"),
            prompt("opening") + &prompt("latest"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("transcript.jsonl");
            let pending = prompt("finished later");
            let split = pending.len() / 2;
            std::fs::write(&path, prefix.clone() + &pending[..split]).unwrap();
            let before = read_transcript_stats(&path, None);
            assert_eq!(
                before.antigravity_cursor.as_ref().unwrap().offset,
                prefix.len() as u64
            );
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&pending.as_bytes()[split..]).unwrap();
            let after = read_transcript_stats(&path, Some(&before));
            assert_eq!(after.last_prompt.as_deref(), Some("finished later"));
            assert_eq!(
                after.first_prompt.as_deref(),
                Some(if prefix.is_empty() {
                    "finished later"
                } else {
                    "opening"
                })
            );
        }
    }

    #[test]
    fn prompts_follow_appends_and_retry_partial_lines_without_rereading_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let first = prompt("first request");
        std::fs::write(&path, &first).unwrap();
        let initial = read_transcript_stats(&path, None);
        assert_eq!(initial.first_prompt.as_deref(), Some("first request"));
        assert_eq!(initial.last_prompt, initial.first_prompt);
        assert_eq!(
            initial.antigravity_cursor.as_ref().unwrap().offset,
            first.len() as u64
        );
        assert_eq!(read_transcript_stats(&path, Some(&initial)), initial);

        // Change the consumed prompt and append a non-prompt. A full reread
        // would adopt the changed prompt; the cursor must skip old bytes.
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let changed = prompt("wrong request");
        assert_eq!(changed.len(), first.len());
        file.write_all(changed.as_bytes()).unwrap();
        file.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"content\":\"done\"}\n")
            .unwrap();
        let consumed = read_transcript_stats(&path, Some(&initial));
        assert_eq!(
            consumed.last_prompt, initial.last_prompt,
            "old bytes must not be read again"
        );
        let offset = consumed.antigravity_cursor.as_ref().unwrap().offset;
        let second = prompt("next\n request 🐈");
        let split = second.len() / 2;
        file.write_all(&second.as_bytes()[..split]).unwrap();
        let partial = read_transcript_stats(&path, Some(&consumed));
        assert_eq!(partial.last_prompt, initial.last_prompt);
        assert_eq!(partial.antigravity_cursor.as_ref().unwrap().offset, offset);
        file.write_all(&second.as_bytes()[split..]).unwrap();
        // Synthetic user steps and metadata must never become prompts.
        file.write_all(b"{\"type\":\"USER_INPUT\",\"content\":\"CHECKPOINT\"}\ninvalid\n")
            .unwrap();
        let current = read_transcript_stats(&path, Some(&partial));
        assert_eq!(current.first_prompt, initial.first_prompt);
        assert_eq!(current.last_prompt.as_deref(), Some("next request 🐈"));
        assert!(current.context_tokens.is_none());
        assert!(current.model.is_none());
    }

    #[test]
    fn missing_truncated_and_replaced_logs_recover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        assert_eq!(
            read_transcript_stats(&path, None),
            TranscriptStats::default()
        );
        std::fs::write(&path, prompt("a long original prompt")).unwrap();
        let old = read_transcript_stats(&path, None);
        std::fs::write(&path, prompt("short")).unwrap();
        let short = read_transcript_stats(&path, Some(&old));
        assert_eq!(short.last_prompt.as_deref(), Some("short"));
        let replacement = dir.path().join("replacement");
        std::fs::write(
            &replacement,
            prompt("a replacement longer than the original prompt"),
        )
        .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        let replaced = read_transcript_stats(&path, Some(&short));
        assert_eq!(replaced.first_prompt, replaced.last_prompt);
        assert_eq!(
            replaced.last_prompt.as_deref(),
            Some("a replacement longer than the original prompt")
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_transcript_stats(&path, Some(&replaced)), replaced);
    }

    #[test]
    fn large_tool_history_is_consumed_once_and_does_not_hide_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(prompt("original").as_bytes()).unwrap();
        let tool = serde_json::json!({"type":"TOOL_RESULT", "content":"x".repeat(1024 * 1024)})
            .to_string()
            + "\n";
        for _ in 0..8 {
            file.write_all(tool.as_bytes()).unwrap();
        }
        let prior = read_transcript_stats(&path, None);
        let offset = prior.antigravity_cursor.as_ref().unwrap().offset;
        let next = prompt("latest");
        file.write_all(next.as_bytes()).unwrap();
        let current = read_transcript_stats(&path, Some(&prior));
        assert_eq!(current.last_prompt.as_deref(), Some("latest"));
        assert_eq!(
            current.antigravity_cursor.as_ref().unwrap().offset - offset,
            next.len() as u64
        );
        assert_eq!(read_transcript_stats(&path, Some(&current)), current);
    }

    #[test]
    fn title_queries_see_live_wal_renames_and_ignore_previews() {
        let dir = tempfile::tempdir().unwrap();
        let ids = vec!["session-a".into(), "untitled".into(), "absent".into()];
        assert!(read_titles_at(dir.path(), &ids).is_empty());
        assert!(!dir.path().join("conversation_summaries.db").exists());
        let conn = Connection::open(dir.path().join("conversation_summaries.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, preview TEXT); INSERT INTO conversation_summaries VALUES ('session-a', 'Generated title', 'preview'), ('untitled', '', 'not a title'), ('other', 'Other session', '');").unwrap();
        assert_eq!(
            read_titles_at(dir.path(), &ids),
            HashMap::from([("session-a".into(), "Generated title".into())])
        );
        conn.execute(
            "UPDATE conversation_summaries SET title = ?1 WHERE conversation_id = 'session-a'",
            ["Renamed 🐈"],
        )
        .unwrap();
        assert_eq!(read_titles_at(dir.path(), &ids)["session-a"], "Renamed 🐈");
        conn.execute_batch("BEGIN EXCLUSIVE; UPDATE conversation_summaries SET title = 'uncommitted' WHERE conversation_id = 'session-a';").unwrap();
        assert_eq!(read_titles_at(dir.path(), &ids)["session-a"], "Renamed 🐈");
    }
}
