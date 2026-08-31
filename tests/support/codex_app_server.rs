use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub(crate) fn expected_boundary_wire() -> (&'static str, Value) {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("whoami")
            .output()
            .expect("whoami");
        assert!(output.status.success(), "whoami failed");
        let account = String::from_utf8_lossy(&output.stdout);
        let account = account.trim().rsplit('\\').next().unwrap_or_default();
        if account.eq_ignore_ascii_case("CodexSandboxOffline") {
            return (
                "danger-full-access",
                json!({"type": "externalSandbox", "networkAccess": "restricted"}),
            );
        }
        if account.eq_ignore_ascii_case("CodexSandboxOnline") {
            return (
                "danger-full-access",
                json!({"type": "externalSandbox", "networkAccess": "enabled"}),
            );
        }
    }
    (
        "workspace-write",
        json!({"type": "workspaceWrite", "networkAccess": true}),
    )
}

pub(crate) fn fake_codex(root: &Path, messages: &[Value]) -> PathBuf {
    #[cfg(windows)]
    {
        let script = root.join("fake-codex.ps1");
        fs::write(&script, scripted_fake_windows(messages)).unwrap();
        let launcher = root.join("fake-codex.cmd");
        fs::write(
            &launcher,
            "@echo off\npowershell.exe -NoProfile -ExecutionPolicy Bypass -File \"%~dp0fake-codex.ps1\"\n",
        )
        .unwrap();
        launcher
    }
    #[cfg(not(windows))]
    {
        let path = root.join("fake-codex");
        fs::write(&path, scripted_fake_unix(messages)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }
}

#[cfg(windows)]
fn scripted_fake_windows(messages: &[Value]) -> String {
    let mut lines = vec![
        "$log = Join-Path $PSScriptRoot 'requests.jsonl'".to_string(),
        "function Read-Request {".to_string(),
        "  $request = [Console]::In.ReadLine()".to_string(),
        "  if ($null -eq $request) { exit 0 }".to_string(),
        "  [System.IO.File]::AppendAllText($log, $request + [Environment]::NewLine)".to_string(),
        "}".to_string(),
    ];
    for (index, message) in messages.iter().enumerate() {
        if index == 0 || message.get("id").is_some() {
            lines.push("Read-Request".to_string());
        }
        let message = message.to_string().replace('\'', "''");
        lines.push(format!("[Console]::Out.WriteLine('{message}')"));
        lines.push("[Console]::Out.Flush()".to_string());
        if index == 0 {
            lines.push("Read-Request".to_string());
        }
    }
    lines.push("while ($true) { Read-Request }".to_string());
    format!("{}\n", lines.join("\n"))
}

#[cfg(not(windows))]
fn scripted_fake_unix(messages: &[Value]) -> String {
    let mut lines = vec![
        "#!/bin/sh".to_string(),
        "log=\"$(dirname \"$0\")/requests.jsonl\"".to_string(),
        "read_request() {".to_string(),
        "  IFS= read -r request || exit 0".to_string(),
        "  printf '%s\\n' \"$request\" >> \"$log\"".to_string(),
        "}".to_string(),
    ];
    for (index, message) in messages.iter().enumerate() {
        if index == 0 || message.get("id").is_some() {
            lines.push("read_request".to_string());
        }
        lines.push(format!(
            "printf '%s\\n' '{}'",
            message.to_string().replace('\'', "'\\''")
        ));
        if index == 0 {
            lines.push("read_request".to_string());
        }
    }
    lines.push("while true; do read_request; done".to_string());
    format!("{}\n", lines.join("\n"))
}

pub(crate) fn recorded_requests(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub(crate) fn request_methods(root: &Path) -> Vec<String> {
    methods(&recorded_requests(root))
}

pub(crate) fn methods(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_string())
        .collect()
}
