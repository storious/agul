use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use clap::{Args, ValueEnum};
use serde_json::json;

use crate::runtime::billing::UsageEntry;
use crate::runtime::{
    ChatConfig, ChatEngine, ChatError, ChatEvent, ChatSession, CodexChatConfig,
    DEEPSEEK_DEFAULT_MODEL, DEFAULT_MAX_ROUNDS, DEFAULT_MAX_TOKENS, DEFAULT_MAX_TOOL_CALLS,
    DEFAULT_TIMEOUT_SECONDS, INTERRUPTED_TURN_NOTE, NativeConnectionPreset, NativeProvider,
    NativeSessionConfig, PluginCallContext, PluginEvent, PluginExecutionError, PluginTerminal,
    PriceCatalog, PriceCatalogStore, PriceSelection, PricingStatus, Project, ProviderConfig,
    ProviderIdentity, RelatedSession, SessionEngine, SessionStatus, SessionStore, TurnCancellation,
    UsagePurpose, UsageSummary, format_femto_amount_3dp,
};
use crate::terminal::plain_text;

mod display;
mod input;
mod message;
mod resume;
mod terminal_markdown;
mod theme;
mod workbench;

use input::{
    InteractiveInput, InteractivePrinter, InteractiveRead, TriggerCandidate, TurnWake,
    submitted_lines,
};
use message::{CHAT_COMMANDS, ChatCommand, ParsedMessage, parse_message, parse_user_text};
use resume::{
    SessionRequest, resolve_session_request, resumed_line, session_age, session_status,
    session_turns,
};
use terminal_markdown::TerminalMarkdown;
use theme::render_transcript_text;
use workbench::{
    TranscriptKind, TranscriptLine, TranscriptStyle, TranscriptText, TranscriptTone,
    WorkbenchPhase, WorkbenchStatus, format_duration, format_tokens, push_group_gap,
    user_message_lines,
};

const RETAIN_AFTER_COMPACTION: usize = 4;
const STREAM_TRACE_BUFFER_BYTES: usize = 16 * 1024;
const MAX_RESUMED_TRANSCRIPT_TURNS: usize = 64;
const MAX_RESUMED_TRANSCRIPT_BYTES: usize = 512 * 1024;
const DEEPSEEK_V4_CONTEXT_WINDOW: u32 = 1_000_000;
static PLUGIN_COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum EngineArg {
    Native,
    Codex,
}

impl From<EngineArg> for SessionEngine {
    fn from(value: EngineArg) -> Self {
        match value {
            EngineArg::Native => Self::Native,
            EngineArg::Codex => Self::Codex,
        }
    }
}

#[derive(Default)]
struct StreamTraceBuffer {
    pending: Option<PendingStreamTrace>,
}

struct PendingStreamTrace {
    operation_id: String,
    kind: &'static str,
    text: String,
}

impl StreamTraceBuffer {
    fn push(
        &mut self,
        store: Option<&SessionStore>,
        session: &mut ChatSession,
        operation_id: &str,
        kind: &'static str,
        text: &str,
    ) -> Result<(), String> {
        let Some(store) = store else {
            return Ok(());
        };
        let continues = self.pending.as_ref().is_some_and(|pending| {
            pending.operation_id == operation_id
                && pending.kind == kind
                && pending.text.len().saturating_add(text.len()) <= STREAM_TRACE_BUFFER_BYTES
        });
        if continues {
            if let Some(pending) = &mut self.pending {
                pending.text.push_str(text);
            }
            return Ok(());
        }
        self.flush(Some(store), session)?;
        if text.len() > STREAM_TRACE_BUFFER_BYTES {
            return store
                .append_trace(session, operation_id, kind, json!({"text": text}))
                .map(|_| ())
                .map_err(|error| error.to_string());
        }
        self.pending = Some(PendingStreamTrace {
            operation_id: operation_id.to_string(),
            kind,
            text: text.to_string(),
        });
        Ok(())
    }

    fn flush(
        &mut self,
        store: Option<&SessionStore>,
        session: &mut ChatSession,
    ) -> Result<(), String> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let Some(store) = store else {
            return Ok(());
        };
        store
            .append_trace(
                session,
                &pending.operation_id,
                pending.kind,
                json!({"text": pending.text}),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Args, Clone, Debug)]
pub(crate) struct ChatArgs {
    /// Send one message and exit. Without this option Agul opens the workbench.
    #[arg(long, help_heading = "Input")]
    pub(crate) prompt: Option<String>,

    /// Directory the agent works in. Defaults to the current directory.
    #[arg(long, default_value = ".", help_heading = "Workspace")]
    pub(crate) workspace: PathBuf,

    /// Use this Agulater launch file instead of discovering .agents/runtime/launch.json.
    #[arg(long, help_heading = "Workspace")]
    pub(crate) launch: Option<PathBuf>,

    /// Execution engine. Native keeps Agul's four-tool loop; Codex uses ChatGPT quota.
    #[arg(long, value_enum, help_heading = "Model")]
    engine: Option<EngineArg>,

    /// Codex executable used by the account engine.
    #[arg(long, env = "AGUL_CODEX_COMMAND", help_heading = "Model")]
    codex_command: Option<String>,

    /// Native connection: `deepseek` (API billing) or `glm` (GLM Coding Plan).
    #[arg(long, env = "AGUL_PROVIDER", help_heading = "Model")]
    provider: Option<NativeConnectionPreset>,

    /// Native engine: OpenAI-compatible base URL or full chat/completions URL.
    #[arg(long, env = "AGUL_BASE_URL", help_heading = "Model")]
    pub(crate) base_url: Option<String>,

    /// Model name sent to the selected engine. Native and Codex have separate env defaults.
    #[arg(long, help_heading = "Model")]
    pub(crate) model: Option<String>,

    /// Native engine: API-key environment variable; the provider preset supplies its default.
    #[arg(long, help_heading = "Model")]
    pub(crate) api_key_env: Option<String>,

    /// Provider reasoning effort, when supported.
    #[arg(long, help_heading = "Model")]
    pub(crate) reasoning_effort: Option<String>,

    /// Native engine: versioned JSON price card. Official provider cards are built in.
    #[arg(long, help_heading = "Model")]
    pub(crate) price_card: Option<PathBuf>,

    /// Continue a saved chat by session ID.
    #[arg(
        long,
        help_heading = "Sessions",
        conflicts_with_all = ["no_session", "continue_session", "resume"]
    )]
    pub(crate) session: Option<String>,

    /// Continue the most recent chat in the selected workspace.
    #[arg(
        long = "continue",
        help_heading = "Sessions",
        conflicts_with_all = ["no_session", "session", "resume"]
    )]
    pub(crate) continue_session: bool,

    /// Choose a previous chat from the selected workspace.
    #[arg(
        long,
        help_heading = "Sessions",
        conflicts_with_all = ["no_session", "session", "continue_session", "prompt", "json"]
    )]
    pub(crate) resume: bool,

    /// Do not persist visible turns or the usage ledger.
    #[arg(long, help_heading = "Sessions")]
    pub(crate) no_session: bool,

    /// Override the directory containing saved sessions and usage ledgers.
    #[arg(long, help_heading = "Sessions")]
    pub(crate) state_dir: Option<PathBuf>,

    /// Native engine: maximum model responses in one turn.
    #[arg(long, default_value_t = DEFAULT_MAX_ROUNDS, help_heading = "Limits")]
    pub(crate) max_rounds: u32,

    /// Native engine: maximum tool calls in one turn.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_TOOL_CALLS,
        help_heading = "Limits"
    )]
    pub(crate) max_tool_calls: u32,

    /// Native engine: maximum output tokens requested from the provider.
    #[arg(
        long,
        env = "AGUL_MAX_TOKENS",
        default_value_t = DEFAULT_MAX_TOKENS,
        help_heading = "Limits"
    )]
    pub(crate) max_tokens: u32,

    /// Native engine: known context window used to fit the initial response budget.
    #[arg(long, env = "AGUL_CONTEXT_WINDOW", help_heading = "Limits")]
    pub(crate) context_window: Option<u32>,

    /// Native provider-request timeout or total Codex turn timeout, in seconds.
    #[arg(
        long,
        env = "AGUL_TIMEOUT_SECONDS",
        default_value_t = DEFAULT_TIMEOUT_SECONDS,
        help_heading = "Limits"
    )]
    pub(crate) timeout_seconds: u64,

    /// Keep provider reasoning out of interactive output.
    #[arg(long, help_heading = "Output")]
    pub(crate) hide_reasoning: bool,

    /// Disable terminal colors.
    #[arg(long, help_heading = "Output")]
    pub(crate) no_color: bool,

    /// Emit one JSON result. Requires --prompt.
    #[arg(long, requires = "prompt", help_heading = "Output")]
    pub(crate) json: bool,
}

impl Default for ChatArgs {
    fn default() -> Self {
        Self {
            prompt: None,
            workspace: PathBuf::from("."),
            launch: None,
            engine: None,
            codex_command: None,
            provider: None,
            base_url: None,
            model: None,
            api_key_env: None,
            reasoning_effort: None,
            price_card: None,
            session: None,
            continue_session: false,
            resume: false,
            no_session: false,
            state_dir: None,
            max_rounds: DEFAULT_MAX_ROUNDS,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            max_tokens: DEFAULT_MAX_TOKENS,
            context_window: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            hide_reasoning: false,
            no_color: false,
            json: false,
        }
    }
}

pub(crate) struct ChatCommandResult {
    pub(crate) exit_code: u8,
}

#[derive(Debug)]
struct JsonErrorReported;

impl std::fmt::Display for JsonErrorReported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("chat failed; details were emitted as JSON")
    }
}

impl std::error::Error for JsonErrorReported {}

pub(crate) fn run(args: &ChatArgs) -> ChatCommandResult {
    match run_chat(args) {
        Ok(()) => ChatCommandResult { exit_code: 0 },
        Err(error) => {
            if args.json && !error.is::<JsonErrorReported>() {
                println!("{}", json!({"ok": false, "error": error.to_string()}));
            } else if !args.json {
                eprintln!("! {}", plain_text(&error.to_string()));
            }
            ChatCommandResult { exit_code: 1 }
        }
    }
}

