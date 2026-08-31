use std::cell::Cell;
use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

use super::{NativeProvider, TurnCancellation};

const MIN_TOOL_RESULT_CHARS: usize = 512;
const MAX_TRIMMED_TOOL_RESULT_CHARS: usize = 32 * 1024;
const TOOL_RESULT_TRIM_RESERVE_CHARS: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Message {
    role: Role,
    content: Option<String>,
    reasoning: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Message {
    pub(crate) fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub(crate) fn assistant(completion: &Completion) -> Self {
        Self::assistant_with_reasoning_policy(completion, false)
    }

    fn assistant_with_reasoning_policy(
        completion: &Completion,
        replay_completed_reasoning: bool,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: completion.content.clone(),
            // Tool-call envelopes need their reasoning for providers such as
            // DeepSeek. DeepSeek also requires completed reasoning to remain
            // in later requests whenever those requests carry tools.
            reasoning: if completion.tool_calls.is_empty() && !replay_completed_reasoning {
                None
            } else {
                completion.reasoning.clone()
            },
            tool_call_id: None,
            tool_calls: completion.tool_calls.clone(),
        }
    }

    pub(crate) fn assistant_text(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub(crate) fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            reasoning: None,
            tool_call_id: Some(id.into()),
            tool_calls: Vec::new(),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            reasoning: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    fn wire_value(&self) -> Value {
        let mut value = Map::new();
        value.insert(
            "role".to_string(),
            Value::String(
                match self.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                }
                .to_string(),
            ),
        );
        if let Some(content) = &self.content {
            value.insert("content".to_string(), Value::String(content.clone()));
        } else if matches!(self.role, Role::Assistant) {
            // Some OpenAI-compatible providers require a non-null content
            // field even when the assistant response contains only tool calls.
            value.insert("content".to_string(), Value::String(String::new()));
        }
        if let Some(reasoning) = &self.reasoning {
            value.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning.clone()),
            );
        }
        if let Some(id) = &self.tool_call_id {
            value.insert("tool_call_id".to_string(), Value::String(id.clone()));
        }
        if !self.tool_calls.is_empty() {
            value.insert(
                "tool_calls".to_string(),
                Value::Array(self.tool_calls.iter().map(ToolCall::wire_value).collect()),
            );
        }
        Value::Object(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

impl ToolCall {
    fn wire_value(&self) -> Value {
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": serde_json::to_string(&self.arguments).unwrap_or_else(|_| "{}".to_string())
            }
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

impl ToolDefinition {
    fn wire_value(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters
            }
        })
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Usage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_hit_tokens: Option<u64>,
    pub(crate) cache_miss_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct Completion {
    pub(crate) response_id: Option<String>,
    pub(crate) reported_model: Option<String>,
    pub(crate) provider_created_at: Option<u64>,
    pub(crate) content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) promoted_reasoning: bool,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Option<Usage>,
}

#[derive(Clone, Debug)]
pub(crate) struct PartialResponse {
    pub(crate) response_id: Option<String>,
    pub(crate) reported_model: Option<String>,
    pub(crate) provider_created_at: Option<u64>,
    pub(crate) usage: Usage,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DeltaKind {
    Text,
    Reasoning,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderConfig {
    pub(crate) base_url: String,
    pub(crate) provider: Option<NativeProvider>,
    pub(crate) model: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) max_tokens: u32,
    pub(crate) context_window: Option<u32>,
    pub(crate) timeout: Duration,
}

pub(crate) struct Provider {
    endpoint: String,
    dialect: ProviderDialect,
    config: ProviderConfig,
    client: Client,
    runtime: Runtime,
    learned_context_limit: Cell<Option<LearnedContextLimit>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderDialect {
    OpenAiCompatible,
    Glm,
}

#[derive(Clone, Copy, Debug)]
struct LearnedContextLimit {
    window: u32,
    input_token_offset: u32,
}

impl Provider {
    pub(crate) fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        let endpoint = completion_endpoint(&config.base_url)?;
        let dialect = provider_dialect(&endpoint, config.provider);
        let mut builder = Client::builder().timeout(config.timeout);
        if is_local_endpoint(&endpoint) {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|error| {
            ProviderError::new(format!("could not create HTTP client: {error}"))
        })?;
        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                ProviderError::new(format!("could not create model I/O runtime: {error}"))
            })?;
        Ok(Self {
            endpoint,
            dialect,
            config,
            client,
            runtime,
            learned_context_limit: Cell::new(None),
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        self.config.reasoning_effort.as_deref()
    }

    pub(crate) fn context_window(&self) -> Option<u32> {
        effective_context_window(
            self.config.context_window,
            self.learned_context_limit.get().map(|limit| limit.window),
        )
    }

    pub(crate) fn assistant_message(
        &self,
        completion: &Completion,
        request_has_tools: bool,
    ) -> Message {
        if requires_completed_reasoning_replay(self.config.provider, request_has_tools) {
            Message::assistant_with_reasoning_policy(completion, true)
        } else {
            Message::assistant(completion)
        }
    }

    pub(crate) fn complete(
        &self,
        messages: &mut [Message],
        tools: &[ToolDefinition],
        cancellation: &TurnCancellation,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<Completion, ProviderError> {
        cap_individual_tool_results(messages);
        let learned = self.learned_context_limit.get();
        let context_window = self.context_window();
        let estimated_input_tokens = context_window.map(|window| {
            prepare_bounded_messages(
                messages,
                tools,
                window,
                self.config.max_tokens,
                learned.map_or(0, |limit| limit.input_token_offset),
            )
        });
        let budgeted_input_tokens = estimated_input_tokens.map(|estimated| {
            learned.map_or(estimated, |limit| {
                estimated.saturating_add(limit.input_token_offset)
            })
        });
        let requested_max_tokens = context_window
            .zip(budgeted_input_tokens)
            .map(|(window, input)| {
                available_output_tokens(window, input).min(self.config.max_tokens)
            })
            .unwrap_or(self.config.max_tokens)
            .max(1);
        let mut body = json!({
            "model": self.config.model,
            "messages": messages.iter().map(Message::wire_value).collect::<Vec<_>>(),
            "stream": true,
            "max_tokens": requested_max_tokens
        });
        if self.dialect == ProviderDialect::OpenAiCompatible {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(ToolDefinition::wire_value).collect());
            if self.dialect == ProviderDialect::Glm {
                body["tool_stream"] = Value::Bool(true);
            }
        }
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning_effort"] = Value::String(effort.clone());
            if self.dialect == ProviderDialect::Glm {
                body["thinking"] = json!({ "type": "enabled" });
            }
        }

        match self.send(&body, cancellation, on_delta) {
            Err(error) if error.context_limit.is_some() => {
                let limit = error.context_limit.expect("checked context limit");
                let baseline_estimate = estimated_input_tokens
                    .unwrap_or_else(|| estimate_input_tokens(messages, tools));
                self.remember_context_limit(limit, baseline_estimate);
                let learned = self
                    .learned_context_limit
                    .get()
                    .expect("context limit was just learned");
                let retry_estimate = prepare_bounded_messages(
                    messages,
                    tools,
                    learned.window,
                    self.config.max_tokens,
                    learned.input_token_offset,
                );
                let retry_input = retry_estimate.saturating_add(learned.input_token_offset);
                let adapted = available_output_tokens(learned.window, retry_input)
                    .min(self.config.max_tokens)
                    .max(1);
                let messages_changed = retry_estimate < baseline_estimate;
                if !messages_changed && adapted >= requested_max_tokens {
                    return Err(error);
                }
                body["messages"] =
                    Value::Array(messages.iter().map(Message::wire_value).collect::<Vec<_>>());
                body["max_tokens"] = Value::from(adapted);
                match self.send(&body, cancellation, on_delta) {
                    Ok(completion) => Ok(completion),
                    Err(retry_error) if retry_error.is_cancelled() => Err(retry_error),
                    Err(retry_error) => {
                        let partial_response = retry_error.partial_response().cloned();
                        Err(ProviderError::new(format!(
                            "{}; one context-adapted retry with max_tokens={adapted} failed: {retry_error}",
                            error.message
                        ))
                        .with_partial_response(partial_response))
                    }
                }
            }
            result => result,
        }
    }

