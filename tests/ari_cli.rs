use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

mod support;

use support::codex_app_server::{
    expected_boundary_wire, fake_codex, recorded_requests, request_methods,
};

/// Nothing listens on port 9, so a session would fail loudly if a model were
/// contacted; this test asserts that start_session never makes that call.
const NEVER_LISTENS: &str = "http://127.0.0.1:9/v1";
const MODEL: &str = "ari-test-model";
const CREATED_AT: u64 = 1_787_788_800;

#[test]
fn stdio_ari_initializes_lists_capabilities_starts_and_closes_a_launched_session() {
    let root = tempfile::tempdir().expect("test root");
    let home = root.path().join("home");
    fs::create_dir_all(root.path().join(".agents/runtime")).expect("launch directory");
    fs::create_dir_all(root.path().join(".agents/plugins/echo")).expect("plugin directory");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        root.path().join(".agents/AGENTS.md"),
        "Keep the launch thin.\n",
    )
    .expect("project instructions");
    fs::write(
        root.path().join(".agents/runtime/launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","plugins":"../plugins"}"#,
    )
    .expect("launch.json");
    fs::write(
        root.path().join(".agents/plugins/echo/plugin.json"),
        r#"{
            "format":"agul/plugin/v2",
            "name":"echo",
            "version":"1.0.0",
            "command":["not-contacted"],
            "capabilities":["agul/dependency-installer/v1"],
            "commands":[{
                "name":"agent",
                "description":"Delegate to a prepared specialist"
            }],
            "tools":[{
                "name":"echo_text",
                "description":"Echo text",
                "parameters":{"type":"object"}
            }]
        }"#,
    )
    .expect("plugin.json");

    let mut child = Command::new(agul_bin())
        .args(["ari", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", &home)
        .env("APPDATA", &home)
        .env("XDG_STATE_HOME", &home)
        .spawn()
        .expect("spawn agul ari serve");
    let mut stdin = child.stdin.take().expect("child stdin");
    let responses = line_reader(child.stdout.take().expect("child stdout"));
    let mut stderr = child.stderr.take().expect("child stderr");

    let initialize = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "initialize",
            "method": "ari.initialize",
            "params": {"client": {"name": "ari_cli_test", "version": "0.1.0"}}
        }),
    );
    assert_eq!(initialize["id"], "initialize");
    assert_eq!(initialize["result"]["ari"], "0.2");
    assert!(
        initialize["result"]["methods"]
            .as_array()
            .expect("initialized methods")
            .iter()
            .any(|method| method == "ari.start_session")
    );

    let capabilities = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "capabilities",
            "method": "ari.capabilities",
            "params": {}
        }),
    );
    assert_eq!(capabilities["id"], "capabilities");
    assert_eq!(
        capabilities["result"]["methods"],
        json!([
            "ari.initialize",
            "ari.capabilities",
            "ari.start_session",
            "ari.send",
            "ari.compact",
            "ari.close_session"
        ])
    );
    assert_eq!(
        capabilities["result"]["engines"]["native"]["tools"],
        json!(["read", "write", "edit", "shell"])
    );
    assert_eq!(
        capabilities["result"]["engines"]["codex"]["billing"],
        "chatgpt_quota"
    );
    assert_eq!(
        capabilities["result"]["usage"],
        json!({"ledger": "per_response"})
    );
    assert_eq!(
        capabilities["result"]["plugins"]["formats"],
        json!(["agul/plugin/v2"])
    );

    let started = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "start",
            "method": "ari.start_session",
            "params": {
                "workspace": root.path(),
                "base_url": NEVER_LISTENS,
                "model": "never-contact-me",
                "api_key_env": ""
            }
        }),
    );
    assert_eq!(started["id"], "start");
    let session_id = started["result"]["session_id"]
        .as_str()
        .expect("started session id")
        .to_string();
    assert_eq!(
        started["result"]["workspace"]
            .as_str()
            .expect("started workspace"),
        fs::canonicalize(root.path())
            .expect("canonical workspace")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(started["result"]["model"], "never-contact-me");
    assert_eq!(
        started["result"]["endpoint"],
        format!("{NEVER_LISTENS}/chat/completions")
    );
    assert_eq!(
        started["result"]["tools"],
        json!(["read", "write", "edit", "shell", "echo_text"])
    );
    assert_eq!(started["result"]["plugin_commands"][0]["name"], "agent");
    assert_eq!(
        started["result"]["plugin_capabilities"][0]["name"],
        "agul/dependency-installer/v1"
    );

    let closed = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "close",
            "method": "ari.close_session",
            "params": {"session_id": session_id}
        }),
    );
    assert_eq!(closed["id"], "close");
    assert_eq!(closed["result"]["session_id"], session_id);
    assert_eq!(closed["result"]["closed"], true);

    drop(stdin);
    let status = child.wait().expect("agul ari serve exits");
    assert_eq!(status.code(), Some(0), "stdout/stderr from the ARI server");
    let mut stderr_text = String::new();
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read child stderr");
    assert!(stderr_text.is_empty(), "unexpected stderr: {stderr_text}");
}

