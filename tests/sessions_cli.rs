use std::fs;
use std::process::Command;

use serde_json::{Value, json};

#[test]
fn sessions_list_and_show_trace_read_the_v5_store() {
    let root = tempfile::tempdir().expect("state root");
    let sessions = root.path().join("sessions");
    let traces = root.path().join("traces");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&traces).unwrap();
    let id = "session-cli-test";
    fs::write(
        sessions.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&json!({
            "schema": "agul/chat-session/v5",
            "id": id,
            "workspace": root.path(),
            "model": "test-model",
            "engine": "native",
            "upstream_thread_id": null,
            "source": "chat",
            "status": "completed",
            "owner_pid": std::process::id(),
            "attribution": {
                "parent_session_id": null,
                "delegation_id": null,
                "task_id": null,
                "specialist_id": null,
                "pool_id": null
            },
            "related_sessions": [],
            "handoff": null,
            "created_at": 1,
            "updated_at": 2,
            "summarized_turns": 0,
            "summary": null,
            "turns": [],
            "native_config": {
                "provider": "deepseek",
                "base_url": "https://api.deepseek.com",
                "api_key_env": "DEEPSEEK_API_KEY",
                "reasoning_effort": null
            },
            "native_history": null,
            "pending_turn": null,
            "usage": [],
            "trace_seq": 1
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        traces.join(format!("{id}.ndjson")),
        concat!(
            "{\"format\":\"agul/trace-event/v1\",\"seq\":1,\"timestamp\":1,",
            "\"operation_id\":\"turn-1\",\"type\":\"operation_started\",",
            "\"data\":{\"input\":\"hello\"}}\n",
            "{\"incomplete\":"
        ),
    )
    .unwrap();

    let listed = Command::new(agul_bin())
        .args(["sessions", "--state-dir"])
        .arg(root.path())
        .arg("list")
        .output()
        .expect("list sessions");
    assert!(listed.status.success(), "{}", stderr(&listed));
    let list = String::from_utf8(listed.stdout).unwrap();
    assert!(list.contains("session-cli-test\tchat\tcompleted"));
    assert!(list.contains("0 child"));

    let shown = Command::new(agul_bin())
        .args(["sessions", "--state-dir"])
        .arg(root.path())
        .args(["show", id, "--trace"])
        .output()
        .expect("show session");
    assert!(shown.status.success(), "{}", stderr(&shown));
    let document: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(document["id"], id);
    assert_eq!(document["source"], "chat");
    assert_eq!(document["status"], "completed");
    assert_eq!(document["trace"].as_array().unwrap().len(), 1);
    assert_eq!(document["trace"][0]["format"], "agul/trace-event/v1");
}

fn agul_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agul")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