    fn remember_context_limit(&self, limit: ContextLimit, estimated_input_tokens: u32) {
        let observed = LearnedContextLimit {
            window: limit.window,
            input_token_offset: limit.input_tokens.saturating_sub(estimated_input_tokens),
        };
        let learned = self
            .learned_context_limit
            .get()
            .map_or(observed, |current| LearnedContextLimit {
                window: current.window.min(observed.window),
                input_token_offset: current.input_token_offset.max(observed.input_token_offset),
            });
        self.learned_context_limit.set(Some(learned));
    }

    fn send(
        &self,
        body: &Value,
        cancellation: &TurnCancellation,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<Completion, ProviderError> {
        self.runtime
            .block_on(self.send_async(body, cancellation, on_delta))
    }

    async fn send_async(
        &self,
        body: &Value,
        cancellation: &TurnCancellation,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<Completion, ProviderError> {
        let mut request = self.client.post(&self.endpoint).json(body);
        if let Some(name) = self
            .config
            .api_key_env
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            && let Ok(key) = env::var(name)
            && !key.trim().is_empty()
        {
            request = request.bearer_auth(key.trim());
        }
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            result = request.send() => result
                .map_err(|error| transport_error("model request failed", error))?,
        };
        decode_response(response, cancellation, on_delta).await
    }
}

fn requires_completed_reasoning_replay(
    provider: Option<NativeProvider>,
    request_has_tools: bool,
) -> bool {
    request_has_tools && provider == Some(NativeProvider::Deepseek)
}

fn effective_context_window(configured: Option<u32>, learned: Option<u32>) -> Option<u32> {
    match (configured, learned) {
        (Some(configured), Some(learned)) => Some(configured.min(learned)),
        (Some(configured), None) => Some(configured),
        (None, learned) => learned,
    }
}

fn available_output_tokens(window: u32, input_tokens: u32) -> u32 {
    window.saturating_sub(input_tokens).saturating_sub(1)
}

fn completion_headroom(window: u32, configured_max_tokens: u32) -> u32 {
    configured_max_tokens.min((window / 4).max(1)).max(1)
}

fn prepare_bounded_messages(
    messages: &mut [Message],
    tools: &[ToolDefinition],
    context_window: u32,
    configured_max_tokens: u32,
    input_token_offset: u32,
) -> u32 {
    let budget = context_window
        .saturating_sub(completion_headroom(context_window, configured_max_tokens))
        .saturating_sub(input_token_offset);
    trim_tool_results_to_budget(messages, tools, budget);
    estimate_input_tokens(messages, tools)
}

fn cap_individual_tool_results(messages: &mut [Message]) {
    for message in messages {
        if !matches!(message.role, Role::Tool) {
            continue;
        }
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        if content.chars().count() > MAX_TRIMMED_TOOL_RESULT_CHARS {
            message.content = Some(compact_tool_result(content, MAX_TRIMMED_TOOL_RESULT_CHARS));
        }
    }
}

fn trim_tool_results_to_budget(
    messages: &mut [Message],
    tools: &[ToolDefinition],
    input_budget: u32,
) {
    let mut estimated = estimate_input_tokens(messages, tools);
    if estimated <= input_budget {
        return;
    }

    for index in 0..messages.len() {
        if estimated <= input_budget || !matches!(messages[index].role, Role::Tool) {
            continue;
        }
        while let Some(content) = messages[index].content.as_deref() {
            let current_chars = content.chars().count();
            if estimated <= input_budget || current_chars <= MIN_TOOL_RESULT_CHARS {
                break;
            }
            let excess_tokens = estimated.saturating_sub(input_budget) as usize;
            let target_chars = current_chars
                .saturating_sub(
                    excess_tokens
                        .saturating_mul(4)
                        .saturating_add(TOOL_RESULT_TRIM_RESERVE_CHARS),
                )
                .clamp(MIN_TOOL_RESULT_CHARS, MAX_TRIMMED_TOOL_RESULT_CHARS);
            messages[index].content = Some(compact_tool_result(content, target_chars));
            estimated = estimate_input_tokens(messages, tools);
        }
    }
}

fn compact_tool_result(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let marker = format!("\n... tool output trimmed from {count} characters ...\n");
    let remaining = max_chars.saturating_sub(marker.chars().count());
    let head = remaining * 2 / 3;
    let tail = remaining - head;
    let start = value.chars().take(head).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}{marker}{end}")
}

