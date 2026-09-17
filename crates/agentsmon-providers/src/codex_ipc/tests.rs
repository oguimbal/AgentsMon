use super::*;
use std::fs;
use std::io::Write;
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::thread::JoinHandle;

const SESSION: &str = "00000000-0000-0000-0000-000000000001";
const CLIENT: &str = "00000000-0000-0000-0000-000000000002";
const OWNER: &str = "00000000-0000-0000-0000-000000000003";

fn status(bytes: &[u8]) -> Result<Option<AgentState>, Fault> {
    decode::<protocol::RuntimeStatus>(bytes, "snapshot")?.state()
}

#[test]
fn status_contract_and_unknown_values_fail_closed_without_echoing_content() {
    assert_eq!(
        status(br#"{"type":"active","activeFlags":["waitingOnApproval"]}"#).unwrap(),
        Some(AgentState::Waiting)
    );
    assert_eq!(
        status(br#"{"type":"active","activeFlags":["waitingOnUserInput"]}"#).unwrap(),
        Some(AgentState::Waiting)
    );
    assert_eq!(
        status(br#"{"type":"active","activeFlags":[]}"#).unwrap(),
        Some(AgentState::Active)
    );
    assert_eq!(
        status(br#"{"type":"idle"}"#).unwrap(),
        Some(AgentState::Idle)
    );
    for kind in ["notLoaded", "systemError"] {
        assert_eq!(
            status(format!(r#"{{"type":"{kind}"}}"#).as_bytes()).unwrap(),
            None
        );
    }
    for (wire, code) in [
        (r#"{"type":"active"}"#, "missing_active_flags"),
        (
            r#"{"type":"active","activeFlags":["PRIVATE_UNKNOWN_FLAG"]}"#,
            "unknown_active_flag",
        ),
        (
            r#"{"type":"PRIVATE_UNKNOWN_STATUS"}"#,
            "unknown_runtime_status",
        ),
        (
            r#"{"type":{"private":"PRIVATE_CONTENT"}}"#,
            "schema_changed",
        ),
        (r#"{"type":"active","activeFlags":null}"#, "schema_changed"),
    ] {
        let fault = status(wire.as_bytes()).unwrap_err();
        assert_eq!(fault.code, code);
        assert!(!fault.detail.contains("PRIVATE"));
    }
    let many_flags = json!({"type":"active","activeFlags":vec!["waitingOnApproval";17]});
    assert_eq!(
        status(many_flags.to_string().as_bytes()).unwrap_err().code,
        "schema_changed"
    );
}

fn snapshot(version: u32, runtime: serde_json::Value) -> serde_json::Value {
    json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":OWNER,"version":version,
        "params":{"hostId":"local","conversationId":SESSION,"change":{"type":"snapshot","revision":42,
        "conversationState":{"id":SESSION,"threadRuntimeStatus":runtime,
            "title":{"intentionally":"not a string"},"turns":[{"text":"PRIVATE_CONVERSATION_DO_NOT_DECODE"}],
            "requests":[{"params":{"message":"PRIVATE_ELICITATION"}}],"unrecognizedField":{"nested":[1,2,3]}}}}})
}

fn recv(peer: &mut UnixStream) -> serde_json::Value {
    let bytes = transport::read(
        peer,
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
        "test",
    )
    .unwrap();
    // Test-generated traffic only. Production never uses Value for responses.
    serde_json::from_slice(&bytes).unwrap()
}
fn send(peer: &mut UnixStream, value: serde_json::Value) {
    transport::write(
        peer,
        value,
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
        "test",
    )
    .unwrap();
}

fn fake_router(
    handler: impl FnOnce(UnixStream) + Send + 'static,
) -> (tempfile::TempDir, PathBuf, JoinHandle<()>) {
    let dir = tempfile::Builder::new()
        .prefix("am-ipc-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("ipc.sock");
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((peer, _)) => {
                    peer.set_nonblocking(true).unwrap();
                    handler(peer);
                    return;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fake router did not accept: {error}"),
            }
        }
    });
    (dir, path, handle)
}

fn initialize(peer: &mut UnixStream) {
    let request = recv(peer);
    assert_eq!(request["method"], "initialize");
    assert_eq!(request["version"], 0);
    assert_eq!(request["params"]["clientType"], "agentsmon");
    send(
        peer,
        json!({"type":"response","requestId":request["requestId"],"resultType":"success","method":"initialize","result":{"clientId":CLIENT}}),
    );
}
fn owner(peer: &mut UnixStream) {
    let request = recv(peer);
    assert_eq!(request["method"], "thread-owner-discovery");
    assert_eq!(request["version"], 1);
    assert_eq!(
        request["params"],
        json!({"hostId":"local","conversationId":SESSION})
    );
    send(
        peer,
        json!({"type":"response","requestId":request["requestId"],"resultType":"success","method":"thread-owner-discovery","handledByClientId":OWNER,"result":{}}),
    );
    let following = recv(peer);
    assert_eq!(following["method"], "thread-stream-following-changed");
    assert_eq!(following["version"], 1);
    assert_eq!(following["targetClientIds"], json!([OWNER]));
    assert_eq!(following["params"]["following"], true);
}
fn unsubscribed(peer: &mut UnixStream) {
    let request = recv(peer);
    assert_eq!(request["method"], "thread-stream-following-changed");
    assert_eq!(request["params"]["following"], false);
}

#[test]
fn full_protocol_reads_status_only_and_unsubscribes_on_success_and_failure() {
    for (version, runtime, expected) in [
        (
            11,
            json!({"type":"active","activeFlags":["waitingOnApproval"]}),
            Ok(Some(AgentState::Waiting)),
        ),
        (
            11,
            json!({"type":"active","activeFlags":[]}),
            Ok(Some(AgentState::Active)),
        ),
        (
            12,
            json!({"type":"active","activeFlags":[]}),
            Err("stream_version_changed"),
        ),
        (11, json!({"type":"active"}), Err("missing_active_flags")),
        (11, json!({"type":"notLoaded"}), Ok(None)),
    ] {
        let (_dir, path, router) = fake_router(move |mut peer| {
            initialize(&mut peer);
            owner(&mut peer);
            send(&mut peer, snapshot(version, runtime));
            unsubscribed(&mut peer);
        });
        let stop = AtomicBool::new(false);
        let mut client = Client::connect(&path, &stop).unwrap();
        assert_eq!(client.sample(SESSION, &stop).map_err(|e| e.code), expected);
        router.join().unwrap();
    }
}

#[test]
fn ignores_unrelated_sources_and_partial_updates_and_refuses_to_handle_commands() {
    let (_dir, path, router) = fake_router(|mut peer| {
        initialize(&mut peer);
        owner(&mut peer);
        send(
            &mut peer,
            json!({"type":"client-discovery-request","requestId":SESSION,"request":{"method":"thread-follower-submit-mcp-server-elicitation-response"}}),
        );
        assert_eq!(recv(&mut peer)["response"]["canHandle"], false);
        let mut unrelated = snapshot(500, json!({"type":"idle"}));
        unrelated["sourceClientId"] = json!(CLIENT);
        send(&mut peer, unrelated);
        let mut other_task = snapshot(500, json!({"private_unknown_shape":true}));
        other_task["params"]["conversationId"] = json!(CLIENT);
        send(&mut peer, other_task);
        send(
            &mut peer,
            json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":OWNER,"version":11,
            "params":{"hostId":"local","conversationId":SESSION,"change":{"type":"patches","patches":[{"path":["turns",0],"value":"PRIVATE_PATCH"}]}}}),
        );
        send(&mut peer, snapshot(11, json!({"type":"idle"})));
        unsubscribed(&mut peer);
    });
    let stop = AtomicBool::new(false);
    let mut client = Client::connect(&path, &stop).unwrap();
    assert_eq!(
        client.sample(SESSION, &stop).unwrap(),
        Some(AgentState::Idle)
    );
    router.join().unwrap();
}

#[test]
fn missing_owner_is_normal_and_missing_socket_never_notifies() {
    let (_dir, path, router) = fake_router(|mut peer| {
        initialize(&mut peer);
        let request = recv(&mut peer);
        send(
            &mut peer,
            json!({"type":"response","requestId":request["requestId"],"resultType":"error","error":"no-client-found"}),
        );
    });
    let stop = AtomicBool::new(false);
    let mut client = Client::connect(&path, &stop).unwrap();
    assert_eq!(client.sample(SESSION, &stop).unwrap(), None);
    router.join().unwrap();
    match Client::connect(&path.with_extension("missing"), &stop) {
        Err(fault) => assert!(!fault.alert),
        Ok(_) => panic!("unexpected connection"),
    }
}

#[test]
fn truncated_oversized_slow_and_invalid_frames_are_bounded() {
    let stop = AtomicBool::new(false);
    for len in [0, protocol::MAX_FRAME as u32 + 1, u32::MAX] {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(&len.to_le_bytes()).unwrap();
        assert_eq!(
            transport::read(
                &mut reader,
                Instant::now() + Duration::from_millis(50),
                &stop,
                "snapshot"
            )
            .unwrap_err()
            .code,
            "frame_size"
        );
    }
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    writer.write_all(&10u32.to_le_bytes()[..2]).unwrap();
    let start = Instant::now();
    assert_eq!(
        transport::read(
            &mut reader,
            start + Duration::from_millis(25),
            &stop,
            "snapshot"
        )
        .unwrap_err()
        .code,
        "response_timeout"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(writer);
    assert_eq!(
        transport::read(
            &mut reader,
            Instant::now() + Duration::from_secs(1),
            &stop,
            "snapshot"
        )
        .unwrap_err()
        .code,
        "transport_unavailable"
    );
    let fault = decode::<Frame<'_>>(b"{broken PRIVATE_CONTENT", "snapshot")
        .err()
        .unwrap();
    assert!(!fault.detail.contains("PRIVATE_CONTENT"));
}

#[test]
fn fragmented_frames_keep_the_length_prefix_and_body_in_order() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    let value = json!({"type":"response","requestId":"test"}).to_string();
    let mut bytes = (value.len() as u32).to_le_bytes().to_vec();
    bytes.extend(value.as_bytes());
    let sender = std::thread::spawn(move || {
        for chunk in bytes.chunks(3) {
            writer.write_all(chunk).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let result = transport::read(
        &mut reader,
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
        "snapshot",
    )
    .unwrap();
    assert_eq!(result, value.as_bytes());
    sender.join().unwrap();
}

#[test]
fn cache_cannot_override_new_hooks_expire_or_survive_disconnect() {
    let (wake, _) = crossbeam_channel::bounded(1);
    let monitor = Monitor {
        shared: Arc::default(),
        wake,
    };
    let target = Target {
        id: SESSION.into(),
        hook_activity: SystemTime::UNIX_EPOCH,
    };
    monitor.update_targets(vec![target.clone()]);
    monitor.shared.publish(&target, Some(AgentState::Waiting));
    assert_eq!(
        monitor.state(SESSION, target.hook_activity),
        Some(AgentState::Waiting)
    );
    assert_eq!(
        monitor.state(SESSION, target.hook_activity + Duration::from_secs(1)),
        None
    );
    monitor
        .shared
        .data()
        .observations
        .get_mut(SESSION)
        .unwrap()
        .received = Instant::now() - MAX_AGE;
    assert_eq!(monitor.state(SESSION, target.hook_activity), None);
    monitor.shared.publish(&target, Some(AgentState::Waiting));
    monitor.shared.fault(
        Some(&Fault::protocol(
            "version",
            "snapshot",
            "expected=11; observed=12",
        )),
        std::path::Path::new("/test/ipc.sock"),
        Some(SESSION),
    );
    assert_eq!(monitor.state(SESSION, target.hook_activity), None);
    assert!(monitor.diagnostic().is_some());
    monitor.update_targets(vec![]);
    monitor.shared.publish(&target, Some(AgentState::Waiting));
    assert_eq!(monitor.state(SESSION, target.hook_activity), None);
}

#[test]
fn worker_publishes_a_status_without_blocking_discovery() {
    let (_dir, path, router) = fake_router(|mut peer| {
        initialize(&mut peer);
        owner(&mut peer);
        send(
            &mut peer,
            snapshot(
                11,
                json!({"type":"active","activeFlags":["waitingOnApproval"]}),
            ),
        );
        unsubscribed(&mut peer);
    });
    let monitor = Monitor::start(path);
    let target = Target {
        id: SESSION.into(),
        hook_activity: SystemTime::UNIX_EPOCH,
    };
    monitor.update_targets(vec![target.clone()]);
    let deadline = Instant::now() + Duration::from_secs(2);
    while monitor.state(SESSION, target.hook_activity).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        monitor.state(SESSION, target.hook_activity),
        Some(AgentState::Waiting)
    );
    assert!(monitor.take_changed());
    assert!(!monitor.take_changed());
    drop(monitor);
    router.join().unwrap();
}

#[test]
fn worker_reports_protocol_drift_and_releases_the_subscription() {
    let (_dir, path, router) = fake_router(|mut peer| {
        initialize(&mut peer);
        owner(&mut peer);
        send(
            &mut peer,
            snapshot(12, json!({"type":"active","activeFlags":[]})),
        );
        unsubscribed(&mut peer);
    });
    let monitor = Monitor::start(path);
    monitor.update_targets(vec![Target {
        id: SESSION.into(),
        hook_activity: SystemTime::UNIX_EPOCH,
    }]);
    let deadline = Instant::now() + Duration::from_secs(2);
    while monitor.diagnostic().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let diagnostic = monitor.diagnostic().expect("protocol diagnostic");
    assert_eq!(diagnostic.code, "stream_version_changed");
    assert!(diagnostic.details.contains("observed=Some(12)"));
    assert!(diagnostic.details.contains("fallback=hooks_and_lifecycle"));
    assert_eq!(monitor.state(SESSION, SystemTime::UNIX_EPOCH), None);
    drop(monitor);
    router.join().unwrap();
}

#[test]
fn rejects_nonprivate_and_symlinked_endpoints_without_modifying_them() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::Builder::new()
        .prefix("am-ipc-sec-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("ipc.sock");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let _listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    let stop = AtomicBool::new(false);
    assert!(transport::connect(&path, Instant::now() + REQUEST_TIMEOUT, &stop).is_err());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o666
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = dir.path().join("alias.sock");
    symlink(&path, &alias).unwrap();
    assert!(transport::connect(&alias, Instant::now() + REQUEST_TIMEOUT, &stop).is_err());
}

/// Opt-in maintenance probe against the installed app. Prints only a state or
/// sanitized fault; never a wire frame. See the documented command/env var.
#[test]
#[ignore = "requires an explicit live desktop task ID and access to its Unix socket"]
fn live_desktop_status() {
    let id =
        std::env::var("AGENTSMON_CODEX_DIAGNOSTIC_SESSION").expect("set an explicit session ID");
    assert!(valid_id(&id));
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::UserDirs::new()
                .unwrap()
                .home_dir()
                .join(".codex")
        });
    let stop = AtomicBool::new(false);
    let mut client = Client::connect(&home.join("ipc/ipc.sock"), &stop).unwrap();
    println!(
        "session={id}; state={:?}",
        client.sample(&id, &stop).unwrap()
    );
}
