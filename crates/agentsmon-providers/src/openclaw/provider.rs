//! The OpenClaw provider and its worker thread. All network I/O happens on the
//! worker; the GPUI thread only reads the cached state. Credential failures
//! park the worker until the user acts: OpenClaw rate-limits failed auth per
//! client IP, and a retry loop could lock the user's own browser out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentsmon_core::{AgentId, AgentSnapshot, AgentState, FocusTarget, ProviderUsage};
use anyhow::Result;
use notify::{RecursiveMode, Watcher};

use super::gateway::{Connection, Credential, GatewayError};
use super::{list_sessions, origin_of, url::session_url, Config, DeviceStore, Session};
use crate::{focus, AgentProvider, ProviderWatcher};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const PAIRING_RETRY: Duration = Duration::from_secs(3);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// While unconfigured, pick up a hand-written configuration file.
const CONFIG_RECHECK: Duration = Duration::from_secs(30);

/// Progress of a pairing started from the connect window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PairingState {
    #[default]
    Idle,
    Connecting,
    /// Waiting for `openclaw devices approve <request_id>` on the gateway host.
    /// `paused`: the gateway asked for no automatic retry.
    AwaitingApproval {
        request_id: Option<String>,
        paused: bool,
    },
    Connected,
    /// Content-free gateway or client code.
    Failed {
        code: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    NotConfigured,
    Connecting,
    Connected,
    /// Transport failure; retried with backoff, credentials still trusted.
    Offline,
    /// Credentials refused or pairing needed: waits for the connect window.
    NeedsCredentials,
}

struct PairingJob {
    url: ::url::Url,
    token: String,
    started: Instant,
}

struct Data {
    link: Link,
    base_url: Option<::url::Url>,
    sessions: Vec<Session>,
    pairing: Option<PairingJob>,
    pairing_state: PairingState,
    kick: bool,
    stop: bool,
}

struct Shared {
    data: Mutex<Data>,
    wake: Condvar,
    changed: AtomicBool,
}

impl Shared {
    fn data(&self) -> MutexGuard<'_, Data> {
        // A worker panic must never poison the application's refresh path.
        self.data
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut Data)) {
        change(&mut self.data());
        self.changed.store(true, Ordering::Relaxed);
    }

    fn set_link(&self, link: Link) {
        let mut data = self.data();
        if data.link != link {
            data.link = link;
            if link != Link::Connected {
                data.sessions.clear();
            }
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    fn kick(&self) {
        self.data().kick = true;
        self.wake.notify_all();
    }

    /// Sleeps up to `timeout`, or until kicked when `None`. True when kicked
    /// or stopping.
    fn wait(&self, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut data = self.data();
        while !data.kick && !data.stop {
            data = match deadline {
                None => self
                    .wake
                    .wait(data)
                    .unwrap_or_else(|poison| poison.into_inner()),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    self.wake
                        .wait_timeout(data, deadline - now)
                        .unwrap_or_else(|poison| poison.into_inner())
                        .0
                }
            };
        }
        data.kick = false;
        true
    }
}

pub struct OpenClawProvider {
    root: PathBuf,
    shared: Arc<Shared>,
}

