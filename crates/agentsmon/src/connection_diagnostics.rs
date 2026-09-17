//! Durable, provider-neutral incident reports and a global hourly notification
//! budget. Only provider-supplied, sanitized metadata enters these files.
use agentsmon_providers::ProviderDiagnostic;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HOUR_MS: u64 = 60 * 60 * 1000;
const MAX_STATE_BYTES: u64 = 128 * 1024;
const MAX_LOG_BYTES: u64 = 512 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Report {
    provider: String,
    code: String,
    summary: String,
    details: String,
    first_seen_ms: u64,
    last_seen_ms: u64,
    cleared_ms: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    last_notification_ms: Option<u64>,
    reports: BTreeMap<String, Report>,
}

pub struct ConnectionDiagnostics {
    root: Option<PathBuf>,
    saved: Saved,
}

impl ConnectionDiagnostics {
    pub fn new() -> Self {
        let root = directories::ProjectDirs::from("com", "oliv", "AgentsMon")
            .map(|dirs| dirs.data_dir().join("diagnostics"));
        Self::load(root)
    }

    fn load(root: Option<PathBuf>) -> Self {
        let saved = root
            .as_ref()
            .and_then(|root| {
                let mut bytes = Vec::new();
                let file = fs::File::open(root.join("provider-diagnostics.json")).ok()?;
                file.take(MAX_STATE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() as u64 > MAX_STATE_BYTES {
                    return None;
                }
                let saved: Saved = serde_json::from_slice(&bytes).ok()?;
                (saved.reports.len() <= 16).then_some(saved)
            })
            .unwrap_or_default();
        Self { root, saved }
    }

    /// The timestamp is reserved/persisted BEFORE the native notification is
    /// attempted. OS denial or a restart must not cause a notification storm.
    pub fn observe(
        &mut self,
        provider: &str,
        name: &str,
        diagnostic: Option<ProviderDiagnostic>,
    ) -> Option<String> {
        self.observe_at(provider, name, diagnostic, now_ms())
    }

    fn observe_at(
        &mut self,
        provider: &str,
        name: &str,
        diagnostic: Option<ProviderDiagnostic>,
        now: u64,
    ) -> Option<String> {
        let previous = self.saved.reports.get(provider);
        let Some(diagnostic) = diagnostic else {
            if let Some(report) = self
                .saved
                .reports
                .get_mut(provider)
                .filter(|r| r.cleared_ms.is_none())
            {
                report.cleared_ms = Some(now);
                let report = report.clone();
                self.log("issue_cleared", &report, now);
                self.persist();
            }
            return None;
        };
        let changed = previous.is_none_or(|r| {
            r.code != diagnostic.code || r.details != diagnostic.details || r.cleared_ms.is_some()
        });
        let repeat_log = previous.is_none_or(|r| now.saturating_sub(r.last_seen_ms) >= HOUR_MS);
        let first_seen_ms = if changed {
            now
        } else {
            previous.map_or(now, |r| r.first_seen_ms)
        };
        let report = Report {
            provider: name.into(),
            code: diagnostic.code.into(),
            summary: diagnostic.summary.into(),
            details: diagnostic.details,
            first_seen_ms,
            last_seen_ms: now,
            cleared_ms: None,
        };
        let notify = notification_due(self.saved.last_notification_ms, now);
        // Avoid disk writes at the provider polling frequency. The on-disk last
        // seen time is the last recorded observation, not a claim of freshness.
        if changed || repeat_log || notify {
            self.log("degraded", &report, now);
            self.saved.reports.insert(provider.into(), report.clone());
            if notify {
                self.saved.last_notification_ms = Some(now);
            }
            self.persist();
        }
        notify.then(|| self.prompt(&report))
    }

    pub fn current_prompt(&self) -> String {
        if self.saved.reports.is_empty() {
            return format!("Aucun incident de connexion enregistré.\nAgentsMon {} — {} / {}\nLogs : {}\nL’absence d’incident ne garantit pas une connexion active. Un registre Claude absent, Codex arrêté, les tâches sans propriétaire desktop et Codex sur Windows utilisent les hooks.\n", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, self.log_path());
        }
        self.saved
            .reports
            .values()
            .map(|r| self.prompt(r))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }

    fn prompt(&self, report: &Report) -> String {
        format!("Diagnostique et corrige le suivi des états dans AgentsMon pour {}.\nConsulte d’abord docs/codex-desktop-ipc.md pour Codex ou docs/claude-session-status.md pour Claude : ces documents décrivent les formats, les investigations, les protections et les tests.\nPréserve le repli sur les hooks et ne décode aucun contenu de conversation.\n\nAgentsMon : {}\nPlateforme : {} / {}\nIncident : {}\n{}\nPremière observation : {}\nDernière observation enregistrée : {}\nIncident non signalé depuis : {}\n\n{}\nJournal persistant : {}\nJournal précédent : {}.1\nRapport et limite de notifications : {}\n\nCes détails contiennent uniquement des métadonnées techniques. Aucune autorisation n’a été acceptée ou refusée par AgentsMon.\n", report.provider, env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, report.code, report.summary, timestamp(report.first_seen_ms), timestamp(report.last_seen_ms), report.cleared_ms.map(timestamp).unwrap_or_else(|| "incident encore signalé à la dernière observation".into()), report.details, self.log_path(), self.log_path(), self.root.as_ref().map(|p| p.join("provider-diagnostics.json").display().to_string()).unwrap_or_else(|| "indisponible".into()))
    }

    fn log_path(&self) -> String {
        self.root
            .as_ref()
            .map(|p| p.join("provider-diagnostics.log").display().to_string())
            .unwrap_or_else(|| "indisponible".into())
    }

    fn log(&self, event: &str, report: &Report, now: u64) {
        // Also emit explicitly through tracing. The dedicated journal works
        // even when Finder discarded stderr or RUST_LOG suppresses warnings.
        tracing::warn!(target: "agentsmon::connection_diagnostics", event, provider = %report.provider, code = %report.code, details = %report.details, "provider connection diagnostic");
        if let Some(root) = &self.root {
            let row = serde_json::json!({"event":event,"timestamp_ms":now,"agentsmon_version":env!("CARGO_PKG_VERSION"),"report":report});
            let result = serde_json::to_vec(&row)
                .map_err(std::io::Error::other)
                .and_then(|line| append_log(root, &line));
            if let Err(error) = result {
                tracing::warn!(%error, "cannot persist provider diagnostic log");
            }
        }
    }

    fn persist(&self) {
        if let Some(root) = &self.root {
            let result = serde_json::to_vec_pretty(&self.saved)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    super::codex_integration::write_atomic(
                        &root.join("provider-diagnostics.json"),
                        &bytes,
                    )
                });
            if let Err(error) = result {
                tracing::warn!(%error, "cannot persist diagnostic notification throttle; hourly limit remains active for this run");
            }
        }
    }
}

