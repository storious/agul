use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::boundary::{CodexBoundary, detect as detect_boundary};
use super::transport::{AppServer, CodexError};
use crate::runtime::TurnCancellation;
use crate::runtime::direct_chat::{
    ChatError, ChatEvent, ResponseObservation, TurnOutcome, VisibleTurn,
};
use crate::runtime::project::Project;
use crate::runtime::provider::Usage;

const MESSAGE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const INTERRUPT_RPC_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) struct CodexChatConfig {
    pub(crate) command: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) resume_thread_id: Option<String>,
    pub(crate) ephemeral: bool,
    pub(crate) timeout: Duration,
}

pub(crate) struct CodexChat {
    server: AppServer,
    thread_id: String,
    model: String,
    reasoning_effort: Option<String>,
    workspace: String,
    developer_instructions: String,
    boundary: CodexBoundary,
    ephemeral: bool,
    timeout: Duration,
}

impl CodexChat {
    pub(crate) fn new(project: &Project, config: CodexChatConfig) -> Result<Self, ChatError> {
        let boundary = detect_boundary();
        let mut server = AppServer::start_timeout(
            config.command.as_deref(),
            config.reasoning_effort.as_deref(),
            config.timeout,
        )
        .map_err(chat_error)?;
        require_chatgpt_account(&mut server, config.timeout)?;
        let model = resolve_model(&mut server, config.model.as_deref(), config.timeout)?;
        let workspace = project.workspace.to_string_lossy().into_owned();
        let developer_instructions = project.system_prompt();
        let thread = create_thread(
            &mut server,
            ThreadRequest {
                workspace: &workspace,
                developer_instructions: &developer_instructions,
                model: &model,
                reasoning_effort: config.reasoning_effort.as_deref(),
                resume_thread_id: config.resume_thread_id.as_deref(),
                boundary,
                ephemeral: config.ephemeral,
                timeout: config.timeout,
            },
        )?;
        Ok(Self {
            server,
            thread_id: thread.id,
            model: thread.model,
            reasoning_effort: config.reasoning_effort.or(thread.reasoning_effort),
            workspace,
            developer_instructions,
            boundary,
            ephemeral: config.ephemeral,
            timeout: config.timeout,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn endpoint(&self) -> &'static str {
        "codex://chatgpt"
    }

    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub(crate) fn reset(&mut self) -> Result<(), ChatError> {
        let thread = create_thread(
            &mut self.server,
            ThreadRequest {
                workspace: &self.workspace,
                developer_instructions: &self.developer_instructions,
                model: &self.model,
                reasoning_effort: self.reasoning_effort.as_deref(),
                resume_thread_id: None,
                boundary: self.boundary,
                ephemeral: self.ephemeral,
                timeout: self.timeout,
            },
        )?;
        self.thread_id = thread.id;
        self.model = thread.model;
        if self.reasoning_effort.is_none() {
            self.reasoning_effort = thread.reasoning_effort;
        }
        Ok(())
    }

    pub(crate) fn restore(&mut self, _summary: Option<&str>, _turns: &[VisibleTurn]) {
        // Codex owns the upstream thread history. The Agul session retains the
        // visible transcript and the upstream id used by thread/resume.
    }

    pub(crate) fn restore_interrupted(&mut self, _model_input: &str, _assistant_note: &str) {}

    pub(crate) fn send_cancellable(
        &mut self,
        input: impl Into<String>,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<TurnOutcome, ChatError> {
        send_turn(
            &mut self.server,
            TurnRequest {
                thread_id: &self.thread_id,
                input: input.into(),
                workspace: &self.workspace,
                model: &self.model,
                reasoning_effort: self.reasoning_effort.as_deref(),
                boundary: self.boundary,
                timeout: self.timeout,
            },
            cancellation,
            on_event,
        )
    }
}

trait TurnServer {
    fn call_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CodexError>;

    fn next_message_timeout(&mut self, timeout: Duration) -> Result<Value, CodexError>;
}

impl TurnServer for AppServer {
    fn call_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CodexError> {
        AppServer::call_timeout(self, method, params, timeout)
    }

    fn next_message_timeout(&mut self, timeout: Duration) -> Result<Value, CodexError> {
        AppServer::next_message_timeout(self, timeout)
    }
}

struct TurnRequest<'a> {
    thread_id: &'a str,
    input: String,
    workspace: &'a str,
    model: &'a str,
    reasoning_effort: Option<&'a str>,
    boundary: CodexBoundary,
    timeout: Duration,
}

fn send_turn(
    server: &mut impl TurnServer,
    request: TurnRequest<'_>,
    cancellation: &TurnCancellation,
    on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
) -> Result<TurnOutcome, ChatError> {
    let started = Instant::now();
    let deadline = started
        .checked_add(request.timeout)
        .ok_or_else(|| ChatError::new("Codex timeout is too large"))?;
    let params = turn_params(
        request.thread_id,
        request.input,
        request.workspace,
        request.model,
        request.reasoning_effort,
        request.boundary,
    );
    let response = server
        .call_timeout("turn/start", params, remaining(deadline)?)
        .map_err(chat_error)?;
    let turn_id = response
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .ok_or_else(|| ChatError::new("Codex turn/start response has no turn id"))?
        .to_string();
    let mut collector = TurnCollector::new(request.thread_id, &turn_id, request.model);
    let mut interrupt_requested = false;
    let mut interrupt_error = None;
    loop {
        if cancellation.is_cancelled() && !interrupt_requested {
            interrupt_requested = true;
            let interrupt_timeout = remaining(deadline)?.min(INTERRUPT_RPC_TIMEOUT);
            if let Err(error) = server.call_timeout(
                "turn/interrupt",
                json!({"threadId": request.thread_id, "turnId": turn_id}),
                interrupt_timeout,
            ) && !error.is_timeout()
            {
                // Keep draining this turn even when the upstream rejects the
                // interrupt. Leaving its notifications behind would corrupt
                // the next send on the same thread.
                interrupt_error = Some(error);
            }
        }

        let poll_timeout = remaining(deadline)?.min(MESSAGE_POLL_INTERVAL);
        let message = match server.next_message_timeout(poll_timeout) {
            Ok(message) => message,
            Err(error) if error.is_timeout() => continue,
            Err(error) => return Err(chat_error(error)),
        };
        if interrupt_requested && collector.is_our_completion(&message) {
            let error = interrupt_error
                .map(|error| format!("Codex could not interrupt turn: {error}"))
                .unwrap_or_else(|| "turn cancelled".to_string());
            return Err(
                ChatError::new(error).with_progress(collector.model_rounds(), collector.tool_calls)
            );
        }
        if collector.accept(&message, on_event)? == TurnSignal::Completed {
            break;
        }
    }
    let tool_calls = collector.tool_calls;
    let model_rounds = collector.model_rounds();
    if collector.usage_responses == 0 {
        let observation = collector.response_observation(None, None);
        on_event(ChatEvent::Response(&observation))?;
    }
    let text = collector.final_text()?;
    Ok(TurnOutcome {
        text,
        model_rounds,
        tool_calls,
        elapsed: started.elapsed(),
    })
}

fn turn_params(
    thread_id: &str,
    input: String,
    workspace: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    boundary: CodexBoundary,
) -> Value {
    let mut params = Map::from_iter([
        ("threadId".to_string(), json!(thread_id)),
        (
            "input".to_string(),
            json!([{"type": "text", "text": input}]),
        ),
        ("cwd".to_string(), json!(workspace)),
        ("model".to_string(), json!(model)),
        ("summary".to_string(), json!("detailed")),
        ("approvalPolicy".to_string(), json!("never")),
        ("sandboxPolicy".to_string(), sandbox_policy(boundary)),
    ]);
    if let Some(effort) = reasoning_effort {
        params.insert("effort".to_string(), json!(effort));
    }
    Value::Object(params)
}

fn sandbox_policy(boundary: CodexBoundary) -> Value {
    match boundary {
        CodexBoundary::Managed => {
            json!({"type": "workspaceWrite", "networkAccess": true})
        }
        CodexBoundary::ExternalRestricted => {
            json!({"type": "externalSandbox", "networkAccess": "restricted"})
        }
        CodexBoundary::ExternalEnabled => {
            json!({"type": "externalSandbox", "networkAccess": "enabled"})
        }
    }
}

fn require_chatgpt_account(server: &mut AppServer, timeout: Duration) -> Result<(), ChatError> {
    let account = server
        .call_timeout("account/read", json!({"refreshToken": false}), timeout)
        .map_err(chat_error)?;
    match account.pointer("/account/type").and_then(Value::as_str) {
        Some("chatgpt") => Ok(()),
        Some("apiKey") => Err(ChatError::new(
            "Codex is signed in with an API key; run `agul account login` to use ChatGPT quota",
        )),
        _ => Err(ChatError::new(
            "ChatGPT is not connected; run `agul account login` first",
        )),
    }
}

fn resolve_model(
    server: &mut AppServer,
    requested: Option<&str>,
    timeout: Duration,
) -> Result<String, ChatError> {
    if let Some(model) = requested.filter(|model| !model.trim().is_empty()) {
        return Ok(model.to_string());
    }
    let models = server
        .call_timeout("model/list", json!({}), timeout)
        .map_err(chat_error)?;
    models
        .get("data")
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|model| model.get("isDefault").and_then(Value::as_bool) == Some(true))
                .or_else(|| {
                    models
                        .iter()
                        .find(|model| !model["hidden"].as_bool().unwrap_or(false))
                })
        })
        .and_then(|model| model.get("model").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| ChatError::new("Codex did not report an available model"))
}

