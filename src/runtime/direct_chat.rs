use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::TurnCancellation;
use super::plugin::{PluginCallContext, PluginEvent};
use super::project::Project;
use super::provider::{
    Completion, DeltaKind, Message, PartialResponse, Provider, ProviderConfig, ProviderError, Usage,
};
use super::tools;

#[derive(Clone, Debug)]
pub(crate) struct ChatConfig {
    pub(crate) provider: ProviderConfig,
    pub(crate) max_rounds: u32,
    pub(crate) max_tool_calls: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct VisibleTurn {
    pub(crate) user: String,
    pub(crate) assistant: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ResponseObservation {
    pub(crate) response_id: Option<String>,
    pub(crate) requested_model: String,
    pub(crate) reported_model: Option<String>,
    pub(crate) provider_created_at: Option<u64>,
    pub(crate) received_at: u64,
    pub(crate) usage: Option<Usage>,
    pub(crate) context_window: Option<u64>,
    pub(crate) promoted_text: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TurnOutcome {
    pub(crate) text: String,
    pub(crate) model_rounds: u32,
    pub(crate) tool_calls: u32,
    pub(crate) elapsed: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct CompactionOutcome {
    pub(crate) summary: String,
    pub(crate) elapsed: Duration,
}

pub(crate) enum ChatEvent<'a> {
    Reasoning(&'a str),
    Text(&'a str),
    ToolStarted {
        name: &'a str,
        detail: &'a str,
    },
    ToolFinished {
        name: &'a str,
        detail: &'a str,
        ok: bool,
        elapsed: Duration,
    },
    ToolProgress {
        call_id: &'a str,
        seq: u64,
        task_id: Option<&'a str>,
        stage: &'a str,
        preview: &'a str,
    },
    RelatedSession {
        call_id: &'a str,
        seq: u64,
        relation: &'a str,
        session_id: &'a str,
        delegation_id: &'a str,
        task_id: &'a str,
    },
    Response(&'a ResponseObservation),
}

pub(crate) struct DirectChat {
    provider: Provider,
    session_id: String,
    workspace: PathBuf,
    launch_path: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    system_prompt: String,
    messages: Vec<Message>,
    tools: tools::ToolSet,
    max_rounds: u32,
    max_tool_calls: u32,
}

impl DirectChat {
    pub(crate) fn new(
        project: &Project,
        config: ChatConfig,
        session_id: impl Into<String>,
        state_dir: Option<PathBuf>,
    ) -> Result<Self, ChatError> {
        let max_rounds = config.max_rounds.max(1);
        let max_tool_calls = config.max_tool_calls.max(1);
        let system_prompt = format!(
            "{}\n\nRuntime limits: {max_rounds} model rounds and {max_tool_calls} tool calls, including the final answer. Work directly: prefer the smallest useful change and focused verification over narrating a plan.",
            project.system_prompt()
        );
        let messages = vec![Message::system(system_prompt.clone())];
        let tools = tools::ToolSet::new(&project.plugin_tools).map_err(ChatError::new)?;
        Ok(Self {
            provider: Provider::new(config.provider).map_err(ChatError::provider)?,
            session_id: session_id.into(),
            workspace: project.workspace.clone(),
            launch_path: project.launch.as_ref().map(|launch| launch.path.clone()),
            state_dir,
            system_prompt,
            messages,
            tools,
            max_rounds,
            max_tool_calls,
        })
    }

    pub(crate) fn model(&self) -> &str {
        self.provider.model()
    }

    pub(crate) fn endpoint(&self) -> &str {
        self.provider.endpoint()
    }

    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        self.provider.reasoning_effort()
    }

    pub(crate) fn context_window(&self) -> Option<u64> {
        self.provider.context_window().map(u64::from)
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.tools.names()
    }

    pub(crate) fn reset(&mut self) {
        self.messages.clear();
        self.messages
            .push(Message::system(self.system_prompt.clone()));
    }

    pub(crate) fn history(&self) -> Vec<Message> {
        self.messages.clone()
    }

    pub(crate) fn restore(
        &mut self,
        summary: Option<&str>,
        turns: &[VisibleTurn],
        native_history: Option<&[Message]>,
    ) {
        if let Some(history) = native_history.filter(|history| !history.is_empty()) {
            self.messages = history.to_vec();
            return;
        }
        self.reset();
        if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
            self.messages.push(Message::system(format!(
                "Summary of the earlier visible conversation:\n{}",
                summary.trim()
            )));
        }
        for turn in turns {
            self.messages.push(Message::user(turn.user.clone()));
            self.messages
                .push(Message::assistant_text(turn.assistant.clone()));
        }
    }

    pub(crate) fn restore_interrupted(&mut self, model_input: &str, assistant_note: &str) {
        self.messages.push(Message::user(model_input.to_string()));
        self.messages
            .push(Message::assistant_text(assistant_note.to_string()));
    }

    pub(crate) fn send_cancellable(
        &mut self,
        input: impl Into<String>,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<TurnOutcome, ChatError> {
        let started = Instant::now();
        self.messages.push(Message::user(input));
        let mut visible = Vec::new();
        let mut tool_calls = 0_u32;

        for round in 1..=self.max_rounds {
            if cancellation.is_cancelled() {
                return Err(ChatError::new("turn cancelled")
                    .with_progress(round.saturating_sub(1), tool_calls));
            }
            let request_has_tools = !self.tools.definitions().is_empty();
            let completion = Self::complete(
                &self.provider,
                &mut self.messages,
                self.tools.definitions(),
                cancellation,
                on_event,
            )
            .map_err(|error| error.with_progress(round - 1, tool_calls))?;
            let observation = response_observation(&self.provider, &completion);
            on_event(ChatEvent::Response(&observation))?;
            if let Some(content) = completion
                .content
                .as_deref()
                .filter(|text| !text.is_empty())
            {
                visible.push(content.to_string());
            }
            self.messages.push(
                self.provider
                    .assistant_message(&completion, request_has_tools),
            );

            if completion.tool_calls.is_empty() {
                let text = visible.join("\n\n");
                if text.trim().is_empty() {
                    return Err(ChatError::new("model completed without visible text")
                        .with_progress(round, tool_calls));
                }
                return Ok(TurnOutcome {
                    text,
                    model_rounds: round,
                    tool_calls,
                    elapsed: started.elapsed(),
                });
            }

            for call in &completion.tool_calls {
                tool_calls = tool_calls.saturating_add(1);
                if tool_calls > self.max_tool_calls {
                    return Err(ChatError::new(format!(
                        "tool-call limit reached ({})",
                        self.max_tool_calls
                    ))
                    .with_progress(round, self.max_tool_calls));
                }
                let tool_started = Instant::now();
                let detail = self.tools.describe_call(call);
                on_event(ChatEvent::ToolStarted {
                    name: &call.name,
                    detail: &detail,
                })?;
                let context = PluginCallContext {
                    call_id: &call.id,
                    session_id: &self.session_id,
                    workspace: &self.workspace,
                    launch_path: self.launch_path.as_deref(),
                    state_dir: self.state_dir.as_deref(),
                };
                if cancellation.is_cancelled() {
                    return Err(ChatError::new("turn cancelled").with_progress(round, tool_calls));
                }
                let result = self
                    .tools
                    .execute_cancellable(call, &context, cancellation, &mut |event| match event {
                        PluginEvent::ToolProgress(progress) => on_event(ChatEvent::ToolProgress {
                            call_id: &progress.call_id,
                            seq: progress.seq,
                            task_id: progress.task_id.as_deref(),
                            stage: &progress.stage,
                            preview: &progress.preview,
                        }),
                        PluginEvent::RelatedSession(session) => {
                            on_event(ChatEvent::RelatedSession {
                                call_id: &session.call_id,
                                seq: session.seq,
                                relation: &session.relation,
                                session_id: &session.session_id,
                                delegation_id: &session.delegation_id,
                                task_id: &session.task_id,
                            })
                        }
                    })
                    .map_err(|error| error.with_progress(round, tool_calls))?;
                if cancellation.is_cancelled() {
                    return Err(ChatError::new("turn cancelled").with_progress(round, tool_calls));
                }
                on_event(ChatEvent::ToolFinished {
                    name: &result.label,
                    detail: &result.detail,
                    ok: result.ok,
                    elapsed: tool_started.elapsed(),
                })?;
                self.messages
                    .push(Message::tool(call.id.clone(), result.content));
            }
        }

        Err(
            ChatError::new(format!("model-round limit reached ({})", self.max_rounds))
                .with_progress(self.max_rounds, tool_calls),
        )
    }

    pub(crate) fn compact_cancellable(
        &self,
        turns: &[VisibleTurn],
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<CompactionOutcome, ChatError> {
        if turns.is_empty() {
            return Err(ChatError::new(
                "there are no older visible turns to compact",
            ));
        }
        let started = Instant::now();
        let transcript = turns
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                format!(
                    "Turn {}\nUser:\n{}\n\nAssistant:\n{}",
                    index + 1,
                    turn.user,
                    turn.assistant
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let mut messages = vec![
            Message::system(
                "Summarize only the visible conversation supplied by the user. Preserve decisions, constraints, unfinished work, exact paths and commands that matter. Do not use tools. Return only the compact summary.",
            ),
            Message::user(transcript),
        ];
        let completion =
            Self::complete(&self.provider, &mut messages, &[], cancellation, on_event)?;
        let observation = response_observation(&self.provider, &completion);
        on_event(ChatEvent::Response(&observation))?;
        if cancellation.is_cancelled() {
            return Err(ChatError::new("compaction cancelled"));
        }
        let summary = completion
            .content
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| ChatError::new("compaction model returned no summary"))?;
        Ok(CompactionOutcome {
            summary,
            elapsed: started.elapsed(),
        })
    }

    fn complete(
        provider: &Provider,
        messages: &mut [Message],
        definitions: &[super::provider::ToolDefinition],
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<Completion, ChatError> {
        match provider.complete(messages, definitions, cancellation, &mut |kind, delta| {
            let event = match kind {
                DeltaKind::Text => ChatEvent::Text(delta),
                DeltaKind::Reasoning => ChatEvent::Reasoning(delta),
            };
            on_event(event).map_err(|error| ProviderError::new(error.to_string()))
        }) {
            Ok(completion) => Ok(completion),
            Err(error) => {
                if let Some(partial) = error.partial_response() {
                    let observation = partial_response_observation(provider, partial);
                    on_event(ChatEvent::Response(&observation))?;
                }
                Err(ChatError::provider(error))
            }
        }
    }
}

fn response_observation(provider: &Provider, completion: &Completion) -> ResponseObservation {
    ResponseObservation {
        response_id: completion.response_id.clone(),
        requested_model: provider.model().to_string(),
        reported_model: completion.reported_model.clone(),
        provider_created_at: completion.provider_created_at,
        received_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        usage: completion.usage.clone(),
        context_window: provider.context_window().map(u64::from),
        promoted_text: if completion.promoted_reasoning {
            completion.content.clone()
        } else {
            None
        },
    }
}

fn partial_response_observation(
    provider: &Provider,
    partial: &PartialResponse,
) -> ResponseObservation {
    ResponseObservation {
        response_id: partial.response_id.clone(),
        requested_model: provider.model().to_string(),
        reported_model: partial.reported_model.clone(),
        provider_created_at: partial.provider_created_at,
        received_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        usage: Some(partial.usage.clone()),
        context_window: provider.context_window().map(u64::from),
        promoted_text: None,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChatError {
    message: String,
    model_rounds: u32,
    tool_calls: u32,
}

impl ChatError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            model_rounds: 0,
            tool_calls: 0,
        }
    }

    fn provider(error: ProviderError) -> Self {
        Self::new(error.to_string())
    }

    pub(crate) fn with_progress(mut self, model_rounds: u32, tool_calls: u32) -> Self {
        self.model_rounds = model_rounds;
        self.tool_calls = tool_calls;
        self
    }

    pub(crate) const fn model_rounds(&self) -> u32 {
        self.model_rounds
    }

    pub(crate) const fn tool_calls(&self) -> u32 {
        self.tool_calls
    }
}

impl fmt::Display for ChatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ChatError {}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread;

    use crate::runtime::ProviderIdentity;
    use crate::runtime::billing::{UsageLedger, UsagePurpose};

    use super::*;

    fn local_provider(address: SocketAddr) -> Provider {
        Provider::new(ProviderConfig {
            base_url: format!("http://{address}/v1"),
            provider: None,
            model: "test-model".to_string(),
            api_key_env: None,
            reasoning_effort: None,
            max_tokens: 256,
            context_window: None,
            timeout: Duration::from_secs(5),
        })
        .unwrap()
    }

    fn local_stream(payload: String) -> (Provider, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        (local_provider(address), server)
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

    fn run_failing_stream(
        payload: String,
        cancel_on_text: bool,
    ) -> (ChatError, UsageLedger, usize) {
        let (provider, server) = local_stream(payload);
        let identity = ProviderIdentity::from_endpoint(provider.endpoint());
        let cancellation = TurnCancellation::default();
        let callback_cancellation = cancellation.clone();
        let mut messages = vec![Message::user("hello")];
        let mut ledger = UsageLedger::new(None);
        let mut response_events = 0;

        let error =
            DirectChat::complete(&provider, &mut messages, &[], &cancellation, &mut |event| {
                match event {
                    ChatEvent::Text(_) if cancel_on_text => callback_cancellation.cancel(),
                    ChatEvent::Response(response) => {
                        response_events += 1;
                        identity.record(&mut ledger, UsagePurpose::Chat, response);
                    }
                    _ => {}
                }
                Ok(())
            })
            .unwrap_err();

        server.join().unwrap();
        (error, ledger, response_events)
    }

    #[test]
    fn reported_usage_before_cancellation_emits_one_ledger_response() {
        let payload = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            serde_json::json!({
                "id": "response-1",
                "model": "test-model",
                "choices": [],
                "usage": {"prompt_tokens": 7, "completion_tokens": 2}
            }),
            serde_json::json!({"choices": [], "usage": null}),
            serde_json::json!({"choices": [{"delta": {"content": "first"}}]})
        );

        let (error, ledger, response_events) = run_failing_stream(payload, true);

        assert!(error.to_string().contains("cancelled"));
        assert_eq!(response_events, 1);
        assert_eq!(ledger.entries().len(), 1);
        assert_eq!(
            ledger.entries()[0].response_id.as_deref(),
            Some("response-1")
        );
        assert_eq!(ledger.entries()[0].input_tokens, Some(7));
        assert_eq!(ledger.entries()[0].output_tokens, Some(2));
    }

    #[test]
    fn cancellation_without_reported_usage_does_not_invent_a_ledger_response() {
        let payload = format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "first"}}]})
        );

        let (error, ledger, response_events) = run_failing_stream(payload, true);

        assert!(error.to_string().contains("cancelled"));
        assert_eq!(response_events, 0);
        assert!(ledger.entries().is_empty());
    }

    #[test]
    fn reported_usage_before_a_protocol_error_is_recorded_once() {
        let payload = format!(
            "data: {}\n\ndata: not-json\n\n",
            serde_json::json!({
                "id": "response-1",
                "choices": [],
                "usage": {"prompt_tokens": 11, "completion_tokens": 3}
            })
        );

        let (error, ledger, response_events) = run_failing_stream(payload, false);

        assert!(error.to_string().contains("invalid JSON"));
        assert_eq!(response_events, 1);
        assert_eq!(ledger.entries().len(), 1);
        assert_eq!(ledger.entries()[0].input_tokens, Some(11));
        assert_eq!(ledger.entries()[0].output_tokens, Some(3));
    }
}