fn estimate_input_tokens(messages: &[Message], tools: &[ToolDefinition]) -> u32 {
    let value = json!({
        "messages": messages.iter().map(Message::wire_value).collect::<Vec<_>>(),
        "tools": tools.iter().map(ToolDefinition::wire_value).collect::<Vec<_>>()
    });
    let text = value.to_string();
    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
    let non_ascii = text
        .chars()
        .filter(|character| !character.is_ascii())
        .count() as u64;
    ((ascii.div_ceil(4) + non_ascii + 128).min(u64::from(u32::MAX))) as u32
}

async fn decode_response(
    response: Response,
    cancellation: &TurnCancellation,
    on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
) -> Result<Completion, ProviderError> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !status.is_success() {
        let bytes = read_buffered_body(response, cancellation).await?;
        let text = truncate_error_body(String::from_utf8_lossy(&bytes).into_owned());
        return Err(ProviderError::from_http_error(format!(
            "model returned HTTP {}{}",
            status.as_u16(),
            if text.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", text.trim())
            }
        )));
    }
    if !content_type.contains("text/event-stream") {
        let bytes = read_buffered_body(response, cancellation).await?;
        let value = serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| ProviderError::new(format!("model returned invalid JSON: {error}")))?;
        let mut guarded_on_delta =
            |kind, delta: &str| emit_cancellable_delta(cancellation, on_delta, kind, delta);
        let partial_response = partial_response_from_value(&value);
        let result = decode_buffered(value, &mut guarded_on_delta);
        if cancellation.is_cancelled() {
            return Err(cancelled_error().with_partial_response(partial_response));
        }
        return result.map_err(|error| error.with_partial_response(partial_response));
    }

    decode_event_stream(response, cancellation, on_delta).await
}

async fn read_buffered_body(
    response: Response,
    cancellation: &TurnCancellation,
) -> Result<Vec<u8>, ProviderError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled_error()),
        result = response.bytes() => result
            .map(|bytes| bytes.to_vec())
            .map_err(|error| transport_error("could not read model response", error)),
    }
}

async fn decode_event_stream(
    mut response: Response,
    cancellation: &TurnCancellation,
    on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
) -> Result<Completion, ProviderError> {
    let mut accumulator = StreamAccumulator::default();
    let mut decoder = EventStreamDecoder::default();
    let mut guarded_on_delta =
        |kind, delta: &str| emit_cancellable_delta(cancellation, on_delta, kind, delta);
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(cancelled_error().with_partial_response(accumulator.partial_response()));
            }
            result = response.chunk() => result,
        };
        let chunk = chunk.map_err(|error| {
            transport_error("could not read model stream", error)
                .with_partial_response(accumulator.partial_response())
        })?;
        let Some(chunk) = chunk else { break };
        match decoder.push(
            &chunk,
            cancellation,
            &mut accumulator,
            &mut guarded_on_delta,
        ) {
            Ok(true) => {
                return finish_stream(accumulator, cancellation);
            }
            Ok(false) => {}
            Err(error) => {
                return Err(error.with_partial_response(accumulator.partial_response()));
            }
        }
    }
    if let Err(error) = decoder.finish(cancellation, &mut accumulator, &mut guarded_on_delta) {
        return Err(error.with_partial_response(accumulator.partial_response()));
    }
    finish_stream(accumulator, cancellation)
}

fn finish_stream(
    accumulator: StreamAccumulator,
    cancellation: &TurnCancellation,
) -> Result<Completion, ProviderError> {
    let partial_response = accumulator.partial_response();
    let result = accumulator.finish();
    if cancellation.is_cancelled() {
        return Err(cancelled_error().with_partial_response(partial_response));
    }
    result.map_err(|error| error.with_partial_response(partial_response))
}

fn cancelled_error() -> ProviderError {
    ProviderError::cancelled()
}

fn transport_error(context: &str, error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::new(format!("{context}: timed out ({error})"))
    } else {
        ProviderError::new(format!("{context}: {error}"))
    }
}

fn emit_cancellable_delta(
    cancellation: &TurnCancellation,
    on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    kind: DeltaKind,
    delta: &str,
) -> Result<(), ProviderError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let result = on_delta(kind, delta);
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    result
}

#[derive(Default)]
struct EventStreamDecoder {
    pending: Vec<u8>,
    scanned: usize,
    data: Vec<String>,
    done: bool,
}

impl EventStreamDecoder {
    fn push(
        &mut self,
        bytes: &[u8],
        cancellation: &TurnCancellation,
        accumulator: &mut StreamAccumulator,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<bool, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        if self.done {
            return Ok(true);
        }
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut search_from = self.scanned.min(self.pending.len());
        while let Some(relative_end) = self.pending[search_from..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = search_from + relative_end;
            let mut line = self.pending[consumed..end].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, accumulator, on_delta)?;
            if cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if self.done {
                return Ok(true);
            }
            consumed = end + 1;
            search_from = consumed;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        self.scanned = self.pending.len();
        Ok(false)
    }

    fn finish(
        &mut self,
        cancellation: &TurnCancellation,
        accumulator: &mut StreamAccumulator,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        if !self.done && !self.pending.is_empty() {
            let mut line = std::mem::take(&mut self.pending);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, accumulator, on_delta)?;
            if cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
        }
        if !self.done {
            self.dispatch(accumulator, on_delta)?;
            if cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
        }
        Ok(())
    }

    fn process_line(
        &mut self,
        line: &[u8],
        accumulator: &mut StreamAccumulator,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        let line = std::str::from_utf8(line).map_err(|error| {
            ProviderError::new(format!("model stream returned invalid UTF-8: {error}"))
        })?;
        if line.is_empty() {
            return self.dispatch(accumulator, on_delta);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_string());
            self.dispatch_if_complete(accumulator, on_delta)?;
        }
        Ok(())
    }

    fn dispatch_if_complete(
        &mut self,
        accumulator: &mut StreamAccumulator,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        let data = self.data.join("\n");
        if data.trim() == "[DONE]" {
            self.data.clear();
            self.done = true;
            return Ok(());
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
            return Ok(());
        };
        self.data.clear();
        apply_stream_chunk(chunk, accumulator, on_delta)
    }

    fn dispatch(
        &mut self,
        accumulator: &mut StreamAccumulator,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data).join("\n");
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk = serde_json::from_str::<Value>(&data).map_err(|error| {
            ProviderError::new(format!("model stream returned invalid JSON: {error}"))
        })?;
        apply_stream_chunk(chunk, accumulator, on_delta)
    }
}