struct ThreadRequest<'a> {
    workspace: &'a str,
    developer_instructions: &'a str,
    model: &'a str,
    reasoning_effort: Option<&'a str>,
    resume_thread_id: Option<&'a str>,
    boundary: CodexBoundary,
    ephemeral: bool,
    timeout: Duration,
}

fn create_thread(
    server: &mut AppServer,
    request: ThreadRequest<'_>,
) -> Result<CreatedThread, ChatError> {
    let (method, params) = thread_params(
        request.workspace,
        request.developer_instructions,
        request.model,
        request.resume_thread_id,
        request.boundary,
        request.ephemeral,
    );
    let response = server
        .call_timeout(method, params, request.timeout)
        .map_err(chat_error)?;
    parse_created_thread(method, &response, request.model, request.reasoning_effort)
}

fn thread_params(
    workspace: &str,
    developer_instructions: &str,
    model: &str,
    resume_thread_id: Option<&str>,
    boundary: CodexBoundary,
    ephemeral: bool,
) -> (&'static str, Value) {
    let mut params = Map::from_iter([
        ("cwd".to_string(), json!(workspace)),
        ("model".to_string(), json!(model)),
        (
            "developerInstructions".to_string(),
            json!(developer_instructions),
        ),
        ("approvalPolicy".to_string(), json!("never")),
        ("sandbox".to_string(), json!(thread_sandbox(boundary))),
        ("config".to_string(), json!({"web_search": "live"})),
    ]);
    let method = if let Some(thread_id) = resume_thread_id {
        params.insert("threadId".to_string(), json!(thread_id));
        "thread/resume"
    } else {
        params.insert("ephemeral".to_string(), json!(ephemeral));
        "thread/start"
    };
    (method, Value::Object(params))
}