#[test]
fn stdio_ari_runs_the_codex_engine_with_live_web_and_chatgpt_quota() {
    let root = tempfile::tempdir().expect("test root");
    let home = root.path().join("home");
    fs::create_dir_all(root.path().join(".agents/runtime")).expect("launch directory");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        root.path().join(".agents/AGENTS.md"),
        "Use live Web Search when verification needs a source.\n",
    )
    .expect("project instructions");
    fs::write(
        root.path().join(".agents/runtime/launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md"}"#,
    )
    .expect("launch.json");
    let fake = fake_codex(
        root.path(),
        &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true}}),
            json!({"id":3,"result":{"data":[{"model":"gpt-test","isDefault":true,"hidden":false}]}}),
            json!({"id":4,"result":{"thread":{"id":"thread-ari"},"model":"gpt-test","reasoningEffort":"medium"}}),
            json!({"id":5,"result":{"turn":{"id":"turn-ari","status":"inProgress"}}}),
            json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"thread-ari","turnId":"turn-ari","delta":"checking live sources"}}),
            json!({"method":"item/started","params":{"threadId":"thread-ari","turnId":"turn-ari","item":{"type":"webSearch","id":"web-search","action":{"type":"search","query":"Agul runtime"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-ari","turnId":"turn-ari","item":{"type":"webSearch","id":"web-search","action":{"type":"search","query":"Agul runtime"}}}}),
            json!({"method":"item/started","params":{"threadId":"thread-ari","turnId":"turn-ari","item":{"type":"webSearch","id":"web-open","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-ari","turnId":"turn-ari","item":{"type":"webSearch","id":"web-open","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-ari","turnId":"turn-ari","tokenUsage":{"last":{"inputTokens":80,"cachedInputTokens":60,"outputTokens":15,"reasoningOutputTokens":5,"totalTokens":95},"total":{"inputTokens":80,"cachedInputTokens":60,"outputTokens":15,"reasoningOutputTokens":5,"totalTokens":95}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-ari","turnId":"turn-ari","item":{"type":"agentMessage","id":"answer","text":"Verified with a live source.","phase":"final_answer"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-ari","turn":{"id":"turn-ari","status":"completed"}}}),
            json!({"id":6,"error":{"code":-32000,"message":"upstream turn failed"}}),
        ],
    );

    let mut child = Command::new(agul_bin())
        .args(["ari", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", &home)
        .env("APPDATA", &home)
        .env("XDG_STATE_HOME", &home)
        .spawn()
        .expect("spawn agul ari serve");
    let mut stdin = child.stdin.take().expect("child stdin");
    let responses = line_reader(child.stdout.take().expect("child stdout"));
    let mut stderr = child.stderr.take().expect("child stderr");

    call(
        &mut stdin,
        &responses,
        json!({"jsonrpc":"2.0","id":"init","method":"ari.initialize","params":{}}),
    );
    let started = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "start-codex",
            "method": "ari.start_session",
            "params": {
                "workspace": root.path(),
                "engine": "codex",
                "codex_command": fake
            }
        }),
    );
    assert_eq!(started["result"]["engine"], "codex");
    assert_eq!(started["result"]["billing"], "chatgpt_quota");
    assert_eq!(started["result"]["endpoint"], "codex://chatgpt");
    assert_eq!(
        started["result"]["capabilities"],
        json!({
            "tool_owner": "codex_app_server",
            "plugin_tools": false,
            "manual_compaction": false,
            "web_search": "live"
        })
    );
    assert_eq!(started["result"]["tools"], json!([]));
    assert_eq!(started["result"]["model"], "gpt-test");
    assert_eq!(started["result"]["upstream_thread_id"], "thread-ari");
    let session_id = started["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session id: {started}"))
        .to_string();

    let (invalid, invalid_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "invalid-skill",
            "method": "ari.send",
            "params": {
                "session_id": session_id,
                "input": "Use @skill:missing"
            }
        }),
    );
    assert_eq!(invalid["error"]["code"], -32602);
    assert!(
        invalid["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Skill not found: missing"))
    );
    assert!(invalid_events.is_empty());

    let (sent, events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "send-codex",
            "method": "ari.send",
            "params": {
                "session_id": session_id,
                "input": "Verify Agul with live Web Search"
            }
        }),
    );
    assert_eq!(sent["result"]["text"], "Verified with a live source.");
    assert_eq!(sent["result"]["model_rounds"], 1);
    assert_eq!(sent["result"]["tool_calls"], 2);
    assert_eq!(sent["result"]["usage"]["chat_responses"], 1);
    assert!(
        events.iter().any(|event| {
            event["kind"] == "reasoning" && event["text"] == "checking live sources"
        })
    );
    assert!(events.iter().any(|event| {
        event["kind"] == "tool" && event["phase"] == "started" && event["name"] == "web"
    }));
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "tool_progress" && event["stage"] == "search")
    );
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "tool_progress" && event["stage"] == "open_page")
    );
    let usage = usage_events(&events);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["ledger_entry"]["provider"], "codex");
    assert_eq!(
        usage[0]["ledger_entry"]["unpriced_reason"],
        "subscription_quota"
    );
    assert_eq!(
        usage[0]["ledger_entry"]["response_id"],
        "thread-ari:turn-ari:1"
    );
    assert_eq!(usage[0]["ledger_entry"]["cache_hit_input_tokens"], 60);
    assert!(usage[0]["ledger_entry"]["cost"].is_null());

    let (compact, compact_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "compact-codex",
            "method": "ari.compact",
            "params": {"session_id": session_id}
        }),
    );
    assert_eq!(compact["error"]["code"], -32602);
    assert!(compact_events.is_empty());

    let (failed, failed_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "fail-codex",
            "method": "ari.send",
            "params": {"session_id": session_id, "input": "trigger upstream failure"}
        }),
    );
    assert_eq!(failed["error"]["code"], -32002);
    assert!(
        failed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("upstream turn failed"))
    );
    assert!(failed_events.is_empty());

    let missing = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "reuse-failed-codex",
            "method": "ari.send",
            "params": {"session_id": session_id, "input": "must not reuse bridge"}
        }),
    );
    assert_eq!(missing["error"]["code"], -32001);
    drop(stdin);
    assert_eq!(child.wait().expect("ARI exits").code(), Some(0));
    let mut stderr_text = String::new();
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read child stderr");
    assert!(stderr_text.is_empty(), "unexpected stderr: {stderr_text}");

    let shown = Command::new(agul_bin())
        .args(["sessions", "--state-dir"])
        .arg(home.join("Agul"))
        .args(["show", &session_id, "--trace"])
        .output()
        .expect("show Codex ARI session");
    assert_eq!(
        shown.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&shown.stdout),
        String::from_utf8_lossy(&shown.stderr)
    );
    let persisted: Value = serde_json::from_slice(&shown.stdout).expect("session JSON");
    assert_eq!(persisted["engine"], "codex");
    assert_eq!(persisted["status"], "failed");
    assert!(persisted["summary"].is_null());
    assert_eq!(persisted["usage"]["summary"]["chat_responses"], 1);
    assert!(persisted["trace"].as_array().is_some_and(|trace| {
        trace.iter().all(|event| {
            !event["operation_id"]
                .as_str()
                .is_some_and(|operation| operation.starts_with("compact-"))
        })
    }));

    assert_eq!(
        request_methods(root.path()),
        [
            "initialize",
            "initialized",
            "account/read",
            "model/list",
            "thread/start",
            "turn/start",
            "turn/start",
        ]
    );
    let requests = recorded_requests(root.path());
    assert_eq!(requests[4]["params"]["config"]["web_search"], "live");
    assert_eq!(requests[4]["params"]["ephemeral"], false);
    let (thread_sandbox, turn_sandbox_policy) = expected_boundary_wire();
    assert_eq!(requests[4]["params"]["sandbox"], thread_sandbox);
    assert_eq!(requests[5]["params"]["sandboxPolicy"], turn_sandbox_policy);
}