impl OpenClawProvider {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir())
    }

    /// `None` disables the provider (no data directory on this system).
    pub fn with_dir(dir: Option<PathBuf>) -> Self {
        let config = dir
            .as_deref()
            .and_then(|dir| Config::load(dir).ok().flatten());
        let shared = Arc::new(Shared {
            data: Mutex::new(Data {
                link: if config.is_some() {
                    Link::Connecting
                } else {
                    Link::NotConfigured
                },
                base_url: config.map(|config| config.url),
                sessions: Vec::new(),
                pairing: None,
                pairing_state: PairingState::Idle,
                kick: false,
                stop: false,
            }),
            wake: Condvar::new(),
            changed: AtomicBool::new(false),
        });
        if let Some(dir) = dir.clone() {
            let worker = shared.clone();
            let spawned = std::thread::Builder::new()
                .name("openclaw-gateway".into())
                .spawn(move || run(&worker, &dir));
            if let Err(error) = spawned {
                tracing::warn!(target: "agentsmon::openclaw", %error, "cannot start the OpenClaw worker");
            }
        }
        Self {
            root: dir.unwrap_or_default(),
            shared,
        }
    }

    /// True when OpenClaw has no working credential: never configured,
    /// refused, or waiting for pairing. A dropped socket does not count.
    pub fn needs_connection(&self) -> bool {
        matches!(
            self.shared.data().link,
            Link::NotConfigured | Link::NeedsCredentials
        )
    }

    /// The gateway URL to pre-fill the connect window with.
    pub fn configured_url(&self) -> Option<String> {
        self.shared
            .data()
            .base_url
            .as_ref()
            .map(|url| url.to_string())
    }

    /// Starts pairing with the gateway's shared token, kept in memory only.
    /// Fails immediately, with a code, when the URL is unusable.
    pub fn pair(&self, url: &str, token: &str) -> Result<(), &'static str> {
        let validated = ::url::Url::parse(url.trim())
            .ok()
            .filter(|url| super::gateway::websocket_url(url).is_ok())
            .ok_or("invalid-url")
            .and_then(|url| {
                let token = token.trim();
                (!token.is_empty())
                    .then(|| (url, token.to_owned()))
                    .ok_or("missing-token")
            });
        let (url, token) = match validated {
            Ok(valid) => valid,
            Err(code) => {
                // The connect window shows the pairing state: record the
                // refusal there so a stale state does not overwrite it.
                self.shared.update(|data| {
                    if data.pairing.is_none() {
                        data.pairing_state = PairingState::Failed { code: code.into() };
                    }
                });
                return Err(code);
            }
        };
        self.shared.update(|data| {
            data.pairing = Some(PairingJob {
                url,
                token,
                started: Instant::now(),
            });
            data.pairing_state = PairingState::Connecting;
        });
        self.shared.kick();
        Ok(())
    }

    /// Retry now, e.g. after approving on the gateway host.
    pub fn retry_pairing(&self) {
        self.shared.kick();
    }

    pub fn cancel_pairing(&self) {
        self.shared.update(|data| {
            data.pairing = None;
            data.pairing_state = PairingState::Idle;
        });
        self.shared.kick();
    }

    /// Forgets the outcome of a finished pairing, e.g. when a new connect
    /// window opens. A pairing in progress keeps its state.
    pub fn clear_pairing_status(&self) {
        let mut data = self.shared.data();
        if data.pairing.is_none() {
            data.pairing_state = PairingState::Idle;
        }
    }

    pub fn pairing_state(&self) -> PairingState {
        self.shared.data().pairing_state.clone()
    }
}

impl Default for OpenClawProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OpenClawProvider {
    fn drop(&mut self) {
        self.shared.data().stop = true;
        self.shared.wake.notify_all();
    }
}

impl AgentProvider for OpenClawProvider {
    fn id(&self) -> &'static str {
        "openclaw"
    }

    fn display_name(&self) -> &'static str {
        "OpenClaw"
    }

    fn icon_png(&self) -> Option<&'static [u8]> {
        Some(include_bytes!("../../assets/openclaw-16.png"))
    }

    fn accent_rgb(&self) -> Option<u32> {
        // The lobster red of the OpenClaw mark.
        Some(0xff4d4d)
    }

    fn is_enabled(&self) -> bool {
        self.shared.data().link != Link::NotConfigured
    }

    fn discover(&self) -> Result<Vec<AgentSnapshot>> {
        let data = self.shared.data();
        let Some(base) = data.base_url.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(data
            .sessions
            .iter()
            .map(|session| snapshot(self.id(), base, session))
            .collect())
    }

    fn usage(&self) -> Result<Option<ProviderUsage>> {
        Ok(None)
    }

    fn focus(&self, target: &FocusTarget) -> Result<()> {
        focus::focus(target)
    }

    fn watch(&self) -> Result<ProviderWatcher> {
        std::fs::create_dir_all(&self.root)?;
        let (tx, changes) = crossbeam_channel::bounded(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = tx.try_send(());
                }
            })?;
        watcher.watch(&self.root, RecursiveMode::NonRecursive)?;
        Ok(ProviderWatcher::new(watcher, changes))
    }

    fn runtime_root(&self) -> &Path {
        &self.root
    }

    fn take_changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::Relaxed)
    }
}

