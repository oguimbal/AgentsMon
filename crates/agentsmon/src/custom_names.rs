//! Durable user-entered labels. No agent snapshots or provider titles are saved.
//! Save during rename/broadcast, not at shutdown: SIGTERM must not lose a name.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentsmon_core::CustomNames;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "custom-names.json";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    #[serde(flatten)]
    names: CustomNames,
}

pub struct Persistence {
    path: Option<PathBuf>,
    saved: CustomNames,
    needs_recovery: bool,
    failed_attempt: Option<(CustomNames, Instant)>,
}

impl Persistence {
    pub fn new() -> Self {
        let path = agentsmon_providers::app_dirs::project_dirs()
            .map(|dirs| dirs.data_dir().join(FILE_NAME));
        Self::load(path)
    }

    fn load(path: Option<PathBuf>) -> Self {
        let result = path
            .as_deref()
            .context("cannot resolve custom names data directory")
            .and_then(load_from);
        let needs_recovery = result.is_err();
        let saved = result.unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot restore custom names; original file is preserved");
            CustomNames::default()
        });
        Self {
            path,
            saved,
            needs_recovery,
            failed_attempt: None,
        }
    }

    pub fn restored(&self) -> CustomNames {
        self.saved.clone()
    }

    pub fn save_if_changed(&mut self, names: CustomNames) {
        self.save_at(names, Instant::now());
    }

    fn save_at(&mut self, names: CustomNames, now: Instant) {
        if names == self.saved {
            self.failed_attempt = None;
            return;
        }
        if self.failed_attempt.as_ref().is_some_and(|(failed, at)| {
            *failed == names && now.saturating_duration_since(*at) < RETRY_DELAY
        }) {
            return;
        }
        let result = (|| {
            let path = self
                .path
                .as_deref()
                .context("cannot resolve custom names data directory")?;
            // A malformed or newer-format preference file must not be silently
            // destroyed by the next edit. Move it aside only on an actual edit;
            // a launch/refresh alone never replaces or rewrites it.
            if self.needs_recovery {
                let recovery = path.with_extension(format!("json.recovery.{}", nonce()));
                match fs::rename(path, &recovery) {
                    Ok(()) => {
                        tracing::warn!(path = %recovery.display(), "preserved unreadable custom names before saving new preferences")
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("preserve unreadable custom names"),
                }
                self.needs_recovery = false;
            }
            save_to(path, &names)
        })();
        match result {
            Ok(()) => {
                self.saved = names;
                self.failed_attempt = None;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot persist custom names; retained in memory, retry in 30 seconds or on next edit");
                self.failed_attempt = Some((names, now));
            }
        }
    }
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn load_from(path: &Path) -> Result<CustomNames> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CustomNames::default())
        }
        Err(error) => return Err(error).context("read custom names"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("custom names file exceeds the 4 MiB limit");
    }
    let saved: Saved = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid custom names JSON (values omitted)"))?;
    if saved.version != VERSION {
        bail!("unsupported custom names version {}", saved.version);
    }
    Ok(saved.names)
}

fn save_to(path: &Path, names: &CustomNames) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&Saved {
        version: VERSION,
        names: names.clone(),
    })?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("custom names exceed the 4 MiB limit");
    }
    let parent = path.parent().context("custom names path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), nonce()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file); // Windows cannot replace an open temporary file.
        super::codex_integration::replace_atomically(&temporary, path)?;
        // The rename is sufficient for an app kill; syncing the directory also
        // improves durability across an OS restart. Some filesystems refuse it.
        #[cfg(unix)]
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsmon_core::{AgentId, GroupKey, GroupName, SessionName};

    fn names() -> CustomNames {
        CustomNames {
            sessions: vec![SessionName {
                id: AgentId::new("claude", "s"),
                name: "Réparer l’auth \"v2\"".into(),
            }],
            groups: vec![GroupName {
                key: GroupKey::Directory("/tmp/projet français".into()),
                name: "Mon projet".into(),
            }],
        }
    }

    #[test]
    fn persisted_rename_and_clear_survive_a_restart_without_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        let mut first = Persistence::load(Some(path.clone()));
        first.save_if_changed(names());
        assert_eq!(Persistence::load(Some(path.clone())).restored(), names());
        first.save_if_changed(CustomNames::default());
        assert_eq!(
            Persistence::load(Some(path)).restored(),
            CustomNames::default()
        );
        assert_eq!(fs::read_dir(dir.path().join("nested")).unwrap().count(), 1);
    }

    #[test]
    fn invalid_or_future_files_are_preserved_until_an_edit_and_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for contents in ["{broken", r#"{"version":2,"sessions":[],"groups":[]}"#] {
            fs::write(&path, contents).unwrap();
            let mut persistence = Persistence::load(Some(path.clone()));
            persistence.save_if_changed(CustomNames::default());
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            persistence.save_if_changed(names());
            assert_eq!(load_from(&path).unwrap(), names());
            assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|p| {
                p.file_name().to_string_lossy().contains(".recovery.")
                    && fs::read_to_string(p.path()).unwrap() == contents
            }));
        }
        fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(load_from(&path).is_err());
    }

    #[test]
    fn failed_writes_retry_without_losing_in_memory_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("blocked");
        fs::write(&parent, "file prevents directory creation").unwrap();
        let path = parent.join(FILE_NAME);
        let mut persistence = Persistence::load(Some(path.clone()));
        let now = Instant::now();
        persistence.save_at(names(), now);
        assert!(persistence.failed_attempt.is_some());
        fs::remove_file(&parent).unwrap();
        persistence.save_at(names(), now + Duration::from_secs(1));
        assert!(!path.exists());
        persistence.save_at(names(), now + RETRY_DELAY);
        assert_eq!(load_from(&path).unwrap(), names());
        assert!(persistence.failed_attempt.is_none());
    }
}
