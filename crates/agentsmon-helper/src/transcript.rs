//! Best-effort context usage derived from a session transcript.
//!
//! Sessions hosted outside a terminal (VS Code extension, claude.ai) never
//! render a status line, so the proxy cannot report their context window.
//! Hooks do receive `transcript_path`; the tail of that file carries the
//! token counters of the latest assistant turn. Only `usage` numbers are
//! decoded — conversation content is never inspected or persisted.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

/// Claude Code does not expose the active context-window size; 200k is the
/// standard window and 1M the long-context variant ("[1m]" models).
const STANDARD_WINDOW: u64 = 200_000;
const LONG_WINDOW: u64 = 1_000_000;
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Deserialize)]
struct TranscriptLine {
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    usage: Option<TranscriptUsage>,
}

#[derive(Deserialize)]
struct TranscriptUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

pub fn context_percentage(transcript_path: &str) -> Option<f32> {
    let tokens = latest_context_tokens(Path::new(transcript_path))?;
    let window = context_window(tokens, settings_use_long_context());
    Some(((tokens as f32 / window as f32) * 100.0).clamp(0.0, 100.0))
}

fn context_window(used_tokens: u64, long_context_settings: bool) -> u64 {
    // A session that already exceeds the standard window must be running a
    // long-context model, whatever the settings say.
    if used_tokens > STANDARD_WINDOW || long_context_settings {
        LONG_WINDOW
    } else {
        STANDARD_WINDOW
    }
}

fn settings_use_long_context() -> bool {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default)]
        model: Option<String>,
    }
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".claude/settings.json"))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
        .and_then(|settings| settings.model)
        .is_some_and(|model| model.contains("[1m]"))
}

fn latest_context_tokens(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = String::new();
    file.take(TAIL_BYTES).read_to_string(&mut tail).ok()?;

    let mut lines = tail.lines().rev();
    if start > 0 {
        // The first line of the buffer is truncated; lines() yields it last.
        let _ = lines.next_back();
    }
    lines
        .filter(|line| line.contains("\"usage\""))
        .find_map(|line| {
            let line: TranscriptLine = serde_json::from_str(line).ok()?;
            if line.is_sidechain {
                return None;
            }
            let usage = line.message?.usage?;
            let tokens = usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_creation_input_tokens;
            (tokens > 0).then_some(tokens)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_the_latest_main_conversation_usage() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"message":{{"usage":{{"input_tokens":2,"cache_read_input_tokens":50000,"cache_creation_input_tokens":1000}}}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"isSidechain":true,"message":{{"usage":{{"input_tokens":2,"cache_read_input_tokens":190000,"cache_creation_input_tokens":0}}}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();

        let tokens = latest_context_tokens(file.path()).unwrap();
        assert_eq!(tokens, 51_002);
    }

    #[test]
    fn overflowing_the_standard_window_implies_long_context() {
        assert_eq!(context_window(150_000, false), STANDARD_WINDOW);
        assert_eq!(context_window(150_000, true), LONG_WINDOW);
        assert_eq!(context_window(300_000, false), LONG_WINDOW);
    }

    #[test]
    fn ignores_a_missing_or_usage_free_transcript() {
        assert_eq!(context_percentage("/nonexistent/transcript.jsonl"), None);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user"}}"#).unwrap();
        assert_eq!(latest_context_tokens(file.path()), None);
    }
}