fn snapshot(provider: &str, base: &::url::Url, session: &Session) -> AgentSnapshot {
    // No working directory: sessions of one OpenClaw agent share a card.
    let cwd = PathBuf::from(format!("openclaw/{}", session.agent_id));
    AgentSnapshot {
        id: AgentId::new(provider, session.key.clone()),
        pid: 0,
        project_name: session.agent_id.clone(),
        cwd,
        title: session.title.clone(),
        custom_name: None,
        state: if session.active {
            AgentState::Active
        } else {
            AgentState::Idle
        },
        context_usage: session.context,
        provider_usage: None,
        focus_target: FocusTarget {
            uri: session_url(base, &session.key, Some(&session.agent_id)),
            ..FocusTarget::default()
        },
        last_activity: session.last_activity,
    }
}

fn warn(error: &GatewayError, phase: &'static str) {
    tracing::warn!(target: "agentsmon::openclaw", phase, code = %error.code(), "OpenClaw gateway");
}

fn run(shared: &Shared, dir: &Path) {
    let mut store = match DeviceStore::open(dir) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(target: "agentsmon::openclaw", %error, "OpenClaw device identity unavailable");
            shared.set_link(Link::NeedsCredentials);
            return;
        }
    };
    let mut backoff = POLL_INTERVAL;
    loop {
        let job = {
            let data = shared.data();
            if data.stop {
                return;
            }
            data.pairing
                .as_ref()
                .map(|job| (job.url.clone(), job.token.clone(), job.started))
        };
        if let Some((url, token, started)) = job {
            if let Some(connection) = pair_once(shared, dir, &mut store, &url, &token, started) {
                backoff = POLL_INTERVAL;
                let recent = Config::load(dir)
                    .ok()
                    .flatten()
                    .map_or(Duration::from_secs(24 * 3600), |config| config.recent);
                poll(shared, connection, recent);
            }
            continue;
        }

        let config = match Config::load(dir) {
            Ok(Some(config)) => config,
            Ok(None) => {
                shared.set_link(Link::NotConfigured);
                shared.wait(Some(CONFIG_RECHECK));
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "agentsmon::openclaw", %error, "OpenClaw configuration unusable");
                shared.set_link(Link::NeedsCredentials);
                shared.wait(None);
                continue;
            }
        };
        shared.update(|data| data.base_url = Some(config.url.clone()));
        let device_token = store.device_token(&config.origin()).map(str::to_owned);
        let credential = match &device_token {
            Some(token) => Credential::Device(token),
            None => config.shared_credential(),
        };
        if matches!(credential, Credential::None) {
            shared.set_link(Link::NeedsCredentials);
            shared.wait(None);
            continue;
        }
        match Connection::connect(&config.url, &store.identity, credential) {
            Ok((connection, hello)) => {
                // New or rotated: the next reconnect must use the latest token.
                if let Some(token) = hello.device_token {
                    let _ = store.set_device_token(config.origin(), Some(token));
                }
                backoff = POLL_INTERVAL;
                shared.set_link(Link::Connected);
                poll(shared, connection, config.recent);
            }
            Err(error @ (GatewayError::Transport(_) | GatewayError::Protocol(_))) => {
                warn(&error, "connect");
                shared.set_link(Link::Offline);
                shared.wait(Some(backoff));
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(error) => {
                warn(&error, "connect");
                // Only an explicit device-token refusal proves the stored token
                // is dead; anything else keeps it for the next attempt.
                if device_token.is_some()
                    && matches!(&error, GatewayError::Auth { code } if code == "AUTH_DEVICE_TOKEN_MISMATCH")
                {
                    let _ = store.set_device_token(config.origin(), None);
                }
                shared.set_link(Link::NeedsCredentials);
                shared.wait(None);
            }
        }
    }
}

