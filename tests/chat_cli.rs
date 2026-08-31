use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const MODEL: &str = "repair-model";
const CREATED_AT: u64 = 1_787_788_800;

#[test]
fn session_selection_flags_reject_ambiguous_invocations() {
    for arguments in [
        ["--continue", "--session", "saved"].as_slice(),
        ["--continue", "--no-session"].as_slice(),
        ["--resume", "--prompt", "hello"].as_slice(),
    ] {
        let output = Command::new(agul_bin())
            .arg("chat")
            .args(arguments)
            .output()
            .expect("parse chat arguments");
        assert!(
            !output.status.success(),
            "arguments unexpectedly passed: {arguments:?}"
        );
    }
}

#[test]
fn resume_requires_a_terminal_without_creating_session_state() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--resume")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .output()
        .expect("run non-interactive resume");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--resume needs an interactive terminal")
    );
    assert!(!state.exists());
}

#[test]
fn continue_without_a_candidate_does_not_create_a_chat() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--continue")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .args(["--prompt", "hello"])
        .arg("--json")
        .output()
        .expect("run empty continue");

    assert!(!output.status.success());
    assert!(
        one_json_line(&output)["error"]
            .as_str()
            .is_some_and(|error| error.contains("no resumable chats"))
    );
    assert_eq!(
        fs::read_dir(state.join("sessions"))
            .expect("session directory")
            .count(),
        0
    );
}

#[test]
fn agul_checkout_is_a_working_self_maintenance_workspace() {
    let root = tempfile::tempdir().expect("test root");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("isolated home");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let server = FakeServer::start(vec![text_response(
        "response-self-maintenance",
        "Ready to maintain Agul.",
        20,
        5,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--prompt", "State the maintenance objective."])
        .arg("--no-session")
        .arg("--json")
        .current_dir(workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run Agul in its own checkout");
    let requests = server.finish();

    assert_success(&output);
    let result = one_json_line(&output);
    assert_eq!(result["response"], "Ready to maintain Agul.");
    assert!(result["session_id"].is_null());
    assert!(
        requests[0].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("Maintain Agul as a small"))
            })
    );
}

#[test]
fn system_skills_expose_local_agulater_and_its_registered_agentkube_catalog() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    fs::create_dir_all(&bin).expect("fake PATH");
    write_fake_agulater(&bin, true);
    let server = FakeServer::start(vec![text_response(
        "response-system-ecosystem",
        "Catalog available.",
        20,
        5,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args([
            "--prompt",
            "Use @skill:system/agentkube to find a review Skill.",
        ])
        .arg("--no-session")
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PATH", &bin)
        .env("PATHEXT", ".EXE;.CMD;.BAT")
        .env_remove("AGUL_HOST_TOOLS")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run Agul with fake Agulater");
    let requests = server.finish();

    assert_success(&output);
    let messages = requests[0].body["messages"]
        .as_array()
        .expect("request messages");
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .expect("system prompt");
    assert!(system.contains("system/agulater: install, update, and prepare"));
    assert!(system.contains("system/agentkube: find optional AgentKube"));
    assert!(
        !system.contains("agulater catalog search QUERY"),
        "full instructions belong only to an activated Skill"
    );
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .expect("expanded user prompt");
    assert!(user.contains("agulater catalog search QUERY --json"));
    assert!(user.contains("AgentKube is content, not another CLI"));
    let tool_names = requests[0].body["tools"]
        .as_array()
        .expect("tool definitions")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, ["read", "write", "edit", "shell"]);
}

#[test]
fn project_launch_keeps_prepared_user_skills_available() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    let empty_bin = root.path().join("empty-bin");
    let project_runtime = workspace.join(".agents/runtime");
    let user_runtime = home.join(".agents/runtime");
    let project_skill = project_runtime.join("skills/project-review");
    let user_skill = user_runtime.join("skills/user-helper");
    for directory in [
        &project_runtime,
        &user_runtime,
        &project_skill,
        &user_skill,
        &empty_bin,
    ] {
        fs::create_dir_all(directory).expect("Skill fixture directory");
    }
    fs::write(
        project_runtime.join("instructions.md"),
        "Project instructions.\n",
    )
    .expect("project instructions");
    fs::write(user_runtime.join("instructions.md"), "User instructions.\n")
        .expect("user instructions");
    fs::write(
        project_runtime.join("launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"instructions.md","skills":"skills"}"#,
    )
    .expect("project launch");
    fs::write(
        user_runtime.join("launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"instructions.md","skills":"skills"}"#,
    )
    .expect("user launch");
    fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: project-review\ndescription: Project review\n---\nReview the project.\n",
    )
    .expect("project Skill");
    fs::write(
        user_skill.join("SKILL.md"),
        "---\nname: user-helper\ndescription: Prepared user helper\n---\nApply the user helper.\n",
    )
    .expect("user Skill");
    let server = FakeServer::start(vec![text_response(
        "response-user-prepared-skill",
        "User Skill available.",
        20,
        5,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--prompt", "Use @skill:user-helper for this task."])
        .arg("--no-session")
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PATH", &empty_bin)
        .env_remove("AGUL_HOST_TOOLS")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run Agul with project and user launches");
    let requests = server.finish();

    assert_success(&output);
    let messages = requests[0].body["messages"]
        .as_array()
        .expect("request messages");
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .expect("system prompt");
    assert!(system.contains("project-review: Project review"));
    assert!(system.contains("user-helper: Prepared user helper"));
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .expect("expanded user prompt");
    assert!(user.contains("Apply the user helper."));
}

#[test]
fn unconfigured_prices_do_not_add_a_request_or_notice_to_local_chat() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    let server = FakeServer::start(vec![text_response(
        "response-no-price-sync",
        "Ready.",
        10,
        2,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--prompt", "Hello."])
        .arg("--no-session")
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    assert_eq!(one_json_line(&output)["response"], "Ready.");
    assert_eq!(requests.len(), 1, "only the model request should be made");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert!(
        requests[0].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("Runtime limits: 32 model rounds"))
            })
    );
    assert!(!state.join("prices").exists());
}

#[test]
fn glm_provider_uses_coding_plan_defaults_through_a_compatible_proxy() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");
    let server = FakeServer::start(vec![text_response("response-glm", "Ready.", 10, 2)]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--provider")
        .arg("glm")
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--reasoning-effort", "high"])
        .args(["--prompt", "Hello."])
        .arg("--no-session")
        .arg("--json")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .env("GLM_API_KEY", "glm-test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run GLM-compatible chat");
    let requests = server.finish();

    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["model"], "glm-4.7");
    assert!(requests[0].body.get("stream_options").is_none());
    assert_eq!(requests[0].body["tool_stream"], true);
    assert_eq!(requests[0].body["thinking"]["type"], "enabled");
    assert_eq!(requests[0].body["reasoning_effort"], "high");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer glm-test-key")
    );
    let result = one_json_line(&output);
    assert_eq!(result["model"], "glm-4.7");
    assert_eq!(result["billing"], "subscription_quota");
    assert_eq!(result["cost"], Value::Null);
    assert_eq!(result["usage"]["entries"][0]["provider"], "glm");
    assert_eq!(result["usage"]["entries"][0]["price_ref"], Value::Null);
}

#[test]
fn glm_coding_alias_uses_glm_wire_protocol_and_records_subscription_usage() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");
    let server = FakeServer::start(vec![text_response("response-coding", "Ready.", 19, 3)]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .args(["--provider", "glm-coding"])
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--prompt", "Hello."])
        .arg("--no-session")
        .arg("--json")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .env("GLM_API_KEY", "glm-coding-test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run GLM Coding Plan-compatible chat");
    let requests = server.finish();

    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["model"], "glm-4.7");
    assert!(requests[0].body.get("stream_options").is_none());
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer glm-coding-test-key")
    );
    let result = one_json_line(&output);
    assert_eq!(result["model"], "glm-4.7");
    assert_eq!(result["billing"], "subscription_quota");
    assert_eq!(result["cost"], Value::Null);
    assert_eq!(result["usage"]["summary"]["responses"], 1);
    assert_eq!(result["usage"]["summary"]["priced_responses"], 0);
    assert_eq!(result["usage"]["summary"]["unpriced_responses"], 1);
    assert_eq!(result["usage"]["entries"][0]["provider"], "glm");
    assert_eq!(
        result["usage"]["entries"][0]["unpriced_reason"],
        "subscription_quota"
    );
    assert!(!state.join("prices").exists());
}