fn apply_stream_chunk(
    chunk: Value,
    accumulator: &mut StreamAccumulator,
    on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    if let Some(error) = chunk.get("error") {
        return Err(ProviderError::new(format!("model error: {error}")));
    }
    accumulator.push(&chunk, on_delta)
}

fn truncate_error_body(mut text: String) -> String {
    const MAX_BYTES: usize = 4096;
    if text.len() <= MAX_BYTES {
        return text;
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("...");
    text
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    reported_model: Option<String>,
    provider_created_at: Option<u64>,
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialToolCall>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
}

impl StreamAccumulator {
    fn partial_response(&self) -> Option<PartialResponse> {
        Some(PartialResponse {
            response_id: self.response_id.clone(),
            reported_model: self.reported_model.clone(),
            provider_created_at: self.provider_created_at,
            usage: self.usage.clone()?,
        })
    }

    fn push(
        &mut self,
        chunk: &Value,
        on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if let Some(value) = chunk
            .get("id")
            .or_else(|| chunk.get("request_id"))
            .and_then(Value::as_str)
        {
            self.response_id = Some(value.to_string());
        }
        if let Some(value) = chunk.get("model").and_then(Value::as_str) {
            self.reported_model = Some(value.to_string());
        }
        if let Some(value) = chunk.get("created").and_then(Value::as_u64) {
            self.provider_created_at = Some(value);
        }
        if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        let delta = choice.get("delta").unwrap_or(choice);
        if let Some(value) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(value.to_string());
        }
        if let Some(value) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(value);
            on_delta(DeltaKind::Reasoning, value)?;
        }
        if let Some(value) = delta.get("content").and_then(Value::as_str) {
            self.content.push_str(value);
            on_delta(DeltaKind::Text, value)?;
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.calls.len() as u64) as usize;
                let partial = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    partial.id.push_str(id);
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        partial.name.push_str(name);
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        partial.arguments.push_str(arguments);
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Completion, ProviderError> {
        let tool_calls = self
            .calls
            .into_values()
            .map(finish_tool_call)
            .collect::<Result<Vec<_>, _>>()?;
        let mut content = (!self.content.is_empty()).then_some(self.content);
        let mut reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        let promoted_reasoning = content.is_none()
            && tool_calls.is_empty()
            && self.finish_reason.as_deref() == Some("stop");
        if promoted_reasoning {
            // GLM Coding Plan can place a terse final answer entirely in
            // reasoning_content. Preserve a completed answer instead of
            // turning a successful response into an empty-response failure.
            content = reasoning.take();
        }
        if content.is_none() && tool_calls.is_empty() {
            return Err(ProviderError::new(
                "model returned neither text nor a tool call",
            ));
        }
        Ok(Completion {
            response_id: self.response_id,
            reported_model: self.reported_model,
            provider_created_at: self.provider_created_at,
            content,
            reasoning,
            promoted_reasoning,
            tool_calls,
            usage: self.usage,
        })
    }
}

fn partial_response_from_value(value: &Value) -> Option<PartialResponse> {
    Some(PartialResponse {
        response_id: value
            .get("id")
            .or_else(|| value.get("request_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        reported_model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        provider_created_at: value.get("created").and_then(Value::as_u64),
        usage: value.get("usage").and_then(parse_usage)?,
    })
}

fn finish_tool_call(partial: PartialToolCall) -> Result<ToolCall, ProviderError> {
    if partial.id.is_empty() || partial.name.is_empty() {
        return Err(ProviderError::new("model returned an incomplete tool call"));
    }
    let arguments = if partial.arguments.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&partial.arguments).map_err(|error| {
            ProviderError::new(format!(
                "model returned invalid arguments for {}: {error}",
                partial.name
            ))
        })?
    };
    Ok(ToolCall {
        id: partial.id,
        name: partial.name,
        arguments,
    })
}

fn decode_buffered(
    value: Value,
    on_delta: &mut dyn FnMut(DeltaKind, &str) -> Result<(), ProviderError>,
) -> Result<Completion, ProviderError> {
    if let Some(error) = value.get("error") {
        return Err(ProviderError::new(format!("model error: {error}")));
    }
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::new("model response has no assistant message"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::new("model response has no assistant message"))?;
    let mut reasoning = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(reasoning) = &reasoning {
        on_delta(DeltaKind::Reasoning, reasoning)?;
    }
    if let Some(content) = &content {
        on_delta(DeltaKind::Text, content)?;
    }
    let mut tool_calls = Vec::new();
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = call
            .get("function")
            .ok_or_else(|| ProviderError::new("model tool call has no function"))?;
        let arguments = match function.get("arguments") {
            Some(Value::String(arguments)) => serde_json::from_str(arguments).map_err(|error| {
                ProviderError::new(format!("model returned invalid tool arguments: {error}"))
            })?,
            Some(Value::Object(arguments)) => Value::Object(arguments.clone()),
            _ => json!({}),
        };
        tool_calls.push(ToolCall {
            id: call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::new("model tool call has no id"))?
                .to_string(),
            name: function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::new("model tool call has no name"))?
                .to_string(),
            arguments,
        });
    }
    let promoted_reasoning = content.is_none()
        && tool_calls.is_empty()
        && choice.get("finish_reason").and_then(Value::as_str) == Some("stop");
    if promoted_reasoning {
        content = reasoning.take();
    }
    if content.is_none() && tool_calls.is_empty() {
        return Err(ProviderError::new(
            "model returned neither text nor a tool call",
        ));
    }
    Ok(Completion {
        response_id: value
            .get("id")
            .or_else(|| value.get("request_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        reported_model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        provider_created_at: value.get("created").and_then(Value::as_u64),
        content,
        reasoning,
        promoted_reasoning,
        tool_calls,
        usage: value.get("usage").and_then(parse_usage),
    })
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let input_tokens = value
        .get("prompt_tokens")
        .or_else(|| value.get("input_tokens"))
        .and_then(Value::as_u64)?;
    let output_tokens = value
        .get("completion_tokens")
        .or_else(|| value.get("output_tokens"))
        .and_then(Value::as_u64)?;
    let cache_hit_tokens = value
        .get("prompt_cache_hit_tokens")
        .or_else(|| {
            value
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
        })
        .and_then(Value::as_u64);
    let cache_miss_tokens = value
        .get("prompt_cache_miss_tokens")
        .or_else(|| {
            value
                .get("prompt_tokens_details")
                .and_then(|details| details.get("uncached_tokens"))
        })
        .and_then(Value::as_u64);
    let reasoning_tokens = value
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .or_else(|| value.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_hit_tokens,
        cache_miss_tokens,
        reasoning_tokens,
    })
}