#[test]
fn stdio_ari_runs_skills_tools_usage_and_transactional_compaction() {
    const ANSWER_WITH_HANDOFF: &str = concat!(
        "The note says hello from the note.\n",
        "<agul-handoff format=\"agul/handoff/v1\">",
        "{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",",
        "\"summary\":\"The note says hello from the note.\",",
        "\"evidence\":[\"note.txt\"],\"verification\":[]}",
        "</agul-handoff>"
    );
    const SCALAR_VERIFICATION_HANDOFF_ANSWER: &str = concat!(
        "A later response with scalar verification.\n",
        "<agul-handoff format=\"agul/handoff/v1\">",
        "{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",",
        "\"summary\":\"needs verification\",\"verification\":\"required\"}",
        "</agul-handoff>"
    );
    let root = tempfile::tempdir().expect("test root");
    let home = root.path().join("home");
    fs::create_dir_all(root.path().join(".agents/runtime")).expect("launch directory");
    fs::create_dir_all(root.path().join(".agents/skills/proof")).expect("Skill directory");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(root.path().join("note.txt"), "hello from the note\n").expect("note");
    fs::write(
        root.path().join(".agents/AGENTS.md"),
        "Use the requested Skill and report what the file says.\n",
    )
    .expect("project instructions");
    fs::write(
        root.path().join(".agents/skills/proof/SKILL.md"),
        "---\nname: proof\ndescription: Read the proof note\n---\nARI_SKILL_SENTINEL\nUse the read tool on note.txt.\n",
    )
    .expect("Skill");
    fs::write(
        root.path().join(".agents/runtime/launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","skills":"../skills"}"#,
    )
    .expect("launch.json");

    let provider = FakeProvider::start(vec![
        Reply::sse(tool_response(
            "response-read",
            "call-read",
            "read",
            json!({"path": "note.txt"}),
            20,
            4,
        )),
        Reply::sse(text_response("response-answer", ANSWER_WITH_HANDOFF, 30, 5)),
        Reply::sse(text_response(
            "response-scalar-verification-handoff",
            SCALAR_VERIFICATION_HANDOFF_ANSWER,
            10,
            2,
        )),
        Reply::failure("provider is temporarily asleep"),
        Reply::sse(text_response(
            "response-compact",
            "The user asked to read note.txt and the answer reported its contents.",
            40,
            6,
        )),
    ]);
    fs::write(
        root.path().join("price-card.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "agul/price-catalog/v0.3",
            "id": "ari-test-usd",
            "version": "2026-08-27",
            "source": "https://example.test/ari-pricing",
            "source_checked_at": CREATED_AT,
            "review_after": null,
            "currency": "USD",
            "cards": [{
                "id": "ari-test-standard",
                "provider": "openai-compatible",
                "origin": provider.base_url().trim_end_matches("/v1"),
                "models": [MODEL],
                "effective_from": 0,
                "effective_until": null,
                "default_band": "standard",
                "bands": [{
                    "id": "standard",
                    "rates": {
                        "cache_hit_input_nanos_per_million": 1_000_000_000_u64,
                        "cache_miss_input_nanos_per_million": 1_000_000_000_u64,
                        "output_nanos_per_million": 2_000_000_000_u64
                    }
                }],
                "schedule": []
            }]
        }))
        .expect("price card JSON"),
    )
    .expect("price card");

    let mut child = Command::new(agul_bin())
        .args(["ari", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", &home)
        .env("APPDATA", &home)
        .env("XDG_STATE_HOME", &home)
        .spawn()
        .expect("spawn agul ari serve");
    let mut stdin = child.stdin.take().expect("child stdin");
    let responses = line_reader(child.stdout.take().expect("child stdout"));
    let mut stderr = child.stderr.take().expect("child stderr");

    call(
        &mut stdin,
        &responses,
        json!({"jsonrpc":"2.0","id":"init","method":"ari.initialize","params":{}}),
    );
    let started = call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "start",
            "method": "ari.start_session",
            "params": {
                "workspace": root.path(),
                "base_url": provider.base_url(),
                "model": MODEL,
                "api_key_env": "",
                "price_card": "price-card.json"
            }
        }),
    );
    let session_id = started["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session id: {started}"))
        .to_string();

    let (sent, send_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "send",
            "method": "ari.send",
            "params": {
                "session_id": session_id,
                "input": "Use @skill:proof to inspect note.txt"
            }
        }),
    );
    assert_eq!(sent["result"]["text"], ANSWER_WITH_HANDOFF);
    assert_eq!(
        sent["result"]["handoff"],
        json!({
            "format": "agul/handoff/v1",
            "status": "completed",
            "summary": "The note says hello from the note.",
            "evidence": ["note.txt"],
            "verification": []
        })
    );
    assert_eq!(sent["result"]["model_rounds"], 2);
    assert_eq!(sent["result"]["tool_calls"], 1);
    assert_eq!(sent["result"]["usage"]["chat_responses"], 2);
    let chat_usage = usage_events(&send_events);
    assert_eq!(chat_usage.len(), 2);
    assert!(chat_usage.iter().all(|event| {
        event["ledger_entry"]["purpose"] == "chat"
            && event["ledger_entry"]["cost"].is_object()
            && event["ledger_entry"]["price_ref"]["catalog_version"] == "2026-08-27"
    }));
    assert_eq!(
        chat_usage[0]["ledger_entry"]["response_id"],
        "response-read"
    );
    assert_eq!(
        chat_usage[1]["ledger_entry"]["response_id"],
        "response-answer"
    );
    assert!(send_events.iter().any(|event| {
        event["kind"] == "tool" && event["phase"] == "finished" && event["ok"] == true
    }));

    let (scalar_verification, scalar_verification_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "send-scalar-verification-handoff",
            "method": "ari.send",
            "params": {
                "session_id": session_id,
                "input": "Return a handoff with scalar verification"
            }
        }),
    );
    assert_eq!(
        scalar_verification["result"]["text"],
        SCALAR_VERIFICATION_HANDOFF_ANSWER
    );
    assert_eq!(
        scalar_verification["result"]["handoff"],
        json!({
            "format": "agul/handoff/v1",
            "status": "completed",
            "summary": "needs verification",
            "verification": ["required"]
        })
    );
    assert_eq!(scalar_verification["result"]["usage"]["chat_responses"], 3);
    let scalar_verification_usage = usage_events(&scalar_verification_events);
    assert_eq!(scalar_verification_usage.len(), 1);
    assert_eq!(
        scalar_verification_usage[0]["ledger_entry"]["response_id"],
        "response-scalar-verification-handoff"
    );

    let (failed, failed_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "compact-failed",
            "method": "ari.compact",
            "params": {"session_id": session_id}
        }),
    );
    assert_eq!(failed["error"]["code"], -32002);
    assert!(failed["error"]["message"].as_str().is_some_and(|message| {
        message.contains("HTTP 503") && message.contains("temporarily asleep")
    }));
    assert!(failed_events.is_empty());

    let (compacted, compact_events) = call_with_events(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "compact",
            "method": "ari.compact",
            "params": {"session_id": session_id}
        }),
    );
    assert_eq!(
        compacted["result"]["summary"],
        "The user asked to read note.txt and the answer reported its contents."
    );
    assert_eq!(compacted["result"]["usage"]["responses"], 4);
    assert_eq!(compacted["result"]["usage"]["chat_responses"], 3);
    assert_eq!(compacted["result"]["usage"]["compaction_responses"], 1);
    assert_eq!(compacted["result"]["usage"]["priced_responses"], 4);
    assert_eq!(
        compacted["result"]["usage"]["total_cost"]["femto_units"],
        "134000000000"
    );
    let compact_usage = usage_events(&compact_events);
    assert_eq!(compact_usage.len(), 1);
    assert_eq!(compact_usage[0]["ledger_entry"]["purpose"], "compaction");
    assert!(compact_usage[0]["ledger_entry"]["cost"].is_object());
    assert_eq!(
        compact_usage[0]["ledger_entry"]["response_id"],
        "response-compact"
    );

    call(
        &mut stdin,
        &responses,
        json!({
            "jsonrpc": "2.0",
            "id": "close",
            "method": "ari.close_session",
            "params": {"session_id": session_id}
        }),
    );
    drop(stdin);
    assert_eq!(child.wait().expect("ARI exits").code(), Some(0));
    let mut stderr_text = String::new();
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read child stderr");
    assert!(stderr_text.is_empty(), "unexpected stderr: {stderr_text}");

    let requests = provider.finish();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0]["model"], MODEL);
    assert!(requests[0]["messages"].as_array().is_some_and(|messages| {
        messages.last().is_some_and(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains("ARI_SKILL_SENTINEL"))
        })
    }));
    let observation = requests[1]["messages"]
        .as_array()
        .expect("follow-up messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .map(|content| serde_json::from_str::<Value>(content).expect("tool observation"))
        .expect("read observation");
    assert_eq!(observation["ok"], true);
    assert!(
        observation["result"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("hello from the note"))
    );
    for request in &requests[3..] {
        assert!(
            request.get("tools").is_none(),
            "compact must expose no tools"
        );
        let transcript = request["messages"][1]["content"]
            .as_str()
            .expect("compact transcript");
        assert!(transcript.contains("Use @skill:proof to inspect note.txt"));
        assert!(transcript.contains("The note says hello from the note."));
        assert!(!transcript.contains("ARI_SKILL_SENTINEL"));
    }
}

