use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::runtime::{
    AGUL_PLUGIN_FORMAT, ChatConfig, ChatEngine, ChatError, ChatEvent, ChatSession, CodexChatConfig,
    DEFAULT_MAX_ROUNDS, DEFAULT_MAX_TOKENS, DEFAULT_MAX_TOOL_CALLS, DEFAULT_TIMEOUT_SECONDS,
    NativeConnectionPreset, NativeProvider, NativeSessionConfig, PriceCatalog, Project,
    ProviderConfig, ProviderIdentity, RelatedSession, ResponseObservation, SessionAttribution,
    SessionEngine, SessionStatus, SessionStore, TraceAppender, UsagePurpose,
};

const METHODS: [&str; 6] = [
    "ari.initialize",
    "ari.capabilities",
    "ari.start_session",
    "ari.send",
    "ari.compact",
    "ari.close_session",
];

#[derive(Args, Debug)]
pub(crate) struct AriArgs {
    #[command(subcommand)]
    pub(crate) command: AriCommand,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AriCommand {
    /// Serve ARI JSON-RPC over standard input and output.
    Serve,
}

pub(crate) fn run(args: &AriArgs) -> io::Result<()> {
    match &args.command {
        AriCommand::Serve => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            serve_connection(&mut stdin.lock(), &mut stdout.lock())
        }
    }
}

fn serve_connection(input: &mut impl BufRead, output: &mut impl Write) -> io::Result<()> {
    let store = SessionStore::discover(None).map_err(io::Error::other)?;
    serve_connection_with_store(input, output, store)
}

fn serve_connection_with_store(
    input: &mut impl BufRead,
    output: &mut impl Write,
    store: SessionStore,
) -> io::Result<()> {
    let mut server = Server::new(store);
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }

        let request = match parse_request(&line) {
            Ok(request) => request,
            Err(error) => {
                write_message(output, &error_response(Value::Null, error))?;
                continue;
            }
        };
        let id = request.id.clone();
        let result = server.dispatch(request, output);
        if let Some(id) = id {
            let response = match result {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => error_response(id, error),
            };
            write_message(output, &response)?;
        }
    }
}

#[derive(Deserialize)]
struct Request {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default = "empty_params")]
    params: Value,
}

fn empty_params() -> Value {
    json!({})
}

fn parse_request(line: &str) -> Result<Request, RpcError> {
    let request: Request = serde_json::from_str(line)
        .map_err(|error| RpcError::new(-32700, format!("invalid JSON: {error}")))?;
    if request.jsonrpc != "2.0" || request.method.trim().is_empty() {
        return Err(RpcError::new(-32600, "invalid JSON-RPC request"));
    }
    Ok(request)
}

struct Server {
    initialized: bool,
    store: SessionStore,
    sessions: HashMap<String, Session>,
}

impl Server {
    fn new(store: SessionStore) -> Self {
        Self {
            initialized: false,
            store,
            sessions: HashMap::new(),
        }
    }

    fn dispatch(&mut self, request: Request, output: &mut impl Write) -> Result<Value, RpcError> {
        if request.method == "ari.initialize" {
            self.initialized = true;
            return Ok(json!({"ari": "0.2", "methods": METHODS}));
        }
        if !self.initialized {
            return Err(RpcError::new(-32000, "call ari.initialize first"));
        }

        match request.method.as_str() {
            "ari.capabilities" => Ok(json!({
                "methods": METHODS,
                "events": ["reasoning", "text", "tool", "tool_progress", "related_session", "usage"],
                "plugins": {"formats": [AGUL_PLUGIN_FORMAT]},
                "skills": "@skill:name",
                "usage": {"ledger": "per_response"},
                "engines": {
                    "native": {
                        "billing": "price_catalog",
                        "providers": ["deepseek", "glm"],
                        "tools": ["read", "write", "edit", "shell"],
                        "tool_owner": "agul",
                        "plugin_tools": true,
                        "manual_compaction": true,
                        "web_search": "optional"
                    },
                    "codex": {
                        "billing": "chatgpt_quota",
                        "tools": [],
                        "tool_owner": "codex_app_server",
                        "plugin_tools": false,
                        "manual_compaction": false,
                        "web_search": "live"
                    }
                }
            })),
            "ari.start_session" => self.start_session(request.params),
            "ari.send" => self.send(request.params, output),
            "ari.compact" => self.compact(request.params, output),
            "ari.close_session" => self.close_session(request.params),
            _ => Err(RpcError::new(-32601, "method not found")),
        }
    }