fn run_chat(args: &ChatArgs) -> Result<(), Box<dyn std::error::Error>> {
    let terminal_available = io::stdin().is_terminal() && io::stdout().is_terminal();
    let interactive = args.prompt.is_none() && !args.json && terminal_available;
    if args.resume && !terminal_available {
        return Err(
            "--resume needs an interactive terminal; use --continue or --session <id>".into(),
        );
    }
    if args.prompt.is_none() && !interactive {
        return Err("no interactive terminal; use --prompt".into());
    }
    let state_dir = args
        .state_dir
        .as_deref()
        .map(std::path::absolute)
        .transpose()?;
    let store = if args.no_session {
        None
    } else {
        Some(SessionStore::discover(state_dir.as_deref())?)
    };
    let requested_session = match resolve_session_request(args, store.as_ref())? {
        SessionRequest::New => None,
        SessionRequest::Resume(id) => Some(id),
        SessionRequest::Cancelled => return Ok(()),
    };
    let requested_engine = args.engine.map(SessionEngine::from);
    let stored_engine = match (&store, requested_session.as_deref()) {
        (Some(store), Some(id)) => Some(store.session_engine(id)?),
        _ => None,
    };
    let resumed_session_lease = match (&store, requested_session.as_deref()) {
        (Some(store), Some(id)) => Some(store.lease_session(id)?),
        _ => None,
    };
    if let (Some(requested), Some(stored)) = (requested_engine, stored_engine)
        && requested != stored
    {
        return Err(format!(
            "session {} uses the {} engine, not {}",
            requested_session.as_deref().unwrap_or_default(),
            stored,
            requested
        )
        .into());
    }
    let engine = stored_engine
        .or(requested_engine)
        .unwrap_or(SessionEngine::Native);
    let stored_native_config = match (&store, requested_session.as_deref(), engine) {
        (Some(store), Some(id), SessionEngine::Native) => Some(
            store
                .session_native_config(id)?
                .ok_or_else(|| format!("native session {id} has no saved provider connection"))?,
        ),
        _ => None,
    };
    let native = if engine == SessionEngine::Native {
        Some(NativeConnection::resolve(
            args,
            stored_native_config.as_ref(),
        )?)
    } else {
        None
    };
    let PriceSelection { catalog, notice } = if engine == SessionEngine::Codex {
        PriceSelection {
            catalog: None,
            notice: None,
        }
    } else {
        load_price_catalog(args, native.as_ref().expect("native connection"))?
    };
    let loaded = match (&store, requested_session.as_deref()) {
        (Some(store), Some(id)) => Some(store.load(id, catalog.clone())?),
        _ => None,
    };
    if engine == SessionEngine::Native
        && let Some(notice) = notice
    {
        eprintln!("· {}", plain_text(&notice));
    }
    let resumed = loaded.is_some();
    let (project, mut session) = match loaded {
        Some(session) => {
            let project = Project::discover(&session.workspace, args.launch.as_deref())?;
            (project, session)
        }
        None => {
            let project = Project::discover(&args.workspace, args.launch.as_deref())?;
            let model = engine_model(args, engine).unwrap_or_else(|| match engine {
                SessionEngine::Native => native
                    .as_ref()
                    .expect("native connection")
                    .default_model
                    .to_string(),
                SessionEngine::Codex => "codex".to_string(),
            });
            let session = match engine {
                SessionEngine::Native => {
                    ChatSession::new(project.workspace.clone(), model, catalog)
                }
                SessionEngine::Codex => {
                    ChatSession::new_codex(project.workspace.clone(), model, "pending")
                }
            };
            (project, session)
        }
    };
    session.resume();
    session.set_native_config(native.as_ref().map(NativeConnection::session_config));
    for command in &project.plugin_commands {
        let slash_name = format!("/{}", command.name);
        if CHAT_COMMANDS
            .iter()
            .any(|(builtin, _, _)| *builtin == slash_name)
        {
            return Err(
                format!("plugin command conflicts with built-in command: {slash_name}").into(),
            );
        }
    }
    let mut chat = match engine {
        SessionEngine::Native => ChatEngine::native(
            &project,
            ChatConfig {
                provider: ProviderConfig {
                    base_url: native.as_ref().expect("native connection").base_url.clone(),
                    provider: native.as_ref().expect("native connection").provider,
                    model: session.model.clone(),
                    api_key_env: native
                        .as_ref()
                        .expect("native connection")
                        .api_key_env
                        .clone(),
                    reasoning_effort: native
                        .as_ref()
                        .expect("native connection")
                        .reasoning_effort
                        .clone(),
                    max_tokens: args.max_tokens,
                    context_window: args.context_window.or_else(|| {
                        native
                            .as_ref()
                            .expect("native connection")
                            .known_context_window(&session.model)
                    }),
                    timeout: Duration::from_secs(args.timeout_seconds.max(1)),
                },
                max_rounds: args.max_rounds,
                max_tool_calls: args.max_tool_calls,
            },
            &session.id,
            state_dir.clone(),
        )?,
        SessionEngine::Codex => {
            session.replace_price_catalog(None);
            let requested_model = if resumed {
                Some(session.model.clone())
            } else {
                engine_model(args, engine)
            };
            let resume_thread_id = if resumed {
                Some(session.upstream_thread_id.clone().ok_or_else(|| {
                    format!("Codex session {} has no upstream thread", session.id)
                })?)
            } else {
                None
            };
            ChatEngine::codex(
                &project,
                CodexChatConfig {
                    command: args.codex_command.clone(),
                    model: requested_model,
                    reasoning_effort: args.reasoning_effort.clone(),
                    resume_thread_id,
                    ephemeral: args.no_session,
                    timeout: Duration::from_secs(args.timeout_seconds.max(1)),
                },
            )?
        }
    };
    session.model = chat.model().to_string();
    session.engine = engine;
    session.set_upstream_thread_id(chat.thread_id());
    restore_chat(&mut chat, &session, store.as_ref(), None);
    let _session_lease = match (resumed_session_lease, store.as_ref()) {
        (Some(lease), _) => Some(lease),
        (None, Some(store)) => Some(store.lease_session(&session.id)?),
        (None, None) => None,
    };
    if let Some(store) = &store {
        store.save(&session)?;
    }

    let rich = interactive;
    let color = rich && !args.no_color && env::var_os("NO_COLOR").is_none();
    let references = project
        .skill_summaries()
        .into_iter()
        .map(|(name, description)| TriggerCandidate::new(format!("@skill:{name}"), description))
        .chain(project.plugin_commands.iter().map(|command| {
            TriggerCandidate::new(format!("/{}", command.name), command.description.clone())
        }))
        .collect();
    let identity = if engine == SessionEngine::Codex {
        ProviderIdentity::codex_subscription()
    } else {
        ProviderIdentity::from_native_preset(
            session
                .native_config()
                .and_then(|connection| connection.preset),
            session
                .native_config()
                .and_then(|connection| connection.provider),
            chat.endpoint(),
        )
    };
    let mut initial_status = WorkbenchStatus::default();
    initialize_status(
        &mut initial_status,
        chat.model(),
        chat.reasoning_effort().or(args.reasoning_effort.as_deref()),
        chat.context_window(),
    );
    if resumed {
        restore_last_completed_turn_status(
            &mut initial_status,
            &session,
            store.as_ref(),
            &identity,
        );
    }
    let input = InteractiveInput::install(rich, color, references, initial_status.clone())?;
    let mut presenter = Presenter::new(
        color,
        args.json,
        args.hide_reasoning,
        initial_status,
        input.printer(),
    );
    if interactive {
        if resumed {
            replay_resumed_transcript(&mut presenter, &session)?;
        } else {
            present_new_chat_intro(&mut presenter)?;
        }
    }

    if let Some(prompt) = &args.prompt {
        let (text, skills) = parse_user_text(prompt);
        let model_input =
            project.activate_skills(text.as_ref(), skills.iter().map(String::as_str))?;
        let outcome = match run_turn(
            &mut chat,
            &mut session,
            store.as_ref(),
            &identity,
            text.as_ref(),
            &model_input,
            &mut presenter,
            &TurnCancellation::default(),
        ) {
            Ok(outcome) => outcome,
            Err(RunTurnError::Failed(error)) if args.json => {
                session.set_status(SessionStatus::Failed);
                if let Some(store) = &store {
                    let _ = store.save(&session);
                }
                let summary = aggregate_usage(&session, store.as_ref());
                let stored_session_id = store.as_ref().map(|_| session.id.as_str());
                let resume = match stored_session_id {
                    Some(session_id) => format!(
                        "Workspace changes were preserved. Continue with --session {session_id} and ask Agul to inspect the current worktree before proceeding."
                    ),
                    None => "Workspace changes were preserved. Start a new chat and ask Agul to inspect the current worktree before proceeding."
                        .to_string(),
                };
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "ok": false,
                        "error": error.to_string(),
                        "session_id": stored_session_id,
                        "billing": billing_label(&identity, &summary),
                        "rounds": error.model_rounds(),
                        "tool_calls": error.tool_calls(),
                        "usage": {
                            "summary": summary,
                            "entries": session.usage.entries(),
                        },
                        "resume": resume,
                    }))?
                );
                return Err(JsonErrorReported.into());
            }
            Err(RunTurnError::Failed(error)) => return Err(error.into()),
            Err(RunTurnError::Stopped) => {
                return Err("one-shot turn stopped without an interactive controller".into());
            }
        };
        session.set_status(SessionStatus::Completed);
        if let Some(store) = &store {
            store.save(&session)?;
        }
        if args.json {
            let summary = aggregate_usage(&session, store.as_ref());
            let cost = visible_cost(&identity, &summary);
            let stored_session_id = store.as_ref().map(|_| session.id.as_str());
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "ok": true,
                    "session_id": stored_session_id,
                    "model": chat.model(),
                    "engine": session.engine,
                    "billing": billing_label(&identity, &summary),
                    "response": outcome.text,
                    "rounds": outcome.model_rounds,
                    "tool_calls": outcome.tool_calls,
                    "cost": cost,
                    "usage": {
                        "summary": summary,
                        "entries": session.usage.entries(),
                    },
                }))?
            );
        }
        return Ok(());
    }

    let mut queued_input = VecDeque::new();
    let mut exit_requested = false;
    while !exit_requested {
        let next = queued_input
            .pop_front()
            .map_or_else(|| input.read_message(), InteractiveRead::Line);
        match next {
            InteractiveRead::Line(line) => match parse_message(&line) {
                ParsedMessage::Empty => {}
                ParsedMessage::UnknownCommand(command) => {
                    let live =
                        run_live_operation(&input, &mut presenter, |cancellation, presenter| {
                            handle_plugin_command(
                                command,
                                &project,
                                &mut session,
                                store.as_ref(),
                                state_dir.as_deref(),
                                presenter,
                                cancellation,
                            )
                        })?;
                    apply_live_command(
                        live,
                        &mut queued_input,
                        &mut exit_requested,
                        &mut presenter,
                    );
                }
                ParsedMessage::Command(ChatCommand::Exit) => {
                    exit_requested = true;
                }
                ParsedMessage::Command(ChatCommand::Compact) => {
                    let live =
                        run_live_operation(&input, &mut presenter, |cancellation, presenter| {
                            compact(
                                &mut chat,
                                &mut session,
                                store.as_ref(),
                                &identity,
                                presenter,
                                cancellation,
                            )
                        })?;
                    apply_live_command(
                        live,
                        &mut queued_input,
                        &mut exit_requested,
                        &mut presenter,
                    );
                }
                ParsedMessage::Command(command) => {
                    if let Err(error) = handle_command(
                        command,
                        &project,
                        &mut chat,
                        &mut session,
                        store.as_ref(),
                        &identity,
                        &mut presenter,
                    ) {
                        presenter.failed();
                        presenter.warning(&plain_text(&error.to_string()));
                    }
                }
                ParsedMessage::UserText { text, skills } => {
                    let model_input = match project
                        .activate_skills(text.as_ref(), skills.iter().map(String::as_str))
                    {
                        Ok(input) => input,
                        Err(error) => {
                            presenter.warning(&plain_text(&error.to_string()));
                            continue;
                        }
                    };
                    let live = run_live_turn(
                        &input,
                        &mut chat,
                        &mut session,
                        store.as_ref(),
                        &identity,
                        text.as_ref(),
                        &model_input,
                        &mut presenter,
                    )?;
                    queued_input.extend(live.queued_input);
                    exit_requested = live.exit_requested;
                    match live.turn {
                        Ok(_) | Err(RunTurnError::Stopped) => {}
                        Err(RunTurnError::Failed(error))
                            if session.engine == SessionEngine::Codex =>
                        {
                            session.set_status(SessionStatus::Failed);
                            if let Some(store) = &store {
                                let _ = store.save(&session);
                            }
                            return Err(error.into());
                        }
                        Err(RunTurnError::Failed(error)) => {
                            presenter.failed();
                            presenter.warning(&plain_text(&error.to_string()));
                        }
                    }
                }
            },
            InteractiveRead::Interrupted => {}
            InteractiveRead::Eof => break,
            InteractiveRead::TurnFinished => {}
            InteractiveRead::Failed(error) => return Err(error.into()),
        }
    }
    session.set_status(closed_session_status(&session, exit_requested));
    if let Some(store) = &store {
        store.save(&session)?;
    }
    input.close()?;
    display::goodbye(
        color,
        store.is_some(),
        store.is_some() && session.has_resumable_history(),
    )?;
    Ok(())
}

fn replay_resumed_transcript(presenter: &mut Presenter, session: &ChatSession) -> io::Result<()> {
    print_transcript_entries(
        presenter,
        resumed_transcript_entries(session, presenter.color),
    )
}

fn present_new_chat_intro(presenter: &mut Presenter) -> io::Result<()> {
    print_transcript_entries(presenter, new_chat_intro_entries())
}

fn print_transcript_entries(
    presenter: &mut Presenter,
    entries: impl IntoIterator<Item = TranscriptLine>,
) -> io::Result<()> {
    for line in entries {
        let (kind, content) = line.into_parts();
        presenter.output.print_text_line_as(kind, content)?;
    }
    Ok(())
}

fn new_chat_intro_entries() -> Vec<TranscriptLine> {
    let mut title = TranscriptText::default();
    title.push_safe("agul", TranscriptStyle::tone(TranscriptTone::Label).bold());
    title.push_safe(
        concat!(" v", env!("CARGO_PKG_VERSION")),
        TranscriptStyle::tone(TranscriptTone::Muted),
    );

    let mut controls = TranscriptText::default();
    controls.push_safe("/", TranscriptStyle::tone(TranscriptTone::Accent).bold());
    controls.push_safe(" commands · ", TranscriptStyle::tone(TranscriptTone::Muted));
    controls.push_safe("@", TranscriptStyle::tone(TranscriptTone::Accent).bold());
    controls.push_safe(
        " skills · type while running to steer",
        TranscriptStyle::tone(TranscriptTone::Muted),
    );

    vec![
        TranscriptLine::styled(TranscriptKind::Activity, title),
        TranscriptLine::styled(TranscriptKind::Activity, controls),
        TranscriptLine::new(TranscriptKind::Notice, ""),
        TranscriptLine::new(
            TranscriptKind::Activity,
            "A small terminal agent runtime for working directly in this workspace.",
        ),
    ]
}

fn resumed_transcript_entries(session: &ChatSession, styled: bool) -> Vec<TranscriptLine> {
    let start = resumed_transcript_start(session);
    let mut lines = Vec::new();
    if session.summarized_turns > 0 {
        lines.push(TranscriptLine::new(
            TranscriptKind::Notice,
            format!("↳ {} earlier turns compacted", session.summarized_turns),
        ));
    }
    if start > 0 {
        lines.push(TranscriptLine::new(
            TranscriptKind::Notice,
            format!("↳ {start} older visible turns folded"),
        ));
    }
    for turn in &session.turns[start..] {
        lines.extend(user_message_lines(submitted_lines(&turn.user)));
        push_group_gap(&mut lines, TranscriptKind::Assistant);
        let assistant = TerminalMarkdown::render_complete(&turn.assistant, styled);
        lines.extend(
            assistant
                .lines()
                .into_iter()
                .map(|line| TranscriptLine::styled(TranscriptKind::Assistant, line)),
        );
    }
    if let Some(pending) = session.pending_visible_user() {
        lines.extend(user_message_lines(submitted_lines(pending)));
    }
    if lines.last().is_some_and(|line| !line.text().is_empty()) {
        lines.push(TranscriptLine::new(TranscriptKind::Notice, ""));
    }
    lines.push(TranscriptLine::new(
        TranscriptKind::Notice,
        resumed_line(session),
    ));
    lines
}

#[cfg(test)]
fn resumed_transcript_lines(session: &ChatSession) -> Vec<String> {
    resumed_transcript_entries(session, false)
        .into_iter()
        .map(|line| line.text().to_string())
        .collect()
}

fn resumed_transcript_start(session: &ChatSession) -> usize {
    let mut start = session.turns.len();
    let mut bytes = 0usize;
    for (turns, (index, turn)) in session.turns.iter().enumerate().rev().enumerate() {
        let turn_bytes = turn.user.len().saturating_add(turn.assistant.len());
        if turns >= MAX_RESUMED_TRANSCRIPT_TURNS
            || (turns > 0 && bytes.saturating_add(turn_bytes) > MAX_RESUMED_TRANSCRIPT_BYTES)
        {
            break;
        }
        start = index;
        bytes = bytes.saturating_add(turn_bytes);
    }
    start
}

fn closed_session_status(session: &ChatSession, clean_exit: bool) -> SessionStatus {
    if session.status == SessionStatus::Failed {
        return SessionStatus::Failed;
    }
    if session.pending_model_input().is_none() {
        return SessionStatus::Completed;
    }
    if clean_exit {
        SessionStatus::Cancelled
    } else {
        SessionStatus::Interrupted
    }
}

struct LiveOperation<T> {
    result: Result<T, String>,
    queued_input: Vec<String>,
    exit_requested: bool,
    stop_requested: bool,
}

#[derive(Debug)]
enum LiveCommandError {
    Failed(String),
    Stopped,
}

fn apply_live_command(
    live: LiveOperation<Result<(), LiveCommandError>>,
    queued_input: &mut VecDeque<String>,
    exit_requested: &mut bool,
    presenter: &mut Presenter,
) {
    let LiveOperation {
        result,
        queued_input: pending,
        exit_requested: exit,
        stop_requested,
    } = live;
    queued_input.extend(pending);
    *exit_requested |= exit;
    let result = result.unwrap_or_else(|error| Err(LiveCommandError::Failed(error)));
    match result {
        Ok(()) => {
            if stop_requested {
                let _ = presenter.print_line("■ idle");
            }
        }
        Err(LiveCommandError::Stopped) => {}
        Err(LiveCommandError::Failed(error)) => {
            presenter.abort_response();
            presenter.failed();
            presenter.warning(&plain_text(&error));
        }
    }
}

