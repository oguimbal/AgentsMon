//! OpenClaw sessions, observed read-only through a (usually remote) Gateway.
//! Maintenance notes and privacy rules: docs/openclaw-gateway.md.

pub mod gateway;
mod provider;
pub mod url;

pub use provider::{OpenClawProvider, PairingState};

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentsmon_core::ContextUsage;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::json;

use gateway::{Connection, Credential, GatewayError, Identity};

pub const CONFIG_FILE: &str = "openclaw.json";
const DEVICE_FILE: &str = "openclaw-device.json";
const DEFAULT_RECENT_HOURS: u64 = 24;
const MAX_SESSIONS: u32 = 24;

pub fn data_dir() -> Option<PathBuf> {
    crate::app_dirs::project_dirs().map(|dirs| dirs.data_dir().into())
}

/// `openclaw.json` in the AgentsMon data directory. AgentsMon writes only
/// `{"url": …}` after a successful pairing; the shared gateway secret typed in
/// the connect window is never persisted. A hand-written `"token"` or
/// `"password"` and an optional `"recentHours"` are honoured, and
/// `OPENCLAW_GATEWAY_URL`, `OPENCLAW_GATEWAY_TOKEN` and
/// `OPENCLAW_GATEWAY_PASSWORD` override the file.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// The Control UI URL, as opened in the browser.
    pub url: ::url::Url,
    pub token: Option<String>,
    pub password: Option<String>,
    pub recent: Duration,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("url", &self.url.as_str())
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("recent", &self.recent)
            .finish()
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    recent_hours: Option<u64>,
}

impl Config {
    /// `Ok(None)` when OpenClaw is not configured: the provider stays silent.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let file = match fs::read(dir.join(CONFIG_FILE)) {
            Ok(bytes) => serde_json::from_slice::<ConfigFile>(&bytes)
                .map_err(|_| anyhow::anyhow!("{CONFIG_FILE} is not valid JSON"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(error) => return Err(error).context("read OpenClaw configuration"),
        };
        let env = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let non_empty = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
        let Some(raw_url) = env("OPENCLAW_GATEWAY_URL").or(non_empty(file.url)) else {
            return Ok(None);
        };
        let url = ::url::Url::parse(raw_url.trim()).context("invalid OpenClaw gateway URL")?;
        gateway::websocket_url(&url).context("unsupported OpenClaw gateway URL")?;
        Ok(Some(Self {
            url,
            token: env("OPENCLAW_GATEWAY_TOKEN").or(non_empty(file.token)),
            password: env("OPENCLAW_GATEWAY_PASSWORD").or(non_empty(file.password)),
            recent: Duration::from_secs(
                file.recent_hours.unwrap_or(DEFAULT_RECENT_HOURS).max(1) * 3600,
            ),
        }))
    }

    /// Writes the gateway URL, keeping the file's other fields.
    pub fn save_url(dir: &Path, url: &::url::Url) -> Result<()> {
        let path = dir.join(CONFIG_FILE);
        let mut file = fs::read(&path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes).ok()
            })
            .unwrap_or_default();
        file.insert("url".into(), url.as_str().into());
        write_private(&path, &serde_json::to_vec_pretty(&file)?)
    }

    fn shared_credential(&self) -> Credential<'_> {
        match (&self.token, &self.password) {
            (Some(token), _) => Credential::SharedToken(token),
            (None, Some(password)) => Credential::Password(password),
            (None, None) => Credential::None,
        }
    }

    /// Device tokens are only ever sent back to the gateway that issued them.
    fn origin(&self) -> String {
        origin_of(&self.url)
    }
}

/// The paired device: its Ed25519 secret and the tokens gateways minted for
/// it. Owner-only permissions; never contains conversation data.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeviceFile {
    #[serde(default)]
    secret_key: Option<String>,
    #[serde(default)]
    device_tokens: BTreeMap<String, String>,
}

