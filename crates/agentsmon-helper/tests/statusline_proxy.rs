use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn preserves_original_stdout_stderr_and_exit_code() {
    let directory = tempfile::tempdir().unwrap();

    #[cfg(not(target_os = "windows"))]
    let original = "cat; printf 'original-stderr' >&2; exit 7";
    #[cfg(target_os = "windows")]
    let original = "powershell -NoProfile -Command \"$v = [Console]::In.ReadToEnd(); [Console]::Out.Write($v); [Console]::Error.Write('original-stderr'); exit 7\"";

    fs::write(
        directory.path().join("statusline-original.json"),
        serde_json::to_vec(&serde_json::json!({ "command": original })).unwrap(),
    )
    .unwrap();

    let input = br#"{"session_id":"transparent-proxy","context_window":{"used_percentage":68.5}}"#;
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentsmon-helper"));
    command
        .arg("statusline-proxy")
        .env("AGENTSMON_CONFIG_DIR", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    command.env_remove("SHELL");
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, input);
    assert_eq!(output.stderr, b"original-stderr");
}