fn thread_sandbox(boundary: CodexBoundary) -> &'static str {
    match boundary {
        CodexBoundary::Managed => "workspace-write",
        CodexBoundary::ExternalRestricted | CodexBoundary::ExternalEnabled => "danger-full-access",
    }
}

fn parse_created_thread(
    method: &str,
    response: &Value,
    fallback_model: &str,
    fallback_reasoning_effort: Option<&str>,
) -> Result<CreatedThread, ChatError> {
    let id = response
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| ChatError::new(format!("Codex {method} response has no thread id")))?;
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(fallback_model)
        .to_string();
    Ok(CreatedThread {
        id,
        model,
        reasoning_effort: response
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .filter(|effort| !effort.trim().is_empty())
            .map(str::to_string)
            .or_else(|| fallback_reasoning_effort.map(str::to_string)),
    })
}

#[derive(Debug)]
struct CreatedThread {
    id: String,
    model: String,
    reasoning_effort: Option<String>,
}

fn remaining(deadline: Instant) -> Result<Duration, ChatError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(ChatError::new("Codex turn timed out"))
    } else {
        Ok(remaining)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnSignal {
    Continue,
    Completed,
}

struct ActiveTool {
    name: String,
    detail: String,
    progress: Option<(String, String)>,
    started: Instant,
}

struct TurnCollector<'a> {
    thread_id: &'a str,
    turn_id: &'a str,
    model: &'a str,
    reported_model: String,
    final_messages: Vec<String>,
    fallback_message: Option<String>,
    streamed_text: String,
    streamed_items: HashMap<String, String>,
    last_usage_total: Option<UsageTotal>,
    usage_responses: u32,
    active_tools: HashMap<String, ActiveTool>,
    tool_calls: u32,
    error: Option<String>,
}

impl<'a> TurnCollector<'a> {
    fn new(thread_id: &'a str, turn_id: &'a str, model: &'a str) -> Self {
        Self {
            thread_id,
            turn_id,
            model,
            reported_model: model.to_string(),
            final_messages: Vec::new(),
            fallback_message: None,
            streamed_text: String::new(),
            streamed_items: HashMap::new(),
            last_usage_total: None,
            usage_responses: 0,
            active_tools: HashMap::new(),
            tool_calls: 0,
            error: None,
        }
    }