    fn start_session(&mut self, params: Value) -> Result<Value, RpcError> {
        let params: StartSessionParams = decode(params)?;
        let engine = params.engine.unwrap_or_default();
        params.validate_for_engine(engine)?;
        let limits = params.limits()?;
        let requested_reasoning_effort = params.reasoning_effort.clone();
        let attribution = SessionAttribution {
            parent_session_id: params.parent_session_id,
            delegation_id: params.delegation_id,
            task_id: params.task_id,
            specialist_id: params.specialist_id,
            pool_id: params.pool_id,
        };
        let workspace = params.workspace.unwrap_or_else(|| PathBuf::from("."));
        let project = Project::discover(&workspace, params.launch_path.as_deref())
            .map_err(|error| RpcError::runtime(error.to_string()))?;
        let workspace = project.workspace.display().to_string();
        let timeout = Duration::from_secs(limits.timeout_seconds);
        let (identity, state, chat, provider_name) = match engine {
            SessionEngine::Native => {
                let explicit_preset = params.provider;
                let default_preset = explicit_preset.unwrap_or_default();
                let base_url = params
                    .base_url
                    .unwrap_or_else(|| default_preset.base_url().to_string());
                if let Some(preset) = explicit_preset {
                    preset
                        .validate_official_endpoint(&base_url)
                        .map_err(|error| RpcError::new(-32602, error))?;
                }
                let endpoint_identity = ProviderIdentity::from_endpoint(&base_url);
                endpoint_identity
                    .validate_native_provider(explicit_preset.map(NativeConnectionPreset::provider))
                    .map_err(|error| RpcError::new(-32602, error))?;
                let preset = explicit_preset
                    .or_else(|| NativeConnectionPreset::from_official_endpoint(&base_url));
                let provider = explicit_preset
                    .map(NativeConnectionPreset::provider)
                    .or_else(|| endpoint_identity.native_provider());
                let identity = ProviderIdentity::from_native_preset(preset, provider, &base_url);
                let catalog = if identity.is_subscription() {
                    None
                } else {
                    match params.price_card {
                        Some(path) => {
                            let path = if path.is_absolute() {
                                path
                            } else {
                                project.workspace.join(path)
                            };
                            let json = fs::read_to_string(&path).map_err(|error| {
                                RpcError::runtime(format!(
                                    "could not read {}: {error}",
                                    path.display()
                                ))
                            })?;
                            Some(
                                PriceCatalog::from_json(&json)
                                    .map_err(|error| RpcError::runtime(error.to_string()))?,
                            )
                        }
                        None => provider
                            .map(NativeProvider::catalog)
                            .or_else(|| identity.default_catalog()),
                    }
                };
                let default_model = preset
                    .map(NativeConnectionPreset::model)
                    .or_else(|| provider.map(NativeProvider::model))
                    .or_else(|| identity.default_model())
                    .unwrap_or_else(|| NativeProvider::Deepseek.model());
                let requested_model = params.model.unwrap_or_else(|| default_model.to_string());
                let default_api_key_env = preset
                    .map(|preset| preset.api_key_env().to_string())
                    .or_else(|| provider.map(|provider| provider.api_key_env().to_string()))
                    .or_else(|| identity.default_api_key_env());
                let api_key_env = params.api_key_env.or(default_api_key_env);
                let reasoning_effort = match provider {
                    Some(provider) => provider
                        .normalize_reasoning_effort(params.reasoning_effort.as_deref())
                        .map_err(|error| RpcError::new(-32602, error))?,
                    None => params
                        .reasoning_effort
                        .as_deref()
                        .map(str::trim)
                        .filter(|effort| !effort.is_empty())
                        .map(str::to_string),
                };
                let config = ChatConfig {
                    provider: ProviderConfig {
                        base_url: base_url.clone(),
                        provider,
                        model: requested_model.clone(),
                        api_key_env: api_key_env.clone(),
                        reasoning_effort: reasoning_effort.clone(),
                        max_tokens: limits.max_tokens,
                        context_window: limits.context_window,
                        timeout,
                    },
                    max_rounds: limits.max_rounds,
                    max_tool_calls: limits.max_tool_calls,
                };
                let mut state = ChatSession::new_ari(
                    project.workspace.clone(),
                    requested_model,
                    catalog,
                    attribution,
                );
                state.set_native_config(Some(NativeSessionConfig {
                    preset,
                    provider,
                    base_url,
                    api_key_env,
                    reasoning_effort,
                }));
                let chat = ChatEngine::native(&project, config, &state.id)
                    .map_err(|error| RpcError::runtime(error.to_string()))?;
                let provider_name = identity.provider_name().to_string();
                (identity, state, chat, provider_name)
            }
            SessionEngine::Codex => {
                let requested_model = params.model.or_else(|| non_empty_env("AGUL_CODEX_MODEL"));
                let command = params
                    .codex_command
                    .or_else(|| non_empty_env("AGUL_CODEX_COMMAND"));
                let chat = ChatEngine::codex(
                    &project,
                    CodexChatConfig {
                        command,
                        model: requested_model,
                        reasoning_effort: params.reasoning_effort,
                        resume_thread_id: None,
                        ephemeral: false,
                        timeout,
                    },
                )
                .map_err(|error| RpcError::runtime(error.to_string()))?;
                let upstream_thread_id = chat
                    .thread_id()
                    .ok_or_else(|| RpcError::runtime("Codex engine did not create a thread"))?
                    .to_string();
                let state = ChatSession::new_ari_codex(
                    project.workspace.clone(),
                    chat.model(),
                    upstream_thread_id,
                    attribution,
                );
                let identity = ProviderIdentity::codex_subscription();
                let provider_name = identity.provider_name().to_string();
                (identity, state, chat, provider_name)
            }
        };
        let session_id = state.id.clone();
        let model = chat.model().to_string();
        let endpoint = chat.endpoint().to_string();
        let tools = chat.tool_names();
        let upstream_thread_id = state.upstream_thread_id.clone();
        let billing = identity.billing_label();
        let is_codex = chat.is_codex();
        let tool_owner = if is_codex { "codex_app_server" } else { "agul" };
        let reasoning_effort = chat
            .reasoning_effort()
            .or(requested_reasoning_effort.as_deref())
            .map(str::to_string);
        let commands = if is_codex {
            Vec::new()
        } else {
            project
                .plugin_commands
                .iter()
                .map(|command| {
                    json!({
                        "name": command.name,
                        "description": command.description,
                        "plugin": command.detail()
                    })
                })
                .collect::<Vec<_>>()
        };
        let plugin_capabilities = if is_codex {
            Vec::new()
        } else {
            project
                .plugin_capabilities
                .iter()
                .map(|capability| {
                    json!({
                        "name": capability.name,
                        "plugin": capability.plugin
                    })
                })
                .collect::<Vec<_>>()
        };

        self.store
            .save(&state)
            .map_err(|error| RpcError::runtime(error.to_string()))?;
        self.sessions.insert(
            session_id.clone(),
            Session {
                chat,
                project,
                state,
                identity,
            },
        );
        Ok(json!({
            "session_id": session_id,
            "workspace": workspace,
            "model": model,
            "engine": engine,
            "provider": provider_name,
            "endpoint": endpoint,
            "billing": billing,
            "upstream_thread_id": upstream_thread_id,
            "reasoning_effort": reasoning_effort,
            "capabilities": {
                "tool_owner": tool_owner,
                "plugin_tools": !is_codex,
                "manual_compaction": !is_codex,
                "web_search": if is_codex { "live" } else { "optional" }
            },
            "tools": tools,
            "plugin_commands": commands,
            "plugin_capabilities": plugin_capabilities
        }))
    }