fn notification_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| now.saturating_sub(last) >= HOUR_MS)
}

fn append_log(root: &Path, line: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join("provider-diagnostics.log");
    if fs::metadata(&path)
        .is_ok_and(|m| m.len().saturating_add(line.len() as u64 + 1) > MAX_LOG_BYTES)
    {
        let previous = root.join("provider-diagnostics.log.1");
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(&path, previous)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line)?;
    file.write_all(b"\n")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn timestamp(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| format!("unix_ms={ms}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fault(code: &'static str) -> Option<ProviderDiagnostic> {
        Some(ProviderDiagnostic {
            code,
            summary: "Test",
            details: "expected=11; observed=12".into(),
        })
    }
    #[test]
    fn hourly_budget_survives_restart_recovery_and_different_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = ConnectionDiagnostics::load(Some(dir.path().into()));
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 100)
            .is_some());
        assert!(d
            .observe_at("codex", "Codex", fault("schema"), 101)
            .is_none());
        d.observe_at("codex", "Codex", None, 102);
        let mut d = ConnectionDiagnostics::load(Some(dir.path().into()));
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 103)
            .is_none());
        assert!(d
            .observe_at("other", "Other", fault("schema"), 104)
            .is_none());
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 100 + HOUR_MS - 1)
            .is_none());
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 100 + HOUR_MS)
            .is_some());
        assert!(d.current_prompt().contains("docs/codex-desktop-ipc.md"));
        let log = fs::read_to_string(dir.path().join("provider-diagnostics.log")).unwrap();
        assert!(log.contains("issue_cleared"));
        assert!(log.contains("observed=12"));
    }
    #[test]
    fn clock_rollback_and_unwritable_storage_do_not_spam_or_panic() {
        assert!(!notification_due(Some(100), 50));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        fs::write(&file, b"x").unwrap();
        let mut d = ConnectionDiagnostics::load(Some(file));
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 100)
            .is_some());
        assert!(d
            .observe_at("codex", "Codex", fault("version"), 101)
            .is_none());
    }
    #[test]
    fn diagnostic_journal_rotates_and_corrupt_state_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("provider-diagnostics.json"), b"bad json").unwrap();
        let mut d = ConnectionDiagnostics::load(Some(dir.path().into()));
        fs::write(
            dir.path().join("provider-diagnostics.log"),
            vec![b'x'; MAX_LOG_BYTES as usize],
        )
        .unwrap();
        d.observe_at("codex", "Codex", fault("version"), 100);
        assert_eq!(
            fs::metadata(dir.path().join("provider-diagnostics.log.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES
        );
        assert!(
            fs::metadata(dir.path().join("provider-diagnostics.log"))
                .unwrap()
                .len()
                < 4096
        );
    }
}
