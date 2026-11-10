//! Optional, read-only observation of the desktop's private IPC. No approvals,
//! focus commands, task mutations, app-server spawning, or socket repair.
//! All transport/decoding lives off the GPUI thread; stale data fails closed to
//! hook/lifecycle state. Maintenance procedure: docs/codex-desktop-ipc.md.
mod protocol;
mod transport;

use crate::ProviderDiagnostic;
use agentsmon_core::AgentState;
use protocol::{
    decode, valid_id, Fault, Frame, Initialized, RemoteError, Snapshot, SnapshotRoute,
    FOLLOW_VERSION, OWNER_VERSION, STREAM_VERSION,
};
use serde_json::json;
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard,
};
use std::time::{Duration, Instant, SystemTime};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_AGE: Duration = Duration::from_secs(15);
const MAX_TARGETS: usize = 32;
const MAX_MESSAGES: usize = 128;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub id: String,
    pub hook_activity: SystemTime,
}

struct Observation {
    state: AgentState,
    hook_activity: SystemTime,
    received: Instant,
}
#[derive(Default)]
struct Data {
    targets: Vec<Target>,
    observations: HashMap<String, Observation>,
    diagnostic: Option<ProviderDiagnostic>,
}
#[derive(Default)]
struct Shared {
    data: Mutex<Data>,
    changed: AtomicBool,
    stop: AtomicBool,
}
impl Shared {
    fn data(&self) -> MutexGuard<'_, Data> {
        // A worker panic must never poison the application's refresh path.
        self.data
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
    fn fault(&self, fault: Option<&Fault>, path: &std::path::Path, session: Option<&str>) {
        let diagnostic = fault.filter(|f| f.alert).map(|f| ProviderDiagnostic {
            code: f.code,
            summary: "Codex approval tracking is degraded.",
            details: format!("adapter=codex-desktop-ipc\nphase={}\ncode={}\n{}\nexpected_versions: initialize=0; owner={OWNER_VERSION}; follow={FOLLOW_VERSION}; stream={STREAM_VERSION}\nsocket={}\nsession={}\nframe_limit_bytes={}\nrequest_timeout_ms={}\nfallback=hooks_and_lifecycle\n", f.phase, f.code, f.detail, path.display(), session.unwrap_or("none"), protocol::MAX_FRAME, REQUEST_TIMEOUT.as_millis()),
        });
        let mut data = self.data();
        if fault.is_some() && !data.observations.is_empty() {
            data.observations.clear();
            self.changed.store(true, Ordering::Relaxed);
        }
        if data.diagnostic != diagnostic {
            let had_diagnostic = data.diagnostic.is_some();
            data.diagnostic = diagnostic.clone();
            drop(data);
            self.changed.store(true, Ordering::Relaxed);
            // A blocked tracing writer must not keep the mutex needed by GPUI.
            if let Some(d) = &diagnostic {
                tracing::warn!(target: "agentsmon::codex_ipc", code = d.code, details = %d.details, "Codex desktop IPC incompatible or unresponsive; falling back to hook/lifecycle state");
            } else if had_diagnostic {
                tracing::info!(target: "agentsmon::codex_ipc", "Codex desktop IPC diagnostic cleared; see current connection availability");
            }
        }
    }
    fn publish(&self, target: &Target, state: Option<AgentState>) {
        let mut data = self.data();
        // The user may have completed/hidden the task while the worker waited.
        if !data.targets.contains(target) {
            return;
        }
        if let Some(state) = state {
            let changed = data.observations.get(&target.id).is_none_or(|o| {
                o.state != state
                    || o.hook_activity != target.hook_activity
                    || o.received.elapsed() >= MAX_AGE
            });
            data.observations.insert(
                target.id.clone(),
                Observation {
                    state,
                    hook_activity: target.hook_activity,
                    received: Instant::now(),
                },
            );
            if changed {
                self.changed.store(true, Ordering::Relaxed);
            }
        } else if data.observations.remove(&target.id).is_some() {
            self.changed.store(true, Ordering::Relaxed);
        }
    }
}