fn completion_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let mut url = reqwest::Url::parse(base_url.trim())
        .map_err(|error| ProviderError::new(format!("invalid model URL: {error}")))?;
    if url.host_str().is_none() {
        return Err(ProviderError::new("model URL has no host"));
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/chat/completions") {
        let endpoint_path = if path.is_empty() {
            "/chat/completions".to_string()
        } else {
            format!("{path}/chat/completions")
        };
        url.set_path(&endpoint_path);
    }
    Ok(url.into())
}

fn provider_dialect(
    endpoint: &str,
    configured_provider: Option<NativeProvider>,
) -> ProviderDialect {
    let is_glm = reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host.eq_ignore_ascii_case("open.bigmodel.cn"));
    if is_glm || configured_provider == Some(NativeProvider::Glm) {
        ProviderDialect::Glm
    } else {
        ProviderDialect::OpenAiCompatible
    }
}

fn is_local_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.parse::<IpAddr>().is_ok_and(|address| match address {
                    IpAddr::V4(address) => {
                        address.is_private() || address.is_loopback() || address.is_link_local()
                    }
                    IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
                })
        })
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderError {
    message: String,
    context_limit: Option<ContextLimit>,
    cancelled: bool,
    partial_response: Option<Box<PartialResponse>>,
}

#[derive(Clone, Copy, Debug)]
struct ContextLimit {
    window: u32,
    input_tokens: u32,
}

