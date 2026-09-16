//! Persist the sessions closed with the × button so a restart does not bring
//! every dormant conversation back. Only IDs and timestamps are stored.

use std::fs;
use std::path::{Path, PathBuf};

use agentsmon_core::HiddenSession;
use anyhow::{Context, Result};

const FILE_NAME: &str = "hidden-sessions.json";

pub(crate) fn path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .context("cannot resolve AgentsMon data directory")?;
    Ok(dirs.data_dir().join(FILE_NAME))
}

/// Missing or unreadable files yield an empty list; a corrupt file is logged and ignored.
pub(crate) fn load_from(path: &Path) -> Vec<HiddenSession> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read hidden sessions");
            return Vec::new();
        }
    };
    match serde_json::from_slice(&contents) {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring corrupt hidden sessions file");
            Vec::new()
        }
    }
}

pub(crate) fn save_to(path: &Path, sessions: &[HiddenSession]) -> Result<()> {
    if sessions.is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    }
    super::codex_integration::write_atomic(path, &serde_json::to_vec_pretty(sessions)?)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use agentsmon_core::AgentId;

    use super::*;

    #[test]
    fn round_trips_and_removes_the_file_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        assert!(load_from(&path).is_empty());

        let sessions = vec![HiddenSession {
            id: AgentId::new("codex", "abc"),
            hidden_at: SystemTime::UNIX_EPOCH + Duration::from_secs(42),
        }];
        save_to(&path, &sessions).unwrap();
        assert_eq!(load_from(&path), sessions);

        save_to(&path, &[]).unwrap();
        assert!(!path.exists());
        save_to(&path, &[]).unwrap();
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, b"{not json").unwrap();
        assert!(load_from(&path).is_empty());
    }
}
