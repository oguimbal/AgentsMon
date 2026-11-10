//! Persist the sessions closed with the × button so a restart does not bring
//! every dormant conversation back. Only IDs and timestamps are stored.
//! Providers toggled off from their bar icon are kept beside them, as IDs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

const FILE_NAME: &str = "hidden-sessions.json";
const PROVIDERS_FILE_NAME: &str = "hidden-providers.json";

pub(crate) fn path() -> Result<PathBuf> {
    data_file(FILE_NAME)
}

pub(crate) fn providers_path() -> Result<PathBuf> {
    data_file(PROVIDERS_FILE_NAME)
}

fn data_file(name: &str) -> Result<PathBuf> {
    let dirs = agentsmon_providers::app_dirs::project_dirs()
        .context("cannot resolve AgentsMon data directory")?;
    Ok(dirs.data_dir().join(name))
}

/// Missing or unreadable files yield an empty list; a corrupt file is logged and ignored.
pub(crate) fn load_from<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read hidden entries");
            return Vec::new();
        }
    };
    match serde_json::from_slice(&contents) {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring corrupt hidden entries file");
            Vec::new()
        }
    }
}

pub(crate) fn save_to<T: Serialize>(path: &Path, sessions: &[T]) -> Result<()> {
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

    use agentsmon_core::{AgentId, HiddenSession};

    use super::*;

    #[test]
    fn round_trips_and_removes_the_file_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        assert!(load_from::<HiddenSession>(&path).is_empty());

        let sessions = vec![HiddenSession {
            id: AgentId::new("codex", "abc"),
            hidden_at: SystemTime::UNIX_EPOCH + Duration::from_secs(42),
        }];
        save_to(&path, &sessions).unwrap();
        assert_eq!(load_from::<HiddenSession>(&path), sessions);

        save_to::<HiddenSession>(&path, &[]).unwrap();
        assert!(!path.exists());
        save_to::<HiddenSession>(&path, &[]).unwrap();

        let providers = dir.path().join(PROVIDERS_FILE_NAME);
        save_to(&providers, &["openclaw".to_owned()]).unwrap();
        assert_eq!(load_from::<String>(&providers), ["openclaw"]);
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, b"{not json").unwrap();
        assert!(load_from::<HiddenSession>(&path).is_empty());
    }
}