impl ProviderError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            context_limit: None,
            cancelled: false,
            partial_response: None,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: "model request cancelled".to_string(),
            context_limit: None,
            cancelled: true,
            partial_response: None,
        }
    }

    fn from_http_error(message: String) -> Self {
        let context_limit = parse_context_limit(&message);
        Self {
            message,
            context_limit,
            cancelled: false,
            partial_response: None,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    fn with_partial_response(mut self, partial_response: Option<PartialResponse>) -> Self {
        if self.partial_response.is_none() {
            self.partial_response = partial_response.map(Box::new);
        }
        self
    }

    pub(crate) fn partial_response(&self) -> Option<&PartialResponse> {
        self.partial_response.as_deref()
    }
}

fn parse_context_limit(message: &str) -> Option<ContextLimit> {
    let lower = message.to_ascii_lowercase();
    let window = number_after_any(
        &lower,
        &[
            "maximum context length is ",
            "max context length is ",
            "context window is ",
        ],
    )?;
    let input_tokens = number_after_any(
        &lower,
        &[
            "request has ",
            "messages resulted in ",
            "input tokens: ",
            "prompt contains at least ",
        ],
    )
    .or_else(|| number_before(&lower, " in the messages"))?;
    Some(ContextLimit {
        window,
        input_tokens,
    })
}

fn number_before(text: &str, marker: &str) -> Option<u32> {
    let head = text.split_once(marker)?.0.trim_end();
    let reversed_digits = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let digits = reversed_digits.chars().rev().collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn number_after_any(text: &str, markers: &[&str]) -> Option<u32> {
    markers.iter().find_map(|marker| {
        let tail = text.split_once(marker)?.1.trim_start();
        let digits = tail
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
    })
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread;
    use std::time::Instant;

    use super::*;

    fn local_provider(address: SocketAddr) -> Provider {
        local_provider_with_timeout(address, Duration::from_secs(5))
    }

    fn local_provider_with_timeout(address: SocketAddr, timeout: Duration) -> Provider {
        Provider::new(ProviderConfig {
            base_url: format!("http://{address}/v1"),
            provider: None,
            model: "test-model".to_string(),
            api_key_env: None,
            reasoning_effort: None,
            max_tokens: 256,
            context_window: None,
            timeout,
        })
        .unwrap()
    }

    fn local_server(
        handler: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handler(stream);
        });
        (address, handle)
    }

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let header_end = index + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before its body");
            request.extend_from_slice(&buffer[..read]);
        }
    }

    fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) {
        write!(stream, "{:X}\r\n", bytes.len()).unwrap();
        stream.write_all(bytes).unwrap();
        stream.write_all(b"\r\n").unwrap();
        stream.flush().unwrap();
    }

    fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn cancellation_interrupts_waiting_for_response_headers() {
        let cancellation = TurnCancellation::default();
        let server_cancellation = cancellation.clone();
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            server_cancellation.cancel();
            thread::sleep(Duration::from_millis(500));
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
        });
        let provider = local_provider(address);
        let mut messages = vec![Message::user("hello")];
        let started = Instant::now();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |_, _| Ok(()))
            .unwrap_err();
        let elapsed = started.elapsed();

        server.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(
            elapsed < Duration::from_millis(250),
            "header wait cancellation took {elapsed:?}"
        );
    }

    #[test]
    fn cancellation_interrupts_a_stalled_event_stream() {
        let event = format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": "first"}}]})
        );
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            write_chunk(&mut stream, event.as_bytes());
            thread::sleep(Duration::from_millis(500));
            let _ = stream.write_all(b"0\r\n\r\n");
        });
        let provider = local_provider(address);
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut messages = vec![Message::user("hello")];
        let mut deltas = String::new();
        let started = Instant::now();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |kind, delta| {
                if matches!(kind, DeltaKind::Text) {
                    deltas.push_str(delta);
                }
                callback_cancellation.cancel();
                Ok(())
            })
            .unwrap_err();
        let elapsed = started.elapsed();

        server.join().unwrap();
        assert_eq!(deltas, "first");
        assert!(error.to_string().contains("cancelled"));
        assert!(
            elapsed < Duration::from_millis(250),
            "stream cancellation took {elapsed:?}"
        );
    }

    #[test]
    fn request_timeout_still_covers_a_stalled_event_stream_body() {
        let usage = format!(
            "data: {}\n\n",
            json!({
                "id": "response-1",
                "choices": [],
                "usage": {"prompt_tokens": 9, "completion_tokens": 4}
            })
        );
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            write_chunk(&mut stream, usage.as_bytes());
            thread::sleep(Duration::from_millis(300));
        });
        let provider = local_provider_with_timeout(address, Duration::from_millis(50));
        let cancellation = TurnCancellation::default();
        let mut messages = vec![Message::user("hello")];
        let started = Instant::now();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |_, _| Ok(()))
            .unwrap_err();
        let elapsed = started.elapsed();

        server.join().unwrap();
        assert!(error.to_string().contains("timed out"));
        assert!(
            elapsed < Duration::from_millis(250),
            "stream timeout took {elapsed:?}"
        );
        let partial = error
            .partial_response()
            .expect("reported usage must survive the timeout");
        assert_eq!(partial.response_id.as_deref(), Some("response-1"));
        assert_eq!(partial.usage.input_tokens, 9);
        assert_eq!(partial.usage.output_tokens, 4);
    }

    #[test]
    fn cancellation_stops_events_coalesced_in_one_http_chunk() {
        let payload = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": "first"}}]}),
            json!({"choices": [{"delta": {"content": "second"}}]})
        );
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            write_chunk(&mut stream, payload.as_bytes());
            stream.write_all(b"0\r\n\r\n").unwrap();
        });
        let provider = local_provider(address);
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut messages = vec![Message::user("hello")];
        let mut deltas = Vec::new();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |kind, delta| {
                if matches!(kind, DeltaKind::Text) {
                    deltas.push(delta.to_string());
                    callback_cancellation.cancel();
                }
                Ok(())
            })
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(deltas, ["first"]);
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn event_decoder_handles_many_events_in_one_push() {
        let event = format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": "x"}}]})
        );
        let mut payload = event.repeat(1_024);
        payload.push_str("data: [DONE]\n\n");
        let cancellation = TurnCancellation::default();
        let mut decoder = EventStreamDecoder::default();
        let mut accumulator = StreamAccumulator::default();
        let mut deltas = String::new();

        let done = decoder
            .push(
                payload.as_bytes(),
                &cancellation,
                &mut accumulator,
                &mut |kind, delta| {
                    if matches!(kind, DeltaKind::Text) {
                        deltas.push_str(delta);
                    }
                    Ok(())
                },
            )
            .unwrap();

        assert!(done);
        assert_eq!(deltas, "x".repeat(1_024));
        assert_eq!(
            accumulator.finish().unwrap().content.as_deref(),
            Some(deltas.as_str())
        );
    }

    #[test]
    fn event_decoder_handles_a_long_line_split_into_small_pushes() {
        let expected = "界".repeat(16_384);
        let payload = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": expected}}]})
        );
        let cancellation = TurnCancellation::default();
        let mut decoder = EventStreamDecoder::default();
        let mut accumulator = StreamAccumulator::default();
        let mut deltas = String::new();
        let mut done = false;

        for bytes in payload.as_bytes().chunks(17) {
            done = decoder
                .push(
                    bytes,
                    &cancellation,
                    &mut accumulator,
                    &mut |kind, delta| {
                        if matches!(kind, DeltaKind::Text) {
                            deltas.push_str(delta);
                        }
                        Ok(())
                    },
                )
                .unwrap();
            if done {
                break;
            }
        }

        assert!(done);
        assert_eq!(deltas, expected);
        assert_eq!(
            accumulator.finish().unwrap().content.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn cancellation_from_an_unterminated_final_event_is_not_lost_at_eof() {
        let payload = format!(
            "data: {}",
            json!({"choices": [{"delta": {"content": "tail"}}]})
        );
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            write_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                payload.as_bytes(),
            );
        });
        let provider = local_provider(address);
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut messages = vec![Message::user("hello")];
        let mut deltas = Vec::new();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |kind, delta| {
                if matches!(kind, DeltaKind::Text) {
                    deltas.push(delta.to_string());
                    callback_cancellation.cancel();
                }
                Ok(())
            })
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(deltas, ["tail"]);
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn cancellation_from_a_buffered_response_callback_is_not_lost() {
        let body = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "buffered"}
            }]
        })
        .to_string();
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            write_response(&mut stream, "200 OK", "application/json", body.as_bytes());
        });
        let provider = local_provider(address);
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut messages = vec![Message::user("hello")];
        let mut deltas = Vec::new();

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |kind, delta| {
                if matches!(kind, DeltaKind::Text) {
                    deltas.push(delta.to_string());
                    callback_cancellation.cancel();
                }
                Ok(())
            })
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(deltas, ["buffered"]);
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn cancellation_during_context_retry_keeps_the_cancelled_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = TurnCancellation::default();
        let server_cancellation = cancellation.clone();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_request(&mut first);
            let body = json!({
                "error": {
                    "message": "maximum context length is 100 tokens; request has 120 input tokens"
                }
            })
            .to_string();
            write_response(
                &mut first,
                "400 Bad Request",
                "application/json",
                body.as_bytes(),
            );
            drop(first);

            let (mut retry, _) = listener.accept().unwrap();
            read_request(&mut retry);
            server_cancellation.cancel();
            thread::sleep(Duration::from_millis(100));
        });
        let provider = local_provider(address);
        let mut messages = vec![Message::user("hello")];

        let error = provider
            .complete(&mut messages, &[], &cancellation, &mut |_, _| Ok(()))
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(error.to_string(), "model request cancelled");
    }

    #[test]
    fn event_stream_handles_arbitrary_chunks_utf8_and_line_boundaries() {
        let first = format!(
            "data: {}\r\n\r\n",
            json!({
                "id": "response-1",
                "model": "test-model",
                "choices": [{"delta": {"content": "你界"}}]
            })
        );
        let last = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{"finish_reason": "stop", "delta": {}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 2}
            })
        );
        let mut payload = first.into_bytes();
        payload.extend_from_slice(last.as_bytes());
        let utf8_start = payload
            .windows("界".len())
            .position(|bytes| bytes == "界".as_bytes())
            .unwrap();
        let line_break = payload
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .unwrap();
        let split_points = [
            1,
            7,
            utf8_start + 1,
            utf8_start + 2,
            line_break + 1,
            payload.len() - 3,
        ];
        let (address, server) = local_server(move |mut stream| {
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut start = 0;
            for end in split_points
                .into_iter()
                .chain(std::iter::once(payload.len()))
            {
                write_chunk(&mut stream, &payload[start..end]);
                start = end;
            }
            stream.write_all(b"0\r\n\r\n").unwrap();
        });
        let provider = local_provider(address);
        let cancellation = TurnCancellation::default();
        let mut messages = vec![Message::user("hello")];
        let mut deltas = String::new();

        let completion = provider
            .complete(&mut messages, &[], &cancellation, &mut |kind, delta| {
                if matches!(kind, DeltaKind::Text) {
                    deltas.push_str(delta);
                }
                Ok(())
            })
            .unwrap();

        server.join().unwrap();
        assert_eq!(deltas, "你界");
        assert_eq!(completion.content.as_deref(), Some("你界"));
        assert_eq!(completion.response_id.as_deref(), Some("response-1"));
        let usage = completion.usage.unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn error_body_truncation_preserves_utf8() {
        let body = "界".repeat(2_000);
        let truncated = truncate_error_body(body);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= 4_099);
    }

    #[test]
    fn accepts_a_full_completion_url_or_appends_the_resource() {
        assert_eq!(
            completion_endpoint("http://127.0.0.1:51100/v1/chat/completions").unwrap(),
            "http://127.0.0.1:51100/v1/chat/completions"
        );
        assert_eq!(
            completion_endpoint("https://api.deepseek.com").unwrap(),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            completion_endpoint("http://localhost:8000/v1").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
    }

    #[test]
    fn recognizes_glms_documented_stream_dialect() {
        assert_eq!(
            provider_dialect(
                "https://open.bigmodel.cn/api/paas/v4/chat/completions",
                None
            ),
            ProviderDialect::Glm
        );
        assert_eq!(
            provider_dialect("https://api.deepseek.com/chat/completions", None),
            ProviderDialect::OpenAiCompatible
        );
        assert_eq!(
            provider_dialect(
                "https://glm-proxy.example/v1/chat/completions",
                Some(NativeProvider::Glm)
            ),
            ProviderDialect::Glm
        );
    }

    #[test]
    fn accumulates_glm_reasoning_tools_usage_and_request_id() {
        let mut accumulator = StreamAccumulator::default();
        let mut deltas = Vec::new();
        let mut on_delta = |kind, value: &str| {
            deltas.push((matches!(kind, DeltaKind::Reasoning), value.to_string()));
            Ok(())
        };

        accumulator
            .push(
                &json!({
                    "request_id": "glm-request-1",
                    "model": "glm-5.3",
                    "choices": [{
                        "delta": {
                            "reasoning_content": "inspect",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call-1",
                                "function": {"name": "read", "arguments": "{\"pa"}
                            }]
                        }
                    }]
                }),
                &mut on_delta,
            )
            .unwrap();
        accumulator
            .push(
                &json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "function": {"arguments": "th\":\"Cargo.toml\"}"}
                            }]
                        }
                    }]
                }),
                &mut on_delta,
            )
            .unwrap();
        accumulator
            .push(
                &json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 20,
                        "completion_tokens": 4,
                        "prompt_tokens_details": {"cached_tokens": 15}
                    }
                }),
                &mut on_delta,
            )
            .unwrap();

        let completion = accumulator.finish().unwrap();
        assert_eq!(completion.response_id.as_deref(), Some("glm-request-1"));
        assert_eq!(completion.reasoning.as_deref(), Some("inspect"));
        assert_eq!(completion.tool_calls[0].name, "read");
        assert_eq!(
            completion.tool_calls[0].arguments,
            json!({"path": "Cargo.toml"})
        );
        assert_eq!(completion.usage.unwrap().cache_hit_tokens, Some(15));
        assert_eq!(deltas, vec![(true, "inspect".to_string())]);
    }

    #[test]
    fn completed_reasoning_only_response_becomes_the_final_answer() {
        let mut accumulator = StreamAccumulator::default();
        let mut deltas = Vec::new();
        let mut on_delta = |kind, value: &str| {
            deltas.push((matches!(kind, DeltaKind::Reasoning), value.to_string()));
            Ok(())
        };
        accumulator
            .push(
                &json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "delta": {"reasoning_content": "GLM_CODING_OK"}
                    }]
                }),
                &mut on_delta,
            )
            .unwrap();

        let completion = accumulator.finish().unwrap();
        assert_eq!(completion.content.as_deref(), Some("GLM_CODING_OK"));
        assert_eq!(completion.reasoning, None);
        assert!(completion.promoted_reasoning);
        assert_eq!(deltas, vec![(true, "GLM_CODING_OK".to_string())]);
    }

    #[test]
    fn parses_openai_and_deepseek_usage_shapes() {
        let openai = parse_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 8}
        }))
        .unwrap();
        assert_eq!(openai.cache_hit_tokens, Some(8));
        let deepseek = parse_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 3,
            "prompt_cache_hit_tokens": 7
        }))
        .unwrap();
        assert_eq!(deepseek.cache_hit_tokens, Some(7));
    }

    #[test]
    fn assistant_tool_message_keeps_reasoning_and_non_null_content() {
        let value = Message::assistant(&Completion {
            response_id: Some("response-1".to_string()),
            reported_model: Some("model".to_string()),
            provider_created_at: None,
            content: None,
            reasoning: Some("inspect the workspace".to_string()),
            promoted_reasoning: false,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "Cargo.toml"}),
            }],
            usage: None,
        })
        .wire_value();

        assert_eq!(value["content"], "");
        assert_eq!(value["reasoning_content"], "inspect the workspace");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read");
    }

    #[test]
    fn completed_assistant_reasoning_is_not_replayed_by_default() {
        let value = Message::assistant(&Completion {
            response_id: Some("response-1".to_string()),
            reported_model: Some("model".to_string()),
            provider_created_at: None,
            content: Some("Done.".to_string()),
            reasoning: Some("private completed reasoning".to_string()),
            promoted_reasoning: false,
            tool_calls: Vec::new(),
            usage: None,
        })
        .wire_value();

        assert_eq!(value["content"], "Done.");
        assert!(value.get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_replays_completed_reasoning_when_the_request_has_tools() {
        assert!(requires_completed_reasoning_replay(
            Some(NativeProvider::Deepseek),
            true
        ));
        assert!(!requires_completed_reasoning_replay(
            Some(NativeProvider::Deepseek),
            false
        ));
        assert!(!requires_completed_reasoning_replay(
            Some(NativeProvider::Glm),
            true
        ));
        assert!(!requires_completed_reasoning_replay(None, true));

        let value = Message::assistant_with_reasoning_policy(
            &Completion {
                response_id: Some("response-1".to_string()),
                reported_model: Some("model".to_string()),
                provider_created_at: None,
                content: Some("Done.".to_string()),
                reasoning: Some("completed reasoning".to_string()),
                promoted_reasoning: false,
                tool_calls: Vec::new(),
                usage: None,
            },
            true,
        )
        .wire_value();

        assert_eq!(value["content"], "Done.");
        assert_eq!(value["reasoning_content"], "completed reasoning");
    }

    #[test]
    fn parses_common_context_limit_errors_for_one_bounded_retry() {
        let qwen = parse_context_limit(
            "maximum context length is 32768 tokens and your request has 16385 input tokens",
        )
        .unwrap();
        assert_eq!(qwen.window, 32_768);
        assert_eq!(qwen.input_tokens, 16_385);
        assert_eq!(
            available_output_tokens(qwen.window, qwen.input_tokens),
            16_382
        );

        let openai = parse_context_limit(
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 127000 tokens",
        )
        .unwrap();
        assert_eq!(openai.window, 128_000);
        assert_eq!(openai.input_tokens, 127_000);

        let vllm = parse_context_limit(
            "This model's maximum context length is 32768 tokens. However, you requested 32769 tokens (16385 in the messages, 16384 in the completion)",
        )
        .unwrap();
        assert_eq!(vllm.window, 32_768);
        assert_eq!(vllm.input_tokens, 16_385);

        let qwen = parse_context_limit(
            "maximum context length is 32768 tokens; requested 16384 output tokens and your prompt contains at least 16385 input tokens",
        )
        .unwrap();
        assert_eq!(qwen.window, 32_768);
        assert_eq!(qwen.input_tokens, 16_385);
    }

    #[test]
    fn explicit_context_window_caps_the_initial_output_budget() {
        let messages = vec![Message::user("x".repeat(8_000))];
        let estimated = estimate_input_tokens(&messages, &[]);
        assert!(estimated > 2_000);
        assert!(available_output_tokens(4_096, estimated) < 2_096);
        assert_eq!(
            effective_context_window(Some(4_096), Some(8_192)),
            Some(4_096)
        );
        assert_eq!(
            effective_context_window(Some(8_192), Some(4_096)),
            Some(4_096)
        );
    }

    #[test]
    fn configured_context_trims_tool_output_and_preserves_completion_headroom() {
        let mut messages = vec![
            Message::system("system"),
            Message::user("inspect the result"),
            Message::tool("call-1", format!("HEAD{}TAIL", "x".repeat(200_000))),
        ];

        let estimated = prepare_bounded_messages(&mut messages, &[], 32_768, 3_072, 0);
        assert!(estimated <= 32_768 - 3_072, "estimate: {estimated}");
        let content = messages[2].content.as_deref().unwrap();
        assert!(content.starts_with("HEAD"));
        assert!(content.ends_with("TAIL"));
        assert!(content.contains("tool output trimmed"));
        assert!(content.chars().count() <= MAX_TRIMMED_TOOL_RESULT_CHARS);

        let stable_content = content.to_string();
        messages.push(Message::user("a short follow-up"));
        let estimated = prepare_bounded_messages(&mut messages, &[], 32_768, 3_072, 0);
        assert!(estimated <= 32_768 - 3_072, "estimate: {estimated}");
        assert_eq!(
            messages[2].content.as_deref(),
            Some(stable_content.as_str())
        );
    }

    #[test]
    fn individual_tool_result_cap_is_stable_without_context_pressure() {
        let mut messages = vec![Message::tool(
            "call-1",
            format!("HEAD{}TAIL", "x".repeat(100_000)),
        )];

        cap_individual_tool_results(&mut messages);
        let content = messages[0].content.as_deref().unwrap();
        assert!(content.starts_with("HEAD"));
        assert!(content.ends_with("TAIL"));
        assert!(content.contains("tool output trimmed"));
        assert!(content.chars().count() <= MAX_TRIMMED_TOOL_RESULT_CHARS);

        let stable_content = content.to_string();
        cap_individual_tool_results(&mut messages);
        assert_eq!(
            messages[0].content.as_deref(),
            Some(stable_content.as_str())
        );
    }

    #[test]
    fn learned_tokenizer_offset_is_reserved_before_retrying() {
        let mut messages = vec![
            Message::system("system"),
            Message::tool("call-1", "x".repeat(116_000)),
        ];
        let reserved_budget = 32_768 - 3_072 - 2_048;
        let inflated_budget = 32_768 - 3_072 + 2_048;
        let untrimmed = estimate_input_tokens(&messages, &[]);

        assert!(untrimmed > reserved_budget, "estimate: {untrimmed}");
        assert!(untrimmed <= inflated_budget, "estimate: {untrimmed}");

        let estimated = prepare_bounded_messages(&mut messages, &[], 32_768, 3_072, 2_048);
        assert!(estimated <= reserved_budget, "estimate: {estimated}");
    }
}