pub(super) struct Monitor {
    shared: Arc<Shared>,
    wake: crossbeam_channel::Sender<()>,
}
impl Monitor {
    pub fn start(path: PathBuf) -> Self {
        let shared = Arc::new(Shared::default());
        let (wake, receiver) = crossbeam_channel::bounded(1);
        let worker = shared.clone();
        let worker_path = path.clone();
        let result = std::thread::Builder::new()
            .name("agentsmon-codex-ipc".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&worker_path, &worker, &receiver)
                }))
                .is_err()
                {
                    worker.fault(
                        Some(&Fault::protocol(
                            "worker_panicked",
                            "worker",
                            "worker stopped; restart AgentsMon after checking diagnostics",
                        )),
                        &worker_path,
                        None,
                    );
                }
            });
        if result.is_err() {
            shared.fault(
                Some(&Fault::protocol(
                    "worker_start_failed",
                    "worker",
                    "cannot create IPC worker thread",
                )),
                &path,
                None,
            );
        }
        Self { shared, wake }
    }

    /// Caller supplies only live non-idle sessions, newest activity first.
    pub fn update_targets(&self, mut targets: Vec<Target>) {
        targets.retain(|t| valid_id(&t.id));
        targets.truncate(MAX_TARGETS);
        let mut data = self.shared.data();
        if data.targets == targets {
            return;
        }
        data.observations.retain(|id, o| {
            targets
                .iter()
                .any(|t| &t.id == id && t.hook_activity == o.hook_activity)
        });
        data.targets = targets;
        let _ = self.wake.try_send(());
    }

    pub fn state(&self, id: &str, hook_activity: SystemTime) -> Option<AgentState> {
        let data = self.shared.data();
        let observation = data.observations.get(id)?;
        (observation.hook_activity == hook_activity && observation.received.elapsed() < MAX_AGE)
            .then_some(observation.state)
    }
    pub fn take_changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::Relaxed)
    }
    pub fn diagnostic(&self) -> Option<ProviderDiagnostic> {
        self.shared.data().diagnostic.clone()
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
        // Do not join from the UI thread. All I/O observes stop/deadline; dropping
        // the connection also unregisters any temporary follower at the router.
    }
}

fn run(path: &std::path::Path, shared: &Shared, wake: &crossbeam_channel::Receiver<()>) {
    let mut client = None;
    let mut retry = SAMPLE_INTERVAL;
    while !shared.stop.load(Ordering::Relaxed) {
        // Consume the wake that scheduled THIS sample; otherwise a newly added
        // target gets sampled twice immediately, bypassing SAMPLE_INTERVAL.
        while wake.try_recv().is_ok() {}
        let targets = shared.data().targets.clone();
        let mut delay = SAMPLE_INTERVAL;
        let mut failed = false;
        let mut sampled = false;
        if targets.is_empty() {
            client = None;
            shared.fault(None, path, None);
        }
        for target in targets {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            let result = (|| {
                if client.is_none() {
                    client = Some(Client::connect(path, &shared.stop)?);
                }
                // A successful connection is guaranteed above; avoid panicking
                // even if this control flow changes during future maintenance.
                let Some(client) = client.as_mut() else {
                    return Ok(None);
                };
                client.sample(&target.id, &shared.stop)
            })();
            match result {
                Ok(state) => {
                    sampled |= state.is_some();
                    shared.publish(&target, state);
                }
                Err(fault) => {
                    tracing::debug!(target: "agentsmon::codex_ipc", code = fault.code, phase = fault.phase, detail = %fault.detail, "IPC observation unavailable");
                    shared.fault(Some(&fault), path, Some(&target.id));
                    client = None;
                    failed = true;
                    retry = (retry * 2).min(Duration::from_secs(60));
                    delay = if fault.alert {
                        Duration::from_secs(60)
                    } else {
                        retry
                    };
                    break;
                }
            }
        }
        if !failed && sampled {
            shared.fault(None, path, None);
            retry = SAMPLE_INTERVAL;
        }
        // Only target changes wake normal sampling. Protocol failures always
        // back off, even while hooks generate a stream of updates.
        if failed {
            let until = Instant::now() + delay;
            while !shared.stop.load(Ordering::Relaxed) && Instant::now() < until {
                let _ = wake.recv_timeout(Duration::from_millis(250));
            }
        } else {
            let _ = wake.recv_timeout(delay);
        }
    }
}

