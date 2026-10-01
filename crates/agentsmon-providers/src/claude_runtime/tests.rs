use super::*;

const PID: i32 = 42;

fn fixture(status: &str, timestamp: u64) -> String {
    format!(
        r#"{{"pid":42,"sessionId":"s","startedAt":10000,"kind":"interactive","version":"2.1.273","status":"{status}","statusUpdatedAt":{timestamp},"name":"must not be read","detail":{{"arbitrary":"ignored"}}}}"#
    )
}

fn read(
    registry: &Registry,
    state: AgentState,
    hook_ms: u64,
) -> Result<Option<(AgentState, SystemTime)>, ProviderDiagnostic> {
    registry.read(
        PID,
        "s",
        millis(9000).unwrap(),
        LastHook {
            state,
            time: millis(hook_ms).unwrap(),
            subagent: false,
        },
        millis(30000).unwrap(),
    )
}

#[test]
fn escape_during_reasoning_overrides_user_prompt_without_stop() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    let path = dir.path().join("42.json");
    fs::write(&path, fixture("idle", 11000)).unwrap();
    assert!(read(&registry, AgentState::Active, 12000)
        .unwrap()
        .is_none());
    fs::write(&path, fixture("busy", 13000)).unwrap();
    assert_eq!(
        read(&registry, AgentState::Active, 12000)
            .unwrap()
            .unwrap()
            .0,
        AgentState::Active
    );
    // Escape writes idle even though no Stop hook was received.
    fs::write(&path, fixture("idle", 14000)).unwrap();
    assert_eq!(
        read(&registry, AgentState::Active, 12000).unwrap().unwrap(),
        (AgentState::Idle, millis(14000).unwrap())
    );
    // The next prompt must immediately turn green, before the registry catches up.
    assert!(read(&registry, AgentState::Active, 15000)
        .unwrap()
        .is_none());
}

#[test]
fn explicit_wait_compaction_and_shell_states() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    for (status, hook, expected) in [
        ("waiting", AgentState::Active, AgentState::Waiting),
        ("idle", AgentState::Waiting, AgentState::Idle),
        ("busy", AgentState::Compacting, AgentState::Compacting),
        ("idle", AgentState::Compacting, AgentState::Idle),
        ("shell", AgentState::Active, AgentState::Idle),
        // Last on purpose: the silent-tool check below reuses this file.
        ("busy", AgentState::Idle, AgentState::Active),
    ] {
        fs::write(dir.path().join("42.json"), fixture(status, 20000)).unwrap();
        assert_eq!(read(&registry, hook, 12000).unwrap().unwrap().0, expected);
    }
    // A long, silent tool does not expire.
    assert_eq!(
        registry
            .read(
                PID,
                "s",
                millis(9000).unwrap(),
                LastHook {
                    state: AgentState::Active,
                    time: millis(12000).unwrap(),
                    subagent: false,
                },
                millis(86_400_000).unwrap()
            )
            .unwrap()
            .unwrap()
            .0,
        AgentState::Active
    );
}

#[test]
fn ignores_wrong_session_pid_generation_and_background_workers() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    for record in [
        fixture("idle", 20000).replace("\"s\"", "\"different\""),
        fixture("idle", 20000).replace("\"pid\":42", "\"pid\":43"),
        fixture("idle", 20000).replace("10000", "8000"),
        fixture("idle", 20000).replace("interactive", "daemon"),
    ] {
        fs::write(dir.path().join("42.json"), record).unwrap();
        assert!(read(&registry, AgentState::Active, 12000)
            .unwrap()
            .is_none());
    }
}

#[test]
fn malformed_unknown_large_and_future_records_fail_safely() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    for (record, code) in [
        ("{partial".into(), "claude.registry.schema"),
        (
            fixture("SECRET UNKNOWN VALUE", 20000),
            "claude.registry.status",
        ),
        (fixture("idle", 40000), "claude.registry.timestamp"),
        (fixture("idle", 9000), "claude.registry.timestamp"),
        ("x".repeat(MAX_BYTES as usize + 1), "claude.registry.file"),
    ] {
        fs::write(dir.path().join("42.json"), record).unwrap();
        let error = read(&registry, AgentState::Active, 12000).unwrap_err();
        assert_eq!(error.code, code);
        assert!(!error.details.contains("SECRET"));
    }
    fs::remove_file(dir.path().join("42.json")).unwrap();
    assert!(read(&registry, AgentState::Active, 12000)
        .unwrap()
        .is_none());
}

#[cfg(unix)]
#[test]
fn refuses_symlink_and_fifo_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    let path = dir.path().join("42.json");
    fs::write(dir.path().join("target"), fixture("idle", 20000)).unwrap();
    std::os::unix::fs::symlink(dir.path().join("target"), &path).unwrap();
    assert_eq!(
        read(&registry, AgentState::Active, 12000).unwrap_err().code,
        "claude.registry.file"
    );
    fs::remove_file(&path).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert_eq!(
        read(&registry, AgentState::Active, 12000).unwrap_err().code,
        "claude.registry.file"
    );
}

#[test]
fn old_versions_need_a_restart_not_an_idle_guess() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    fs::write(
        dir.path().join("42.json"),
        r#"{"pid":42,"sessionId":"s","startedAt":10000,"version":"2.1.266"}"#,
    )
    .unwrap();
    let issue = read(&registry, AgentState::Active, 12000).unwrap_err();
    assert_eq!(issue.code, "claude.registry.status_unavailable");
    assert!(issue.details.contains("2.1.266"));
    assert!(read(&registry, AgentState::Idle, 12000).unwrap().is_none());
}

#[test]
fn transient_partial_writes_do_not_notify_and_recovery_clears_diagnostic() {
    let issue = ProviderDiagnostic {
        code: "claude.registry.schema",
        summary: "test",
        details: "safe".into(),
    };
    let now = Instant::now();
    let mut health = Health::default();
    health.observe(Some(issue.clone()), now);
    health.observe(Some(issue.clone()), now + Duration::from_secs(14));
    assert!(health.current.is_none());
    health.observe(None, now + Duration::from_secs(15));
    health.observe(Some(issue.clone()), now + Duration::from_secs(16));
    assert!(health.current.is_none());
    health.observe(Some(issue), now + Duration::from_secs(31));
    assert!(health.current.is_some());
    health.observe(None, now + Duration::from_secs(32));
    assert!(health.current.is_none());
}

#[test]
fn late_subagent_hook_does_not_revive_an_idle_turn() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().into());
    let read = |status: &str, subagent_hook: bool| {
        fs::write(dir.path().join("42.json"), fixture(status, 11000)).unwrap();
        registry
            .read(
                PID,
                "s",
                millis(9000).unwrap(),
                LastHook {
                    state: AgentState::Active,
                    time: millis(15000).unwrap(),
                    subagent: subagent_hook,
                },
                millis(30000).unwrap(),
            )
            .unwrap()
            .map(|(state, _)| state)
    };
    // Observed on 2.1.285: registry idle, then SubagentStop minutes later.
    assert_eq!(read("idle", true), Some(AgentState::Idle));
    assert_eq!(read("shell", true), Some(AgentState::Idle));
    // Observed on 2.1.281 (VS Code): a question, then SubagentStop.
    assert_eq!(read("waiting", true), Some(AgentState::Waiting));
    // An ordinary newer hook still wins, and so does a subagent of a busy turn.
    assert_eq!(read("idle", false), None);
    assert_eq!(read("busy", true), None);
}
