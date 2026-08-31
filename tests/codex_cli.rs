use std::fs;
use std::process::{Command, Output};

use serde_json::{Value, json};

mod support;

use support::codex_app_server::{
    expected_boundary_wire, fake_codex, methods, recorded_requests, request_methods,
};

#[test]
fn account_status_reads_chatgpt_plan_quota_and_activity_when_supported() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake_codex(
        root.path(),
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","email":"a@example.com","planType":"plus"},"requiresOpenaiAuth":true}}),
            json!({"id":3,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":25,"windowDurationMins":15}}}}),
            json!({"id":4,"result":{"summary":{"lifetimeTokens":1234567},"dailyUsageBuckets":[]}}),
        ],
    );

    let output = Command::new(agul_bin())
        .args(["account", "status", "--json", "--codex-command"])
        .arg(fake)
        .output()
        .unwrap();

    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["account"]["account"]["type"], "chatgpt");
    assert_eq!(
        value["rate_limits"]["rateLimits"]["primary"]["usedPercent"],
        25
    );
    assert_eq!(value["usage"]["summary"]["lifetimeTokens"], 1_234_567);
    assert_eq!(
        request_methods(root.path()),
        [
            "initialize",
            "initialized",
            "account/read",
            "account/rateLimits/read",
            "account/usage/read",
        ]
    );
}

#[test]
fn device_login_waits_for_the_matching_managed_chatgpt_notification() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake_codex(
        root.path(),
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"type":"chatgptDeviceCode","loginId":"login-1","verificationUrl":"https://auth.openai.com/codex/device","userCode":"ABCD-1234"}}),
            json!({"method":"account/login/completed","params":{"loginId":"other","success":true,"error":null}}),
            json!({"method":"account/login/completed","params":{"loginId":"login-1","success":true,"error":null}}),
            json!({"id":3,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
        ],
    );

    let output = Command::new(agul_bin())
        .args([
            "account",
            "login",
            "--device-code",
            "--no-open",
            "--codex-command",
        ])
        .arg(fake)
        .output()
        .unwrap();

    assert_success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("https://auth.openai.com/codex/device · ABCD-1234"));
    assert!(text.contains("● ChatGPT · plus"));
    let requests = recorded_requests(root.path());
    assert_eq!(
        methods(&requests),
        [
            "initialize",
            "initialized",
            "account/login/start",
            "account/read",
        ]
    );
    assert_eq!(requests[2]["params"], json!({"type": "chatgptDeviceCode"}));
}

#[test]
fn codex_engine_records_live_web_progress_and_subscription_usage() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let fake = fake_codex(
        root.path(),
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
            json!({"id":3,"result":{"data":[{"model":"gpt-test","isDefault":true,"hidden":false}]}}),
            json!({"id":4,"result":{"thread":{"id":"thread-1"},"model":"gpt-test","reasoningEffort":"medium"}}),
            json!({"id":5,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}),
            json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"thread-1","turnId":"turn-1","delta":"checking sources"}}),
            json!({"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-1","query":"Agul","action":{"type":"search","query":"Agul"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-1","query":"Agul","action":{"type":"search","query":"Agul"}}}}),
            json!({"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-2","query":"Agul","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-2","query":"Agul","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":100,"cachedInputTokens":75,"outputTokens":20,"reasoningOutputTokens":5,"totalTokens":120},"total":{"inputTokens":100,"cachedInputTokens":75,"outputTokens":20,"reasoningOutputTokens":5,"totalTokens":120}}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"answer","delta":"Verified source."}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","id":"answer","text":"Verified source.","phase":"final_answer"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}),
        ],
    );

    let output = Command::new(agul_bin())
        .args([
            "chat",
            "--engine",
            "codex",
            "--prompt",
            "verify Agul",
            "--json",
            "--state-dir",
        ])
        .arg(&state)
        .args(["--codex-command"])
        .arg(fake)
        .env("AGUL_MODEL", "local-only")
        .output()
        .unwrap();

    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["engine"], "codex");
    assert_eq!(value["model"], "gpt-test");
    assert_eq!(value["billing"], "chatgpt_quota");
    assert!(value["cost"].is_null());
    assert_eq!(value["response"], "Verified source.");
    assert_eq!(value["tool_calls"], 2);
    assert_eq!(value["usage"]["entries"][0]["provider"], "codex");
    assert_eq!(
        value["usage"]["entries"][0]["unpriced_reason"],
        "subscription_quota"
    );
    assert_eq!(value["usage"]["entries"][0]["cache_hit_input_tokens"], 75);

    let session_id = value["session_id"].as_str().unwrap();
    let shown = Command::new(agul_bin())
        .args(["sessions", "--state-dir"])
        .arg(&state)
        .args(["show", session_id, "--trace"])
        .output()
        .unwrap();
    assert_success(&shown);
    let session: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(session["schema"], "agul/chat-session/v5");
    assert_eq!(session["engine"], "codex");
    assert_eq!(session["upstream_thread_id"], "thread-1");
    assert!(session["trace"].as_array().unwrap().iter().any(|event| {
        event["type"] == "tool_progress" && event["data"]["stage"] == "open_page"
    }));
    let requests = recorded_requests(root.path());
    assert_eq!(
        methods(&requests),
        [
            "initialize",
            "initialized",
            "account/read",
            "model/list",
            "thread/start",
            "turn/start",
        ]
    );
    assert_eq!(requests[4]["params"]["config"]["web_search"], "live");
    assert_eq!(requests[4]["params"]["ephemeral"], false);
    assert!(requests[4]["params"].get("excludeTurns").is_none());
    let (thread_sandbox, turn_sandbox_policy) = expected_boundary_wire();
    assert_eq!(requests[4]["params"]["sandbox"], thread_sandbox);
    assert_eq!(requests[5]["params"]["sandboxPolicy"], turn_sandbox_policy);
}