    fn accept(
        &mut self,
        message: &Value,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<TurnSignal, ChatError> {
        let method = message.get("method").and_then(Value::as_str);
        if is_turn_notification(method) && !self.belongs_to_turn(message) {
            return Ok(TurnSignal::Continue);
        }
        match method {
            Some("item/agentMessage/delta") => {
                if let Some(delta) = message.pointer("/params/delta").and_then(Value::as_str) {
                    self.streamed_text.push_str(delta);
                    if let Some(item_id) = message.pointer("/params/itemId").and_then(Value::as_str)
                    {
                        self.streamed_items
                            .entry(item_id.to_string())
                            .or_default()
                            .push_str(delta);
                    }
                    on_event(ChatEvent::Text(delta))?;
                }
            }
            Some("item/reasoning/summaryTextDelta" | "item/reasoning/textDelta") => {
                if let Some(delta) = message.pointer("/params/delta").and_then(Value::as_str) {
                    on_event(ChatEvent::Reasoning(delta))?;
                }
            }
            Some("item/started") => {
                if let Some(item) = message.pointer("/params/item") {
                    self.start_item(item, on_event)?;
                }
            }
            Some("item/completed") => {
                if let Some(item) = message.pointer("/params/item") {
                    self.complete_item(item, on_event)?;
                }
            }
            Some("thread/tokenUsage/updated") => {
                if let Some((usage, total, context_window)) = parse_usage(message)
                    && self.last_usage_total.as_ref() != Some(&total)
                {
                    self.last_usage_total = Some(total);
                    self.usage_responses = self.usage_responses.saturating_add(1);
                    let observation = self.response_observation(Some(usage), context_window);
                    on_event(ChatEvent::Response(&observation))?;
                }
            }
            Some("model/rerouted") => {
                if let Some(model) = message.pointer("/params/toModel").and_then(Value::as_str) {
                    self.reported_model = model.to_string();
                }
            }
            Some("error") => {
                self.error = message
                    .pointer("/params/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("turn/completed") if self.is_our_completion(message) => {
                let status = message
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if status == "completed" {
                    return Ok(TurnSignal::Completed);
                }
                let detail = message
                    .pointer("/params/turn/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.error.clone())
                    .unwrap_or_else(|| format!("Codex turn {status}"));
                return Err(
                    ChatError::new(detail).with_progress(self.model_rounds(), self.tool_calls)
                );
            }
            _ => {}
        }
        Ok(TurnSignal::Continue)
    }

    fn belongs_to_turn(&self, message: &Value) -> bool {
        message.pointer("/params/threadId").and_then(Value::as_str) == Some(self.thread_id)
            && message.pointer("/params/turnId").and_then(Value::as_str) == Some(self.turn_id)
    }

    fn is_our_completion(&self, message: &Value) -> bool {
        message.pointer("/params/threadId").and_then(Value::as_str) == Some(self.thread_id)
            && message.pointer("/params/turn/id").and_then(Value::as_str) == Some(self.turn_id)
    }

    fn response_observation(
        &self,
        usage: Option<Usage>,
        context_window: Option<u64>,
    ) -> ResponseObservation {
        let ordinal = self.usage_responses.max(1);
        ResponseObservation {
            response_id: Some(format!("{}:{}:{ordinal}", self.thread_id, self.turn_id)),
            requested_model: self.model.to_string(),
            reported_model: Some(self.reported_model.clone()),
            provider_created_at: None,
            received_at: now(),
            usage,
            context_window,
            promoted_text: None,
        }
    }

    fn model_rounds(&self) -> u32 {
        self.usage_responses.max(1)
    }

    fn start_item(
        &mut self,
        item: &Value,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<(), ChatError> {
        let Some(tool) = tool_info(item) else {
            return Ok(());
        };
        if self.active_tools.contains_key(&tool.id) {
            return Ok(());
        }
        self.tool_calls = self.tool_calls.saturating_add(1);
        on_event(ChatEvent::ToolStarted {
            name: &tool.name,
            detail: &tool.detail,
        })?;
        if let Some((stage, preview)) = &tool.progress {
            on_event(ChatEvent::ToolProgress {
                call_id: &tool.id,
                seq: 1,
                task_id: None,
                stage,
                preview,
            })?;
        }
        self.active_tools.insert(
            tool.id,
            ActiveTool {
                name: tool.name,
                detail: tool.detail,
                progress: tool.progress,
                started: Instant::now(),
            },
        );
        Ok(())
    }

    fn complete_item(
        &mut self,
        item: &Value,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<(), ChatError> {
        if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
            if let Some(text) = item.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                let streamed = item
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.streamed_items.remove(id))
                    .unwrap_or_default();
                if streamed != text {
                    if !streamed.is_empty() {
                        on_event(ChatEvent::Text("\n\n"))?;
                    }
                    on_event(ChatEvent::Text(text))?;
                }
                match item.get("phase").and_then(Value::as_str) {
                    Some("final_answer") => self.final_messages.push(text.to_string()),
                    Some("commentary") => {}
                    _ => self.fallback_message = Some(text.to_string()),
                }
            }
            return Ok(());
        }
        let Some(tool) = tool_info(item) else {
            return Ok(());
        };
        if !self.active_tools.contains_key(&tool.id) {
            self.start_item(item, on_event)?;
        }
        let active = self.active_tools.remove(&tool.id).unwrap_or(ActiveTool {
            name: tool.name,
            detail: tool.detail,
            progress: tool.progress.clone(),
            started: Instant::now(),
        });
        if let Some((stage, preview)) = &tool.progress
            && active.progress.as_ref() != tool.progress.as_ref()
        {
            on_event(ChatEvent::ToolProgress {
                call_id: &tool.id,
                seq: 2,
                task_id: None,
                stage,
                preview,
            })?;
        }
        let ok = item_succeeded(item);
        on_event(ChatEvent::ToolFinished {
            name: &active.name,
            detail: &active.detail,
            ok,
            elapsed: active.started.elapsed(),
        })
    }

    fn final_text(self) -> Result<String, ChatError> {
        let model_rounds = self.model_rounds();
        let tool_calls = self.tool_calls;
        let text = if !self.final_messages.is_empty() {
            self.final_messages.join("\n\n")
        } else if let Some(message) = self.fallback_message {
            message
        } else {
            self.streamed_text
        };
        if text.trim().is_empty() {
            Err(ChatError::new("Codex completed without visible text")
                .with_progress(model_rounds, tool_calls))
        } else {
            Ok(text)
        }
    }
}

fn item_succeeded(item: &Value) -> bool {
    if let Some(success) = item.get("success").and_then(Value::as_bool) {
        return success;
    }
    match item.get("status").and_then(Value::as_str) {
        None => true,
        Some("completed" | "success" | "succeeded") => true,
        Some(_) => false,
    }
}

fn is_turn_notification(method: Option<&str>) -> bool {
    matches!(
        method,
        Some(
            "item/agentMessage/delta"
                | "item/reasoning/summaryTextDelta"
                | "item/reasoning/textDelta"
                | "item/started"
                | "item/completed"
                | "thread/tokenUsage/updated"
                | "model/rerouted"
                | "error"
        )
    )
}

struct ToolInfo {
    id: String,
    name: String,
    detail: String,
    progress: Option<(String, String)>,
}

fn tool_info(item: &Value) -> Option<ToolInfo> {
    let kind = item.get("type")?.as_str()?;
    let id = item.get("id")?.as_str()?.to_string();
    let (name, detail, progress) = match kind {
        "commandExecution" => (
            "shell".to_string(),
            compact_value(item.get("command"), 96),
            None,
        ),
        "fileChange" => ("edit".to_string(), file_change_detail(item), None),
        "mcpToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            (format!("{server}/{tool}"), String::new(), None)
        }
        "dynamicToolCall" => (
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            compact_value(item.get("arguments"), 96),
            None,
        ),
        "collabToolCall" | "collabAgentToolCall" => (
            "agent".to_string(),
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("delegate")
                .to_string(),
            None,
        ),
        "webSearch" => {
            let (stage, preview) = web_progress(item);
            ("web".to_string(), preview.clone(), Some((stage, preview)))
        }
        _ => return None,
    };
    Some(ToolInfo {
        id,
        name,
        detail,
        progress,
    })
}

fn web_progress(item: &Value) -> (String, String) {
    let action = item.pointer("/action/type").and_then(Value::as_str);
    match action {
        Some("openPage") => (
            "open_page".to_string(),
            compact_value(item.pointer("/action/url"), 120),
        ),
        Some("findInPage") => {
            let url = compact_value(item.pointer("/action/url"), 80);
            let pattern = compact_value(item.pointer("/action/pattern"), 40);
            ("find_in_page".to_string(), format!("{url} · {pattern}"))
        }
        _ => {
            let query = item.pointer("/action/query").or_else(|| item.get("query"));
            let preview = match query {
                Some(query) => compact_value(Some(query), 120),
                None => item
                    .pointer("/action/queries")
                    .and_then(Value::as_array)
                    .map(|queries| {
                        queries
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" · ")
                    })
                    .map(|queries| compact_text(&queries, 120))
                    .unwrap_or_default(),
            };
            ("search".to_string(), preview)
        }
    }
}