    fn send(&mut self, params: Value, output: &mut impl Write) -> Result<Value, RpcError> {
        let params: SendParams = decode(params)?;
        if params.input.trim().is_empty() {
            return Err(RpcError::new(-32602, "input must not be empty"));
        }
        let store = self.store.clone();
        let session = self
            .sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| RpcError::new(-32001, "session not found"))?;
        let session_id = params.session_id.clone();
        ensure_session_can_continue(session)?;
        let model_input = match session.project.activate_references(&params.input) {
            Ok(input) => input,
            Err(error) => return Err(RpcError::new(-32602, error.to_string())),
        };
        session.state.begin_turn(
            params.input.clone(),
            model_input.clone(),
            session.chat.native_history(),
        );
        let operation_id = match begin_operation(
            &store,
            &mut session.state,
            "send",
            json!({"input": params.input}),
        ) {
            Ok(operation_id) => operation_id,
            Err(error) => {
                if session.state.engine == SessionEngine::Codex {
                    self.sessions.remove(&session_id);
                }
                return Err(error);
            }
        };
        let mut trace = match store.trace_appender(&session.state) {
            Ok(trace) => trace,
            Err(error) => {
                let message = error.to_string();
                fail_session(&store, &mut session.state, Some(&operation_id), &message);
                if session.state.engine == SessionEngine::Codex {
                    self.sessions.remove(&session_id);
                }
                return Err(RpcError::runtime(message));
            }
        };
        let Session {
            chat,
            state,
            identity,
            ..
        } = session;
        let outcome = match chat.send(model_input, &mut |event| {
            persist_session_event(
                &store,
                output,
                &session_id,
                &operation_id,
                event,
                state,
                identity,
                UsagePurpose::Chat,
                &mut trace,
            )
            .map_err(|error| ChatError::new(error.to_string()))
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                drop(trace);
                let terminate_bridge = state.engine == SessionEngine::Codex;
                let message = error.to_string();
                fail_session(&store, state, Some(&operation_id), &message);
                if terminate_bridge {
                    self.sessions.remove(&session_id);
                }
                return Err(RpcError::runtime(message));
            }
        };
        drop(trace);
        state.finish_turn(outcome.text.clone(), chat.native_history());
        let turn_handoff = if state.capture_handoff(&outcome.text) {
            state.handoff.clone()
        } else {
            None
        };
        if let Err(error) = complete_operation(
            &store,
            state,
            &operation_id,
            json!({
                "model_rounds": outcome.model_rounds,
                "tool_calls": outcome.tool_calls,
                "elapsed_ms": duration_millis(outcome.elapsed),
                "handoff": turn_handoff.clone()
            }),
        ) {
            if state.engine == SessionEngine::Codex {
                self.sessions.remove(&session_id);
            }
            return Err(error);
        }
        Ok(json!({
            "session_id": session_id,
            "engine": state.engine,
            "billing": identity.billing_label(),
            "text": outcome.text,
            "handoff": turn_handoff,
            "model_rounds": outcome.model_rounds,
            "tool_calls": outcome.tool_calls,
            "elapsed_ms": duration_millis(outcome.elapsed),
            "usage": state.usage.summary()
        }))
    }

    fn compact(&mut self, params: Value, output: &mut impl Write) -> Result<Value, RpcError> {
        let params: SessionParams = decode(params)?;
        let store = self.store.clone();
        let session = self
            .sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| RpcError::new(-32001, "session not found"))?;
        let session_id = params.session_id.clone();
        ensure_session_can_continue(session)?;
        if session.chat.is_codex() {
            return Err(RpcError::new(
                -32602,
                "manual visible-turn compaction is not available for the Codex engine",
            ));
        }
        let Session {
            chat,
            state,
            identity,
            ..
        } = session;
        let source = state.compaction_source(0).to_vec();
        state.resume();
        let operation_id = begin_operation(
            &store,
            state,
            "compact",
            json!({"visible_turns": source.len()}),
        )?;
        let mut trace = match store.trace_appender(state) {
            Ok(trace) => trace,
            Err(error) => {
                let message = error.to_string();
                fail_session(&store, state, Some(&operation_id), &message);
                return Err(RpcError::runtime(message));
            }
        };
        let outcome = match chat.compact(&source, &mut |event| {
            persist_session_event(
                &store,
                output,
                &session_id,
                &operation_id,
                event,
                state,
                identity,
                UsagePurpose::Compaction,
                &mut trace,
            )
            .map_err(|error| ChatError::new(error.to_string()))
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                drop(trace);
                fail_session(&store, state, Some(&operation_id), &error.to_string());
                return Err(RpcError::runtime(error.to_string()));
            }
        };
        drop(trace);
        state.commit_compaction(0, outcome.summary);
        chat.restore(state.summary.as_deref(), &state.turns, None);
        state.set_native_history(chat.native_history());
        let summary = state.summary.clone().unwrap_or_default();
        complete_operation(
            &store,
            state,
            &operation_id,
            json!({
                "elapsed_ms": duration_millis(outcome.elapsed),
                "summary": summary
            }),
        )?;
        Ok(json!({
            "session_id": session_id,
            "summary": summary,
            "elapsed_ms": duration_millis(outcome.elapsed),
            "usage": state.usage.summary()
        }))
    }

    fn close_session(&mut self, params: Value) -> Result<Value, RpcError> {
        let params: SessionParams = decode(params)?;
        let session = self
            .sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| RpcError::new(-32001, "session not found"))?;
        if session.state.status == SessionStatus::Active {
            session.state.set_status(SessionStatus::Completed);
        }
        self.store
            .save(&session.state)
            .map_err(|error| RpcError::runtime(error.to_string()))?;
        self.sessions.remove(&params.session_id);
        Ok(json!({"session_id": params.session_id, "closed": true}))
    }
}