/// One pairing attempt with the shared token. Returns the live connection once
/// the gateway has minted a device token and it is stored.
fn pair_once(
    shared: &Shared,
    dir: &Path,
    store: &mut DeviceStore,
    url: &::url::Url,
    token: &str,
    started: Instant,
) -> Option<Connection> {
    let finish = |state: PairingState| {
        shared.update(|data| {
            data.pairing = None;
            data.pairing_state = state;
        });
    };
    match Connection::connect(url, &store.identity, Credential::SharedToken(token)) {
        Ok((connection, hello)) => {
            // The shared secret is never stored: without a device token the
            // next reconnect would have no credential.
            let Some(device_token) = hello.device_token else {
                finish(PairingState::Failed {
                    code: "no-device-token".into(),
                });
                return None;
            };
            if store
                .set_device_token(origin_of(url), Some(device_token))
                .and_then(|()| Config::save_url(dir, url))
                .is_err()
            {
                finish(PairingState::Failed {
                    code: "save-failed".into(),
                });
                return None;
            }
            shared.update(|data| {
                data.pairing = None;
                data.pairing_state = PairingState::Connected;
                data.base_url = Some(url.clone());
            });
            shared.set_link(Link::Connected);
            Some(connection)
        }
        Err(GatewayError::PairingRequired { request_id, pause }) => {
            if started.elapsed() > PAIRING_TIMEOUT {
                finish(PairingState::Failed {
                    code: "pairing-timeout".into(),
                });
                return None;
            }
            shared.update(|data| {
                data.pairing_state = PairingState::AwaitingApproval {
                    request_id,
                    paused: pause,
                };
            });
            shared.wait((!pause).then_some(PAIRING_RETRY));
            None
        }
        Err(error) => {
            warn(&error, "pair");
            finish(PairingState::Failed { code: error.code() });
            None
        }
    }
}

/// Lists sessions every few seconds until the connection fails, a pairing
/// starts or the provider stops.
fn poll(shared: &Shared, mut connection: Connection, recent: Duration) {
    loop {
        match list_sessions(&mut connection, recent) {
            Ok(sessions) => {
                let mut data = shared.data();
                if data.sessions != sessions {
                    data.sessions = sessions;
                    shared.changed.store(true, Ordering::Relaxed);
                }
            }
            Err(error @ (GatewayError::Transport(_) | GatewayError::Protocol(_))) => {
                warn(&error, "sessions.list");
                shared.set_link(Link::Offline);
                return;
            }
            Err(error) => {
                // e.g. a missing scope: reconnecting would fail the same way.
                warn(&error, "sessions.list");
                shared.set_link(Link::NeedsCredentials);
                shared.wait(None);
                return;
            }
        }
        if shared.wait(Some(POLL_INTERVAL)) {
            let data = shared.data();
            if data.stop || data.pairing.is_some() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_provider_is_hidden_and_offers_connection() {
        let dir = tempfile::tempdir().unwrap();
        if std::env::var_os("OPENCLAW_GATEWAY_URL").is_some() {
            return;
        }
        let provider = OpenClawProvider::with_dir(Some(dir.path().to_owned()));
        assert!(!provider.is_enabled());
        assert!(provider.needs_connection());
        assert!(provider.discover().unwrap().is_empty());
        assert_eq!(provider.pairing_state(), PairingState::Idle);
        assert_eq!(provider.pair("not a url", "t"), Err("invalid-url"));
        assert_eq!(
            provider.pairing_state(),
            PairingState::Failed {
                code: "invalid-url".into()
            }
        );
        assert_eq!(
            provider.pair("http://gw.example", " "),
            Err("missing-token")
        );
        assert_eq!(
            provider.pairing_state(),
            PairingState::Failed {
                code: "missing-token".into()
            }
        );
        provider.clear_pairing_status();
        assert_eq!(provider.pairing_state(), PairingState::Idle);
    }

    #[test]
    fn snapshots_link_to_the_control_ui_and_group_by_agent() {
        let base = ::url::Url::parse("http://gw.example:18789/").unwrap();
        let session = Session {
            key: "agent:ops:telegram:direct:42".into(),
            agent_id: "ops".into(),
            title: Some("Deploy".into()),
            active: true,
            last_activity: std::time::SystemTime::UNIX_EPOCH,
            context: None,
        };
        let snapshot = snapshot("openclaw", &base, &session);
        assert_eq!(snapshot.project_name, "ops");
        assert_eq!(snapshot.cwd, PathBuf::from("openclaw/ops"));
        assert_eq!(snapshot.state, AgentState::Active);
        assert_eq!(
            snapshot.focus_target.uri.as_deref(),
            Some("http://gw.example:18789/chat/ops/telegram/direct/42")
        );
    }
}