fn file_change_detail(item: &Value) -> String {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| change.get("path").and_then(Value::as_str))
                .take(3)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| "workspace".to_string())
}

fn compact_value(value: Option<&Value>, max_chars: usize) -> String {
    let text = match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) if !value.is_null() => value.to_string(),
        _ => String::new(),
    };
    compact_text(&text, max_chars)
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsageTotal {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

fn parse_usage(message: &Value) -> Option<(Usage, UsageTotal, Option<u64>)> {
    let usage = message.pointer("/params/tokenUsage/last")?;
    let total = message.pointer("/params/tokenUsage/total")?;
    let input_tokens = usage.get("inputTokens")?.as_u64()?;
    let output_tokens = usage.get("outputTokens")?.as_u64()?;
    let cached = usage.get("cachedInputTokens").and_then(Value::as_u64);
    Some((
        Usage {
            input_tokens,
            output_tokens,
            cache_hit_tokens: cached,
            cache_miss_tokens: cached.map(|cached| input_tokens.saturating_sub(cached)),
            reasoning_tokens: usage.get("reasoningOutputTokens").and_then(Value::as_u64),
        },
        UsageTotal {
            input_tokens: total.get("inputTokens")?.as_u64()?,
            output_tokens: total.get("outputTokens")?.as_u64()?,
            cached_input_tokens: total.get("cachedInputTokens")?.as_u64()?,
            reasoning_output_tokens: total.get("reasoningOutputTokens")?.as_u64()?,
            total_tokens: total.get("totalTokens")?.as_u64()?,
        },
        message
            .pointer("/params/tokenUsage/modelContextWindow")
            .and_then(Value::as_u64),
    ))
}

fn chat_error(error: CodexError) -> ChatError {
    ChatError::new(error.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeTurnServer {
        calls: Vec<(String, Value)>,
        messages: VecDeque<Value>,
        cancel_on_next_poll: Option<TurnCancellation>,
        turn_starts: u32,
        poll_timeouts: Vec<Duration>,
    }

    impl TurnServer for FakeTurnServer {
        fn call_timeout(
            &mut self,
            method: &str,
            params: Value,
            _timeout: Duration,
        ) -> Result<Value, CodexError> {
            self.calls.push((method.to_string(), params));
            match method {
                "turn/start" => {
                    self.turn_starts += 1;
                    Ok(json!({"turn": {"id": format!("turn-{}", self.turn_starts)}}))
                }
                "turn/interrupt" => Ok(json!({})),
                method => Err(CodexError::new(format!("unexpected call: {method}"))),
            }
        }

        fn next_message_timeout(&mut self, timeout: Duration) -> Result<Value, CodexError> {
            self.poll_timeouts.push(timeout);
            if let Some(cancellation) = self.cancel_on_next_poll.take() {
                cancellation.cancel();
                return Err(CodexError::timeout("fake poll timeout"));
            }
            self.messages
                .pop_front()
                .ok_or_else(|| CodexError::new("fake message stream exhausted"))
        }
    }

    #[test]
    fn request_builders_use_stable_account_engine_fields() {
        let (method, start) = thread_params(
            "C:/work",
            "maintain it",
            "gpt-test",
            None,
            CodexBoundary::Managed,
            true,
        );
        assert_eq!(method, "thread/start");
        assert_eq!(
            start,
            json!({
                "cwd": "C:/work",
                "model": "gpt-test",
                "developerInstructions": "maintain it",
                "approvalPolicy": "never",
                "sandbox": "workspace-write",
                "config": {"web_search": "live"},
                "ephemeral": true
            })
        );

        let (method, resume) = thread_params(
            "C:/work",
            "maintain it",
            "gpt-test",
            Some("thread-1"),
            CodexBoundary::Managed,
            false,
        );
        assert_eq!(method, "thread/resume");
        assert_eq!(resume["threadId"], "thread-1");
        assert!(resume.get("ephemeral").is_none());
        assert!(resume.get("excludeTurns").is_none());

        assert_eq!(
            turn_params(
                "thread-1",
                "fix it".to_string(),
                "C:/work",
                "gpt-test",
                Some("high"),
                CodexBoundary::Managed,
            ),
            json!({
                "threadId": "thread-1",
                "input": [{"type": "text", "text": "fix it"}],
                "cwd": "C:/work",
                "model": "gpt-test",
                "summary": "detailed",
                "effort": "high",
                "approvalPolicy": "never",
                "sandboxPolicy": {"type": "workspaceWrite", "networkAccess": true}
            })
        );

        for (boundary, network_access) in [
            (CodexBoundary::ExternalRestricted, "restricted"),
            (CodexBoundary::ExternalEnabled, "enabled"),
        ] {
            let (method, start) =
                thread_params("C:/work", "maintain it", "gpt-test", None, boundary, false);
            assert_eq!(method, "thread/start");
            assert_eq!(start["sandbox"], "danger-full-access");
            assert_eq!(
                turn_params(
                    "thread-1",
                    "fix it".to_string(),
                    "C:/work",
                    "gpt-test",
                    None,
                    boundary,
                )["sandboxPolicy"],
                json!({"type": "externalSandbox", "networkAccess": network_access})
            );

            let (method, resume) = thread_params(
                "C:/work",
                "maintain it",
                "gpt-test",
                Some("thread-1"),
                boundary,
                false,
            );
            assert_eq!(method, "thread/resume");
            assert_eq!(resume["sandbox"], "danger-full-access");
        }
    }

    #[test]
    fn cancellation_interrupts_and_drains_before_reusing_the_thread() {
        let cancellation = TurnCancellation::default();
        let mut server = FakeTurnServer {
            calls: Vec::new(),
            messages: VecDeque::from([
                json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"interrupted"}}}),
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-2","itemId":"answer","delta":"done"}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-2","status":"completed"}}}),
            ]),
            cancel_on_next_poll: Some(cancellation.clone()),
            turn_starts: 0,
            poll_timeouts: Vec::new(),
        };

        let error = send_turn(
            &mut server,
            TurnRequest {
                thread_id: "thread-1",
                input: "stop this".to_string(),
                workspace: "C:/work",
                model: "gpt-test",
                reasoning_effort: None,
                boundary: CodexBoundary::Managed,
                timeout: Duration::from_secs(5),
            },
            &cancellation,
            &mut ignore_event,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "turn cancelled");

        let outcome = send_turn(
            &mut server,
            TurnRequest {
                thread_id: "thread-1",
                input: "continue".to_string(),
                workspace: "C:/work",
                model: "gpt-test",
                reasoning_effort: None,
                boundary: CodexBoundary::Managed,
                timeout: Duration::from_secs(5),
            },
            &TurnCancellation::default(),
            &mut ignore_event,
        )
        .unwrap();

        assert_eq!(outcome.text, "done");
        assert!(
            server
                .poll_timeouts
                .iter()
                .all(|timeout| *timeout <= MESSAGE_POLL_INTERVAL)
        );
        assert_eq!(
            server
                .calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["turn/start", "turn/interrupt", "turn/start"]
        );
        assert_eq!(
            server.calls[1].1,
            json!({"threadId": "thread-1", "turnId": "turn-1"})
        );
        assert!(server.messages.is_empty());
    }

    #[test]
    fn cancellation_known_after_turn_start_interrupts_before_polling() {
        let cancellation = TurnCancellation::default();
        cancellation.cancel();
        let mut server = FakeTurnServer {
            calls: Vec::new(),
            messages: VecDeque::from([json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "status": "interrupted"}
                }
            })]),
            cancel_on_next_poll: None,
            turn_starts: 0,
            poll_timeouts: Vec::new(),
        };

        let error = send_turn(
            &mut server,
            TurnRequest {
                thread_id: "thread-1",
                input: "already stopped".to_string(),
                workspace: "C:/work",
                model: "gpt-test",
                reasoning_effort: None,
                boundary: CodexBoundary::Managed,
                timeout: Duration::from_secs(5),
            },
            &cancellation,
            &mut ignore_event,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "turn cancelled");
        assert_eq!(
            server
                .calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["turn/start", "turn/interrupt"]
        );
        assert_eq!(server.poll_timeouts.len(), 1);
    }

    #[test]
    fn created_thread_requires_a_non_empty_upstream_id() {
        let valid = parse_created_thread(
            "thread/start",
            &json!({"thread": {"id": "thread-1"}}),
            "gpt-requested",
            Some("high"),
        )
        .unwrap();
        assert_eq!(valid.id, "thread-1");
        assert_eq!(valid.model, "gpt-requested");
        assert_eq!(valid.reasoning_effort.as_deref(), Some("high"));

        let overridden = parse_created_thread(
            "thread/start",
            &json!({
                "thread": {"id": "thread-2"},
                "model": "gpt-effective",
                "reasoningEffort": "medium"
            }),
            "gpt-requested",
            Some("high"),
        )
        .unwrap();
        assert_eq!(overridden.model, "gpt-effective");
        assert_eq!(overridden.reasoning_effort.as_deref(), Some("medium"));

        for id in ["", "   "] {
            let error = parse_created_thread(
                "thread/start",
                &json!({"thread": {"id": id}, "model": "gpt-test"}),
                "gpt-requested",
                None,
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Codex thread/start response has no thread id"
            );
        }
    }

    #[test]
    fn web_progress_keeps_multi_query_searches_visible() {
        assert_eq!(
            web_progress(&json!({
                "action": {
                    "type": "search",
                    "queries": ["Agul runtime", "Codex app-server"]
                }
            })),
            (
                "search".to_string(),
                "Agul runtime · Codex app-server".to_string()
            )
        );
    }

    #[test]
    fn collector_streams_reasoning_live_web_actions_and_exact_turn_usage() {
        let mut collector = TurnCollector::new("thread-1", "turn-1", "gpt-test");
        let mut events = Vec::new();
        let mut record = |event: ChatEvent<'_>| {
            events.push(match event {
                ChatEvent::Reasoning(text) => format!("reasoning:{text}"),
                ChatEvent::Text(text) => format!("text:{text}"),
                ChatEvent::ToolStarted { name, detail } => format!("start:{name}:{detail}"),
                ChatEvent::ToolProgress { stage, preview, .. } => {
                    format!("progress:{stage}:{preview}")
                }
                ChatEvent::ToolFinished { name, ok, .. } => format!("finish:{name}:{ok}"),
                ChatEvent::Response(response) => format!(
                    "usage:{}:{}:{}:{}",
                    response.response_id.as_deref().unwrap(),
                    response.usage.as_ref().unwrap().input_tokens,
                    response.context_window.unwrap(),
                    response.reported_model.as_deref().unwrap()
                ),
                ChatEvent::RelatedSession { .. } => unreachable!(),
            });
            Ok(())
        };
        let messages = [
            json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"other","turnId":"turn-1","delta":"ignore me"}}),
            json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"thread-1","turnId":"turn-1","delta":"checking"}}),
            json!({"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-1","query":"Agul","action":{"type":"search","query":"Agul"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-1","query":"Agul","action":{"type":"search","query":"Agul"}}}}),
            json!({"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-2","query":"Agul","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"webSearch","id":"web-2","query":"Agul","action":{"type":"openPage","url":"https://example.com/agul"}}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":120,"cachedInputTokens":80,"outputTokens":30,"reasoningOutputTokens":10,"totalTokens":150},"total":{"inputTokens":120,"cachedInputTokens":80,"outputTokens":30,"reasoningOutputTokens":10,"totalTokens":150},"modelContextWindow":200000}}}),
            json!({"method":"model/rerouted","params":{"threadId":"thread-1","turnId":"turn-1","fromModel":"gpt-test","toModel":"gpt-fallback","reason":"highRiskCyberActivity"}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":130,"cachedInputTokens":20,"outputTokens":15,"reasoningOutputTokens":2,"totalTokens":145},"total":{"inputTokens":250,"cachedInputTokens":100,"outputTokens":45,"reasoningOutputTokens":12,"totalTokens":295},"modelContextWindow":200000}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":130,"cachedInputTokens":20,"outputTokens":15,"reasoningOutputTokens":2,"totalTokens":145},"total":{"inputTokens":250,"cachedInputTokens":100,"outputTokens":45,"reasoningOutputTokens":12,"totalTokens":295},"modelContextWindow":300000}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"answer","delta":"done"}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","id":"comment","text":"interim","phase":"commentary"}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","id":"answer","text":"done","phase":"final_answer"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}),
        ];

        for message in messages {
            collector.accept(&message, &mut record).unwrap();
        }

        assert!(events.contains(&"reasoning:checking".to_string()));
        assert!(events.contains(&"progress:search:Agul".to_string()));
        assert!(events.contains(&"progress:open_page:https://example.com/agul".to_string()));
        assert_eq!(collector.tool_calls, 2);
        assert_eq!(collector.usage_responses, 2);
        assert_eq!(collector.model_rounds(), 2);
        assert!(events.contains(&"usage:thread-1:turn-1:1:120:200000:gpt-test".to_string()));
        assert!(events.contains(&"usage:thread-1:turn-1:2:130:200000:gpt-fallback".to_string()));
        assert!(!events.iter().any(|event| event.contains("ignore me")));
        assert_eq!(collector.final_text().unwrap(), "done");
    }

    #[test]
    fn collector_uses_authoritative_completed_message_over_streamed_text() {
        let mut collector = TurnCollector::new("thread", "turn", "gpt-test");
        collector
            .accept(
                &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"draft"}}),
                &mut ignore_event,
            )
            .unwrap();
        collector
            .accept(
                &json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"type":"agentMessage","id":"answer","text":"final"}}}),
                &mut ignore_event,
            )
            .unwrap();

        assert_eq!(collector.final_text().unwrap(), "final");
    }

    #[test]
    fn completed_tool_items_keep_failure_statuses_visible() {
        assert!(item_succeeded(&json!({"status": "completed"})));
        assert!(item_succeeded(&json!({})));
        assert!(!item_succeeded(&json!({"status": "errored"})));
        assert!(!item_succeeded(&json!({"status": "interrupted"})));
        assert!(!item_succeeded(&json!({"status": "notFound"})));
        assert!(!item_succeeded(
            &json!({"status": "completed", "success": false})
        ));
    }

    fn ignore_event(_event: ChatEvent<'_>) -> Result<(), ChatError> {
        Ok(())
    }
}