struct Session {
    chat: ChatEngine,
    project: Project,
    state: ChatSession,
    identity: ProviderIdentity,
}

fn ensure_session_can_continue(session: &Session) -> Result<(), RpcError> {
    if session.state.engine == SessionEngine::Codex && session.state.status != SessionStatus::Active
    {
        return Err(RpcError::runtime(format!(
            "Codex session is {}; start a new session",
            session.state.status
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartSessionParams {
    workspace: Option<PathBuf>,
    launch_path: Option<PathBuf>,
    parent_session_id: Option<String>,
    delegation_id: Option<String>,
    task_id: Option<String>,
    specialist_id: Option<String>,
    pool_id: Option<String>,
    engine: Option<SessionEngine>,
    #[serde(default, deserialize_with = "deserialize_public_provider")]
    provider: Option<NativeConnectionPreset>,
    model: Option<String>,
    codex_command: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    reasoning_effort: Option<String>,
    price_card: Option<PathBuf>,
    context_window: Option<u32>,
    timeout_seconds: Option<u64>,
    max_tokens: Option<u32>,
    max_rounds: Option<u32>,
    max_tool_calls: Option<u32>,
}

fn deserialize_public_provider<'de, D>(
    deserializer: D,
) -> Result<Option<NativeConnectionPreset>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .transpose()
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Eq, PartialEq)]
struct SessionLimits {
    context_window: Option<u32>,
    timeout_seconds: u64,
    max_tokens: u32,
    max_rounds: u32,
    max_tool_calls: u32,
}

impl StartSessionParams {
    fn validate_for_engine(&self, engine: SessionEngine) -> Result<(), RpcError> {
        let unsupported = match engine {
            SessionEngine::Native => self
                .codex_command
                .is_some()
                .then_some("codex_command")
                .into_iter()
                .collect::<Vec<_>>(),
            SessionEngine::Codex => [
                (self.provider.is_some(), "provider"),
                (self.base_url.is_some(), "base_url"),
                (self.api_key_env.is_some(), "api_key_env"),
                (self.price_card.is_some(), "price_card"),
                (self.context_window.is_some(), "context_window"),
                (self.max_tokens.is_some(), "max_tokens"),
                (self.max_rounds.is_some(), "max_rounds"),
                (self.max_tool_calls.is_some(), "max_tool_calls"),
            ]
            .into_iter()
            .filter_map(|(present, name)| present.then_some(name))
            .collect(),
        };
        if unsupported.is_empty() {
            Ok(())
        } else {
            Err(RpcError::new(
                -32602,
                format!(
                    "the {engine} engine does not support: {}",
                    unsupported.join(", ")
                ),
            ))
        }
    }

    fn limits(&self) -> Result<SessionLimits, RpcError> {
        Ok(SessionLimits {
            context_window: optional_positive_u32("context_window", self.context_window)?,
            timeout_seconds: positive_u64(
                "timeout_seconds",
                self.timeout_seconds,
                DEFAULT_TIMEOUT_SECONDS,
            )?,
            max_tokens: positive_u32("max_tokens", self.max_tokens, DEFAULT_MAX_TOKENS)?,
            max_rounds: positive_u32("max_rounds", self.max_rounds, DEFAULT_MAX_ROUNDS)?,
            max_tool_calls: positive_u32(
                "max_tool_calls",
                self.max_tool_calls,
                DEFAULT_MAX_TOOL_CALLS,
            )?,
        })
    }
}

fn optional_positive_u32(name: &str, value: Option<u32>) -> Result<Option<u32>, RpcError> {
    match value {
        Some(0) => Err(RpcError::new(
            -32602,
            format!("{name} must be greater than zero"),
        )),
        value => Ok(value),
    }
}

fn positive_u32(name: &str, value: Option<u32>, default: u32) -> Result<u32, RpcError> {
    optional_positive_u32(name, value).map(|value| value.unwrap_or(default))
}

fn positive_u64(name: &str, value: Option<u64>, default: u64) -> Result<u64, RpcError> {
    match value {
        Some(0) => Err(RpcError::new(
            -32602,
            format!("{name} must be greater than zero"),
        )),
        Some(value) => Ok(value),
        None => Ok(default),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendParams {
    session_id: String,
    input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionParams {
    session_id: String,
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new(-32602, format!("invalid params: {error}")))
}

#[cfg(test)]
fn write_chat_event(
    output: &mut impl Write,
    session_id: &str,
    event: ChatEvent<'_>,
) -> io::Result<()> {
    let mut params = chat_event_params(event);
    params.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    write_message(
        output,
        &json!({"jsonrpc": "2.0", "method": "ari.event", "params": params}),
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_session_event(
    store: &SessionStore,
    output: &mut impl Write,
    session_id: &str,
    operation_id: &str,
    event: ChatEvent<'_>,
    state: &mut ChatSession,
    identity: &ProviderIdentity,
    purpose: UsagePurpose,
    trace: &mut TraceAppender,
) -> io::Result<()> {
    let mut params = match event {
        ChatEvent::Response(observation) => {
            let entry = identity
                .record(&mut state.usage, purpose, observation)
                .clone();
            let mut params = usage_event(observation);
            params.insert(
                "ledger_entry".to_string(),
                serde_json::to_value(entry).map_err(io::Error::other)?,
            );
            params
        }
        ChatEvent::RelatedSession {
            call_id,
            seq,
            relation,
            session_id,
            delegation_id,
            task_id,
        } => {
            state.add_related_session(RelatedSession {
                relation: relation.to_string(),
                session_id: session_id.to_string(),
                delegation_id: Some(delegation_id.to_string()),
                task_id: Some(task_id.to_string()),
            });
            object(json!({
                "kind": "related_session",
                "call_id": call_id,
                "seq": seq,
                "relation": relation,
                "related_session_id": session_id,
                "delegation_id": delegation_id,
                "task_id": task_id
            }))
        }
        event => chat_event_params(event),
    };
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_string();
    let trace_seq = trace
        .append(state, operation_id, &kind, Value::Object(params.clone()))
        .map_err(io::Error::other)?;
    if kind == "usage" {
        store.save(state).map_err(io::Error::other)?;
    }
    params.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    params.insert(
        "operation_id".to_string(),
        Value::String(operation_id.to_string()),
    );
    params.insert("trace_seq".to_string(), Value::from(trace_seq));
    write_message(
        output,
        &json!({"jsonrpc": "2.0", "method": "ari.event", "params": params}),
    )
}

fn begin_operation(
    store: &SessionStore,
    state: &mut ChatSession,
    kind: &str,
    data: Value,
) -> Result<String, RpcError> {
    let operation_id = match store.begin_trace_operation(state, kind, data) {
        Ok(operation_id) => operation_id,
        Err(error) => {
            fail_session(store, state, None, &error.to_string());
            return Err(RpcError::runtime(error.to_string()));
        }
    };
    if let Err(error) = store.save(state) {
        fail_session(store, state, Some(&operation_id), &error.to_string());
        return Err(RpcError::runtime(error.to_string()));
    }
    Ok(operation_id)
}

fn complete_operation(
    store: &SessionStore,
    state: &mut ChatSession,
    operation_id: &str,
    data: Value,
) -> Result<(), RpcError> {
    if let Err(error) = store.append_trace(state, operation_id, "operation_completed", data) {
        fail_session(store, state, Some(operation_id), &error.to_string());
        return Err(RpcError::runtime(error.to_string()));
    }
    if let Err(error) = store.save(state) {
        fail_session(store, state, Some(operation_id), &error.to_string());
        return Err(RpcError::runtime(error.to_string()));
    }
    Ok(())
}

fn fail_session(
    store: &SessionStore,
    state: &mut ChatSession,
    operation_id: Option<&str>,
    message: &str,
) {
    state.set_status(SessionStatus::Failed);
    if let Some(operation_id) = operation_id {
        let _ = store.append_trace(
            state,
            operation_id,
            "operation_failed",
            json!({"error": message}),
        );
    }
    let _ = store.save(state);
}

fn chat_event_params(event: ChatEvent<'_>) -> Map<String, Value> {
    match event {
        ChatEvent::Reasoning(text) => object(json!({"kind": "reasoning", "text": text})),
        ChatEvent::Text(text) => object(json!({"kind": "text", "text": text})),
        ChatEvent::ToolStarted { name, detail } => object(json!({
            "kind": "tool",
            "phase": "started",
            "name": name,
            "detail": detail
        })),
        ChatEvent::ToolFinished {
            name,
            detail,
            ok,
            elapsed,
        } => object(json!({
            "kind": "tool",
            "phase": "finished",
            "name": name,
            "detail": detail,
            "ok": ok,
            "elapsed_ms": duration_millis(elapsed)
        })),
        ChatEvent::ToolProgress {
            call_id,
            seq,
            task_id,
            stage,
            preview,
        } => object(json!({
            "kind": "tool_progress",
            "call_id": call_id,
            "seq": seq,
            "task_id": task_id,
            "stage": stage,
            "preview": preview
        })),
        ChatEvent::RelatedSession {
            call_id,
            seq,
            relation,
            session_id,
            delegation_id,
            task_id,
        } => object(json!({
            "kind": "related_session",
            "call_id": call_id,
            "seq": seq,
            "relation": relation,
            "related_session_id": session_id,
            "delegation_id": delegation_id,
            "task_id": task_id
        })),
        ChatEvent::Response(observation) => usage_event(observation),
    }
}

// Response observations meet ARI here; billing can enrich this object without
// changing the model loop or the wire framing above.
fn usage_event(observation: &ResponseObservation) -> Map<String, Value> {
    let usage = observation.usage.as_ref().map(|usage| {
        json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_hit_tokens": usage.cache_hit_tokens,
            "cache_miss_tokens": usage.cache_miss_tokens,
            "reasoning_tokens": usage.reasoning_tokens
        })
    });
    object(json!({
        "kind": "usage",
        "response_id": observation.response_id,
        "requested_model": observation.requested_model,
        "reported_model": observation.reported_model,
        "provider_created_at": observation.provider_created_at,
        "received_at": observation.received_at,
        "usage": usage
    }))
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("event body is an object")
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn write_message(output: &mut impl Write, message: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, message).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self::new(-32002, message)
    }
}

fn error_response(id: Value, error: RpcError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": error.code, "message": error.message}
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::runtime::Usage;

    #[test]
    fn initialize_and_capabilities_use_the_small_protocol() {
        let state = tempfile::tempdir().unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":"1","method":"ari.initialize","params":{"client":{"name":"agentkube","version":"0.1.0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"2","method":"ari.capabilities","params":{}}"#,
            "\n"
        );
        let mut output = Vec::new();

        serve_connection_with_store(
            &mut Cursor::new(input),
            &mut output,
            SessionStore::discover(Some(state.path())).unwrap(),
        )
        .unwrap();

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[0]["result"]["ari"], "0.2");
        assert_eq!(messages[1]["result"]["methods"], json!(METHODS));
        assert_eq!(
            messages[1]["result"]["events"],
            json!([
                "reasoning",
                "text",
                "tool",
                "tool_progress",
                "related_session",
                "usage"
            ])
        );
        assert!(messages[1]["result"].get("tools").is_none());
        assert!(messages[1]["result"].get("billing").is_none());
        assert_eq!(
            messages[1]["result"]["engines"]["native"],
            json!({
                "billing": "price_catalog",
                "providers": ["deepseek", "glm"],
                "tools": ["read", "write", "edit", "shell"],
                "tool_owner": "agul",
                "plugin_tools": true,
                "manual_compaction": true,
                "web_search": "optional"
            })
        );
        assert_eq!(
            messages[1]["result"]["engines"]["codex"],
            json!({
                "billing": "chatgpt_quota",
                "tools": [],
                "tool_owner": "codex_app_server",
                "plugin_tools": false,
                "manual_compaction": false,
                "web_search": "live"
            })
        );
        assert_eq!(
            messages[1]["result"]["plugins"]["formats"],
            json!([AGUL_PLUGIN_FORMAT])
        );
        assert_eq!(
            messages[1]["result"]["usage"],
            json!({"ledger": "per_response"})
        );
    }

    #[test]
    fn start_session_limits_keep_defaults_and_accept_overrides() {
        let defaults: StartSessionParams = decode(json!({})).unwrap();
        assert_eq!(
            defaults.limits().unwrap(),
            SessionLimits {
                context_window: None,
                timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
                max_tokens: DEFAULT_MAX_TOKENS,
                max_rounds: DEFAULT_MAX_ROUNDS,
                max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            }
        );

        let overrides: StartSessionParams = decode(json!({
            "context_window": 32_768,
            "timeout_seconds": 600,
            "max_tokens": 8_192,
            "max_rounds": 4,
            "max_tool_calls": 12
        }))
        .unwrap();
        assert_eq!(
            overrides.limits().unwrap(),
            SessionLimits {
                context_window: Some(32_768),
                timeout_seconds: 600,
                max_tokens: 8_192,
                max_rounds: 4,
                max_tool_calls: 12,
            }
        );
    }

    #[test]
    fn start_session_limits_reject_zero() {
        for (name, params) in [
            ("context_window", json!({"context_window": 0})),
            ("timeout_seconds", json!({"timeout_seconds": 0})),
            ("max_tokens", json!({"max_tokens": 0})),
            ("max_rounds", json!({"max_rounds": 0})),
            ("max_tool_calls", json!({"max_tool_calls": 0})),
        ] {
            let params: StartSessionParams = decode(params).unwrap();
            let error = params.limits().unwrap_err();
            assert_eq!(error.code, -32602);
            assert_eq!(error.message, format!("{name} must be greater than zero"));
        }
    }

    #[test]
    fn start_session_rejects_engine_specific_parameters() {
        let native: StartSessionParams = decode(json!({
            "engine": "native",
            "codex_command": "codex-test"
        }))
        .unwrap();
        let error = native
            .validate_for_engine(SessionEngine::Native)
            .unwrap_err();
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.message,
            "the native engine does not support: codex_command"
        );

        let codex: StartSessionParams = decode(json!({
            "engine": "codex",
            "provider": "glm",
            "base_url": "http://127.0.0.1:9/v1",
            "api_key_env": "IGNORED",
            "price_card": "prices.json",
            "context_window": 32768,
            "max_tokens": 4096,
            "max_rounds": 4,
            "max_tool_calls": 12
        }))
        .unwrap();
        let error = codex.validate_for_engine(SessionEngine::Codex).unwrap_err();
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.message,
            "the codex engine does not support: provider, base_url, api_key_env, price_card, context_window, max_tokens, max_rounds, max_tool_calls"
        );
    }

    #[test]
    fn start_and_close_accept_a_thin_launch_without_contacting_a_model() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".agents/runtime")).unwrap();
        std::fs::write(root.path().join(".agents/AGENTS.md"), "Help directly.").unwrap();
        std::fs::write(
            root.path().join(".agents/runtime/launch.json"),
            r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md"}"#,
        )
        .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut server = Server::new(SessionStore::discover(Some(state.path())).unwrap());
        server.initialized = true;
        let mut output = Vec::new();

        let started = server
            .dispatch(
                Request {
                    jsonrpc: "2.0".to_string(),
                    id: Some(json!("start")),
                    method: "ari.start_session".to_string(),
                    params: json!({
                        "workspace": root.path(),
                        "base_url": "http://127.0.0.1:9/v1",
                        "api_key_env": "",
                        "parent_session_id": "parent-1",
                        "delegation_id": "delegation-1",
                        "task_id": "task-1",
                        "specialist_id": "repo-scout",
                        "pool_id": "local"
                    }),
                },
                &mut output,
            )
            .unwrap();
        let session_id = started["session_id"].as_str().unwrap();
        assert_eq!(server.sessions.len(), 1);
        let persisted = server.store.load(session_id, None).unwrap();
        assert_eq!(persisted.source, crate::runtime::SessionSource::Ari);
        assert_eq!(persisted.status, SessionStatus::Active);
        assert_eq!(
            persisted.attribution,
            SessionAttribution {
                parent_session_id: Some("parent-1".to_string()),
                delegation_id: Some("delegation-1".to_string()),
                task_id: Some("task-1".to_string()),
                specialist_id: Some("repo-scout".to_string()),
                pool_id: Some("local".to_string()),
            }
        );

        let closed = server
            .close_session(json!({"session_id": session_id}))
            .unwrap();
        assert_eq!(closed["closed"], true);
        assert!(server.sessions.is_empty());
        let persisted = server.store.load(session_id, None).unwrap();
        assert_eq!(persisted.status, SessionStatus::Completed);
    }

    #[test]
    fn glm_provider_resolves_coding_plan_and_keeps_ordinary_api_explicit() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut server = Server::new(SessionStore::discover(Some(state.path())).unwrap());
        server.initialized = true;

        let started = server
            .start_session(json!({
                "workspace": workspace.path(),
                "provider": "glm",
                "reasoning_effort": "medium"
            }))
            .unwrap();

        assert_eq!(started["provider"], "glm");
        assert_eq!(started["model"], "glm-4.7");
        assert_eq!(
            started["endpoint"],
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(started["billing"], "subscription_quota");
        assert_eq!(started["reasoning_effort"], "high");
        let session_id = started["session_id"].as_str().unwrap();
        let persisted = server.store.load(session_id, None).unwrap();
        assert_eq!(
            persisted.native_config(),
            Some(&NativeSessionConfig {
                preset: Some(NativeConnectionPreset::GlmCoding),
                provider: Some(NativeProvider::Glm),
                base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
                api_key_env: Some("GLM_API_KEY".to_string()),
                reasoning_effort: Some("high".to_string()),
            })
        );

        let compatibility_alias = server
            .start_session(json!({
                "workspace": workspace.path(),
                "provider": "glm-coding"
            }))
            .unwrap();
        assert_eq!(compatibility_alias["provider"], "glm");
        assert_eq!(compatibility_alias["model"], "glm-4.7");
        assert_eq!(
            compatibility_alias["endpoint"],
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(compatibility_alias["billing"], "subscription_quota");
        let coding_id = compatibility_alias["session_id"].as_str().unwrap();
        let persisted = server.store.load(coding_id, None).unwrap();
        assert_eq!(
            persisted.native_config(),
            Some(&NativeSessionConfig {
                preset: Some(NativeConnectionPreset::GlmCoding),
                provider: Some(NativeProvider::Glm),
                base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
                api_key_env: Some("GLM_API_KEY".to_string()),
                reasoning_effort: None,
            })
        );

        let ordinary_api = server
            .start_session(json!({
                "workspace": workspace.path(),
                "base_url": "https://open.bigmodel.cn/api/paas/v4"
            }))
            .unwrap();
        assert_eq!(ordinary_api["provider"], "glm");
        assert_eq!(ordinary_api["model"], "glm-5.3");
        assert_eq!(ordinary_api["billing"], "price_catalog");
        let ordinary_id = ordinary_api["session_id"].as_str().unwrap();
        let persisted = server.store.load(ordinary_id, None).unwrap();
        assert_eq!(
            persisted.native_config(),
            Some(&NativeSessionConfig {
                preset: Some(NativeConnectionPreset::Glm),
                provider: Some(NativeProvider::Glm),
                base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
                api_key_env: Some("GLM_API_KEY".to_string()),
                reasoning_effort: None,
            })
        );

        let conflict = server
            .start_session(json!({
                "workspace": workspace.path(),
                "provider": "glm",
                "base_url": "https://api.deepseek.com"
            }))
            .unwrap_err();
        assert_eq!(conflict.code, -32602);
        assert!(
            conflict
                .message
                .contains("provider glm conflicts with URL provider deepseek")
        );

        let coding_route_conflict = server
            .start_session(json!({
                "workspace": workspace.path(),
                "provider": "glm-coding",
                "base_url": "https://open.bigmodel.cn/api/paas/v4"
            }))
            .unwrap_err();
        assert_eq!(coding_route_conflict.code, -32602);
        assert!(
            coding_route_conflict
                .message
                .contains("provider glm conflicts with URL provider glm-api")
        );
    }

    #[test]
    fn close_preserves_terminal_failure_statuses() {
        for status in [SessionStatus::Failed, SessionStatus::Interrupted] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join(".agents/runtime")).unwrap();
            std::fs::write(root.path().join(".agents/AGENTS.md"), "Help directly.").unwrap();
            std::fs::write(
                root.path().join(".agents/runtime/launch.json"),
                r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md"}"#,
            )
            .unwrap();
            let state = tempfile::tempdir().unwrap();
            let mut server = Server::new(SessionStore::discover(Some(state.path())).unwrap());
            server.initialized = true;
            let mut output = Vec::new();
            let started = server
                .dispatch(
                    Request {
                        jsonrpc: "2.0".to_string(),
                        id: Some(json!("start")),
                        method: "ari.start_session".to_string(),
                        params: json!({
                            "workspace": root.path(),
                            "base_url": "http://127.0.0.1:9/v1",
                            "api_key_env": ""
                        }),
                    },
                    &mut output,
                )
                .unwrap();
            let session_id = started["session_id"].as_str().unwrap();
            server
                .sessions
                .get_mut(session_id)
                .unwrap()
                .state
                .set_status(status);

            server
                .close_session(json!({"session_id": session_id}))
                .unwrap();

            let persisted = server.store.load(session_id, None).unwrap();
            assert_eq!(persisted.status, status);
        }
    }

    #[test]
    fn persisted_events_share_an_operation_and_save_usage() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut state = ChatSession::new_ari(
            root.path().to_path_buf(),
            "model",
            None,
            SessionAttribution::default(),
        );
        let session_id = state.id.clone();
        store.save(&state).unwrap();
        let operation_id = store
            .begin_trace_operation(&mut state, "send", json!({"input": "hello"}))
            .unwrap();
        let identity = ProviderIdentity::from_endpoint("http://127.0.0.1:51100/v1");
        let observation = ResponseObservation {
            response_id: Some("response-1".to_string()),
            requested_model: "model".to_string(),
            reported_model: Some("model".to_string()),
            provider_created_at: Some(1),
            received_at: 2,
            usage: Some(Usage {
                input_tokens: 20,
                output_tokens: 8,
                cache_hit_tokens: Some(10),
                cache_miss_tokens: Some(10),
                reasoning_tokens: Some(3),
            }),
            context_window: None,
            promoted_text: None,
        };
        let mut output = Vec::new();
        let mut trace = store.trace_appender(&state).unwrap();

        persist_session_event(
            &store,
            &mut output,
            &session_id,
            &operation_id,
            ChatEvent::Reasoning("thinking"),
            &mut state,
            &identity,
            UsagePurpose::Chat,
            &mut trace,
        )
        .unwrap();
        persist_session_event(
            &store,
            &mut output,
            &session_id,
            &operation_id,
            ChatEvent::Response(&observation),
            &mut state,
            &identity,
            UsagePurpose::Chat,
            &mut trace,
        )
        .unwrap();

        let persisted = store.load(&state.id, None).unwrap();
        assert_eq!(persisted.usage.entries().len(), 1);
        assert_eq!(
            persisted.usage.entries()[0].response_id,
            Some("response-1".to_string())
        );
        let trace = store.read_trace(&state.id).unwrap();
        let events = trace
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|event| event["operation_id"] == operation_id)
        );
        assert_eq!(events[1]["type"], "reasoning");
        assert_eq!(events[2]["type"], "usage");
        assert!(events[2]["data"]["ledger_entry"].is_object());
        let notifications = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(notifications[0]["params"]["operation_id"], operation_id);
        assert_eq!(notifications[1]["params"]["trace_seq"], 3);
    }

    #[test]
    fn failed_operation_is_persisted() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut state = ChatSession::new_ari(
            root.path().to_path_buf(),
            "model",
            None,
            SessionAttribution::default(),
        );
        let operation_id = store
            .begin_trace_operation(&mut state, "send", json!({"input": "hello"}))
            .unwrap();

        fail_session(&store, &mut state, Some(&operation_id), "provider asleep");

        let persisted = store.load(&state.id, None).unwrap();
        assert_eq!(persisted.status, SessionStatus::Failed);
        let trace = store.read_trace(&state.id).unwrap();
        let failed: Value = serde_json::from_str(trace.lines().last().unwrap()).unwrap();
        assert_eq!(failed["operation_id"], operation_id);
        assert_eq!(failed["type"], "operation_failed");
        assert_eq!(failed["data"]["error"], "provider asleep");
    }

    #[test]
    fn chat_events_are_plain_json_rpc_notifications() {
        let observation = ResponseObservation {
            response_id: Some("response-1".to_string()),
            requested_model: "deepseek-v4-pro".to_string(),
            reported_model: Some("deepseek-v4-pro".to_string()),
            provider_created_at: Some(1),
            received_at: 2,
            usage: Some(Usage {
                input_tokens: 20,
                output_tokens: 8,
                cache_hit_tokens: Some(10),
                cache_miss_tokens: Some(10),
                reasoning_tokens: Some(3),
            }),
            context_window: None,
            promoted_text: None,
        };
        let mut output = Vec::new();
        write_chat_event(&mut output, "session-1", ChatEvent::Reasoning("thinking")).unwrap();
        write_chat_event(&mut output, "session-1", ChatEvent::Text("answer")).unwrap();
        write_chat_event(
            &mut output,
            "session-1",
            ChatEvent::ToolStarted {
                name: "read",
                detail: "README.md",
            },
        )
        .unwrap();
        write_chat_event(
            &mut output,
            "session-1",
            ChatEvent::ToolProgress {
                call_id: "call-1",
                seq: 1,
                task_id: Some("scan"),
                stage: "thinking",
                preview: "locating",
            },
        )
        .unwrap();
        write_chat_event(
            &mut output,
            "session-1",
            ChatEvent::RelatedSession {
                call_id: "call-1",
                seq: 2,
                relation: "delegated",
                session_id: "child-1",
                delegation_id: "delegation-1",
                task_id: "scan",
            },
        )
        .unwrap();
        write_chat_event(&mut output, "session-1", ChatEvent::Response(&observation)).unwrap();

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 6);
        assert!(messages.iter().all(|value| value["method"] == "ari.event"));
        assert_eq!(messages[0]["params"]["kind"], "reasoning");
        assert_eq!(messages[2]["params"]["kind"], "tool");
        assert_eq!(messages[3]["params"]["kind"], "tool_progress");
        assert_eq!(messages[4]["params"]["kind"], "related_session");
        assert_eq!(messages[4]["params"]["related_session_id"], "child-1");
        assert_eq!(messages[5]["params"]["kind"], "usage");
        assert_eq!(messages[5]["params"]["usage"]["input_tokens"], 20);
    }
}
