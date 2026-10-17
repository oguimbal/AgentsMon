//! Claude's private, metadata-only `sessions/<pid>.json` registry.
//! See docs/claude-session-status.md before changing this reader. In particular,
//! silence never means idle, and `updatedAt` is NOT a status timestamp.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentsmon_core::AgentState;
use serde::Deserialize;

use crate::ProviderDiagnostic;

const MAX_BYTES: u64 = 256 * 1024;
const DIAGNOSTIC_GRACE: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeRecord {
    pid: i32,
    session_id: String,
    started_at: u64,
    status: Option<String>,
    status_updated_at: Option<u64>,
    kind: Option<String>,
    version: Option<String>,
}

pub(crate) struct Registry {
    pub root: PathBuf,
    health: Mutex<Health>,
}

/// The session's latest Claude hook, as recorded by the helper.
#[derive(Debug, Clone, Copy)]
pub struct LastHook {
    pub state: AgentState,
    pub time: SystemTime,
    /// SubagentStart/SubagentStop: may come from a background or internal
    /// forked agent after the main turn ended.
    pub subagent: bool,
    /// Awaiting a one-shot session cron (ScheduleWakeup…) rather than a
    /// monitor. Such a wait involves no process, so the registry says `idle`.
    pub wakeup: bool,
}

#[derive(Default)]
struct Health {
    pending: Option<(ProviderDiagnostic, Instant)>,
    current: Option<ProviderDiagnostic>,
}

impl Registry {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            health: Mutex::default(),
        }
    }

    /// Caller has already checked that this PID is a live Claude process.
    /// Only inspect that process's own record, never enumerate arbitrary files.
    pub fn read(
        &self,
        pid: i32,
        session_id: &str,
        process_started: SystemTime,
        hook: LastHook,
        now: SystemTime,
    ) -> Result<Option<(AgentState, SystemTime)>, ProviderDiagnostic> {
        let path = self.root.join(format!("{pid}.json"));
        let issue = |code, summary| {
            ProviderDiagnostic {
            code,
            summary,
            details: format!("Source : {}\nPID : {pid}\nLecture bornée à {MAX_BYTES} octets.\nRepli : derniers hooks Claude connus.\nDocumentation : docs/claude-session-status.md", path.display()),
        }
        };
        let invalid = || {
            issue(
                "claude.registry.schema",
                "Le format du registre de sessions Claude est incompatible ou incomplet.",
            )
        };
        // Refuse symlinks, directories, pipes and devices. O_NOFOLLOW also
        // closes the check/open race on Unix; O_NONBLOCK prevents a replaced
        // FIFO from hanging the UI thread. No cache of an earlier idle state.
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(issue(
                    "claude.registry.read",
                    "Le registre de sessions Claude est illisible.",
                ))
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(issue(
                "claude.registry.file",
                "Le fichier de statut Claude n’est pas un fichier régulier de taille acceptée.",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let read = || -> io::Result<Vec<u8>> {
            let file = options.open(&path)?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        let bytes =
            match read() {
                Ok(bytes) if bytes.len() as u64 <= MAX_BYTES => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                _ => return Err(issue(
                    "claude.registry.read",
                    "Le registre de sessions Claude est illisible ou dépasse la taille acceptée.",
                )),
            };
        // Serde errors can echo input values. Return only a fixed error code.
        let record: RuntimeRecord = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if record.pid != pid || record.session_id != session_id {
            return Ok(None); // Session resumed elsewhere or PID reused.
        }
        match record.kind.as_deref() {
            None | Some("interactive") => {}
            Some("bg" | "daemon" | "daemon-worker") => return Ok(None),
            Some(_) => {
                return Err(issue(
                    "claude.registry.kind",
                    "Claude publie un type de session inconnu.",
                ))
            }
        }
        let started = millis(record.started_at).ok_or_else(invalid)?;
        if started < process_started || started > now {
            return Ok(None); // Registry belongs to an earlier process generation.
        }
        let (Some(status), Some(timestamp)) = (record.status.as_deref(), record.status_updated_at)
        else {
            if hook.state == AgentState::Idle {
                return Ok(None);
            }
            let mut diagnostic = issue("claude.registry.status_unavailable", "Cette session Claude ne publie pas son état courant. Relancez-la avec une version récente de Claude Code.");
            // Version is useful for mixed running installations, but it is an
            // untrusted field. Never include arbitrary source strings in logs.
            if let Some(version) = record
                .version
                .filter(|v| v.len() <= 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            {
                diagnostic
                    .details
                    .push_str(&format!("\nVersion du processus Claude : {version}"));
            }
            return Err(diagnostic);
        };
        let state = match status {
            // A Monitor tool watch is a `local_bash` task: the registry says
            // `shell` while the turn rests on it, `idle` once it has gone.
            "shell" if hook.state == AgentState::Awaiting => AgentState::Awaiting,
            "idle" if hook.state == AgentState::Awaiting && hook.wakeup => AgentState::Awaiting,
            // `shell`: the turn is over but a background `local_bash` task
            // (e.g. a dev server) still runs. The model is not working on it.
            "idle" | "shell" => AgentState::Idle,
            "busy" => AgentState::Active,
            "waiting" => AgentState::Waiting,
            _ => {
                return Err(issue(
                    "claude.registry.status",
                    "Claude publie une valeur de statut inconnue.",
                ))
            }
        };
        let timestamp = millis(timestamp).ok_or_else(invalid)?;
        if timestamp < started || timestamp > now + Duration::from_secs(2) {
            return Err(issue(
                "claude.registry.timestamp",
                "Les horodatages du statut Claude sont incohérents.",
            ));
        }
        // A new prompt/hook beats an old registry idle, except a subagent
        // lifecycle hook: once the main turn published idle or waiting, a
        // later SubagentStart/SubagentStop comes from a background or
        // internal forked agent, and a resumed main turn republishes busy.
        let resting = matches!(state, AgentState::Idle | AgentState::Waiting);
        if timestamp <= hook.time && !(hook.subagent && resting) {
            return Ok(None);
        }
        let state = if state == AgentState::Active && hook.state == AgentState::Compacting {
            AgentState::Compacting
        } else {
            state
        };
        Ok(Some((state, timestamp)))
    }

    pub fn finish_scan(&self, issue: Option<ProviderDiagnostic>) {
        if let Ok(mut health) = self.health.lock() {
            health.observe(issue, Instant::now());
        }
    }

    pub fn diagnostic(&self) -> Option<ProviderDiagnostic> {
        self.health.lock().ok()?.current.clone()
    }
}

impl Health {
    fn observe(&mut self, issue: Option<ProviderDiagnostic>, now: Instant) {
        let Some(issue) = issue else {
            *self = Self::default();
            return;
        };
        if let Some((pending, since)) = &self.pending {
            if pending == &issue {
                if now.saturating_duration_since(*since) >= DIAGNOSTIC_GRACE {
                    self.current = Some(issue);
                }
                return;
            }
        }
        self.current = None;
        self.pending = Some((issue, now));
    }
}

fn millis(value: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_millis(value))
}

#[cfg(test)]
mod tests;