struct Client {
    stream: UnixStream,
    id: String,
    sequence: u64,
}
impl Client {
    fn connect(path: &std::path::Path, stop: &AtomicBool) -> Result<Self, Fault> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let stream = transport::connect(path, deadline, stop)?;
        let mut client = Self {
            stream,
            id: "initializing-client".into(),
            sequence: 0,
        };
        let request = client.request(
            "initialize",
            0,
            json!({"clientType":"agentsmon"}),
            deadline,
            stop,
            "initialize",
        )?;
        let bytes = client.response(&request, deadline, stop, "initialize")?;
        let frame: Frame<'_> = decode(&bytes, "initialize")?;
        if frame.result_type != Some("success") || frame.method != Some("initialize") {
            return Err(Fault::protocol(
                "initialize_rejected",
                "initialize",
                "unexpected initialization response",
            ));
        }
        let initialized: Initialized = decode(
            frame
                .result
                .ok_or_else(|| {
                    Fault::protocol("initialize_result_missing", "initialize", "no result")
                })?
                .get()
                .as_bytes(),
            "initialize",
        )?;
        if !valid_id(&initialized.client_id) {
            return Err(Fault::protocol(
                "invalid_client_id",
                "initialize",
                "client ID is not a UUID",
            ));
        }
        client.id = initialized.client_id;
        Ok(client)
    }

    fn request(
        &mut self,
        method: &str,
        version: u32,
        params: serde_json::Value,
        deadline: Instant,
        stop: &AtomicBool,
        phase: &'static str,
    ) -> Result<String, Fault> {
        self.sequence = self.sequence.wrapping_add(1);
        let request = format!("agentsmon-{}-{}", std::process::id(), self.sequence);
        transport::write(
            &mut self.stream,
            json!({"type":"request","requestId":request,"sourceClientId":self.id,"version":version,"method":method,"params":params,"timeoutMs":2000}),
            deadline,
            stop,
            phase,
        )?;
        Ok(request)
    }

    fn response(
        &mut self,
        id: &str,
        deadline: Instant,
        stop: &AtomicBool,
        phase: &'static str,
    ) -> Result<Vec<u8>, Fault> {
        for _ in 0..MAX_MESSAGES {
            let bytes = self.receive(deadline, stop, phase)?;
            let frame: Frame<'_> = decode(&bytes, phase)?;
            if frame.kind == "response" && frame.request_id == Some(id) {
                return Ok(bytes);
            }
        }
        Err(Fault::protocol(
            "message_limit",
            phase,
            "too many unrelated messages before response",
        ))
    }

    fn receive(
        &mut self,
        deadline: Instant,
        stop: &AtomicBool,
        phase: &'static str,
    ) -> Result<Vec<u8>, Fault> {
        let bytes = transport::read(&mut self.stream, deadline, stop, phase)?;
        let frame: Frame<'_> = decode(&bytes, phase)?;
        // This observer offers no commands/services. In particular it never
        // claims ownership of a task or responds to an approval request.
        if frame.kind == "client-discovery-request" {
            if let Some(id) = frame.request_id.filter(|id| valid_id(id)) {
                transport::write(
                    &mut self.stream,
                    json!({"type":"client-discovery-response","requestId":id,"response":{"canHandle":false}}),
                    deadline,
                    stop,
                    phase,
                )?;
            }
        }
        Ok(bytes)
    }

    fn follow(
        &mut self,
        session: &str,
        owner: &str,
        following: bool,
        stop: &AtomicBool,
    ) -> Result<(), Fault> {
        transport::write(
            &mut self.stream,
            json!({"type":"broadcast","method":"thread-stream-following-changed","sourceClientId":self.id,"version":FOLLOW_VERSION,"targetClientIds":[owner],"params":{"conversationId":session,"hostId":"local","following":following}}),
            Instant::now() + REQUEST_TIMEOUT,
            stop,
            "follow",
        )
    }

    fn sample(&mut self, session: &str, stop: &AtomicBool) -> Result<Option<AgentState>, Fault> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let request = self.request(
            "thread-owner-discovery",
            OWNER_VERSION,
            json!({"hostId":"local","conversationId":session}),
            deadline,
            stop,
            "owner",
        )?;
        let bytes = self.response(&request, deadline, stop, "owner")?;
        let frame: Frame<'_> = decode(&bytes, "owner")?;
        if frame.result_type == Some("error") && frame.error == Some(RemoteError::NoClient) {
            return Ok(None);
        }
        if frame.result_type != Some("success") || frame.method != Some("thread-owner-discovery") {
            return Err(Fault::protocol(
                "owner_response_changed",
                "owner",
                "unexpected owner discovery response (remote error omitted)",
            ));
        }
        let owner = frame
            .handled_by_client_id
            .filter(|id| valid_id(id))
            .ok_or_else(|| {
                Fault::protocol(
                    "invalid_owner_id",
                    "owner",
                    "owner ID missing or not a UUID",
                )
            })?;
        self.follow(session, owner, true, stop)?;
        let result = self.snapshot(session, owner, stop);
        // Unsubscribe on EVERY outcome. If this fails, the caller drops the
        // socket, which unregisters the observer and releases the subscription.
        let unsubscribe = self.follow(session, owner, false, stop);
        match result {
            Err(error) => Err(error),
            Ok(state) => {
                unsubscribe?;
                Ok(state)
            }
        }
    }

    fn snapshot(
        &mut self,
        session: &str,
        owner: &str,
        stop: &AtomicBool,
    ) -> Result<Option<AgentState>, Fault> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        for _ in 0..MAX_MESSAGES {
            let bytes = self.receive(deadline, stop, "snapshot")?;
            let frame: Frame<'_> = decode(&bytes, "snapshot")?;
            if frame.kind != "broadcast"
                || frame.method != Some("thread-stream-state-changed")
                || frame.source_client_id != Some(owner)
            {
                continue;
            }
            let params = frame
                .params
                .ok_or_else(|| Fault::protocol("snapshot_missing", "snapshot", "params absent"))?;
            // A window can own several tasks. Ignore another task BEFORE
            // validating its version/schema/status: it is not our observation.
            let route: SnapshotRoute<'_> = decode(params.get().as_bytes(), "snapshot_route")?;
            if route.conversation_id != session || route.host_id != "local" {
                continue;
            }
            if frame.version != Some(STREAM_VERSION) {
                return Err(Fault::protocol(
                    "stream_version_changed",
                    "snapshot",
                    format!("expected={STREAM_VERSION}; observed={:?}", frame.version),
                ));
            }
            let snapshot: Snapshot<'_> = decode(params.get().as_bytes(), "snapshot")?;
            if snapshot.conversation_id != session || snapshot.host_id != "local" {
                continue;
            }
            match snapshot.change.kind {
                "patches" => continue, // Never decode a patch path/value; await full state.
                "snapshot" => {
                    let state = snapshot.change.conversation_state.ok_or_else(|| {
                        Fault::protocol(
                            "snapshot_state_missing",
                            "snapshot",
                            "conversationState absent",
                        )
                    })?;
                    if state.id != session {
                        return Err(Fault::protocol(
                            "session_mismatch",
                            "snapshot",
                            "snapshot ID differs from requested session",
                        ));
                    }
                    return state.thread_runtime_status.state();
                }
                _ => {
                    return Err(Fault::protocol(
                        "unknown_change_type",
                        "snapshot",
                        "unrecognized change type (value omitted)",
                    ))
                }
            }
        }
        Err(Fault::protocol(
            "message_limit",
            "snapshot",
            "too many unrelated messages before snapshot",
        ))
    }
}

#[cfg(test)]
mod tests;
