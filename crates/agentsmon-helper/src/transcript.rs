//! Best-effort session metadata derived from a session transcript.
//!
//! Sessions hosted outside a terminal (VS Code extension, claude.ai) never
//! render a status line, so the proxy cannot report their context window.
//! Hooks do receive `transcript_path`; the tail of that file carries the
//! token counters of the latest assistant turn and the title Claude Code
//! assigned to the conversation (or the one the user picked with `/rename`).
//! Only `usage` numbers and title lines are decoded — message content is
//! never inspected or persisted.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

/// Claude Code does not expose the active context-window size; 200k is the
/// standard window and 1M the long-context variant ("[1m]" models).
const STANDARD_WINDOW: u64 = 200_000;
const LONG_WINDOW: u64 = 1_000_000;
const TAIL_BYTES: u64 = 256 * 1024;
const AI_TITLE_MARKER: &str = "\"type\":\"ai-title\"";
const CUSTOM_TITLE_MARKER: &str = "\"type\":\"custom-title\"";

#[derive(Debug, Default, PartialEq)]
pub struct TranscriptInfo {
    pub context_percentage: Option<f32>,
    pub title: Option<String>,
}

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

/// Title lines are standalone JSON objects; only their `type` and title
/// fields are decoded.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum TitleLine {
    #[serde(rename = "ai-title")]
    Ai {
        #[serde(rename = "aiTitle")]
        title: String,
    },
    #[serde(rename = "custom-title")]
    Custom {
        #[serde(rename = "customTitle")]
        title: String,
    },
    #[serde(other)]
    Other,
}

pub fn inspect(transcript_path: &str) -> TranscriptInfo {
    let path = Path::new(transcript_path);
    let Some(tail) = read_tail(path) else {
        return TranscriptInfo::default();
    };
    let context_percentage = latest_context_tokens(tail.lines()).map(|tokens| {
        let window = context_window(tokens, settings_use_long_context());
        ((tokens as f32 / window as f32) * 100.0).clamp(0.0, 100.0)
    });
    // Claude Code rewrites the title line every few messages, so the tail
    // almost always carries it. A very large final tool result can push it
    // out; fall back to a forward scan that only decodes title lines.
    let title =
        latest_title(tail.lines()).or_else(|| tail.truncated.then(|| scan_title(path)).flatten());
    TranscriptInfo {
        context_percentage,
        title,
    }
}

struct Tail {
    buffer: String,
    truncated: bool,
}

impl Tail {
    /// Lines in file order, skipping the partial first line of a truncated tail.
    fn lines(&self) -> impl DoubleEndedIterator<Item = &str> {
        let mut lines = self.buffer.lines();
        if self.truncated {
            let _ = lines.next();
        }
        lines
    }
}

fn read_tail(path: &Path) -> Option<Tail> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = String::new();
    file.take(TAIL_BYTES).read_to_string(&mut buffer).ok()?;
    Some(Tail {
        buffer,
        truncated: start > 0,
    })
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

fn latest_context_tokens<'a>(lines: impl DoubleEndedIterator<Item = &'a str>) -> Option<u64> {
    lines
        .rev()
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

/// The most recent title, a user rename (`custom-title`) taking precedence
/// over the generated one, as Claude Code itself does.
fn latest_title<'a>(lines: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut ai = None;
    let mut custom = None;
    for line in lines {
        if !line.contains(AI_TITLE_MARKER) && !line.contains(CUSTOM_TITLE_MARKER) {
            continue;
        }
        match serde_json::from_str::<TitleLine>(line) {
            Ok(TitleLine::Ai { title }) => ai = clean_title(title).or(ai),
            Ok(TitleLine::Custom { title }) => custom = clean_title(title).or(custom),
            _ => {}
        }
    }
    custom.or(ai)
}

fn scan_title(path: &Path) -> Option<String> {
    let reader = BufReader::new(File::open(path).ok()?);
    let lines: Vec<String> = reader
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains(AI_TITLE_MARKER) || line.contains(CUSTOM_TITLE_MARKER))
        .collect();
    latest_title(lines.iter().map(String::as_str))
}

fn clean_title(title: String) -> Option<String> {
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
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

        let tail = read_tail(file.path()).unwrap();
        assert_eq!(latest_context_tokens(tail.lines()), Some(51_002));
    }

    #[test]
    fn overflowing_the_standard_window_implies_long_context() {
        assert_eq!(context_window(150_000, false), STANDARD_WINDOW);
        assert_eq!(context_window(150_000, true), LONG_WINDOW);
        assert_eq!(context_window(300_000, false), LONG_WINDOW);
    }

    #[test]
    fn ignores_a_missing_or_usage_free_transcript() {
        assert_eq!(
            inspect("/nonexistent/transcript.jsonl"),
            TranscriptInfo::default()
        );
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user"}}"#).unwrap();
        assert_eq!(
            inspect(file.path().to_str().unwrap()),
            TranscriptInfo::default()
        );
    }

    #[test]
    fn a_user_rename_wins_over_the_generated_title() {
        let lines = [
            r#"{"type":"ai-title","sessionId":"s","aiTitle":"First draft"}"#,
            r#"{"type":"user","message":{"content":"private \"type\":\"ai-title\" text"}}"#,
            r#"{"type":"custom-title","sessionId":"s","customTitle":" Renamed "}"#,
            r#"{"type":"ai-title","aiTitle":"Later generated","sessionId":"s"}"#,
        ];
        assert_eq!(
            latest_title(lines.iter().copied()).as_deref(),
            Some("Renamed")
        );
        assert_eq!(
            latest_title(lines[..2].iter().copied()).as_deref(),
            Some("First draft")
        );
        assert_eq!(latest_title(lines[1..2].iter().copied()), None);
    }

    #[test]
    fn falls_back_to_a_forward_scan_when_the_title_left_the_tail() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"ai-title","sessionId":"s","aiTitle":"Early title"}}"#
        )
        .unwrap();
        let filler = "x".repeat(4096);
        for _ in 0..80 {
            writeln!(
                file,
                r#"{{"type":"user","message":{{"content":"{filler}"}}}}"#
            )
            .unwrap();
        }
        assert!(file.as_file().metadata().unwrap().len() > TAIL_BYTES);
        assert_eq!(
            inspect(file.path().to_str().unwrap()).title.as_deref(),
            Some("Early title")
        );
    }
}