fn agul_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agul")
}

fn line_reader(stdout: ChildStdout) -> mpsc::Receiver<Value> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("ARI response line");
            if line.trim().is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(&line).expect("ARI response JSON");
            if sender.send(response).is_err() {
                break;
            }
        }
    });
    receiver
}

fn call(stdin: &mut ChildStdin, responses: &mpsc::Receiver<Value>, request: Value) -> Value {
    call_with_events(stdin, responses, request).0
}

fn call_with_events(
    stdin: &mut ChildStdin,
    responses: &mpsc::Receiver<Value>,
    request: Value,
) -> (Value, Vec<Value>) {
    let request_id = request["id"].clone();
    serde_json::to_writer(&mut *stdin, &request).expect("write ARI request");
    stdin.write_all(b"\n").expect("terminate ARI request line");
    stdin.flush().expect("flush ARI request");
    let mut events = Vec::new();
    loop {
        let message = responses
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for an ARI response");
        if message["method"] == "ari.event" {
            events.push(message["params"].clone());
            continue;
        }
        assert_eq!(message["id"], request_id, "unexpected ARI response");
        return (message, events);
    }
}

fn usage_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["kind"] == "usage")
        .collect()
}

enum Reply {
    Sse(Vec<u8>),
    Failure(&'static str),
}

impl Reply {
    fn sse(body: Vec<u8>) -> Self {
        Self::Sse(body)
    }