pub struct DeviceStore {
    path: PathBuf,
    identity: Identity,
    device_tokens: BTreeMap<String, String>,
}

impl DeviceStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(DEVICE_FILE);
        let file = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<DeviceFile>(&bytes).unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DeviceFile::default(),
            Err(error) => return Err(error).context("read OpenClaw device identity"),
        };
        let secret = file
            .secret_key
            .and_then(|key| STANDARD.decode(key).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        let store = Self {
            path,
            identity: match secret {
                Some(secret) => Identity::from_secret(secret),
                None => Identity::generate().context("generate OpenClaw device identity")?,
            },
            device_tokens: file.device_tokens,
        };
        if secret.is_none() {
            store.save()?;
        }
        Ok(store)
    }

    pub fn device_id(&self) -> String {
        self.identity.device_id()
    }

    fn save(&self) -> Result<()> {
        let file = DeviceFile {
            secret_key: Some(STANDARD.encode(self.identity.secret())),
            device_tokens: self.device_tokens.clone(),
        };
        write_private(&self.path, &serde_json::to_vec_pretty(&file)?)
    }

    fn device_token(&self, origin: &str) -> Option<&str> {
        self.device_tokens.get(origin).map(String::as_str)
    }

    fn set_device_token(&mut self, origin: String, token: Option<String>) -> Result<()> {
        let changed = match token {
            Some(token) => self.device_tokens.insert(origin, token.clone()) != Some(token),
            None => self.device_tokens.remove(&origin).is_some(),
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }
}