struct WakeOnDrop(Option<TurnWake>);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        if let Some(wake) = self.0.take() {
            wake.finished();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveInputAction {
    Ignore,
    Stop,
    Exit,
    Steer,
}

fn classify_live_input(line: &str) -> LiveInputAction {
    match parse_message(line) {
        ParsedMessage::Empty => LiveInputAction::Ignore,
        ParsedMessage::Command(ChatCommand::Stop) => LiveInputAction::Stop,
        ParsedMessage::Command(ChatCommand::Exit) => LiveInputAction::Exit,
        _ => LiveInputAction::Steer,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_live_turn(
    input: &InteractiveInput,
    chat: &mut ChatEngine,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
    visible_input: &str,
    model_input: &str,
    presenter: &mut Presenter,
) -> io::Result<LiveTurn> {
    let live = run_live_operation(input, presenter, |cancellation, presenter| {
        run_turn(
            chat,
            session,
            store,
            identity,
            visible_input,
            model_input,
            presenter,
            cancellation,
        )
    })?;
    let LiveOperation {
        result,
        queued_input,
        exit_requested,
        stop_requested,
    } = live;
    let turn = result.unwrap_or_else(|error| {
        presenter.abort_response();
        Err(RunTurnError::Failed(ChatError::new(error)))
    });
    if stop_requested && turn.is_ok() {
        presenter.print_line("■ idle")?;
    }
    Ok(LiveTurn {
        turn,
        queued_input,
        exit_requested,
    })
}

struct LiveTurn {
    turn: Result<crate::runtime::TurnOutcome, RunTurnError>,
    queued_input: Vec<String>,
    exit_requested: bool,
}

fn run_live_operation<T, F>(
    input: &InteractiveInput,
    presenter: &mut Presenter,
    operation: F,
) -> io::Result<LiveOperation<T>>
where
    T: Send,
    F: FnOnce(&TurnCancellation, &mut Presenter) -> T + Send,
{
    let cancellation = TurnCancellation::default();
    let mut queued_input = Vec::new();
    let mut exit_requested = false;
    let mut stop_requested = false;
    let mut input_error = None;
    input.begin_live_turn();
    let wake = input.turn_wake();

    let turn = thread::scope(|scope| {
        let operation_cancellation = &cancellation;
        let worker_presenter = &mut *presenter;
        let worker = scope.spawn(move || {
            let _wake = WakeOnDrop(Some(wake));
            operation(operation_cancellation, worker_presenter)
        });

        loop {
            match input.read_live_message() {
                InteractiveRead::TurnFinished => {
                    break;
                }
                InteractiveRead::Line(line) => {
                    if apply_live_input(
                        line,
                        &mut queued_input,
                        &mut exit_requested,
                        &mut stop_requested,
                    ) {
                        cancellation.cancel();
                    }
                }
                InteractiveRead::Interrupted => {
                    queued_input.clear();
                    stop_requested = true;
                    cancellation.cancel();
                }
                InteractiveRead::Eof => {
                    queued_input.clear();
                    stop_requested = false;
                    cancellation.cancel();
                    exit_requested = true;
                }
                InteractiveRead::Failed(error) => {
                    cancellation.cancel();
                    input_error = Some(error);
                    break;
                }
            }
        }

        worker.join()
    });
    input.finish_live_turn();

    if let Some(error) = input_error {
        presenter.abort_response();
        return Err(error);
    }
    Ok(LiveOperation {
        result: turn.map_err(|_| "live operation worker panicked".to_string()),
        queued_input,
        exit_requested,
        stop_requested,
    })
}

fn apply_live_input(
    line: String,
    queued_input: &mut Vec<String>,
    exit_requested: &mut bool,
    stop_requested: &mut bool,
) -> bool {
    match classify_live_input(&line) {
        LiveInputAction::Ignore => false,
        LiveInputAction::Stop => {
            queued_input.clear();
            *stop_requested = true;
            true
        }
        LiveInputAction::Exit => {
            queued_input.clear();
            *exit_requested = true;
            *stop_requested = false;
            true
        }
        LiveInputAction::Steer => {
            if !*exit_requested {
                *stop_requested = false;
                queued_input.push(line);
            }
            true
        }
    }
}

fn engine_model(args: &ChatArgs, engine: SessionEngine) -> Option<String> {
    args.model.clone().or_else(|| {
        let variable = match engine {
            SessionEngine::Native => "AGUL_MODEL",
            SessionEngine::Codex => "AGUL_CODEX_MODEL",
        };
        env::var(variable)
            .ok()
            .filter(|model| !model.trim().is_empty())
    })
}

#[derive(Debug)]
struct NativeConnection {
    preset: Option<NativeConnectionPreset>,
    provider: Option<NativeProvider>,
    base_url: String,
    default_model: &'static str,
    api_key_env: Option<String>,
    reasoning_effort: Option<String>,
}

impl NativeConnection {
    fn resolve(args: &ChatArgs, stored: Option<&NativeSessionConfig>) -> Result<Self, String> {
        if let Some(stored) = stored {
            return Self::resume(args, stored);
        }
        let preset = args.provider.unwrap_or_default();
        let base_url = args
            .base_url
            .clone()
            .unwrap_or_else(|| preset.base_url().to_string());
        if let Some(preset) = args.provider {
            preset.validate_official_endpoint(&base_url)?;
        }
        let identity = ProviderIdentity::from_endpoint(&base_url);
        identity.validate_native_provider(args.provider.map(NativeConnectionPreset::provider))?;
        let inferred_preset = NativeConnectionPreset::from_official_endpoint(&base_url);
        let connection_preset = args.provider.or(inferred_preset);
        let provider = args
            .provider
            .map(NativeConnectionPreset::provider)
            .or_else(|| identity.native_provider());
        let default_model = connection_preset
            .map(NativeConnectionPreset::model)
            .or_else(|| provider.map(NativeProvider::model))
            .or_else(|| identity.default_model())
            .unwrap_or(DEEPSEEK_DEFAULT_MODEL);
        let api_key_env = args.api_key_env.clone().or_else(|| {
            connection_preset
                .map(|preset| preset.api_key_env().to_string())
                .or_else(|| provider.map(|provider| provider.api_key_env().to_string()))
                .or_else(|| identity.default_api_key_env())
        });
        let reasoning_effort =
            normalize_reasoning_effort(provider, args.reasoning_effort.as_deref())?;
        Ok(Self {
            preset: connection_preset,
            provider,
            base_url,
            default_model,
            api_key_env,
            reasoning_effort,
        })
    }

    fn resume(args: &ChatArgs, stored: &NativeSessionConfig) -> Result<Self, String> {
        let saved_preset = stored
            .preset
            .or_else(|| NativeConnectionPreset::from_official_endpoint(&stored.base_url));
        if let Some(requested) = args.provider
            && saved_preset
                .map(|saved| saved != requested)
                .unwrap_or_else(|| Some(requested.provider()) != stored.provider)
        {
            return Err(format!(
                "--provider {requested} conflicts with the saved provider {}",
                display_provider(saved_preset, stored.provider)
            ));
        }
        if let Some(requested) = args.base_url.as_deref() {
            if let Some(preset) = saved_preset.or(args.provider) {
                preset.validate_official_endpoint(requested)?;
            }
            ProviderIdentity::from_endpoint(requested).validate_native_provider(stored.provider)?;
        }
        let preset = saved_preset.or(args.provider);
        let api_key_env = args
            .api_key_env
            .clone()
            .or_else(|| stored.api_key_env.clone());
        let reasoning_effort = if args.reasoning_effort.is_some() {
            normalize_reasoning_effort(stored.provider, args.reasoning_effort.as_deref())?
        } else {
            normalize_reasoning_effort(stored.provider, stored.reasoning_effort.as_deref())?
        };
        let base_url = args
            .base_url
            .clone()
            .unwrap_or_else(|| stored.base_url.clone());
        let identity = ProviderIdentity::from_endpoint(&base_url);
        let default_model = preset
            .map(NativeConnectionPreset::model)
            .or_else(|| stored.provider.map(NativeProvider::model))
            .or_else(|| identity.default_model())
            .unwrap_or(DEEPSEEK_DEFAULT_MODEL);
        Ok(Self {
            preset,
            provider: stored.provider,
            base_url,
            default_model,
            api_key_env,
            reasoning_effort,
        })
    }

    fn session_config(&self) -> NativeSessionConfig {
        NativeSessionConfig {
            preset: self.preset,
            provider: self.provider,
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    fn known_context_window(&self, model: &str) -> Option<u32> {
        let official_deepseek = NativeConnectionPreset::from_official_endpoint(&self.base_url)
            == Some(NativeConnectionPreset::Deepseek);
        (official_deepseek && matches!(model, "deepseek-v4-flash" | "deepseek-v4-pro"))
            .then_some(DEEPSEEK_V4_CONTEXT_WINDOW)
    }
}

fn normalize_reasoning_effort(
    provider: Option<NativeProvider>,
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    match provider {
        Some(provider) => provider.normalize_reasoning_effort(requested),
        None => Ok(requested
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string)),
    }
}

fn display_provider(
    preset: Option<NativeConnectionPreset>,
    provider: Option<NativeProvider>,
) -> String {
    preset
        .map(|preset| preset.to_string())
        .or_else(|| provider.map(|provider| provider.to_string()))
        .unwrap_or_else(|| "openai-compatible".to_string())
}

#[derive(Debug)]
enum RunTurnError {
    Failed(ChatError),
    Stopped,
}

#[allow(clippy::too_many_arguments)]
fn run_turn(
    chat: &mut ChatEngine,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
    visible_input: &str,
    model_input: &str,
    presenter: &mut Presenter,
    cancellation: &TurnCancellation,
) -> Result<crate::runtime::TurnOutcome, RunTurnError> {
    presenter.thinking();
    let usage_start = session.usage.entries().len();
    let related_start = session.related_sessions.len();
    let mut persistence_warning = None;
    session.begin_turn(
        visible_input.to_string(),
        model_input.to_string(),
        chat.native_history(),
    );
    let fallback_operation_id = next_operation_id("turn");
    let operation_id = store
        .and_then(|store| {
            match store.begin_trace_operation(session, "turn", json!({"input": visible_input})) {
                Ok(operation_id) => Some(operation_id),
                Err(error) => {
                    persistence_warning = Some(error.to_string());
                    None
                }
            }
        })
        .unwrap_or(fallback_operation_id);
    let mut trace_buffer = StreamTraceBuffer::default();
    if let Some(store) = store
        && let Err(error) = store.save(session)
    {
        persistence_warning = Some(error.to_string());
    }
    let outcome = match chat.send_cancellable(model_input.to_string(), cancellation, &mut |event| {
        if let Err(error) = record_chat_event(
            &event,
            session,
            store,
            identity,
            UsagePurpose::Chat,
            &operation_id,
            &mut trace_buffer,
        ) && persistence_warning.is_none()
        {
            persistence_warning = Some(error);
        }
        if cancellation.is_cancelled() {
            return Err(ChatError::new("turn stopped by user"));
        }
        presenter.event(event)
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            let stopped = cancellation.is_cancelled();
            presenter.abort_response();
            session.set_status(if stopped {
                SessionStatus::Active
            } else {
                SessionStatus::Failed
            });
            if let Err(trace_error) = trace_buffer.flush(store, session)
                && persistence_warning.is_none()
            {
                persistence_warning = Some(trace_error);
            }
            if stopped
                && let Some(store) = store
                && let Err(error) = store.settle_interrupted_related_sessions(
                    &session.id,
                    &session.related_sessions[related_start..],
                )
                && persistence_warning.is_none()
            {
                persistence_warning = Some(error.to_string());
            }
            if let Some(store) = store {
                let _ = store.append_trace(
                    session,
                    &operation_id,
                    if stopped {
                        "operation_stopped"
                    } else {
                        "operation_failed"
                    },
                    if stopped {
                        json!({"reason": "user"})
                    } else {
                        json!({"error": error.to_string()})
                    },
                );
                let _ = store.save(session);
            }
            if let Some(warning) = persistence_warning {
                presenter.warning(&format!("session: {}", plain_text(&warning)));
            }
            if stopped {
                presenter.stopped();
                restore_chat(chat, session, store, Some(&operation_id));
                presenter.update_status(None, identity);
                return Err(RunTurnError::Stopped);
            }
            return Err(RunTurnError::Failed(error));
        }
    };
    if let Err(error) = trace_buffer.flush(store, session)
        && persistence_warning.is_none()
    {
        persistence_warning = Some(error);
    }
    presenter.finish(&outcome);
    // Keep the state mutation outside debug_assert: release builds elide its expression.
    let finished = session.finish_turn(outcome.text.clone(), chat.native_history());
    debug_assert!(finished);
    session.capture_handoff(&outcome.text);
    if let Some(store) = store {
        if let Err(error) = store.append_trace(
            session,
            &operation_id,
            "operation_completed",
            json!({
                "model_rounds": outcome.model_rounds,
                "tool_calls": outcome.tool_calls,
                "elapsed_ms": elapsed_millis(outcome.elapsed),
            }),
        ) && persistence_warning.is_none()
        {
            persistence_warning = Some(error.to_string());
        }
        if let Err(error) = store.save(session) {
            persistence_warning = Some(error.to_string());
        }
    }
    let request_usage = summarize_request_usage(session, usage_start);
    presenter.update_status(Some((&outcome, &request_usage)), identity);
    if let Some(warning) = persistence_warning {
        presenter.warning(&format!("session: {}", plain_text(&warning)));
    }
    Ok(outcome)
}

struct WorkbenchRequestUsage {
    summary: UsageSummary,
    latest_cache_hit_percent: Option<f64>,
    latest_input_tokens: Option<u64>,
}

impl WorkbenchRequestUsage {
    fn from_entries(entries: &[UsageEntry]) -> Self {
        let latest = entries.last();
        Self {
            summary: UsageSummary::from_entries(entries),
            latest_cache_hit_percent: latest.and_then(|entry| {
                UsageSummary::from_entries(std::slice::from_ref(entry)).reported_cache_hit_percent()
            }),
            latest_input_tokens: latest.and_then(|entry| entry.input_tokens),
        }
    }
}

fn summarize_request_usage(session: &ChatSession, usage_start: usize) -> WorkbenchRequestUsage {
    WorkbenchRequestUsage::from_entries(&session.usage.entries()[usage_start..])
}

fn record_chat_event(
    event: &ChatEvent<'_>,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
    purpose: UsagePurpose,
    operation_id: &str,
    trace_buffer: &mut StreamTraceBuffer,
) -> Result<(), String> {
    match event {
        ChatEvent::Reasoning(text) => {
            return trace_buffer.push(store, session, operation_id, "reasoning", text);
        }
        ChatEvent::Text(text) => {
            return trace_buffer.push(store, session, operation_id, "text", text);
        }
        _ => {}
    }
    let mut persistence_error = trace_buffer.flush(store, session).err();
    let mut save_session = false;
    let (kind, data) = match event {
        ChatEvent::ToolStarted { name, detail } => {
            ("tool_started", json!({"name": name, "detail": detail}))
        }
        ChatEvent::ToolFinished {
            name,
            detail,
            ok,
            elapsed,
        } => (
            "tool_finished",
            json!({
                "name": name,
                "detail": detail,
                "ok": ok,
                "elapsed_ms": elapsed_millis(*elapsed),
            }),
        ),
        ChatEvent::ToolProgress {
            call_id,
            seq,
            task_id,
            stage,
            preview,
        } => (
            "tool_progress",
            json!({
                "call_id": call_id,
                "seq": seq,
                "task_id": task_id,
                "stage": stage,
                "preview": preview,
            }),
        ),
        ChatEvent::RelatedSession {
            call_id,
            seq,
            relation,
            session_id,
            delegation_id,
            task_id,
        } => {
            session.add_related_session(RelatedSession {
                relation: (*relation).to_string(),
                session_id: (*session_id).to_string(),
                delegation_id: Some((*delegation_id).to_string()),
                task_id: Some((*task_id).to_string()),
            });
            save_session = true;
            (
                "related_session",
                json!({
                    "call_id": call_id,
                    "seq": seq,
                    "relation": relation,
                    "session_id": session_id,
                    "delegation_id": delegation_id,
                    "task_id": task_id,
                }),
            )
        }
        ChatEvent::Response(response) => {
            let entry = identity
                .record(&mut session.usage, purpose, response)
                .clone();
            save_session = true;
            ("usage", json!({"entry": entry}))
        }
        ChatEvent::Reasoning(_) | ChatEvent::Text(_) => unreachable!(),
    };
    if let Some(store) = store {
        if let Err(error) = store.append_trace(session, operation_id, kind, data)
            && persistence_error.is_none()
        {
            persistence_error = Some(error.to_string());
        }
        if save_session
            && let Err(error) = store.save(session)
            && persistence_error.is_none()
        {
            persistence_error = Some(error.to_string());
        }
    }
    persistence_error.map_or(Ok(()), Err)
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn next_operation_id(kind: &str) -> String {
    format!(
        "{kind}-{}-{}",
        std::process::id(),
        PLUGIN_COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn stop_live_session_operation(
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    operation_id: &str,
    related_start: usize,
    mut persistence_warning: Option<String>,
    presenter: &mut Presenter,
) -> LiveCommandError {
    session.set_status(SessionStatus::Active);
    if let Some(store) = store {
        if let Err(error) = store.settle_interrupted_related_sessions(
            &session.id,
            &session.related_sessions[related_start..],
        ) && persistence_warning.is_none()
        {
            persistence_warning = Some(error.to_string());
        }
        let _ = store.append_trace(
            session,
            operation_id,
            "operation_stopped",
            json!({"reason": "user"}),
        );
        let _ = store.save(session);
    }
    if let Some(warning) = persistence_warning {
        presenter.warning(&format!("session: {}", plain_text(&warning)));
    }
    presenter.stopped();
    LiveCommandError::Stopped
}

fn handle_plugin_command(
    raw: &str,
    project: &Project,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    state_dir: Option<&Path>,
    presenter: &mut Presenter,
    cancellation: &TurnCancellation,
) -> Result<(), LiveCommandError> {
    let body = raw
        .strip_prefix('/')
        .ok_or_else(|| LiveCommandError::Failed(format!("unknown command: {raw}")))?;
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arguments = parts.next().map(str::trim).unwrap_or_default();
    let command = project
        .plugin_commands
        .iter()
        .find(|command| command.name == name)
        .ok_or_else(|| LiveCommandError::Failed(format!("unknown command: {raw}")))?;

    session.resume();
    presenter
        .plugin_command_started(name, command.detail())
        .map_err(|error| LiveCommandError::Failed(error.to_string()))?;
    let mut persistence_warning = None;
    let fallback_call_id = next_operation_id("command");
    let call_id = match store {
        Some(store) => match store.begin_trace_operation(
            session,
            "command",
            json!({"command": name, "arguments": arguments}),
        ) {
            Ok(operation_id) => operation_id,
            Err(error) => {
                persistence_warning = Some(error.to_string());
                fallback_call_id
            }
        },
        None => fallback_call_id,
    };
    let session_id = session.id.clone();
    let related_start = session.related_sessions.len();
    let context = PluginCallContext {
        call_id: &call_id,
        session_id: &session_id,
        workspace: &project.workspace,
        launch_path: project.launch.as_ref().map(|launch| launch.path.as_path()),
        state_dir,
    };
    let terminal = command.execute_cancellable(arguments, &context, cancellation, &mut |event| {
        if cancellation.is_cancelled() {
            return Err("plugin command cancelled".to_string());
        }
        if let Err(error) = record_plugin_event(&event, session, store, &call_id)
            && persistence_warning.is_none()
        {
            persistence_warning = Some(error);
        }
        if cancellation.is_cancelled() {
            return Err("plugin command cancelled".to_string());
        }
        presenter
            .plugin_event(&event)
            .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            return Err("plugin command cancelled".to_string());
        }
        Ok::<(), String>(())
    });
    let terminal = match terminal {
        Ok(terminal) => terminal,
        Err(PluginExecutionError::Plugin(error) | PluginExecutionError::Event(error)) => {
            if cancellation.is_cancelled() {
                return Err(stop_live_session_operation(
                    session,
                    store,
                    &call_id,
                    related_start,
                    persistence_warning,
                    presenter,
                ));
            }
            session.set_status(SessionStatus::Failed);
            if let Some(store) = store {
                let _ = store.append_trace(
                    session,
                    &call_id,
                    "operation_failed",
                    json!({"error": error}),
                );
                let _ = store.save(session);
            }
            if let Some(warning) = persistence_warning {
                presenter.warning(&format!("session: {}", plain_text(&warning)));
            }
            presenter.plugin_command_finished(false);
            return Err(LiveCommandError::Failed(error));
        }
    };
    if cancellation.is_cancelled() {
        return Err(stop_live_session_operation(
            session,
            store,
            &call_id,
            related_start,
            persistence_warning,
            presenter,
        ));
    }
    match terminal {
        PluginTerminal::Success(content) => {
            if let Some(store) = store {
                if let Err(error) = store.append_trace(
                    session,
                    &call_id,
                    "operation_completed",
                    json!({"content": &content}),
                ) && persistence_warning.is_none()
                {
                    persistence_warning = Some(error.to_string());
                }
                if let Err(error) = store.save(session)
                    && persistence_warning.is_none()
                {
                    persistence_warning = Some(error.to_string());
                }
            }
            presenter.plugin_command_finished(true);
            presenter
                .plugin_result(&content)
                .map_err(|error| LiveCommandError::Failed(error.to_string()))?;
            if let Some(warning) = persistence_warning {
                presenter.warning(&format!("session: {}", plain_text(&warning)));
            }
            Ok(())
        }
        PluginTerminal::Failure(error) => {
            session.set_status(SessionStatus::Failed);
            if let Some(store) = store {
                let _ = store.append_trace(
                    session,
                    &call_id,
                    "operation_failed",
                    json!({"error": &error}),
                );
                let _ = store.save(session);
            }
            presenter.plugin_command_finished(false);
            if let Some(warning) = persistence_warning {
                presenter.warning(&format!("session: {}", plain_text(&warning)));
            }
            Err(LiveCommandError::Failed(format!(
                "{}: {}",
                error.code, error.message
            )))
        }
    }
}

fn record_plugin_event(
    event: &PluginEvent,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    operation_id: &str,
) -> Result<(), String> {
    let (kind, data, save) = match event {
        PluginEvent::ToolProgress(progress) => (
            "tool_progress",
            json!({
                "call_id": progress.call_id,
                "seq": progress.seq,
                "task_id": progress.task_id,
                "stage": progress.stage,
                "preview": progress.preview,
            }),
            false,
        ),
        PluginEvent::RelatedSession(related) => {
            session.add_related_session(RelatedSession {
                relation: related.relation.clone(),
                session_id: related.session_id.clone(),
                delegation_id: Some(related.delegation_id.clone()),
                task_id: Some(related.task_id.clone()),
            });
            (
                "related_session",
                json!({
                    "call_id": related.call_id,
                    "seq": related.seq,
                    "relation": related.relation,
                    "session_id": related.session_id,
                    "delegation_id": related.delegation_id,
                    "task_id": related.task_id,
                }),
                true,
            )
        }
    };
    if let Some(store) = store {
        store
            .append_trace(session, operation_id, kind, data)
            .map_err(|error| error.to_string())?;
        if save {
            store.save(session).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn plugin_result_line(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::Null => "✓".to_string(),
        serde_json::Value::String(text) if text.is_empty() => "✓".to_string(),
        serde_json::Value::String(text) => format!("✓ {}", plain_text(text)),
        serde_json::Value::Object(object) => {
            if let Some(summary) = object.get("summary").and_then(serde_json::Value::as_str) {
                format!("✓ {}", plain_text(summary))
            } else if let Some(results) =
                object.get("results").and_then(serde_json::Value::as_array)
            {
                let status = object
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("completed");
                let mut tasks = results
                    .iter()
                    .filter_map(|result| {
                        let id = result.get("id")?.as_str()?;
                        let pool = result.get("pool").and_then(serde_json::Value::as_str);
                        Some(match pool {
                            Some(pool) => format!("{id}@{pool}"),
                            None => id.to_string(),
                        })
                    })
                    .take(3)
                    .collect::<Vec<_>>();
                if results.len() > tasks.len() {
                    tasks.push(format!("+{}", results.len() - tasks.len()));
                }
                let detail = if tasks.is_empty() {
                    format!("{} tasks", results.len())
                } else {
                    tasks.join(" · ")
                };
                workbench::truncate_cells(
                    &format!(
                        "✓ {} · {} tasks · {detail}",
                        plain_text(status),
                        results.len()
                    ),
                    140,
                )
            } else if let Some(status) = object.get("status").and_then(serde_json::Value::as_str) {
                format!("✓ {}", plain_text(status))
            } else {
                "✓ result".to_string()
            }
        }
        _ => "✓ result".to_string(),
    }
}

fn plugin_result_text(content: &serde_json::Value) -> TranscriptText {
    let line = plugin_result_line(content);
    let detail = line.strip_prefix('✓').unwrap_or(&line).trim_start();
    activity_text(
        "✓",
        TranscriptStyle::tone(TranscriptTone::Success),
        "",
        None,
        detail,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    command: ChatCommand,
    project: &Project,
    chat: &mut ChatEngine,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
    presenter: &mut Presenter,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ChatCommand::Help => {
            presenter.print_line(
                "/help · /status · /skills · /usage · /cost · /compact · /sessions · /clear · /stop · /exit"
            )?;
            for command in &project.plugin_commands {
                presenter.print_line(format!(
                    "/{} · {}",
                    command.name,
                    plain_text(&command.description)
                ))?;
            }
        }
        ChatCommand::Status => {
            presenter.print_line(presenter.status_line())?;
        }
        ChatCommand::Skills => {
            let skills = project.skill_summaries();
            if skills.is_empty() {
                presenter.print_line("@—")?;
            } else {
                for (name, description) in skills {
                    presenter
                        .print_line(format!("@skill:{name} · {}", plain_text(&description)))?;
                }
            }
        }
        ChatCommand::Usage => presenter.print_line(usage_line(session, store))?,
        ChatCommand::Cost => presenter.print_line(cost_line(session, store, identity))?,
        ChatCommand::Sessions => {
            for line in session_lines(store, session, identity.is_subscription())? {
                presenter.print_line(line)?;
            }
        }
        ChatCommand::Clear => {
            chat.reset()?;
            session.clear();
            session.set_native_history(chat.native_history());
            session.model = chat.model().to_string();
            session.set_upstream_thread_id(chat.thread_id());
            if let Some(store) = store {
                store.save(session)?;
            }
            presenter.status.model = chat.model().to_string();
            presenter.status.reasoning_effort =
                chat.reasoning_effort().unwrap_or("auto").to_string();
            presenter.status.reset_context();
            presenter.clear_transcript()?;
            presenter.update_status(None, identity);
            presenter.print_line("○")?;
        }
        ChatCommand::Compact => unreachable!("compact uses the live-operation controller"),
        ChatCommand::Stop => presenter.print_line("■ idle")?,
        ChatCommand::Exit => unreachable!(),
    }
    Ok(())
}

fn compact(
    chat: &mut ChatEngine,
    session: &mut ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
    presenter: &mut Presenter,
    cancellation: &TurnCancellation,
) -> Result<(), LiveCommandError> {
    let source = session.compaction_source(RETAIN_AFTER_COMPACTION).to_vec();
    if source.is_empty() {
        presenter
            .print_line("0 turns")
            .map_err(|error| LiveCommandError::Failed(error.to_string()))?;
        return Ok(());
    }
    presenter.thinking();
    let mut persistence_warning = None;
    let fallback_operation_id = next_operation_id("compact");
    let operation_id = store
        .and_then(|store| {
            match store.begin_trace_operation(
                session,
                "compact",
                json!({"visible_turns": source.len()}),
            ) {
                Ok(operation_id) => Some(operation_id),
                Err(error) => {
                    persistence_warning = Some(error.to_string());
                    None
                }
            }
        })
        .unwrap_or(fallback_operation_id);
    let mut trace_buffer = StreamTraceBuffer::default();
    let outcome = match chat.compact_cancellable(&source, cancellation, &mut |event| {
        if cancellation.is_cancelled() && !matches!(&event, ChatEvent::Response(_)) {
            return Err(ChatError::new("compaction stopped by user"));
        }
        if let Err(error) = record_chat_event(
            &event,
            session,
            store,
            identity,
            UsagePurpose::Compaction,
            &operation_id,
            &mut trace_buffer,
        ) && persistence_warning.is_none()
        {
            persistence_warning = Some(error);
        }
        if cancellation.is_cancelled() {
            return Err(ChatError::new("compaction stopped by user"));
        }
        present_compaction_event(presenter, event)?;
        if cancellation.is_cancelled() {
            return Err(ChatError::new("compaction stopped by user"));
        }
        Ok(())
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            let stopped = cancellation.is_cancelled();
            presenter.abort_response();
            if let Err(trace_error) = trace_buffer.flush(store, session)
                && persistence_warning.is_none()
            {
                persistence_warning = Some(trace_error);
            }
            if stopped {
                return Err(stop_live_session_operation(
                    session,
                    store,
                    &operation_id,
                    session.related_sessions.len(),
                    persistence_warning,
                    presenter,
                ));
            }
            session.set_status(SessionStatus::Failed);
            if let Some(store) = store {
                let _ = store.append_trace(
                    session,
                    &operation_id,
                    "operation_failed",
                    json!({"error": error.to_string()}),
                );
                let _ = store.save(session);
            }
            if let Some(warning) = persistence_warning {
                presenter.warning(&format!("session: {}", plain_text(&warning)));
            }
            return Err(LiveCommandError::Failed(error.to_string()));
        }
    };
    if cancellation.is_cancelled() {
        if let Err(error) = trace_buffer.flush(store, session)
            && persistence_warning.is_none()
        {
            persistence_warning = Some(error);
        }
        return Err(stop_live_session_operation(
            session,
            store,
            &operation_id,
            session.related_sessions.len(),
            persistence_warning,
            presenter,
        ));
    }
    if let Err(error) = trace_buffer.flush(store, session)
        && persistence_warning.is_none()
    {
        persistence_warning = Some(error);
    }
    let count = session.commit_compaction(RETAIN_AFTER_COMPACTION, outcome.summary);
    restore_chat(chat, session, store, None);
    session.set_native_history(chat.native_history());
    if let Some(store) = store {
        if let Err(error) = store.append_trace(
            session,
            &operation_id,
            "operation_completed",
            json!({
                "compacted_turns": count,
                "elapsed_ms": elapsed_millis(outcome.elapsed),
            }),
        ) && persistence_warning.is_none()
        {
            persistence_warning = Some(error.to_string());
        }
        if let Err(error) = store.save(session)
            && persistence_warning.is_none()
        {
            persistence_warning = Some(error.to_string());
        }
    }
    presenter.update_status(None, identity);
    presenter
        .print_line(format!(
            "{} turns · {}",
            count,
            format_duration(outcome.elapsed)
        ))
        .map_err(|error| LiveCommandError::Failed(error.to_string()))?;
    if let Some(warning) = persistence_warning {
        presenter.warning(&format!("session: {}", plain_text(&warning)));
    }
    Ok(())
}

fn present_compaction_event(
    presenter: &mut Presenter,
    event: ChatEvent<'_>,
) -> Result<(), ChatError> {
    match event {
        ChatEvent::Text(_) => Ok(()),
        ChatEvent::Response(observation) if observation.promoted_text.is_some() => {
            let mut progress_only = observation.clone();
            progress_only.promoted_text = None;
            presenter.event(ChatEvent::Response(&progress_only))
        }
        event => presenter.event(event),
    }
}

fn restore_chat(
    chat: &mut ChatEngine,
    session: &ChatSession,
    store: Option<&SessionStore>,
    interrupted_operation_id: Option<&str>,
) {
    chat.restore(
        session.summary.as_deref(),
        &session.turns,
        session.native_history(),
    );
    if let Some(model_input) = session.pending_model_input() {
        let note = interrupted_turn_note(session, store, interrupted_operation_id);
        chat.restore_interrupted(model_input, &note);
    }
}

fn interrupted_turn_note(
    session: &ChatSession,
    store: Option<&SessionStore>,
    expected_operation_id: Option<&str>,
) -> String {
    let Some(store) = store else {
        return INTERRUPTED_TURN_NOTE.to_string();
    };
    let Ok(trace) = store.read_trace(&session.id) else {
        return INTERRUPTED_TURN_NOTE.to_string();
    };
    let events = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let Some(terminal_index) = events.iter().rposition(|event| {
        matches!(
            event["type"].as_str(),
            Some("operation_failed" | "operation_stopped")
        ) && expected_operation_id
            .is_none_or(|expected| event["operation_id"].as_str() == Some(expected))
    }) else {
        return INTERRUPTED_TURN_NOTE.to_string();
    };
    let operation_id = events[terminal_index]["operation_id"].as_str();
    let started_index = events[..terminal_index]
        .iter()
        .rposition(|event| {
            event["type"] == "operation_started" && event["operation_id"].as_str() == operation_id
        })
        .unwrap_or(0);
    let mut actions = events[started_index..terminal_index]
        .iter()
        .filter(|event| {
            event["type"] == "tool_finished" && event["operation_id"].as_str() == operation_id
        })
        .rev()
        .take(6)
        .filter_map(|event| {
            let data = event["data"].as_object()?;
            let name = data.get("name")?.as_str()?;
            let detail = data.get("detail")?.as_str()?;
            let marker = if data.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                "succeeded"
            } else {
                "failed"
            };
            Some(format!("- {name} {marker}: {detail}"))
        })
        .collect::<Vec<_>>();
    actions.reverse();
    let terminal = &events[terminal_index];
    let failure = match terminal["type"].as_str() {
        Some("operation_stopped") if terminal["data"]["reason"] == "user" => {
            "stopped by user".to_string()
        }
        Some("operation_stopped") => "the operation stopped".to_string(),
        _ => terminal["data"]["error"]
            .as_str()
            .unwrap_or("the operation failed")
            .to_string(),
    };
    if actions.is_empty() {
        return format!("{INTERRUPTED_TURN_NOTE} Last stop: {failure}");
    }
    format!(
        "{INTERRUPTED_TURN_NOTE} Do not repeat successful actions; continue from the current files. Recent actions:\n{}\nLast stop: {failure}",
        actions.join("\n")
    )
}

fn load_price_catalog(
    args: &ChatArgs,
    connection: &NativeConnection,
) -> Result<PriceSelection, Box<dyn std::error::Error>> {
    let identity = ProviderIdentity::from_native_preset(
        connection.preset,
        connection.provider,
        &connection.base_url,
    );
    if identity.is_subscription() {
        return Ok(PriceSelection {
            catalog: None,
            notice: None,
        });
    }
    if let Some(path) = &args.price_card {
        let json = fs::read_to_string(path)?;
        return Ok(PriceSelection {
            catalog: Some(PriceCatalog::from_json(&json)?),
            notice: None,
        });
    }
    let fallback = connection
        .provider
        .map(NativeProvider::catalog)
        .or_else(|| ProviderIdentity::from_endpoint(&connection.base_url).default_catalog());
    Ok(
        match PriceCatalogStore::discover(args.state_dir.as_deref(), fallback.as_ref()) {
            Ok(store) => store.select_for_chat(fallback),
            Err(_) => PriceSelection {
                catalog: fallback,
                notice: Some("price ! · agul price status".to_string()),
            },
        },
    )
}

fn initialize_status(
    status: &mut WorkbenchStatus,
    model: &str,
    reasoning_effort: Option<&str>,
    context_window: Option<u64>,
) {
    status.model = model.to_string();
    status.reasoning_effort = reasoning_effort.unwrap_or("auto").to_string();
    status.context_window_tokens = context_window;
    status.reset_context();
    status.phase = WorkbenchPhase::Ready;
    status.clear_request();
}

fn restore_last_completed_turn_status(
    status: &mut WorkbenchStatus,
    session: &ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
) {
    let Some((outcome, usage)) = last_completed_turn_status(session, store) else {
        return;
    };
    update_status(status, Some((&outcome, &usage)), identity);
}

fn last_completed_turn_status(
    session: &ChatSession,
    store: Option<&SessionStore>,
) -> Option<(crate::runtime::TurnOutcome, WorkbenchRequestUsage)> {
    let trace = store?.read_trace(&session.id).ok()?;
    let events = trace
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let (completed_index, completed) = events.iter().enumerate().rev().find(|(_, event)| {
        event.get("type").and_then(serde_json::Value::as_str) == Some("operation_completed")
            && event
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|operation_id| operation_id.starts_with("turn-"))
    })?;
    let operation_id = completed
        .get("operation_id")
        .and_then(serde_json::Value::as_str)?;
    let started = events[..completed_index].iter().any(|event| {
        event.get("type").and_then(serde_json::Value::as_str) == Some("operation_started")
            && event
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
                == Some(operation_id)
    });
    if !started {
        return None;
    }

    let data = completed.get("data")?;
    let model_rounds = data.get("model_rounds")?.as_u64()?.min(u64::from(u32::MAX)) as u32;
    let tool_calls = data.get("tool_calls")?.as_u64()?.min(u64::from(u32::MAX)) as u32;
    let elapsed = Duration::from_millis(data.get("elapsed_ms")?.as_u64()?);

    let mut entries = Vec::new();
    for event in &events[..completed_index] {
        if event
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            != Some(operation_id)
            || event.get("type").and_then(serde_json::Value::as_str) != Some("usage")
        {
            continue;
        }
        let entry = event.get("data")?.get("entry")?.clone();
        entries.push(serde_json::from_value::<UsageEntry>(entry).ok()?);
    }
    Some((
        crate::runtime::TurnOutcome {
            text: String::new(),
            model_rounds,
            tool_calls,
            elapsed,
        },
        WorkbenchRequestUsage::from_entries(&entries),
    ))
}

fn update_status(
    status: &mut WorkbenchStatus,
    request: Option<(&crate::runtime::TurnOutcome, &WorkbenchRequestUsage)>,
    identity: &ProviderIdentity,
) {
    status.phase = WorkbenchPhase::Ready;
    status.clear_request();
    if let Some((outcome, usage)) = request {
        let summary = &usage.summary;
        status.rounds = outcome.model_rounds;
        status.tool_calls = outcome.tool_calls;
        status.input_tokens = summary.input_tokens.min(u128::from(u64::MAX)) as u64;
        status.output_tokens = summary.output_tokens.min(u128::from(u64::MAX)) as u64;
        if let Some(input_tokens) = usage.latest_input_tokens {
            status.context_used_tokens = input_tokens;
        }
        status.cache_hit_percent = usage.latest_cache_hit_percent;
        status.cost = workbench_cost(identity, summary);
        status.elapsed = Some(outcome.elapsed);
    }
}

fn aggregate_usage(session: &ChatSession, store: Option<&SessionStore>) -> UsageSummary {
    let Some(store) = store else {
        return session.usage.summary().clone();
    };
    match store.aggregate_usage(session) {
        Ok(summary) => summary,
        Err(_) => {
            let mut summary = session.usage.summary().clone();
            summary.total_cost_unavailable = true;
            summary.pricing_status = PricingStatus::Unavailable;
            summary
        }
    }
}

fn cost_label(summary: &UsageSummary) -> String {
    let prefix = if matches!(
        summary.pricing_status,
        PricingStatus::Partial | PricingStatus::Unavailable
    ) || summary.stale_price_responses > 0
        || summary.assumed_price_responses > 0
    {
        "≈"
    } else {
        ""
    };
    let Some(cost) = &summary.total_cost else {
        return format!("{prefix}$—");
    };
    let currency = if cost.currency == "USD" {
        "$".to_string()
    } else {
        format!("{} ", cost.currency)
    };
    format!(
        "{prefix}{currency}{}",
        format_femto_amount_3dp(cost.femto_units())
    )
}

fn visible_cost(identity: &ProviderIdentity, summary: &UsageSummary) -> Option<String> {
    (!identity.is_subscription() || summary.priced_responses > 0).then(|| cost_label(summary))
}

fn workbench_cost(identity: &ProviderIdentity, summary: &UsageSummary) -> Option<String> {
    summary.total_cost.as_ref()?;
    visible_cost(identity, summary)
}

fn billing_label(identity: &ProviderIdentity, summary: &UsageSummary) -> &'static str {
    if identity.is_subscription() && summary.priced_responses > 0 {
        match identity.billing_label() {
            "subscription_quota" => "subscription_quota+price_catalog",
            _ => "chatgpt_quota+price_catalog",
        }
    } else {
        identity.billing_label()
    }
}

fn usage_line(session: &ChatSession, store: Option<&SessionStore>) -> String {
    let summary = aggregate_usage(session, store);
    let cache = summary
        .reported_cache_hit_percent()
        .map(|percent| format!(" · KV {percent:.1}%"))
        .unwrap_or_default();
    format!(
        "{} · ↑{} ↓{}{}",
        summary.responses,
        format_tokens(summary.input_tokens.min(u128::from(u64::MAX)) as u64),
        format_tokens(summary.output_tokens.min(u128::from(u64::MAX)) as u64),
        cache
    )
}

fn cost_line(
    session: &ChatSession,
    store: Option<&SessionStore>,
    identity: &ProviderIdentity,
) -> String {
    let summary = aggregate_usage(session, store);
    if identity.is_subscription() && summary.priced_responses == 0 {
        return format!("◒ {}", identity.quota_label().unwrap_or("quota"));
    }
    let catalogs = store
        .and_then(|store| store.aggregate_price_catalogs(session).ok())
        .unwrap_or_else(|| {
            session
                .usage
                .entries()
                .iter()
                .filter_map(|entry| entry.price_ref.as_ref())
                .map(|price| (price.catalog_id.clone(), price.catalog_version.clone()))
                .collect()
        });
    let version = if catalogs.is_empty() {
        String::new()
    } else {
        let mut catalogs = catalogs;
        catalogs.sort();
        catalogs.dedup();
        format!(
            " · {}",
            catalogs
                .into_iter()
                .map(|(id, version)| format!("{id}@{version}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let quota = if identity.is_subscription() {
        "◒ + "
    } else {
        ""
    };
    format!("{quota}{}{}", cost_label(&summary), version)
}

fn session_lines(
    store: Option<&SessionStore>,
    current: &ChatSession,
    subscription_quota: bool,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let Some(store) = store else {
        return Ok(vec!["○".to_string()]);
    };
    let current_usage = aggregate_usage(current, Some(store));
    let current_cost = session_list_cost(subscription_quota, &current_usage);
    let mut lines = vec![format!(
        "● current · {} · {} · {}",
        plain_text(&current.model),
        session_turns(current.turns.len(), current.summarized_turns),
        current_cost,
    )];

    // Discover lightweight metadata first, then aggregate only the rows that
    // can actually be shown. A large history (and its delegated children)
    // should not make opening `/sessions` progressively slower.
    let sessions = store.resumable_chats(&current.workspace)?;
    for session in sessions.into_iter().take(5) {
        let previous = store.load(&session.id, None)?;
        let usage = aggregate_usage(&previous, Some(store));
        let cost = session_list_cost(session.subscription_quota, &usage);
        let preview = session
            .preview
            .as_deref()
            .and_then(session_preview_suffix)
            .unwrap_or_default();
        lines.push(format!(
            "↳ {} · {} · {} · {} · {}{}",
            session_age(session.updated_at),
            plain_text(&session.model),
            session_status(session.status),
            session_turns(session.turns, session.summarized_turns),
            cost,
            preview,
        ));
    }
    lines.push("↩ exit · agul chat --resume".to_string());
    Ok(lines)
}

fn session_preview_suffix(preview: &str) -> Option<String> {
    let preview = plain_text(preview);
    let preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");
    (!preview.is_empty()).then(|| format!(" · {}", workbench::truncate_cells(&preview, 18)))
}

fn session_list_cost(subscription_quota: bool, usage: &UsageSummary) -> String {
    if subscription_quota {
        match usage.priced_responses {
            0 => "◒".to_string(),
            _ => format!("◒+{}", cost_label(usage)),
        }
    } else {
        cost_label(usage)
    }
}

struct PresenterOutput {
    printer: InteractivePrinter,
    color: bool,
    line_open: bool,
}

impl PresenterOutput {
    fn new(printer: InteractivePrinter, color: bool) -> Self {
        Self {
            printer,
            color,
            line_open: false,
        }
    }

    fn write_fragment(&mut self, kind: TranscriptKind, fragment: TranscriptText) -> io::Result<()> {
        let line_open = !fragment.is_empty() && !fragment.as_str().ends_with('\n');
        if !self.printer.is_workbench() {
            print!("{}", render_transcript_text(self.color, &fragment));
            io::stdout().flush()?;
            self.line_open = line_open;
            return Ok(());
        }
        self.printer.write_text_fragment_as(kind, fragment)?;
        self.line_open = line_open;
        Ok(())
    }

    fn finish_line(&mut self) -> io::Result<()> {
        if !self.line_open {
            return Ok(());
        }
        if self.printer.is_workbench() {
            self.printer.finish_line()?;
        } else {
            println!();
        }
        self.line_open = false;
        Ok(())
    }

    fn promote_reasoning(&mut self, content: TranscriptText) -> io::Result<()> {
        if !self.printer.is_workbench() {
            return Ok(());
        }
        self.line_open = !content.is_empty() && !content.as_str().ends_with('\n');
        self.printer.promote_reasoning(content)
    }

    fn print_line(&mut self, line: impl Into<String>) -> io::Result<()> {
        self.print_line_as(TranscriptKind::Notice, line)
    }

    fn print_line_as(&mut self, kind: TranscriptKind, line: impl Into<String>) -> io::Result<()> {
        self.finish_line()?;
        self.printer.print_line_as(kind, line)
    }

    fn print_text_line_as(
        &mut self,
        kind: TranscriptKind,
        content: TranscriptText,
    ) -> io::Result<()> {
        self.finish_line()?;
        if self.printer.is_workbench() {
            self.printer.print_text_line_as(kind, content)
        } else {
            println!("{}", render_transcript_text(self.color, &content));
            Ok(())
        }
    }
}

fn activity_text(
    marker: &str,
    marker_style: TranscriptStyle,
    label: &str,
    status: Option<&str>,
    detail: &str,
    elapsed: Option<Duration>,
) -> TranscriptText {
    let mut content = TranscriptText::default();
    content.push_untrusted(marker, marker_style);
    if !label.is_empty() {
        content.push_safe(" ", TranscriptStyle::default());
        content.push_untrusted(label, TranscriptStyle::tone(TranscriptTone::Label).bold());
    }
    let status = status.filter(|status| !status.is_empty());
    if let Some(status) = status {
        content.push_safe(
            if label.is_empty() { " " } else { " · " },
            TranscriptStyle::default(),
        );
        content.push_untrusted(status, TranscriptStyle::tone(TranscriptTone::Accent));
    }
    if !detail.is_empty() {
        content.push_safe(
            if label.is_empty() && status.is_none() {
                " "
            } else {
                "  "
            },
            TranscriptStyle::default(),
        );
        content.push_untrusted(detail, TranscriptStyle::tone(TranscriptTone::Muted));
    }
    if let Some(elapsed) = elapsed {
        content.push_safe(" · ", TranscriptStyle::default());
        content.push_untrusted(
            &format_duration(elapsed),
            TranscriptStyle::tone(TranscriptTone::Muted).dim(),
        );
    }
    content
}

fn reasoning_text(text: &str, marker: bool) -> TranscriptText {
    let style = if marker {
        TranscriptStyle::tone(TranscriptTone::Accent)
    } else {
        TranscriptStyle::tone(TranscriptTone::Muted).dim().italic()
    };
    TranscriptText::stream_untrusted(text, style)
}

struct Presenter {
    color: bool,
    quiet: bool,
    hide_reasoning: bool,
    reasoning_open: bool,
    markdown: TerminalMarkdown,
    status: WorkbenchStatus,
    output: PresenterOutput,
}

impl Presenter {
    fn new(
        color: bool,
        quiet: bool,
        hide_reasoning: bool,
        status: WorkbenchStatus,
        printer: InteractivePrinter,
    ) -> Self {
        Self {
            color,
            quiet,
            hide_reasoning,
            reasoning_open: false,
            markdown: TerminalMarkdown::new(color),
            status,
            output: PresenterOutput::new(printer, color),
        }
    }

    fn sync_status(&self) {
        let _ = self.output.printer.status_changed(self.status.clone());
    }

    fn status_line(&self) -> String {
        self.status.render_status_bar(self.status.terminal_width)
    }

    fn clear_transcript(&mut self) -> io::Result<()> {
        self.output.finish_line()?;
        self.reasoning_open = false;
        self.markdown = TerminalMarkdown::new(self.color);
        self.output.printer.clear_transcript()
    }

    fn update_status(
        &mut self,
        request: Option<(&crate::runtime::TurnOutcome, &WorkbenchRequestUsage)>,
        identity: &ProviderIdentity,
    ) {
        update_status(&mut self.status, request, identity);
        self.sync_status();
    }

    fn thinking(&mut self) {
        self.status.phase = WorkbenchPhase::Thinking;
        self.sync_status();
    }

    fn failed(&mut self) {
        self.status.phase = WorkbenchPhase::Failed;
        self.sync_status();
    }

    fn event(&mut self, event: ChatEvent<'_>) -> Result<(), ChatError> {
        if self.quiet {
            return Ok(());
        }
        match event {
            ChatEvent::Reasoning(delta) if !self.hide_reasoning && !delta.is_empty() => {
                if !self.reasoning_open {
                    self.output
                        .write_fragment(TranscriptKind::Reasoning, reasoning_text("◌ ", true))
                        .map_err(|error| ChatError::new(error.to_string()))?;
                    self.reasoning_open = true;
                }
                self.output
                    .write_fragment(TranscriptKind::Reasoning, reasoning_text(delta, false))
                    .map_err(|error| ChatError::new(error.to_string()))?;
            }
            ChatEvent::Reasoning(_) => {}
            ChatEvent::Text(delta) => {
                self.close_reasoning()?;
                self.output
                    .write_fragment(
                        TranscriptKind::Assistant,
                        self.markdown.push_untrusted(delta),
                    )
                    .map_err(|error| ChatError::new(error.to_string()))?;
            }
            ChatEvent::ToolStarted { name, detail } => {
                self.finish_response()?;
                self.status.phase = WorkbenchPhase::ToolRunning;
                self.sync_status();
                self.output
                    .print_text_line_as(
                        TranscriptKind::Activity,
                        activity_text(
                            "◆",
                            TranscriptStyle::tone(TranscriptTone::Accent),
                            name,
                            None,
                            detail,
                            None,
                        ),
                    )
                    .map_err(|error| ChatError::new(error.to_string()))?;
            }
            ChatEvent::ToolFinished {
                name,
                detail,
                ok,
                elapsed,
            } => {
                let marker = if ok { "✓" } else { "!" };
                let tone = if ok {
                    TranscriptTone::Success
                } else {
                    TranscriptTone::Error
                };
                self.output
                    .print_text_line_as(
                        TranscriptKind::Activity,
                        activity_text(
                            marker,
                            TranscriptStyle::tone(tone),
                            name,
                            None,
                            detail,
                            Some(elapsed),
                        ),
                    )
                    .map_err(|error| ChatError::new(error.to_string()))?;
                self.status.phase = WorkbenchPhase::Thinking;
                self.sync_status();
            }
            ChatEvent::ToolProgress {
                task_id,
                stage,
                preview,
                ..
            } => {
                self.close_reasoning()?;
                self.output
                    .print_text_line_as(
                        TranscriptKind::Activity,
                        activity_text(
                            "↳",
                            TranscriptStyle::tone(TranscriptTone::Accent),
                            task_id.unwrap_or_default(),
                            Some(stage),
                            preview,
                            None,
                        ),
                    )
                    .map_err(|error| ChatError::new(error.to_string()))?;
            }
            ChatEvent::RelatedSession {
                task_id,
                session_id,
                ..
            } => {
                self.output
                    .print_text_line_as(
                        TranscriptKind::Activity,
                        activity_text(
                            "◇",
                            TranscriptStyle::tone(TranscriptTone::Success),
                            task_id,
                            None,
                            session_id,
                            None,
                        ),
                    )
                    .map_err(|error| ChatError::new(error.to_string()))?;
            }
            ChatEvent::Response(observation) => {
                let mut context_changed = false;
                if let Some(window) = observation.context_window.filter(|window| *window > 0) {
                    self.status.context_window_tokens = Some(window);
                    context_changed = true;
                }
                if let Some(usage) = &observation.usage {
                    self.status.context_used_tokens = usage.input_tokens;
                    context_changed = true;
                }
                if context_changed {
                    self.sync_status();
                }
                if let Some(text) = observation.promoted_text.as_deref() {
                    if self.hide_reasoning {
                        self.close_reasoning()?;
                        self.output
                            .write_fragment(
                                TranscriptKind::Assistant,
                                self.markdown.push_untrusted(text),
                            )
                            .map_err(|error| ChatError::new(error.to_string()))?;
                    } else if self.output.printer.is_workbench() {
                        self.output
                            .promote_reasoning(TerminalMarkdown::render_complete(text, self.color))
                            .map_err(|error| ChatError::new(error.to_string()))?;
                        self.reasoning_open = false;
                    }
                }
                self.finish_response()?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, _outcome: &crate::runtime::TurnOutcome) {
        if !self.quiet {
            let _ = self.finish_response();
        }
    }

    fn stopped(&mut self) {
        self.status.phase = WorkbenchPhase::Ready;
        self.status.clear_request();
        self.sync_status();
        if !self.quiet {
            let _ = self.finish_response();
            let _ = self.output.print_text_line_as(
                TranscriptKind::Notice,
                TranscriptText::styled_untrusted(
                    "■ stopped",
                    TranscriptStyle::tone(TranscriptTone::Muted),
                ),
            );
        }
    }

    fn print_line(&mut self, line: impl Into<String>) -> io::Result<()> {
        self.output.print_line(line)
    }

    fn warning(&mut self, warning: &str) {
        if !self.quiet {
            let mut content = TranscriptText::styled_untrusted(
                "!",
                TranscriptStyle::tone(TranscriptTone::Error).bold(),
            );
            content.push_safe(" ", TranscriptStyle::default());
            content.push_untrusted(warning, TranscriptStyle::default());
            let _ = self
                .output
                .print_text_line_as(TranscriptKind::Notice, content);
        }
    }

    fn plugin_command_started(&mut self, name: &str, detail: &str) -> io::Result<()> {
        let _ = self.finish_response();
        self.status.phase = WorkbenchPhase::ToolRunning;
        self.status.clear_request();
        self.sync_status();
        self.output.print_text_line_as(
            TranscriptKind::Activity,
            activity_text(
                "◆",
                TranscriptStyle::tone(TranscriptTone::Accent),
                &format!("/{name}"),
                None,
                detail,
                None,
            ),
        )
    }

    fn plugin_event(&mut self, event: &PluginEvent) -> io::Result<()> {
        match event {
            PluginEvent::ToolProgress(progress) => self.output.print_text_line_as(
                TranscriptKind::Activity,
                activity_text(
                    "↳",
                    TranscriptStyle::tone(TranscriptTone::Accent),
                    progress.task_id.as_deref().unwrap_or_default(),
                    Some(&progress.stage),
                    &progress.preview,
                    None,
                ),
            ),
            PluginEvent::RelatedSession(related) => self.output.print_text_line_as(
                TranscriptKind::Activity,
                activity_text(
                    "◇",
                    TranscriptStyle::tone(TranscriptTone::Success),
                    &related.task_id,
                    None,
                    &related.session_id,
                    None,
                ),
            ),
        }
    }

    fn plugin_result(&mut self, content: &serde_json::Value) -> io::Result<()> {
        self.output
            .print_text_line_as(TranscriptKind::Activity, plugin_result_text(content))
    }

    fn plugin_command_finished(&mut self, ok: bool) {
        self.status.phase = if ok {
            WorkbenchPhase::Ready
        } else {
            WorkbenchPhase::Failed
        };
        self.sync_status();
    }

    fn close_reasoning(&mut self) -> Result<(), ChatError> {
        if std::mem::take(&mut self.reasoning_open) {
            self.output
                .finish_line()
                .map_err(|error| ChatError::new(error.to_string()))?;
        }
        Ok(())
    }

    fn abort_response(&mut self) {
        if !self.quiet {
            let _ = self.finish_response();
        }
    }

    fn finish_response(&mut self) -> Result<(), ChatError> {
        self.close_reasoning()?;
        let trailing = self.markdown.finish_response();
        if !trailing.is_empty() {
            self.output
                .write_fragment(TranscriptKind::Assistant, trailing)
                .map_err(|error| ChatError::new(error.to_string()))?;
        }
        self.output
            .finish_line()
            .map_err(|error| ChatError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::runtime::{FixtureProcess, process_test_lock, process_tree_command_fixture};
    use httpmock::{Method::POST, MockServer};

    fn coordinator_process_tree_fixture(root: &Path) -> (Project, PathBuf) {
        let workspace = root.join("workspace");
        let runtime = workspace.join(".agents/runtime");
        let plugin = workspace.join(".agents/plugins/coordinator");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            workspace.join(".agents/AGENTS.md"),
            "Coordinate test work.\n",
        )
        .unwrap();
        fs::write(
            runtime.join("launch.json"),
            r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","plugins":"../plugins"}"#,
        )
        .unwrap();

        let (command, script_name, script) = process_tree_command_fixture();
        fs::write(plugin.join(script_name), script).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            serde_json::to_vec(&json!({
                "format": "agul/plugin/v2",
                "name": "coordinator",
                "version": "1.0.0",
                "command": command,
                "timeout_seconds": 30,
                "commands": [{
                    "name": "coordinate",
                    "description": "Coordinate test agents"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let marker = plugin.join("child-started");
        (Project::discover(&workspace, None).unwrap(), marker)
    }

    fn compaction_fixture(
        base_url: String,
    ) -> (
        tempfile::TempDir,
        ChatEngine,
        ChatSession,
        ProviderIdentity,
        Presenter,
    ) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::discover(root.path(), None).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "test-model", None);
        for index in 1..=6 {
            let user = format!("user {index}");
            session.begin_turn(user.clone(), user, None);
            assert!(session.finish_turn(format!("assistant {index}"), None));
        }
        session.summary = Some("previous summary".to_string());
        session.summarized_turns = 2;
        let chat = ChatEngine::native(
            &project,
            ChatConfig {
                provider: ProviderConfig {
                    base_url,
                    provider: None,
                    model: "test-model".to_string(),
                    api_key_env: None,
                    reasoning_effort: None,
                    max_tokens: 256,
                    context_window: None,
                    timeout: Duration::from_secs(2),
                },
                max_rounds: 1,
                max_tool_calls: 1,
            },
            session.id.clone(),
            None,
        )
        .unwrap();
        let identity = ProviderIdentity::from_endpoint(chat.endpoint());
        let input = InteractiveInput::install(false, false, Vec::new(), WorkbenchStatus::default())
            .unwrap();
        let presenter = Presenter::new(
            false,
            true,
            true,
            WorkbenchStatus::default(),
            input.printer(),
        );
        (root, chat, session, identity, presenter)
    }

    #[test]
    fn reasoning_only_compaction_stays_progress_and_records_usage() {
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "id": "compact-reasoning-only",
                "model": "test-model",
                "choices": [{
                    "finish_reason": "stop",
                    "delta": {"reasoning_content": "COMPACT_SUMMARY"}
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 3}
            })
        );
        let server = MockServer::start();
        let response = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body);
        });
        let (_root, mut chat, mut session, identity, _presenter) =
            compaction_fixture(format!("{}/v1", server.base_url()));
        let (printer, events) = InteractivePrinter::test_workbench();
        let mut presenter =
            Presenter::new(false, false, false, WorkbenchStatus::default(), printer);

        compact(
            &mut chat,
            &mut session,
            None,
            &identity,
            &mut presenter,
            &TurnCancellation::default(),
        )
        .unwrap();
        response.assert_calls(1);

        assert!(
            session
                .summary
                .as_deref()
                .is_some_and(|summary| summary.ends_with("COMPACT_SUMMARY"))
        );
        let entry = session.usage.entries().last().expect("compaction usage");
        assert_eq!(entry.purpose, UsagePurpose::Compaction);
        assert_eq!(entry.response_id.as_deref(), Some("compact-reasoning-only"));
        assert_eq!(entry.input_tokens, Some(11));
        assert_eq!(entry.output_tokens, Some(3));

        let mut model = workbench::WorkbenchModel::default();
        for event in events.try_iter() {
            model.apply(event);
        }
        assert!(model.transcript().iter().any(|line| {
            line.kind == TranscriptKind::Reasoning && line.text().contains("COMPACT_SUMMARY")
        }));
        assert!(model.transcript().iter().all(|line| {
            line.kind != TranscriptKind::Assistant || !line.text().contains("COMPACT_SUMMARY")
        }));
        assert_eq!(model.status.phase, WorkbenchPhase::Ready);
        assert_eq!(model.status.context_used_tokens, 11);
    }

    #[test]
    fn failed_compaction_keeps_visible_state_and_records_reported_usage() {
        let body = format!(
            "data: {}\n\ndata: not-json\n\n",
            json!({
                "id": "compact-failed",
                "model": "test-model",
                "choices": [],
                "usage": {"prompt_tokens": 11, "completion_tokens": 3}
            })
        );
        let server = MockServer::start();
        let response = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body);
        });
        let (_root, mut chat, mut session, identity, mut presenter) =
            compaction_fixture(format!("{}/v1", server.base_url()));
        let turns = session.turns.clone();
        let summary = session.summary.clone();
        let summarized_turns = session.summarized_turns;

        let error = compact(
            &mut chat,
            &mut session,
            None,
            &identity,
            &mut presenter,
            &TurnCancellation::default(),
        )
        .unwrap_err();
        response.assert_calls(1);

        assert!(
            matches!(error, LiveCommandError::Failed(message) if message.contains("invalid JSON"))
        );
        assert_eq!(session.turns, turns);
        assert_eq!(session.summary, summary);
        assert_eq!(session.summarized_turns, summarized_turns);
        assert_eq!(session.usage.entries().len(), 1);
        let entry = &session.usage.entries()[0];
        assert_eq!(entry.purpose, UsagePurpose::Compaction);
        assert_eq!(entry.response_id.as_deref(), Some("compact-failed"));
        assert_eq!(entry.input_tokens, Some(11));
        assert_eq!(entry.output_tokens, Some(3));
    }

    #[test]
    fn stopped_compaction_keeps_visible_state_and_does_not_invent_usage() {
        let server = MockServer::start();
        let response = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .delay(Duration::from_millis(500))
                .body("data: [DONE]\n\n");
        });
        let (_root, mut chat, mut session, identity, mut presenter) =
            compaction_fixture(format!("{}/v1", server.base_url()));
        let turns = session.turns.clone();
        let summary = session.summary.clone();
        let summarized_turns = session.summarized_turns;
        let cancellation = TurnCancellation::default();
        let (error, request_seen) = std::thread::scope(|scope| {
            let stop_cancellation = cancellation.clone();
            let response_status = &response;
            let stop = scope.spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while response_status.calls() == 0 && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let request_seen = response_status.calls() > 0;
                stop_cancellation.cancel();
                request_seen
            });
            let error = compact(
                &mut chat,
                &mut session,
                None,
                &identity,
                &mut presenter,
                &cancellation,
            )
            .unwrap_err();
            (error, stop.join().unwrap())
        });
        assert!(request_seen, "compaction request did not reach the server");
        response.assert_calls(1);

        assert!(matches!(error, LiveCommandError::Stopped));
        assert_eq!(session.turns, turns);
        assert_eq!(session.summary, summary);
        assert_eq!(session.summarized_turns, summarized_turns);
        assert!(session.usage.entries().is_empty());
    }

    #[test]
    fn live_stop_terminates_coordinator_and_records_the_stopped_operation() {
        let _serial = process_test_lock();
        let root = tempfile::tempdir().unwrap();
        let (project, child_marker) = coordinator_process_tree_fixture(root.path());
        let state = root.path().join("state");
        let store = SessionStore::discover(Some(&state)).unwrap();
        let mut session = ChatSession::new(project.workspace.clone(), "test-model", None);
        let (input, input_sender) = InteractiveInput::scripted();
        let mut presenter = Presenter::new(
            false,
            true,
            true,
            WorkbenchStatus::default(),
            input.printer(),
        );
        let stop_sender = input_sender.clone();
        let watched_marker = child_marker.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            while !watched_marker.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let process_started = watched_marker.exists();
            stop_sender
                .send(InteractiveRead::Line("/stop".to_string()))
                .unwrap();
            process_started
        });

        let live = run_live_operation(&input, &mut presenter, |cancellation, presenter| {
            handle_plugin_command(
                "/coordinate inspect",
                &project,
                &mut session,
                Some(&store),
                Some(&state),
                presenter,
                cancellation,
            )
        })
        .unwrap();
        let process_started = stopper.join().unwrap();
        let LiveOperation {
            result,
            queued_input,
            exit_requested,
            stop_requested,
        } = live;

        assert!(process_started, "coordinator child process did not start");
        assert_eq!(
            fs::read_to_string(
                project
                    .workspace
                    .join(".agents/plugins/coordinator/state-dir.txt")
            )
            .unwrap(),
            state.to_string_lossy()
        );
        assert!(matches!(result, Ok(Err(LiveCommandError::Stopped))));
        assert!(queued_input.is_empty());
        assert!(!exit_requested);
        assert!(stop_requested);
        assert_eq!(session.status, SessionStatus::Active);

        let child_pid = fs::read_to_string(&child_marker)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let child = FixtureProcess::new(child_pid);
        assert!(
            child.wait_for_exit(Duration::from_secs(4)),
            "coordinator child {child_pid} survived /stop"
        );

        let trace = store
            .read_trace(&session.id)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0]["type"], "operation_started");
        assert_eq!(trace[0]["data"]["command"], "coordinate");
        assert_eq!(trace[1]["operation_id"], trace[0]["operation_id"]);
        assert_eq!(trace[1]["type"], "operation_stopped");
        assert_eq!(trace[1]["data"]["reason"], "user");

        input_sender
            .send(InteractiveRead::Line("next request".to_string()))
            .unwrap();
        assert!(matches!(
            input.read_message(),
            InteractiveRead::Line(line) if line == "next request"
        ));
    }

    #[test]
    fn plugin_command_result_keeps_delegation_details_compact() {
        let content = json!({
            "status": "completed",
            "results": [
                {"id": "local-read", "pool": "local-default", "ledger_entries": [1, 2]},
                {"id": "deepseek-read", "pool": "deepseek-subagent", "ledger_entries": [3]}
            ],
            "usage": {"input_tokens": 100_000, "output_tokens": 5_000}
        });

        let line = plugin_result_line(&content);
        assert_eq!(
            line,
            "✓ completed · 2 tasks · local-read@local-default · deepseek-read@deepseek-subagent"
        );
        assert!(!line.contains("ledger"));
        assert!(!line.contains("input_tokens"));
    }

    #[test]
    fn plugin_command_result_never_dumps_unknown_structured_payloads() {
        assert_eq!(plugin_result_line(&json!({"large": [1, 2, 3]})), "✓ result");
    }

    #[test]
    fn plugin_command_result_distinguishes_success_from_its_summary() {
        let content = plugin_result_text(&json!("two agents completed"));
        let segments = content.segments();
        assert_eq!(content.as_str(), "✓ two agents completed");
        assert!(segments.iter().any(|segment| {
            segment.text == "✓"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Success)
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "two agents completed"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Muted)
        }));
    }

    #[test]
    fn live_input_distinguishes_stop_exit_and_steering() {
        assert_eq!(classify_live_input(""), LiveInputAction::Ignore);
        assert_eq!(classify_live_input("/stop"), LiveInputAction::Stop);
        assert_eq!(classify_live_input("/exit"), LiveInputAction::Exit);
        assert_eq!(
            classify_live_input("change direction"),
            LiveInputAction::Steer
        );
        assert_eq!(classify_live_input("/status"), LiveInputAction::Steer);
    }

    #[test]
    fn stop_and_exit_override_queued_steering() {
        let mut queued = Vec::new();
        let mut exit = false;
        let mut stop = false;

        assert!(apply_live_input(
            "change direction".to_string(),
            &mut queued,
            &mut exit,
            &mut stop,
        ));
        assert_eq!(queued, ["change direction"]);
        assert!(!stop);

        assert!(apply_live_input(
            "/stop".to_string(),
            &mut queued,
            &mut exit,
            &mut stop,
        ));
        assert!(queued.is_empty());
        assert!(stop);

        assert!(apply_live_input(
            "new request".to_string(),
            &mut queued,
            &mut exit,
            &mut stop,
        ));
        assert_eq!(queued, ["new request"]);
        assert!(!stop);

        assert!(apply_live_input(
            "/exit".to_string(),
            &mut queued,
            &mut exit,
            &mut stop,
        ));
        assert!(queued.is_empty());
        assert!(exit);
        assert!(!stop);
    }

    #[test]
    fn clean_exit_records_a_user_stopped_turn_as_cancelled() {
        let mut stopped = ChatSession::new(PathBuf::from("."), "model", None);
        stopped.begin_turn("visible".to_string(), "model input".to_string(), None);

        assert_eq!(
            closed_session_status(&stopped, true),
            SessionStatus::Cancelled
        );
        assert_eq!(
            closed_session_status(&stopped, false),
            SessionStatus::Interrupted
        );

        stopped.set_status(SessionStatus::Failed);
        assert_eq!(closed_session_status(&stopped, true), SessionStatus::Failed);

        let mut failed_without_pending = ChatSession::new(PathBuf::from("."), "model", None);
        failed_without_pending.set_status(SessionStatus::Failed);
        assert_eq!(
            closed_session_status(&failed_without_pending, true),
            SessionStatus::Failed
        );

        let mut completed = ChatSession::new(PathBuf::from("."), "model", None);
        completed.begin_turn("visible".to_string(), "model input".to_string(), None);
        assert!(completed.finish_turn("done".to_string(), None));
        assert_eq!(
            closed_session_status(&completed, true),
            SessionStatus::Completed
        );
    }

    #[test]
    fn subscription_sessions_use_the_quota_marker_in_the_session_list() {
        assert_eq!(session_list_cost(true, &UsageSummary::default()), "◒");
        assert_eq!(session_list_cost(false, &UsageSummary::default()), "$—");
    }

    #[test]
    fn session_list_always_identifies_the_current_chat() {
        let root = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(root.path()).unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut current = ChatSession::new(workspace, "current-model", None);
        current.begin_turn(
            "inspect the current workspace".to_string(),
            "inspect the current workspace".to_string(),
            None,
        );
        assert!(current.finish_turn("done".to_string(), None));

        let lines = session_lines(Some(&store), &current, false).unwrap();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("● current · current-model · 1 turn · $—"));
        assert!(!lines.iter().any(|line| line.contains("no previous chats")));
        assert_eq!(lines[1], "↩ exit · agul chat --resume");
    }

    #[test]
    fn session_list_limits_previous_chats_before_loading_usage() {
        let root = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(root.path()).unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let current = ChatSession::new(workspace.clone(), "current-model", None);
        for index in 0_u64..7 {
            let mut previous =
                ChatSession::new(workspace.clone(), format!("previous-model-{index}"), None);
            previous.begin_turn(
                format!("previous request {index}"),
                format!("previous request {index}"),
                None,
            );
            assert!(previous.finish_turn("done".to_string(), None));
            previous.status = SessionStatus::Completed;
            previous.created_at = index;
            previous.updated_at = index;
            store.save(&previous).unwrap();
        }

        let lines = session_lines(Some(&store), &current, false).unwrap();

        assert_eq!(
            lines.len(),
            7,
            "current + five previous chats + resume hint"
        );
        for index in 2..=6 {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(&format!("previous request {index}"))),
                "missing displayed previous chat {index}: {lines:?}"
            );
        }
        for index in 0..=1 {
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains(&format!("previous request {index}"))),
                "hidden previous chat {index} was rendered: {lines:?}"
            );
        }
    }

    #[test]
    fn public_glm_preset_selects_coding_plan_and_ordinary_api_stays_explicit() {
        let preset = ChatArgs {
            provider: Some("glm".parse().unwrap()),
            reasoning_effort: Some("medium".to_string()),
            ..ChatArgs::default()
        };
        let connection = NativeConnection::resolve(&preset, None).unwrap();
        assert_eq!(connection.preset, Some(NativeConnectionPreset::GlmCoding));
        assert_eq!(connection.provider, Some(NativeProvider::Glm));
        assert_eq!(
            connection.base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(connection.default_model, "glm-4.7");
        assert_eq!(connection.api_key_env.as_deref(), Some("GLM_API_KEY"));
        assert_eq!(connection.reasoning_effort.as_deref(), Some("high"));

        let ordinary_api = ChatArgs {
            base_url: Some("https://open.bigmodel.cn/api/paas/v4".to_string()),
            ..ChatArgs::default()
        };
        let connection = NativeConnection::resolve(&ordinary_api, None).unwrap();
        assert_eq!(connection.preset, Some(NativeConnectionPreset::Glm));
        assert_eq!(connection.provider, Some(NativeProvider::Glm));
        assert_eq!(connection.default_model, "glm-5.3");
        assert_eq!(connection.api_key_env.as_deref(), Some("GLM_API_KEY"));
    }

    #[test]
    fn only_exact_official_deepseek_v4_models_get_the_known_window() {
        let official = NativeConnection::resolve(&ChatArgs::default(), None).unwrap();
        assert_eq!(
            official.known_context_window("deepseek-v4-flash"),
            Some(DEEPSEEK_V4_CONTEXT_WINDOW)
        );
        assert_eq!(
            official.known_context_window("deepseek-v4-pro"),
            Some(DEEPSEEK_V4_CONTEXT_WINDOW)
        );
        assert_eq!(official.known_context_window("custom-model"), None);

        let custom = NativeConnection::resolve(
            &ChatArgs {
                base_url: Some("http://127.0.0.1:51100/v1".to_string()),
                ..ChatArgs::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(custom.known_context_window("deepseek-v4-flash"), None);
    }

    #[test]
    fn native_resume_reuses_routing_and_allows_runtime_setting_overrides() {
        let saved = NativeSessionConfig {
            preset: Some(NativeConnectionPreset::Glm),
            provider: Some(NativeProvider::Glm),
            base_url: "http://127.0.0.1:51100/v1".to_string(),
            api_key_env: Some("GLM_API_KEY".to_string()),
            reasoning_effort: Some("high".to_string()),
        };
        let resumed = NativeConnection::resolve(&ChatArgs::default(), Some(&saved)).unwrap();
        assert_eq!(resumed.session_config(), saved);

        let overrides = ChatArgs {
            api_key_env: Some("ROTATED_GLM_API_KEY".to_string()),
            reasoning_effort: Some("ultra".to_string()),
            ..ChatArgs::default()
        };
        let resumed = NativeConnection::resolve(&overrides, Some(&saved)).unwrap();
        assert_eq!(resumed.api_key_env.as_deref(), Some("ROTATED_GLM_API_KEY"));
        assert_eq!(resumed.reasoning_effort.as_deref(), Some("max"));

        let coding_saved = NativeSessionConfig {
            preset: Some(NativeConnectionPreset::GlmCoding),
            provider: Some(NativeProvider::Glm),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
            api_key_env: Some("GLM_API_KEY".to_string()),
            reasoning_effort: None,
        };
        let coding_resumed =
            NativeConnection::resolve(&ChatArgs::default(), Some(&coding_saved)).unwrap();
        assert_eq!(coding_resumed.session_config(), coding_saved);
        assert_eq!(coding_resumed.default_model, "glm-4.7");
        let coding_route_conflict = ChatArgs {
            provider: Some(NativeConnectionPreset::Glm),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&coding_route_conflict, Some(&coding_saved))
                .unwrap_err()
                .contains("conflicts with the saved provider glm")
        );

        let provider_conflict = ChatArgs {
            provider: Some(NativeConnectionPreset::Deepseek),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&provider_conflict, Some(&saved))
                .unwrap_err()
                .contains("conflicts with the saved provider")
        );
        let route_override = ChatArgs {
            base_url: Some("http://127.0.0.1:52200/v1".to_string()),
            ..ChatArgs::default()
        };
        let resumed = NativeConnection::resolve(&route_override, Some(&saved)).unwrap();
        assert_eq!(resumed.base_url, "http://127.0.0.1:52200/v1");

        let route_conflict = ChatArgs {
            base_url: Some("https://api.deepseek.com".to_string()),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&route_conflict, Some(&saved))
                .unwrap_err()
                .contains("provider glm-api conflicts with URL provider deepseek")
        );

        let new_route_conflict = ChatArgs {
            provider: Some(NativeConnectionPreset::Glm),
            base_url: Some("https://api.deepseek.com".to_string()),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&new_route_conflict, None)
                .unwrap_err()
                .contains("provider glm-api conflicts with URL provider deepseek")
        );

        let coding_endpoint_conflict = ChatArgs {
            provider: Some(NativeConnectionPreset::GlmCoding),
            base_url: Some("https://open.bigmodel.cn/api/paas/v4".to_string()),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&coding_endpoint_conflict, None)
                .unwrap_err()
                .contains("provider glm conflicts with URL provider glm-api")
        );

        let resumed_coding_endpoint_conflict = ChatArgs {
            base_url: Some("https://open.bigmodel.cn/api/paas/v4".to_string()),
            ..ChatArgs::default()
        };
        assert!(
            NativeConnection::resolve(&resumed_coding_endpoint_conflict, Some(&coding_saved),)
                .unwrap_err()
                .contains("provider glm conflicts with URL provider glm-api")
        );
    }

    #[test]
    fn streaming_trace_chunks_are_coalesced() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "model", None);
        let operation_id = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "hello"}))
            .unwrap();
        let mut buffer = StreamTraceBuffer::default();
        for _ in 0..100 {
            buffer
                .push(Some(&store), &mut session, &operation_id, "text", "x")
                .unwrap();
        }
        buffer.flush(Some(&store), &mut session).unwrap();

        let events = store
            .read_trace(&session.id)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["type"], "text");
        assert_eq!(events[1]["data"]["text"].as_str().unwrap().len(), 100);
    }

    #[test]
    fn unavailable_aggregate_cost_is_visibly_approximate() {
        let summary = UsageSummary {
            pricing_status: PricingStatus::Unavailable,
            total_cost_unavailable: true,
            ..UsageSummary::default()
        };
        assert_eq!(cost_label(&summary), "≈$—");
    }

    #[test]
    fn workbench_omits_the_unpriced_cost_placeholder() {
        let identity = ProviderIdentity::from_endpoint("http://127.0.0.1:51100/v1");
        assert_eq!(workbench_cost(&identity, &UsageSummary::default()), None);
    }

    #[test]
    fn thinking_keeps_the_last_request_telemetry_visible() {
        let status = WorkbenchStatus {
            rounds: 2,
            tool_calls: 1,
            input_tokens: 3_200,
            output_tokens: 103,
            cache_hit_percent: Some(96.5),
            cost: Some("$0.001".to_string()),
            elapsed: Some(Duration::from_millis(250)),
            ..WorkbenchStatus::default()
        };
        let input = InteractiveInput::install(false, false, Vec::new(), status.clone()).unwrap();
        let mut presenter = Presenter::new(false, true, true, status.clone(), input.printer());

        presenter.thinking();

        assert_eq!(presenter.status.phase, WorkbenchPhase::Thinking);
        assert_eq!(presenter.status.rounds, status.rounds);
        assert_eq!(presenter.status.tool_calls, status.tool_calls);
        assert_eq!(presenter.status.input_tokens, status.input_tokens);
        assert_eq!(presenter.status.cache_hit_percent, status.cache_hit_percent);
        assert_eq!(presenter.status.cost, status.cost);
    }

    #[test]
    fn empty_reasoning_chunks_do_not_open_a_visible_reasoning_line() {
        let input = InteractiveInput::install(false, false, Vec::new(), WorkbenchStatus::default())
            .unwrap();
        let mut presenter = Presenter::new(
            false,
            false,
            false,
            WorkbenchStatus::default(),
            input.printer(),
        );

        presenter.event(ChatEvent::Reasoning("")).unwrap();

        assert!(!presenter.reasoning_open);
    }

    #[test]
    fn response_metadata_updates_live_context_status() {
        let input = InteractiveInput::install(false, false, Vec::new(), WorkbenchStatus::default())
            .unwrap();
        let mut presenter = Presenter::new(
            false,
            false,
            false,
            WorkbenchStatus::default(),
            input.printer(),
        );
        let observation = crate::runtime::ResponseObservation {
            response_id: Some("response-1".to_string()),
            requested_model: "account-model".to_string(),
            reported_model: Some("account-model".to_string()),
            provider_created_at: None,
            received_at: 1,
            usage: Some(crate::runtime::Usage {
                input_tokens: 24_000,
                output_tokens: 800,
                cache_hit_tokens: Some(20_000),
                cache_miss_tokens: Some(4_000),
                reasoning_tokens: None,
            }),
            context_window: Some(200_000),
            promoted_text: None,
        };

        presenter.event(ChatEvent::Response(&observation)).unwrap();

        assert_eq!(presenter.status.context_used_tokens, 24_000);
        assert_eq!(presenter.status.context_window_tokens, Some(200_000));
    }

    #[test]
    fn workbench_promotes_reasoning_only_completion_in_place() {
        let (printer, events) = InteractivePrinter::test_workbench();
        let mut presenter = Presenter::new(true, false, false, WorkbenchStatus::default(), printer);
        let mut model = workbench::WorkbenchModel::default();

        presenter
            .event(ChatEvent::Reasoning("**PROMOTED_ASSISTANT**"))
            .unwrap();
        for event in events.try_iter() {
            model.apply(event);
        }
        assert_eq!(model.live_kind(), TranscriptKind::Reasoning);
        assert!(model.live_output().as_str().contains("PROMOTED_ASSISTANT"));

        let observation = crate::runtime::ResponseObservation {
            response_id: Some("reasoning-only".to_string()),
            requested_model: "glm-4.7".to_string(),
            reported_model: Some("glm-4.7".to_string()),
            provider_created_at: None,
            received_at: 1,
            usage: None,
            context_window: Some(131_072),
            promoted_text: Some("**PROMOTED_ASSISTANT**".to_string()),
        };
        presenter.event(ChatEvent::Response(&observation)).unwrap();
        for event in events.try_iter() {
            model.apply(event);
        }

        assert_eq!(
            model
                .transcript()
                .iter()
                .filter(|line| line.text().contains("PROMOTED_ASSISTANT"))
                .count(),
            1
        );
        let answer = model
            .transcript()
            .iter()
            .find(|line| line.text() == "PROMOTED_ASSISTANT")
            .expect("promoted assistant line");
        assert_eq!(answer.kind, TranscriptKind::Assistant);
        assert!(answer.content().segments()[0].style.bold);
        assert!(
            model
                .transcript()
                .iter()
                .all(|line| line.kind != TranscriptKind::Reasoning)
        );
        assert!(model.live_output().is_empty());
    }

    #[test]
    fn tool_activity_keeps_marker_label_detail_and_duration_distinct() {
        let content = activity_text(
            "✓",
            TranscriptStyle::tone(TranscriptTone::Success),
            "shell",
            None,
            "cargo test",
            Some(Duration::from_millis(480)),
        );
        let segments = content.segments();
        assert!(segments.iter().any(|segment| {
            segment.text == "✓"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Success)
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "shell"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Label)
                && segment.style.bold
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "cargo test"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Muted)
                && !segment.style.dim
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "480ms"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Muted)
                && segment.style.dim
        }));
    }

    #[test]
    fn tool_progress_keeps_task_stage_and_preview_distinct() {
        let content = activity_text(
            "↳",
            TranscriptStyle::tone(TranscriptTone::Accent),
            "research-1",
            Some("reading"),
            "Cargo.toml",
            None,
        );
        let segments = content.segments();
        assert!(segments.iter().any(|segment| {
            segment.text == "research-1"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Label)
                && segment.style.bold
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "reading"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Accent)
                && !segment.style.bold
        }));
        assert!(segments.iter().any(|segment| {
            segment.text == "Cargo.toml"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Muted)
        }));
    }

    #[test]
    fn workbench_status_uses_request_totals_and_the_latest_response_kv() {
        let identity = ProviderIdentity::from_endpoint("https://api.deepseek.com/v1");
        let mut session = ChatSession::new(
            std::env::temp_dir(),
            "deepseek-v4-flash",
            Some(PriceCatalog::builtin_deepseek_usd()),
        );
        let response = |id: &str, input_tokens, output_tokens, cache_hit_tokens| {
            crate::runtime::ResponseObservation {
                response_id: Some(id.to_string()),
                requested_model: "deepseek-v4-flash".to_string(),
                reported_model: Some("deepseek-v4-flash".to_string()),
                provider_created_at: Some(1_787_788_800),
                received_at: 1_787_788_800,
                usage: Some(crate::runtime::Usage {
                    input_tokens,
                    output_tokens,
                    cache_hit_tokens: Some(cache_hit_tokens),
                    cache_miss_tokens: Some(input_tokens - cache_hit_tokens),
                    reasoning_tokens: None,
                }),
                context_window: Some(DEEPSEEK_V4_CONTEXT_WINDOW.into()),
                promoted_text: None,
            }
        };
        identity.record(
            &mut session.usage,
            UsagePurpose::Chat,
            &response("previous", 2_000_000, 100_000, 500_000),
        );
        let usage_start = session.usage.entries().len();
        identity.record(
            &mut session.usage,
            UsagePurpose::Chat,
            &response("current-1", 1_000_000, 20_000, 750_000),
        );
        identity.record(
            &mut session.usage,
            UsagePurpose::Chat,
            &response("current-2", 100_000, 10_000, 90_000),
        );

        let request_usage = summarize_request_usage(&session, usage_start);
        let outcome = crate::runtime::TurnOutcome {
            text: "done".to_string(),
            model_rounds: 2,
            tool_calls: 0,
            elapsed: Duration::from_millis(250),
        };
        let mut status = WorkbenchStatus::default();
        update_status(&mut status, Some((&outcome, &request_usage)), &identity);

        assert_eq!(status.rounds, 2);
        assert_eq!(status.tool_calls, 0);
        assert_eq!(status.input_tokens, 1_100_000);
        assert_eq!(status.output_tokens, 30_000);
        assert_eq!(status.context_used_tokens, 100_000);
        assert!((status.cache_hit_percent.unwrap() - 90.0).abs() < f64::EPSILON * 100.0);
        assert_eq!(
            status.cost,
            workbench_cost(&identity, &request_usage.summary)
        );
        assert_ne!(
            status.cost,
            workbench_cost(&identity, session.usage.summary()),
            "the session total must not leak into the latest-request row"
        );
        assert_eq!(status.elapsed, Some(Duration::from_millis(250)));
        let usage_start = session.usage.entries().len();
        let mut without_cache = response("without-cache", 120_000, 4_000, 0);
        let usage = without_cache.usage.as_mut().unwrap();
        usage.cache_hit_tokens = None;
        usage.cache_miss_tokens = None;
        identity.record(&mut session.usage, UsagePurpose::Chat, &without_cache);
        let request_usage = summarize_request_usage(&session, usage_start);
        update_status(&mut status, Some((&outcome, &request_usage)), &identity);
        assert_eq!(status.context_used_tokens, 120_000);
        assert_eq!(
            status.cache_hit_percent, None,
            "the workbench must not reuse cache telemetry from an earlier response"
        );

        update_status(&mut status, None, &identity);
        assert_eq!(status.rounds, 0);
        assert_eq!(status.tool_calls, 0);
        assert_eq!(status.input_tokens, 0);
        assert_eq!(status.output_tokens, 0);
        assert_eq!(
            status.context_used_tokens, 120_000,
            "clearing request telemetry must preserve conversation context"
        );
        assert_eq!(status.cache_hit_percent, None);
        assert_eq!(status.cost, None);
        assert_eq!(status.elapsed, None);
    }

    #[test]
    fn resumed_fullscreen_replays_visible_turns_after_the_fold_marker() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        session.summarized_turns = 2;
        session.begin_turn(
            "hello\nsecond line".to_string(),
            "hello\nsecond line".to_string(),
            None,
        );
        assert!(session.finish_turn("answer".to_string(), None));

        let lines = resumed_transcript_lines(&session);
        assert_eq!(lines[0], "↳ 2 earlier turns compacted");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "❯ hello");
        assert_eq!(lines[3], "  second line");
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "answer");
        assert_eq!(lines[7], "");
        assert!(lines[8].starts_with("↳ resumed · 1+2 turn"));
    }

    #[test]
    fn new_chat_intro_is_short_styled_and_not_an_assistant_message() {
        let entries = new_chat_intro_entries();
        let text = entries.iter().map(TranscriptLine::text).collect::<Vec<_>>();

        assert_eq!(
            text,
            [
                concat!("agul v", env!("CARGO_PKG_VERSION")),
                "/ commands · @ skills · type while running to steer",
                "",
                "A small terminal agent runtime for working directly in this workspace.",
            ]
        );
        assert!(
            entries
                .iter()
                .all(|line| line.kind != TranscriptKind::Assistant),
            "startup guidance must not look like model output"
        );
        assert!(entries[0].content().segments().iter().any(|segment| {
            segment.text == "agul"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Label)
                && segment.style.bold
        }));
        assert!(entries[1].content().segments().iter().any(|segment| {
            segment.text == "/"
                && segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Accent)
        }));
    }

    #[test]
    fn resumed_fullscreen_bounds_old_ui_history_without_changing_the_session() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        for index in 0..MAX_RESUMED_TRANSCRIPT_TURNS + 3 {
            let user = format!("user {index}");
            session.begin_turn(user.clone(), user, None);
            assert!(session.finish_turn(format!("answer {index}"), None));
        }

        let lines = resumed_transcript_lines(&session);
        assert_eq!(session.turns.len(), MAX_RESUMED_TRANSCRIPT_TURNS + 3);
        assert_eq!(lines[0], "↳ 3 older visible turns folded");
        assert!(!lines.iter().any(|line| line == "❯ user 0"));
        assert!(lines.iter().any(|line| line == "❯ user 3"));
        assert!(
            lines
                .last()
                .is_some_and(|line| line.starts_with("↳ resumed"))
        );
    }

    #[test]
    fn resumed_fullscreen_reuses_the_terminal_markdown_shape() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        session.begin_turn("show".to_string(), "show".to_string(), None);
        assert!(session.finish_turn("- one\n- two".to_string(), None));

        let lines = resumed_transcript_lines(&session);
        assert!(lines.iter().any(|line| line == "• one"));
        assert!(lines.iter().any(|line| line == "• two"));
    }

    #[test]
    fn resumed_fullscreen_restores_user_and_assistant_styles() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        session.begin_turn("hello".to_string(), "hello".to_string(), None);
        assert!(session.finish_turn("answer".to_string(), None));

        let entries = resumed_transcript_entries(&session, false);
        assert!(entries.contains(&TranscriptLine::new(TranscriptKind::User, "❯ hello")));
        assert!(entries.contains(&TranscriptLine::new(TranscriptKind::Assistant, "answer")));
        assert!(
            entries
                .last()
                .is_some_and(|line| line.kind == TranscriptKind::Notice)
        );
    }

    #[test]
    fn resumed_fullscreen_reuses_live_markdown_spans() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        session.begin_turn("show".to_string(), "show".to_string(), None);
        assert!(session.finish_turn("# Title\nUse `cargo test`.".to_string(), None));

        let entries = resumed_transcript_entries(&session, true);
        let heading = entries
            .iter()
            .find(|line| line.kind == TranscriptKind::Assistant && line.text() == "Title")
            .expect("styled heading");
        assert!(heading.content().segments().iter().any(|segment| {
            segment.style.paint == workbench::TranscriptPaint::Tone(TranscriptTone::Label)
                && segment.style.bold
        }));
        let code = entries
            .iter()
            .find(|line| line.kind == TranscriptKind::Assistant && line.text().contains("cargo"))
            .expect("styled inline code");
        assert!(code.content().segments().iter().any(|segment| {
            segment.text == "cargo test"
                && segment.style.paint
                    == workbench::TranscriptPaint::Tone(TranscriptTone::InlineCode)
        }));
    }

    #[test]
    fn resumed_workbench_restores_the_last_completed_turn_from_trace() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let identity = ProviderIdentity::from_endpoint("https://api.deepseek.com/v1");
        let mut session = ChatSession::new(
            root.path().to_path_buf(),
            "deepseek-v4-flash",
            Some(PriceCatalog::builtin_deepseek_usd()),
        );
        let response = |id: &str, input_tokens, output_tokens, cache_hit_tokens| {
            crate::runtime::ResponseObservation {
                response_id: Some(id.to_string()),
                requested_model: "deepseek-v4-flash".to_string(),
                reported_model: Some("deepseek-v4-flash".to_string()),
                provider_created_at: Some(1_787_788_800),
                received_at: 1_787_788_800,
                usage: Some(crate::runtime::Usage {
                    input_tokens,
                    output_tokens,
                    cache_hit_tokens: Some(cache_hit_tokens),
                    cache_miss_tokens: Some(input_tokens - cache_hit_tokens),
                    reasoning_tokens: None,
                }),
                context_window: Some(DEEPSEEK_V4_CONTEXT_WINDOW.into()),
                promoted_text: None,
            }
        };

        let previous_operation = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "previous"}))
            .unwrap();
        let previous_entry = identity
            .record(
                &mut session.usage,
                UsagePurpose::Chat,
                &response("previous", 5_000, 200, 1_000),
            )
            .clone();
        store
            .append_trace(
                &mut session,
                &previous_operation,
                "usage",
                json!({"entry": previous_entry}),
            )
            .unwrap();
        store
            .append_trace(
                &mut session,
                &previous_operation,
                "operation_completed",
                json!({"model_rounds": 8, "tool_calls": 9, "elapsed_ms": 9000}),
            )
            .unwrap();

        let current_operation = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "current"}))
            .unwrap();
        let current_entries = [
            identity
                .record(
                    &mut session.usage,
                    UsagePurpose::Chat,
                    &response("current-1", 1_000, 20, 750),
                )
                .clone(),
            identity
                .record(
                    &mut session.usage,
                    UsagePurpose::Chat,
                    &response("current-2", 100, 10, 90),
                )
                .clone(),
        ];
        for entry in &current_entries {
            store
                .append_trace(
                    &mut session,
                    &current_operation,
                    "usage",
                    json!({"entry": entry}),
                )
                .unwrap();
        }
        store
            .append_trace(
                &mut session,
                &current_operation,
                "operation_completed",
                json!({"model_rounds": 2, "tool_calls": 3, "elapsed_ms": 250}),
            )
            .unwrap();

        let compact_operation = store
            .begin_trace_operation(&mut session, "compact", json!({"turns": 6}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &compact_operation,
                "operation_completed",
                json!({"model_rounds": 1, "tool_calls": 0, "elapsed_ms": 50}),
            )
            .unwrap();
        store
            .begin_trace_operation(&mut session, "turn", json!({"input": "unfinished"}))
            .unwrap();

        let mut status = WorkbenchStatus::default();
        initialize_status(
            &mut status,
            "deepseek-v4-flash",
            Some("high"),
            Some(DEEPSEEK_V4_CONTEXT_WINDOW.into()),
        );
        restore_last_completed_turn_status(&mut status, &session, Some(&store), &identity);

        assert_eq!(status.model, "deepseek-v4-flash");
        assert_eq!(status.reasoning_effort, "high");
        assert_eq!(status.rounds, 2);
        assert_eq!(status.tool_calls, 3);
        assert_eq!(status.input_tokens, 1_100);
        assert_eq!(status.output_tokens, 30);
        assert_eq!(status.context_used_tokens, 100);
        assert_eq!(
            status.context_window_tokens,
            Some(DEEPSEEK_V4_CONTEXT_WINDOW.into())
        );
        assert!((status.cache_hit_percent.unwrap() - 90.0).abs() < f64::EPSILON * 100.0);
        assert_eq!(status.elapsed, Some(Duration::from_millis(250)));
        assert!(status.cost.is_some());
        assert_eq!(
            status.cost,
            workbench_cost(&identity, &UsageSummary::from_entries(&current_entries))
        );
        assert_ne!(
            status.cost,
            workbench_cost(&identity, session.usage.summary()),
            "an earlier turn must not leak into the restored request cost"
        );
    }

    #[test]
    fn resumed_workbench_restores_completed_turn_without_usage() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let identity = ProviderIdentity::from_endpoint("http://127.0.0.1:51100/v1");
        let mut session = ChatSession::new(root.path().to_path_buf(), "local-model", None);
        let operation_id = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "hello"}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &operation_id,
                "operation_completed",
                json!({"model_rounds": 3, "tool_calls": 2, "elapsed_ms": 750}),
            )
            .unwrap();

        let mut status = WorkbenchStatus::default();
        initialize_status(&mut status, "local-model", Some("auto"), None);
        restore_last_completed_turn_status(&mut status, &session, Some(&store), &identity);

        assert_eq!(status.model, "local-model");
        assert_eq!(status.reasoning_effort, "auto");
        assert_eq!(status.rounds, 3);
        assert_eq!(status.tool_calls, 2);
        assert_eq!(status.input_tokens, 0);
        assert_eq!(status.output_tokens, 0);
        assert_eq!(status.cache_hit_percent, None);
        assert_eq!(status.cost, None);
        assert_eq!(status.elapsed, Some(Duration::from_millis(750)));
    }

    #[test]
    fn resumed_workbench_silently_keeps_identity_for_missing_or_old_trace() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let identity = ProviderIdentity::from_endpoint("https://api.deepseek.com/v1");
        let mut session = ChatSession::new(
            root.path().to_path_buf(),
            "deepseek-v4-flash",
            Some(PriceCatalog::builtin_deepseek_usd()),
        );
        let mut status = WorkbenchStatus::default();
        initialize_status(
            &mut status,
            "deepseek-v4-flash",
            Some("high"),
            Some(DEEPSEEK_V4_CONTEXT_WINDOW.into()),
        );
        let expected = status.clone();

        restore_last_completed_turn_status(&mut status, &session, Some(&store), &identity);
        assert_eq!(status, expected);

        let operation_id = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "old"}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &operation_id,
                "operation_completed",
                json!({"rounds": 1, "tools": 0}),
            )
            .unwrap();
        restore_last_completed_turn_status(&mut status, &session, Some(&store), &identity);
        assert_eq!(status, expected);
    }

    #[test]
    fn interrupted_recovery_note_keeps_recent_tool_evidence() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "model", None);
        let operation_id = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "fix it"}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &operation_id,
                "tool_finished",
                json!({"name": "edit", "detail": "src/lib.rs", "ok": true}),
            )
            .unwrap();
        store
            .append_trace(
                &mut session,
                &operation_id,
                "operation_failed",
                json!({"error": "model-round limit reached (8)"}),
            )
            .unwrap();

        let note = interrupted_turn_note(&session, Some(&store), Some(&operation_id));
        assert!(note.contains("edit succeeded: src/lib.rs"));
        assert!(note.contains("Do not repeat successful actions"));
        assert!(note.contains("model-round limit reached (8)"));
    }

    #[test]
    fn interrupted_recovery_note_uses_the_current_stopped_operation() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "model", None);
        let failed = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "old"}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &failed,
                "tool_finished",
                json!({"name": "write", "detail": "old.txt", "ok": true}),
            )
            .unwrap();
        store
            .append_trace(
                &mut session,
                &failed,
                "operation_failed",
                json!({"error": "old failure"}),
            )
            .unwrap();
        let stopped = store
            .begin_trace_operation(&mut session, "turn", json!({"input": "current"}))
            .unwrap();
        store
            .append_trace(
                &mut session,
                &stopped,
                "tool_finished",
                json!({"name": "edit", "detail": "current.txt", "ok": true}),
            )
            .unwrap();
        store
            .append_trace(
                &mut session,
                &stopped,
                "operation_stopped",
                json!({"reason": "user"}),
            )
            .unwrap();

        let note = interrupted_turn_note(&session, Some(&store), Some(&stopped));
        assert!(note.contains("edit succeeded: current.txt"));
        assert!(note.contains("stopped by user"));
        assert!(!note.contains("old.txt"));
        assert!(!note.contains("old failure"));
    }
}