#[test]
fn reasoning_only_final_answer_remains_visible_when_reasoning_is_hidden() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");
    let server = FakeServer::start(vec![stream_response(
        "response-reasoning-only",
        json!({
            "role": "assistant",
            "reasoning_content": "PROMOTED_FINAL_ANSWER"
        }),
        "stop",
        19,
        3,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .args(["--provider", "glm"])
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--prompt", "Reply once."])
        .arg("--hide-reasoning")
        .arg("--no-color")
        .arg("--no-session")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .env("GLM_API_KEY", "glm-coding-test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run reasoning-only terminal response");
    server.finish();

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("PROMOTED_FINAL_ANSWER").count(), 1);
    assert!(!stdout.contains("◌"));
}

#[test]
fn reasoning_only_final_answer_is_not_replayed_when_reasoning_is_visible() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");
    let server = FakeServer::start(vec![stream_response(
        "response-reasoning-only-visible",
        json!({
            "role": "assistant",
            "reasoning_content": "PROMOTED_VISIBLE_ONCE"
        }),
        "stop",
        19,
        3,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .args(["--provider", "glm"])
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--prompt", "Reply once."])
        .arg("--no-color")
        .arg("--no-session")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .env("GLM_API_KEY", "glm-coding-test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run visible reasoning-only terminal response");
    server.finish();

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("PROMOTED_VISIBLE_ONCE").count(), 1);
    assert!(stdout.contains("◌"));
}

#[test]
fn direct_chat_reuses_a_reported_context_window_across_model_rounds() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    let server = FakeServer::start(vec![
        raw_json_response(
            400,
            json!({"error": {"message": "maximum context length is 8192 tokens and your request has 4097 input tokens"}}),
        ),
        tool_response(
            "response-adapted-tool",
            "call-write-after-adaptation",
            "write",
            json!({"path": "adapted.txt", "content": "adapted\n"}),
            4_097,
            20,
        ),
        text_response("response-after-tool", "Adapted across rounds.", 4_120, 20),
    ]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--no-session")
        .args(["--prompt", "Continue the task."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("AGUL_BASE_URL", server.endpoint())
        .env("AGUL_MODEL", MODEL)
        .env("AGUL_MAX_TOKENS", "16384")
        .env("AGUL_TIMEOUT_SECONDS", "10")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    assert_eq!(one_json_line(&output)["response"], "Adapted across rounds.");
    assert_eq!(requests.len(), 3, "only the first model round retries");
    assert_eq!(requests[0].body["max_tokens"], 16_384);
    assert_eq!(requests[1].body["max_tokens"], 4_094);
    let next_round_max = requests[2].body["max_tokens"].as_u64().unwrap();
    assert!(
        next_round_max < 4_094,
        "the next model round must reuse the learned window and account for its larger input"
    );
}

#[test]
fn configured_context_trims_large_tool_results_before_the_next_round() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        workspace.join("large.txt"),
        format!("HEAD{}TAIL", "x".repeat(160_000)),
    )
    .expect("large tool fixture");
    let server = FakeServer::start(vec![
        tool_response(
            "response-read-large",
            "call-read-large",
            "read",
            json!({"path": "large.txt"}),
            1_000,
            20,
        ),
        tool_response(
            "response-write-after-large",
            "call-write-after-large",
            "write",
            json!({"path": "small.txt", "content": "small\n"}),
            20_000,
            20,
        ),
        text_response("response-after-large", "Large output handled.", 20_000, 20),
    ]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--context-window", "32768"])
        .args(["--max-tokens", "3072"])
        .args(["--timeout-seconds", "10"])
        .arg("--no-session")
        .args(["--prompt", "Read large.txt, then report completion."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    assert_eq!(one_json_line(&output)["response"], "Large output handled.");
    assert_eq!(requests.len(), 3);
    let trimmed_read = tool_content(&requests[1], "call-read-large");
    assert!(trimmed_read.contains("tool output trimmed"));
    assert!(trimmed_read.len() < 160_000);
    assert_eq!(requests[1].body["max_tokens"], 3_072);
    assert_eq!(
        tool_content(&requests[2], "call-read-large"),
        trimmed_read,
        "a canonical trimmed result must not change again in the next round"
    );
    assert_eq!(requests[2].body["max_tokens"], 3_072);
}

#[test]
fn native_resume_and_continue_reuse_the_exact_completed_provider_history() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    let skill = workspace.join(".agents/skills/cache-proof");
    fs::create_dir_all(&skill).expect("skill directory");
    fs::create_dir_all(workspace.join(".agents/runtime")).expect("runtime directory");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        workspace.join(".agents/AGENTS.md"),
        "FIRST_PROCESS_SYSTEM_SENTINEL\n",
    )
    .expect("first instructions");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: cache-proof\ndescription: Verify persisted provider history\n---\nEXPANDED_SKILL_SENTINEL\n",
    )
    .expect("skill");
    fs::write(
        workspace.join(".agents/runtime/launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","skills":"../skills"}"#,
    )
    .expect("isolated launch");

    let server = FakeServer::start(vec![
        reasoning_tool_response(
            "response-persisted-tool",
            "call-persisted-write",
            "write",
            json!({"path": "cache.txt", "content": "stable\n"}),
            "PERSISTED_TOOL_REASONING",
            100,
            10,
        ),
        text_response("response-persisted-final", "First turn complete.", 120, 12),
        text_response(
            "response-after-exact-resume",
            "Second turn complete.",
            140,
            14,
        ),
        text_response("response-after-continue", "Third turn complete.", 160, 16),
    ]);
    let provider_endpoint = server.endpoint().to_string();
    let first = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .args(["--provider", "glm"])
        .arg("--base-url")
        .arg(&provider_endpoint)
        .args(["--model", MODEL])
        .args(["--reasoning-effort", "medium"])
        .args(["--timeout-seconds", "10"])
        .args([
            "--prompt",
            "Use @skill:cache-proof to write cache.txt, then finish.",
        ])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("first agul process");
    assert_success(&first);
    let session_id = one_json_line(&first)["session_id"]
        .as_str()
        .expect("persisted session id")
        .to_string();

    fs::write(
        workspace.join(".agents/AGENTS.md"),
        "SECOND_PROCESS_CHANGED_SYSTEM_SENTINEL\n",
    )
    .expect("changed instructions");
    let resumed = Command::new(agul_bin())
        .arg("chat")
        .arg("--session")
        .arg(&session_id)
        .arg("--state-dir")
        .arg(&state)
        .args(["--timeout-seconds", "10"])
        .args(["--prompt", "Continue with the saved prefix."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("resumed agul process");
    assert_success(&resumed);
    assert_eq!(one_json_line(&resumed)["session_id"], session_id);

    let continued = Command::new(agul_bin())
        .arg("chat")
        .arg("--continue")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .args(["--timeout-seconds", "10"])
        .args(["--prompt", "Continue without copying the session id."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("continued agul process");
    assert_success(&continued);
    assert_eq!(one_json_line(&continued)["session_id"], session_id);

    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].body["reasoning_effort"], "high");
    assert_eq!(requests[2].body["reasoning_effort"], "high");
    assert!(requests[0].body.get("stream_options").is_none());
    assert!(requests[2].body.get("stream_options").is_none());
    let mut expected_history = requests[1].body["messages"]
        .as_array()
        .expect("first-process messages")
        .clone();
    assert!(expected_history.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("EXPANDED_SKILL_SENTINEL"))
    }));
    expected_history.push(json!({
        "role": "assistant",
        "content": "First turn complete."
    }));
    let resumed_messages = requests[2].body["messages"]
        .as_array()
        .expect("resumed messages");
    assert_eq!(
        &resumed_messages[..expected_history.len()],
        expected_history.as_slice(),
        "completed assistant/tool messages must cross the process boundary unchanged"
    );
    assert_eq!(
        resumed_messages.last().unwrap(),
        &json!({"role": "user", "content": "Continue with the saved prefix."})
    );
    assert!(
        resumed_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("FIRST_PROCESS_SYSTEM_SENTINEL")
    );
    assert!(
        !resumed_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("SECOND_PROCESS_CHANGED_SYSTEM_SENTINEL")
    );
    let continued_messages = requests[3].body["messages"]
        .as_array()
        .expect("continued messages");
    assert!(continued_messages.iter().any(|message| {
        message["role"] == "assistant" && message["content"] == "Second turn complete."
    }));
    assert_eq!(
        continued_messages.last().unwrap(),
        &json!({"role": "user", "content": "Continue without copying the session id."})
    );

    let stored: Value = serde_json::from_slice(
        &fs::read(state.join("sessions").join(format!("{session_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["schema"], "agul/chat-session/v5");
    assert_eq!(stored["native_config"]["provider"], "glm");
    assert_eq!(stored["native_config"]["base_url"], provider_endpoint);
    assert_eq!(stored["native_config"]["api_key_env"], "GLM_API_KEY");
    assert_eq!(stored["native_config"]["reasoning_effort"], "high");
    assert_eq!(stored["native_history"][2]["role"], "assistant");
    assert_eq!(
        stored["native_history"][2]["reasoning"],
        "PERSISTED_TOOL_REASONING"
    );
}

#[test]
fn deepseek_resume_replays_completed_reasoning_when_tools_are_present() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(workspace.join("note.txt"), "stable\n").expect("fixture file");
    let server = FakeServer::start(vec![
        reasoning_tool_response(
            "response-deepseek-tool",
            "call-deepseek-read",
            "read",
            json!({"path": "note.txt"}),
            "TOOL_REASONING",
            100,
            10,
        ),
        reasoning_text_response(
            "response-deepseek-final",
            "First turn complete.",
            "FINAL_REASONING",
            120,
            12,
        ),
        text_response(
            "response-deepseek-resumed",
            "Second turn complete.",
            140,
            14,
        ),
    ]);

    let first = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .args(["--provider", "deepseek"])
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--prompt", "Read note.txt, then finish."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("DEEPSEEK_API_KEY", "test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("first DeepSeek process");
    assert_success(&first);
    let session_id = one_json_line(&first)["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    let resumed = Command::new(agul_bin())
        .arg("chat")
        .arg("--session")
        .arg(&session_id)
        .arg("--state-dir")
        .arg(&state)
        .args(["--prompt", "Continue from the preserved DeepSeek history."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("DEEPSEEK_API_KEY", "test-key")
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("resumed DeepSeek process");
    assert_success(&resumed);

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    let resumed_messages = requests[2].body["messages"]
        .as_array()
        .expect("resumed messages");
    let tool_envelope = resumed_messages
        .iter()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .expect("tool-call assistant message");
    assert_eq!(tool_envelope["reasoning_content"], "TOOL_REASONING");
    let completed = resumed_messages
        .iter()
        .find(|message| message["content"] == "First turn complete.")
        .expect("completed assistant message");
    assert_eq!(completed["reasoning_content"], "FINAL_REASONING");
}

#[test]
fn failed_json_turn_reports_persisted_session_and_resume_progress() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    let skill = workspace.join(".agents/skills/recovery");
    fs::create_dir_all(&skill).expect("skill directory");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: recovery\ndescription: Restore a failed task\n---\nRECOVERY_SKILL_SENTINEL\n",
    )
    .expect("skill");
    let server = FakeServer::start(vec![tool_response(
        "response-write-before-failure",
        "call-write-before-failure",
        "write",
        json!({"path": "partial.txt", "content": "preserved\n"}),
        100,
        10,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--timeout-seconds", "1"])
        .args(["--prompt", "Use @skill:recovery to start the task."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let _ = server.finish();

    assert_eq!(output.status.code(), Some(1));
    let document = one_json_line(&output);
    let session_id = document["session_id"].as_str().expect("session id");
    assert_eq!(document["ok"], false);
    assert_eq!(document["rounds"], 1);
    assert_eq!(document["tool_calls"], 1);
    assert_eq!(document["usage"]["summary"]["responses"], 1);
    assert!(document["resume"].as_str().unwrap().contains(session_id));
    assert_eq!(
        fs::read_to_string(workspace.join("partial.txt")).unwrap(),
        "preserved\n"
    );
    assert!(
        state
            .join("sessions")
            .join(format!("{session_id}.json"))
            .is_file()
    );
    let stored: Value = serde_json::from_slice(
        &fs::read(state.join("sessions").join(format!("{session_id}.json"))).unwrap(),
    )
    .unwrap();
    assert!(stored["turns"].as_array().unwrap().is_empty());
    assert_eq!(
        stored["pending_turn"]["visible_user"],
        "Use @skill:recovery to start the task."
    );
    assert!(
        stored["pending_turn"]["model_input"]
            .as_str()
            .unwrap()
            .contains("RECOVERY_SKILL_SENTINEL")
    );

    let resume_server =
        FakeServer::start(vec![text_response("response-resumed", "Resumed.", 20, 5)]);
    let resumed = Command::new(agul_bin())
        .arg("chat")
        .arg("--session")
        .arg(session_id)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(resume_server.endpoint())
        .args(["--timeout-seconds", "10"])
        .args(["--prompt", "Continue the task."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("resume agul chat");
    let resumed_requests = resume_server.finish();
    assert_success(&resumed);
    assert!(
        resumed_requests[0].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("RECOVERY_SKILL_SENTINEL"))
            })
    );
    let resumed_stored: Value = serde_json::from_slice(
        &fs::read(state.join("sessions").join(format!("{session_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(resumed_stored["pending_turn"], Value::Null);
    assert_eq!(
        resumed_stored["turns"][0]["user"],
        "Use @skill:recovery to start the task."
    );
    assert!(
        !resumed_stored["turns"][0]["user"]
            .as_str()
            .unwrap()
            .contains("RECOVERY_SKILL_SENTINEL")
    );
    assert_eq!(resumed_stored["turns"][1]["assistant"], "Resumed.");
}

#[test]
fn failed_no_session_json_does_not_offer_an_unusable_session() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    let server = FakeServer::start(vec![tool_response(
        "response-write-before-failure",
        "call-write-before-failure",
        "write",
        json!({"path": "partial.txt", "content": "preserved\n"}),
        100,
        10,
    )]);

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--no-session")
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--timeout-seconds", "1"])
        .args(["--prompt", "Start the task."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let _ = server.finish();

    assert_eq!(output.status.code(), Some(1));
    let document = one_json_line(&output);
    assert!(document["session_id"].is_null());
    assert!(!document["resume"].as_str().unwrap().contains("--session"));
    assert_eq!(
        fs::read_to_string(workspace.join("partial.txt")).unwrap(),
        "preserved\n"
    );
}

#[test]
fn failed_json_progress_is_exact_at_tool_and_round_limits() {
    let tool_limit = failed_progress(
        stream_response(
            "response-tool-limit",
            json!({
                "role": "assistant",
                "tool_calls": [
                    tool_call_delta(0, "call-one", "write", json!({"path": "one.txt", "content": "one"})),
                    tool_call_delta(1, "call-two", "write", json!({"path": "two.txt", "content": "two"}))
                ]
            }),
            "tool_calls",
            100,
            10,
        ),
        4,
        1,
    );
    assert_eq!(tool_limit["rounds"], 1);
    assert_eq!(tool_limit["tool_calls"], 1);
    assert!(
        tool_limit["error"]
            .as_str()
            .unwrap()
            .contains("tool-call limit")
    );

    let round_limit = failed_progress(
        tool_response(
            "response-round-limit",
            "call-round",
            "write",
            json!({"path": "round.txt", "content": "one"}),
            100,
            10,
        ),
        1,
        4,
    );
    assert_eq!(round_limit["rounds"], 1);
    assert_eq!(round_limit["tool_calls"], 1);
    assert!(
        round_limit["error"]
            .as_str()
            .unwrap()
            .contains("model-round limit")
    );
}

fn failed_progress(response: Vec<u8>, max_rounds: u32, max_tool_calls: u32) -> Value {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");
    let server = FakeServer::start(vec![response]);
    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .arg("--max-rounds")
        .arg(max_rounds.to_string())
        .arg("--max-tool-calls")
        .arg(max_tool_calls.to_string())
        .args(["--prompt", "Exercise the limit."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let _ = server.finish();
    assert_eq!(output.status.code(), Some(1));
    one_json_line(&output)
}

#[test]
fn direct_chat_repairs_with_four_tools_and_records_every_response_cost() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("isolated home");

    let shell_command = if cfg!(windows) {
        "if ((Get-Content -Raw -LiteralPath 'repair.txt').Trim() -ne 'fixed') { exit 1 }"
    } else {
        "test \"$(cat repair.txt)\" = fixed"
    };
    let server = FakeServer::start(vec![
        reasoning_tool_response(
            "response-write",
            "call-write",
            "write",
            json!({"path": "repair.txt", "content": "broken\n"}),
            "inspect the file before editing",
            100,
            10,
        ),
        tool_response(
            "response-edit-fail",
            "call-edit-fail",
            "edit",
            json!({
                "path": "repair.txt",
                "old_text": "not-present",
                "new_text": "fixed"
            }),
            120,
            11,
        ),
        tool_response(
            "response-edit-retry",
            "call-edit-retry",
            "edit",
            json!({
                "path": "repair.txt",
                "old_text": "broken",
                "new_text": "fixed"
            }),
            140,
            12,
        ),
        tool_response(
            "response-shell",
            "call-shell",
            "shell",
            json!({"command": shell_command}),
            160,
            13,
        ),
        text_response("response-final", "Repair complete.", 180, 14),
    ]);
    let price_card = root.path().join("price-card.json");
    fs::write(
        &price_card,
        serde_json::to_vec_pretty(&matching_price_card(server.origin()))
            .expect("serialize price card"),
    )
    .expect("write price card");

    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .arg("--price-card")
        .arg(&price_card)
        .arg("--state-dir")
        .arg(&state)
        .args(["--timeout-seconds", "10"])
        .args([
            "--prompt",
            "Repair repair.txt, verify it, and report completion.",
        ])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    let document = one_json_line(&output);
    assert_eq!(document["ok"], true);
    assert_eq!(document["response"], "Repair complete.");
    assert_eq!(document["rounds"], 5);
    assert_eq!(document["tool_calls"], 4);
    assert_eq!(document["cost"], "$0.002");
    assert_eq!(
        fs::read_to_string(workspace.join("repair.txt")).expect("repaired file"),
        "fixed\n"
    );

    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(request.body["stream"], true);
        assert!(
            request.body.get("tool_choice").is_none(),
            "provider defaults choose tools without a forced compatibility field"
        );
        assert!(
            !request.headers.contains_key("authorization"),
            "a credential-less local request must not invent Authorization"
        );
    }
    let tool_names = requests[0].body["tools"]
        .as_array()
        .expect("tool definitions")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, ["read", "write", "edit", "shell"]);
    let assistant_tool_message = requests[1].body["messages"]
        .as_array()
        .expect("follow-up messages")
        .iter()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .expect("assistant tool message");
    assert_eq!(assistant_tool_message["content"], "");
    assert_eq!(
        assistant_tool_message["reasoning_content"],
        "inspect the file before editing"
    );

    let observations = requests[1..]
        .iter()
        .map(last_tool_observation)
        .collect::<Vec<_>>();
    assert_eq!(
        observations
            .iter()
            .filter(|observation| observation["ok"] == false)
            .count(),
        1
    );
    assert_eq!(observations[0]["ok"], true);
    assert_eq!(observations[1]["ok"], false);
    assert!(
        observations[1]["error"]
            .as_str()
            .expect("edit error")
            .contains("old_text was not found")
    );
    assert_eq!(observations[2]["ok"], true);
    assert_eq!(observations[3]["result"]["success"], true);

    let summary = &document["usage"]["summary"];
    assert_eq!(summary["responses"], 5);
    assert_eq!(summary["chat_responses"], 5);
    assert_eq!(summary["responses_with_usage"], 5);
    assert_eq!(summary["priced_responses"], 5);
    assert_eq!(summary["unpriced_responses"], 0);
    assert_eq!(summary["input_tokens"], 700);
    assert_eq!(summary["output_tokens"], 60);
    assert_eq!(summary["total_cost"]["currency"], "USD");
    assert_eq!(summary["total_cost"]["femto_units"], "1580000000000");
    assert_eq!(summary["total_cost_unavailable"], false);

    let entries = document["usage"]["entries"]
        .as_array()
        .expect("per-response usage ledger");
    assert_eq!(entries.len(), 5);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["response_id"].as_str().expect("response id"))
            .collect::<Vec<_>>(),
        [
            "response-write",
            "response-edit-fail",
            "response-edit-retry",
            "response-shell",
            "response-final",
        ]
    );
    for entry in entries {
        assert_eq!(entry["purpose"], "chat");
        assert_eq!(entry["unpriced_reason"], Value::Null);
        assert_eq!(entry["stale"], false);
        assert_eq!(entry["price_ref"]["catalog_id"], "test-local-usd");
        assert_eq!(entry["price_ref"]["catalog_version"], "2026-08-27");
        assert!(
            entry["cost"]["femto_units"]
                .as_str()
                .expect("exact entry cost")
                .parse::<u128>()
                .expect("integer entry cost")
                > 0
        );
    }
}

#[test]
fn direct_chat_discovers_and_runs_a_launch_plugin() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let state_dir = root.path().join("state");
    let runtime = workspace.join(".agents/runtime");
    let plugin = workspace.join(".agents/plugins/echo");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::create_dir_all(&plugin).expect("plugin");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        workspace.join(".agents/AGENTS.md"),
        "Use prepared plugins.\n",
    )
    .expect("instructions");
    fs::write(
        runtime.join("launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","plugins":"../plugins"}"#,
    )
    .expect("launch");
    let (command, script_name, script, _) = plugin_command_and_scripts();
    fs::write(plugin.join(script_name), script).expect("plugin script");
    fs::write(
        plugin.join("plugin.json"),
        serde_json::to_vec_pretty(&json!({
            "format": "agul/plugin/v2",
            "name": "echo",
            "version": "1.0.0",
            "command": command,
            "tools": [{
                "name": "echo_text",
                "description": "Echo text",
                "parameters": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }
            }]
        }))
        .expect("plugin manifest"),
    )
    .expect("write plugin manifest");

    let server = FakeServer::start(vec![
        tool_response(
            "response-plugin",
            "call-plugin",
            "echo_text",
            json!({"text": "hello"}),
            100,
            10,
        ),
        text_response("response-final", "Plugin complete.", 120, 12),
    ]);
    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--timeout-seconds", "10"])
        .arg("--no-session")
        .args(["--prompt", "Use echo_text, then report completion."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    let document = one_json_line(&output);
    assert_eq!(document["response"], "Plugin complete.");
    assert_eq!(document["tool_calls"], 1);
    let names = requests[0].body["tools"]
        .as_array()
        .expect("tool definitions")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "write", "edit", "shell", "echo_text"]);
    assert_eq!(last_tool_content(&requests[1]), "plugin-ok");
    let invocation: Value = serde_json::from_slice(
        &fs::read(plugin.join("invocation.json")).expect("plugin invocation"),
    )
    .expect("plugin invocation JSON");
    assert_eq!(invocation["tool"], "echo_text");
    assert_eq!(invocation["arguments"], json!({"text": "hello"}));
    assert_eq!(invocation["context"]["call_id"], "call-plugin");
    assert!(
        invocation["context"]["session_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace");
    assert_eq!(
        invocation["context"]["workspace"],
        json!(canonical_workspace)
    );
    assert_eq!(
        invocation["context"]["launch_path"],
        json!(canonical_workspace.join(".agents/runtime/launch.json"))
    );
    let context = invocation["context"].as_object().expect("plugin context");
    assert_eq!(context.len(), 4, "Plugin v2 context must remain stable");
    assert!(!context.contains_key("state_dir"));
    assert_eq!(
        fs::read_to_string(plugin.join("state-dir.txt")).expect("plugin state directory"),
        state_dir.to_string_lossy()
    );
}

#[test]
fn direct_chat_reports_plugin_failure_and_continues_to_a_final_answer() {
    let root = tempfile::tempdir().expect("test root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let runtime = workspace.join(".agents/runtime");
    let plugin = workspace.join(".agents/plugins/broken");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::create_dir_all(&plugin).expect("plugin");
    fs::create_dir_all(&home).expect("isolated home");
    fs::write(
        workspace.join(".agents/AGENTS.md"),
        "Use prepared plugins.\n",
    )
    .expect("instructions");
    fs::write(
        runtime.join("launch.json"),
        r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","plugins":"../plugins"}"#,
    )
    .expect("launch");
    let (command, script_name, _, failure_script) = plugin_command_and_scripts();
    fs::write(plugin.join(script_name), failure_script).expect("plugin script");
    fs::write(
        plugin.join("plugin.json"),
        serde_json::to_vec_pretty(&json!({
            "format": "agul/plugin/v2",
            "name": "broken",
            "version": "1.0.0",
            "command": command,
            "tools": [{
                "name": "broken_lookup",
                "description": "Return a test failure",
                "parameters": {"type": "object"}
            }]
        }))
        .expect("plugin manifest"),
    )
    .expect("write plugin manifest");

    let server = FakeServer::start(vec![
        tool_response(
            "response-plugin-failure",
            "call-plugin-failure",
            "broken_lookup",
            json!({}),
            100,
            10,
        ),
        text_response(
            "response-after-plugin-failure",
            "Recovered after plugin failure.",
            120,
            12,
        ),
    ]);
    let output = Command::new(agul_bin())
        .arg("chat")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--base-url")
        .arg(server.endpoint())
        .args(["--model", MODEL])
        .args(["--timeout-seconds", "10"])
        .arg("--no-session")
        .args(["--prompt", "Try broken_lookup, then answer anyway."])
        .arg("--json")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .output()
        .expect("run agul chat");
    let requests = server.finish();

    assert_success(&output);
    let document = one_json_line(&output);
    assert_eq!(document["response"], "Recovered after plugin failure.");
    assert_eq!(document["rounds"], 2);
    assert_eq!(document["tool_calls"], 1);
    let observation = last_tool_observation(&requests[1]);
    assert_eq!(observation["ok"], false);
    assert_eq!(observation["error"]["code"], "plugin_runtime");
    let message = observation["error"]["message"]
        .as_str()
        .expect("plugin error message");
    assert!(message.contains("exited with"), "{message}");
    assert!(message.contains("plugin-broke"), "{message}");
}

fn plugin_command_and_scripts() -> (Vec<String>, &'static str, &'static str, &'static str) {
    #[cfg(windows)]
    {
        (
            vec![
                "powershell.exe".to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                "plugin.ps1".to_string(),
            ],
            "plugin.ps1",
            "$payload = [Console]::In.ReadToEnd()\n[IO.File]::WriteAllText((Join-Path (Get-Location) 'invocation.json'), $payload)\n[IO.File]::WriteAllText((Join-Path (Get-Location) 'state-dir.txt'), [string]$env:AGUL_STATE_DIR)\n[Console]::Out.WriteLine('{\"type\":\"result\",\"call_id\":\"call-plugin\",\"seq\":1,\"ok\":true,\"content\":\"plugin-ok\"}')\n",
            "$null = [Console]::In.ReadToEnd()\n[Console]::Error.Write('plugin-broke')\nexit 7\n",
        )
    }
    #[cfg(unix)]
    {
        (
            vec!["/bin/sh".to_string(), "plugin.sh".to_string()],
            "plugin.sh",
            "#!/bin/sh\npayload=$(cat)\nprintf '%s' \"$payload\" > invocation.json\nprintf '%s' \"${AGUL_STATE_DIR-}\" > state-dir.txt\nprintf '%s\\n' '{\"type\":\"result\",\"call_id\":\"call-plugin\",\"seq\":1,\"ok\":true,\"content\":\"plugin-ok\"}'\n",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' 'plugin-broke' >&2\nexit 7\n",
        )
    }
}

fn agul_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agul")
}

#[cfg(windows)]
fn write_fake_agulater(directory: &Path, registered: bool) {
    let catalogs = if registered {
        r#"[{"id":"agentkube","url":"https://example.test/catalog.json","cached":false,"entries":0}]"#
    } else {
        "[]"
    };
    fs::write(
        directory.join("agulater.cmd"),
        format!(
            "@echo off\r\nif \"%1 %2 %3\"==\"catalog list --json\" (\r\n  echo {{\"format\":\"agulater/catalog-list/v1\",\"catalogs\":{catalogs}}}\r\n  exit /b 0\r\n)\r\nexit /b 2\r\n"
        ),
    )
    .expect("fake Agulater command");
}

#[cfg(unix)]
fn write_fake_agulater(directory: &Path, registered: bool) {
    use std::os::unix::fs::PermissionsExt;

    let catalogs = if registered {
        r#"[{"id":"agentkube","url":"https://example.test/catalog.json","cached":false,"entries":0}]"#
    } else {
        "[]"
    };
    let path = directory.join("agulater");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1 $2 $3\" = \"catalog list --json\" ]; then\n  printf '%s\\n' '{{\"format\":\"agulater/catalog-list/v1\",\"catalogs\":{catalogs}}}'\n  exit 0\nfi\nexit 2\n"
        ),
    )
    .expect("fake Agulater command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("fake Agulater executable");
}

fn matching_price_card(origin: &str) -> Value {
    json!({
        "schema": "agul/price-catalog/v0.3",
        "id": "test-local-usd",
        "version": "2026-08-27",
        "source": "https://example.test/pricing",
        "source_checked_at": CREATED_AT,
        "review_after": null,
        "currency": "USD",
        "cards": [{
            "id": "repair-model-standard",
            "provider": "openai-compatible",
            "origin": origin,
            "models": [MODEL],
            "effective_from": 0,
            "effective_until": null,
            "default_band": "standard",
            "bands": [{
                "id": "standard",
                "rates": {
                    "cache_hit_input_nanos_per_million": 1000000000_u64,
                    "cache_miss_input_nanos_per_million": 2000000000_u64,
                    "output_nanos_per_million": 3000000000_u64
                }
            }],
            "schedule": []
        }]
    })
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
        "tool_calls",
        input_tokens,
        output_tokens,
    )
}

fn reasoning_tool_response(
    response_id: &str,
    call_id: &str,
    name: &str,
    arguments: Value,
    reasoning: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    stream_response(
        response_id,
        json!({
            "role": "assistant",
            "reasoning_content": reasoning,
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
        "tool_calls",
        input_tokens,
        output_tokens,
    )
}

fn tool_call_delta(index: u32, id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(&arguments).expect("tool arguments")
        }
    })
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
        "stop",
        input_tokens,
        output_tokens,
    )
}

fn reasoning_text_response(
    response_id: &str,
    content: &str,
    reasoning: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    stream_response(
        response_id,
        json!({
            "role": "assistant",
            "reasoning_content": reasoning,
            "content": content
        }),
        "stop",
        input_tokens,
        output_tokens,
    )
}

fn stream_response(
    response_id: &str,
    delta: Value,
    finish_reason: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<u8> {
    let chunks = [
        json!({
            "id": response_id,
            "object": "chat.completion.chunk",
            "created": CREATED_AT,
            "model": MODEL,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
        }),
        json!({
            "id": response_id,
            "object": "chat.completion.chunk",
            "created": CREATED_AT,
            "model": MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
            "usage": {
                "prompt_tokens": input_tokens,
                "completion_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
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

fn raw_json_response(status: u16, body: Value) -> Vec<u8> {
    let body = serde_json::to_vec(&body).expect("JSON error body");
    let reason = if status == 400 {
        "Bad Request"
    } else {
        "Error"
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend(body);
    response
}

fn last_tool_observation(request: &RecordedRequest) -> Value {
    serde_json::from_str(last_tool_content(request)).expect("tool observation JSON")
}

fn last_tool_content(request: &RecordedRequest) -> &str {
    request.body["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("follow-up tool observation")
}

fn tool_content<'a>(request: &'a RecordedRequest, call_id: &str) -> &'a str {
    request.body["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .find(|message| {
            message["role"] == "tool" && message["tool_call_id"].as_str() == Some(call_id)
        })
        .and_then(|message| message["content"].as_str())
        .expect("tool observation by id")
}

fn one_json_line(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "JSON stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("UTF-8 JSON stdout");
    assert_eq!(stdout.lines().count(), 1, "JSON mode must emit one line");
    serde_json::from_str(stdout).expect("chat JSON")
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

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct FakeServer {
    origin: String,
    endpoint: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeServer {
    fn start(responses: Vec<Vec<u8>>) -> Self {
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
            for response in responses {
                loop {
                    if should_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let request = read_request_and_respond(stream, &response);
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
        let origin = format!("http://{address}");
        Self {
            endpoint: format!("{origin}/v1/chat/completions"),
            origin,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn origin(&self) -> &str {
        &self.origin
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn finish(mut self) -> Vec<RecordedRequest> {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .expect("fake provider thread")
            .join()
            .expect("fake provider must not panic");
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request_and_respond(mut stream: TcpStream, response: &[u8]) -> RecordedRequest {
    stream
        .set_nonblocking(false)
        .expect("blocking accepted socket");
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
    let header_text =
        String::from_utf8(bytes[..header_end - 4].to_vec()).expect("UTF-8 request headers");
    let mut lines = header_text.split("\r\n");
    let mut request_line = lines.next().expect("request line").split_ascii_whitespace();
    assert_eq!(request_line.next(), Some("POST"));
    let path = request_line.next().expect("request path").to_string();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').expect("HTTP header");
            (name.to_ascii_lowercase(), value.trim().to_string())
        })
        .collect::<BTreeMap<_, _>>();
    let content_length = headers["content-length"]
        .parse::<usize>()
        .expect("Content-Length");
    while bytes.len() < header_end + content_length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .expect("request JSON");

    if response.starts_with(b"HTTP/") {
        stream.write_all(response).expect("raw HTTP response");
    } else {
        let response_headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        stream
            .write_all(response_headers.as_bytes())
            .expect("response headers");
        stream.write_all(response).expect("SSE response");
    }
    stream.flush().expect("flush SSE response");
    RecordedRequest {
        path,
        headers,
        body,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