#[test]
fn codex_engine_applies_the_turn_timeout_to_app_server_calls() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake_codex(
        root.path(),
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
        ],
    );

    let output = Command::new(agul_bin())
        .args([
            "chat",
            "--engine",
            "codex",
            "--prompt",
            "wait forever",
            "--no-session",
            "--timeout-seconds",
            "1",
            "--codex-command",
        ])
        .arg(fake)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    assert_eq!(
        request_methods(root.path()),
        ["initialize", "initialized", "account/read", "model/list",]
    );
}

#[test]
fn codex_session_resumes_the_stored_upstream_thread() {
    let root = tempfile::tempdir().unwrap();
    let first_root = root.path().join("first");
    let second_root = root.path().join("second");
    let state = root.path().join("state");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir_all(&second_root).unwrap();
    let first_fake = fake_codex(
        &first_root,
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
            json!({"id":3,"result":{"data":[{"model":"gpt-test","isDefault":true,"hidden":false}]}}),
            json!({"id":4,"result":{"thread":{"id":"thread-1"},"model":"gpt-test","reasoningEffort":"medium"}}),
            json!({"id":5,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":10,"cachedInputTokens":2,"outputTokens":3,"reasoningOutputTokens":1,"totalTokens":13},"total":{"inputTokens":10,"cachedInputTokens":2,"outputTokens":3,"reasoningOutputTokens":1,"totalTokens":13}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","id":"answer-1","text":"First answer.","phase":"final_answer"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}),
        ],
    );
    let first = Command::new(agul_bin())
        .args([
            "chat",
            "--engine",
            "codex",
            "--prompt",
            "first",
            "--json",
            "--state-dir",
        ])
        .arg(&state)
        .args(["--codex-command"])
        .arg(first_fake)
        .output()
        .unwrap();
    assert_success(&first);
    let first_result: Value = serde_json::from_slice(&first.stdout).unwrap();
    let session_id = first_result["session_id"].as_str().unwrap();

    let second_fake = fake_codex(
        &second_root,
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
            json!({"id":3,"result":{"thread":{"id":"thread-1"},"model":"gpt-test","reasoningEffort":"medium"}}),
            json!({"id":4,"result":{"turn":{"id":"turn-2","status":"inProgress"}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-2","tokenUsage":{"last":{"inputTokens":12,"cachedInputTokens":4,"outputTokens":4,"reasoningOutputTokens":1,"totalTokens":16},"total":{"inputTokens":22,"cachedInputTokens":6,"outputTokens":7,"reasoningOutputTokens":2,"totalTokens":29}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-2","item":{"type":"agentMessage","id":"answer-2","text":"Resumed answer.","phase":"final_answer"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-2","status":"completed"}}}),
        ],
    );
    let second = Command::new(agul_bin())
        .args([
            "chat",
            "--session",
            session_id,
            "--prompt",
            "second",
            "--json",
            "--state-dir",
        ])
        .arg(&state)
        .args(["--codex-command"])
        .arg(second_fake)
        .output()
        .unwrap();
    assert_success(&second);
    let second_result: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_result["response"], "Resumed answer.");

    let requests = recorded_requests(&second_root);
    assert_eq!(
        methods(&requests),
        [
            "initialize",
            "initialized",
            "account/read",
            "thread/resume",
            "turn/start",
        ]
    );
    assert_eq!(requests[3]["params"]["threadId"], "thread-1");
    assert_eq!(requests[3]["params"]["config"]["web_search"], "live");
    assert!(requests[3]["params"].get("ephemeral").is_none());
    let (thread_sandbox, turn_sandbox_policy) = expected_boundary_wire();
    assert_eq!(requests[3]["params"]["sandbox"], thread_sandbox);
    assert_eq!(requests[4]["params"]["sandboxPolicy"], turn_sandbox_policy);
}

fn agul_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agul")
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