    fn failure(message: &'static str) -> Self {
        Self::Failure(message)
    }
}

struct FakeProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeProvider {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake provider");
        let address = listener.local_addr().expect("fake provider address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            for reply in replies {
                loop {
                    if should_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let request = read_request_and_respond(stream, &reply);
                            recorded.lock().expect("request lock").push(request);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fake provider accept failed: {error}"),
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn finish(mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .expect("fake provider thread")
            .join()
            .expect("fake provider must not panic");
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request_and_respond(mut stream: TcpStream, reply: &Reply) -> Value {
    stream
        .set_nonblocking(false)
        .expect("blocking fake provider connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request timeout");
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).expect("read request headers");
        assert!(count > 0, "request ended before its headers");
        bytes.extend_from_slice(&buffer[..count]);
    };
    let headers = String::from_utf8(bytes[..header_end - 4].to_vec()).expect("request headers");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("Content-Length"))
        })
        .expect("Content-Length header");
    while bytes.len() < header_end + content_length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let request = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .expect("request JSON");
    let (status, content_type, body) = match reply {
        Reply::Sse(body) => ("200 OK", "text/event-stream", body.clone()),
        Reply::Failure(message) => (
            "503 Service Unavailable",
            "application/json",
            serde_json::to_vec(&json!({"error": {"message": message}})).expect("error JSON"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("response headers");
    stream.write_all(&body).expect("response body");
    stream.flush().expect("flush response");
    request
}

fn tool_response(
    response_id: &str,
    call_id: &str,
    name: &str,
    arguments: Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    stream_response(
        response_id,
        json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&arguments).expect("tool arguments")
                }
            }]
        }),
        input_tokens,
        output_tokens,
    )
}

fn text_response(
    response_id: &str,
    content: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    stream_response(
        response_id,
        json!({"role": "assistant", "content": content}),
        input_tokens,
        output_tokens,
    )
}

fn stream_response(
    response_id: &str,
    delta: Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    let chunks = [
        json!({
            "id": response_id,
            "created": CREATED_AT,
            "model": MODEL,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
        }),
        json!({
            "id": response_id,
            "created": CREATED_AT,
            "model": MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": input_tokens,
                "completion_tokens": output_tokens,
                "prompt_cache_hit_tokens": 0,
                "prompt_cache_miss_tokens": input_tokens
            }
        }),
    ];
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&chunk).expect("SSE chunk"));
        body.push_str("\r\n\r\n");
    }
    body.push_str("data: [DONE]\r\n\r\n");
    body.into_bytes()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