/// Origin key under which a gateway's device token is stored.
fn origin_of(url: &::url::Url) -> String {
    url.origin().ascii_serialization()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("device file has no parent")?;
    fs::create_dir_all(dir)?;
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

/// One session row, restricted to metadata. `derivedTitle`,
/// `lastMessagePreview`, `activitySummary`, `lastRunError` and the like are
/// neither requested nor decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub key: String,
    pub agent_id: String,
    /// The label typed in OpenClaw, else the gateway's display name.
    pub title: Option<String>,
    pub active: bool,
    pub last_activity: SystemTime,
    pub context: Option<ContextUsage>,
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<SessionRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionRow {
    key: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    updated_at: Option<f64>,
    #[serde(default)]
    last_activity_at: Option<f64>,
    #[serde(default)]
    has_active_run: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    total_tokens: Option<f64>,
    #[serde(default)]
    total_tokens_fresh: Option<bool>,
    #[serde(default)]
    context_tokens: Option<f64>,
}

pub fn list_sessions(
    connection: &mut Connection,
    recent: Duration,
) -> Result<Vec<Session>, GatewayError> {
    let payload = connection.request(
        "sessions.list",
        json!({
            "limit": MAX_SESSIONS,
            "sortBy": "activity",
            "activeMinutes": (recent.as_secs() / 60).max(1),
            "excludeCron": true,
            "excludeSystem": true,
            "excludeSubagents": true,
            "requireLastInteraction": true,
        }),
    )?;
    let list: SessionList = serde_json::from_str(payload.get())
        .map_err(|_| GatewayError::Protocol("sessions-list-payload"))?;
    Ok(list
        .sessions
        .into_iter()
        .filter_map(Session::from_row)
        .collect())
}

impl Session {
    fn from_row(row: SessionRow) -> Option<Self> {
        let agent_id = row
            .agent_id
            .filter(|id| !id.trim().is_empty())
            .map(|id| url::normalize_agent_id(&id))
            .or_else(|| {
                let rest = row
                    .key
                    .get(6..)
                    .filter(|_| row.key.to_ascii_lowercase().starts_with("agent:"))?;
                rest.split_once(':')
                    .map(|(id, _)| url::normalize_agent_id(id))
            })?;
        let millis = row.last_activity_at.or(row.updated_at)?;
        let last_activity = (millis.is_finite() && millis > 0.0)
            .then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(millis as u64))?;
        let title = [row.label, row.display_name]
            .into_iter()
            .flatten()
            .map(|title| title.trim().to_owned())
            .find(|title| !title.is_empty());
        let active = row.has_active_run == Some(true)
            || matches!(row.status.as_deref(), Some("running" | "queued"));
        let context = match (row.total_tokens, row.context_tokens) {
            (Some(used), Some(window)) if window > 0.0 && row.total_tokens_fresh != Some(false) => {
                ContextUsage::new((used / window * 100.0) as f32)
            }
            _ => None,
        };
        Some(Self {
            key: row.key,
            agent_id,
            title,
            active,
            last_activity,
            context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(raw: serde_json::Value) -> Option<Session> {
        Session::from_row(serde_json::from_value(raw).unwrap())
    }

    #[test]
    fn rows_keep_metadata_only() {
        let session = row(json!({
            "key": "agent:Ops:telegram:direct:42",
            "displayName": "  Deploy  ",
            "derivedTitle": "first user message",
            "lastMessagePreview": "latest message",
            "lastActivityAt": 1_700_000_000_000u64,
            "status": "running",
            "totalTokens": 50_000,
            "contextTokens": 100_000,
        }))
        .unwrap();
        assert_eq!(session.agent_id, "ops");
        assert_eq!(session.title.as_deref(), Some("Deploy"));
        assert!(session.active);
        assert_eq!(session.context.unwrap().used_percentage(), 50.0);
        assert_eq!(
            session.last_activity,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        assert!(!format!("{session:?}").contains("message"));
    }

    #[test]
    fn label_wins_and_idle_is_explicit() {
        let session = row(json!({
            "key": "agent:main:main", "agentId": "main", "label": "Mine", "displayName": "Auto",
            "updatedAt": 1_000, "hasActiveRun": false, "status": "done",
            "totalTokens": 10, "totalTokensFresh": false, "contextTokens": 100,
        }))
        .unwrap();
        assert_eq!(session.title.as_deref(), Some("Mine"));
        assert!(!session.active);
        assert!(session.context.is_none());
    }

    #[test]
    fn rows_without_agent_or_time_are_dropped() {
        assert!(row(json!({"key": "global", "updatedAt": 1_000})).is_none());
        assert!(row(json!({"key": "agent:main:main"})).is_none());
    }

    #[test]
    fn configuration_is_optional_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        // Environment overrides would make this test depend on the machine.
        if std::env::var_os("OPENCLAW_GATEWAY_URL").is_some() {
            return;
        }
        assert!(Config::load(dir.path()).unwrap().is_none());
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"url": "http://gw.example:18789/", "token": "s3cret"}"#,
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap().unwrap();
        assert_eq!(config.url.as_str(), "http://gw.example:18789/");
        assert_eq!(config.recent, Duration::from_secs(24 * 3600));
        assert!(!format!("{config:?}").contains("s3cret"));
        fs::write(dir.path().join(CONFIG_FILE), r#"{"url": "ftp://gw"}"#).unwrap();
        assert!(Config::load(dir.path()).is_err());
        fs::write(dir.path().join(CONFIG_FILE), r#"{"recentHours": 2}"#).unwrap();
        Config::save_url(
            dir.path(),
            &::url::Url::parse("https://gw.example/oc").unwrap(),
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap().unwrap();
        assert_eq!(config.url.as_str(), "https://gw.example/oc");
        assert_eq!(config.recent, Duration::from_secs(2 * 3600));
        assert!(config.token.is_none());
    }

    #[test]
    fn device_identity_persists_privately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DeviceStore::open(dir.path()).unwrap();
        let id = store.device_id();
        store
            .set_device_token("http://gw".into(), Some("dt".into()))
            .unwrap();
        let reopened = DeviceStore::open(dir.path()).unwrap();
        assert_eq!(reopened.device_id(), id);
        assert_eq!(
            reopened.device_tokens.get("http://gw").map(String::as_str),
            Some("dt")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(DEVICE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
