use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::atomic_file::replace_file;
use super::billing::{PriceCatalog, UsageEntry, UsageLedger, UsageSummary};
use super::direct_chat::VisibleTurn;
use super::provider::Message;
use super::usage::{NativeConnectionPreset, NativeProvider};

pub(crate) const SESSION_SCHEMA: &str = "agul/chat-session/v5";
const TRACE_EVENT_FORMAT: &str = "agul/trace-event/v1";
const HANDOFF_OPEN: &str = "<agul-handoff format=\"agul/handoff/v1\">";
const HANDOFF_CLOSE: &str = "</agul-handoff>";
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) const INTERRUPTED_TURN_NOTE: &str = "The previous attempt stopped before producing a final response. Inspect the current workspace state before continuing.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionSource {
    #[default]
    Chat,
    Ari,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionEngine {
    #[default]
    Native,
    Codex,
}

impl std::fmt::Display for SessionEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Native => "native",
            Self::Codex => "codex",
        })
    }
}

impl std::fmt::Display for SessionSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Chat => "chat",
            Self::Ari => "ari",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionStatus {
    #[default]
    Active,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionAttribution {
    pub(crate) parent_session_id: Option<String>,
    pub(crate) delegation_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) specialist_id: Option<String>,
    pub(crate) pool_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelatedSession {
    pub(crate) relation: String,
    pub(crate) session_id: String,
    pub(crate) delegation_id: Option<String>,
    pub(crate) task_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionConfig {
    pub(crate) preset: Option<NativeConnectionPreset>,
    pub(crate) provider: Option<NativeProvider>,
    pub(crate) base_url: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingTurn {
    visible_user: String,
    model_input: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatSession {
    pub(crate) id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) model: String,
    pub(crate) engine: SessionEngine,
    pub(crate) upstream_thread_id: Option<String>,
    pub(crate) source: SessionSource,
    pub(crate) status: SessionStatus,
    pub(crate) owner_pid: u32,
    pub(crate) attribution: SessionAttribution,
    pub(crate) related_sessions: Vec<RelatedSession>,
    pub(crate) handoff: Option<Value>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) summarized_turns: u64,
    pub(crate) summary: Option<String>,
    pub(crate) turns: Vec<VisibleTurn>,
    native_config: Option<NativeSessionConfig>,
    native_history: Option<Vec<Message>>,
    pub(crate) usage: UsageLedger,
    trace_seq: u64,
    pending_turn: Option<PendingTurn>,
}

impl ChatSession {
    pub(crate) fn new(
        workspace: PathBuf,
        model: impl Into<String>,
        catalog: Option<PriceCatalog>,
    ) -> Self {
        Self::new_with_metadata(
            workspace,
            model,
            catalog,
            SessionEngine::Native,
            SessionSource::Chat,
            SessionAttribution::default(),
        )
    }

    pub(crate) fn new_codex(
        workspace: PathBuf,
        model: impl Into<String>,
        upstream_thread_id: impl Into<String>,
    ) -> Self {
        let mut session = Self::new_with_metadata(
            workspace,
            model,
            None,
            SessionEngine::Codex,
            SessionSource::Chat,
            SessionAttribution::default(),
        );
        session.upstream_thread_id = Some(upstream_thread_id.into());
        session
    }

    pub(crate) fn new_ari(
        workspace: PathBuf,
        model: impl Into<String>,
        catalog: Option<PriceCatalog>,
        attribution: SessionAttribution,
    ) -> Self {
        Self::new_with_metadata(
            workspace,
            model,
            catalog,
            SessionEngine::Native,
            SessionSource::Ari,
            attribution,
        )
    }

    pub(crate) fn new_ari_codex(
        workspace: PathBuf,
        model: impl Into<String>,
        upstream_thread_id: impl Into<String>,
        attribution: SessionAttribution,
    ) -> Self {
        let mut session = Self::new_with_metadata(
            workspace,
            model,
            None,
            SessionEngine::Codex,
            SessionSource::Ari,
            attribution,
        );
        session.upstream_thread_id = Some(upstream_thread_id.into());
        session
    }

    fn new_with_metadata(
        workspace: PathBuf,
        model: impl Into<String>,
        catalog: Option<PriceCatalog>,
        engine: SessionEngine,
        source: SessionSource,
        attribution: SessionAttribution,
    ) -> Self {
        let now = now();
        Self {
            id: new_session_id(),
            workspace,
            model: model.into(),
            engine,
            upstream_thread_id: None,
            source,
            status: SessionStatus::Active,
            owner_pid: std::process::id(),
            attribution,
            related_sessions: Vec::new(),
            handoff: None,
            created_at: now,
            updated_at: now,
            summarized_turns: 0,
            summary: None,
            turns: Vec::new(),
            native_config: None,
            native_history: None,
            usage: UsageLedger::new(catalog),
            trace_seq: 0,
            pending_turn: None,
        }
    }

    pub(crate) fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
        self.updated_at = now();
    }

    pub(crate) fn set_upstream_thread_id(&mut self, thread_id: Option<&str>) {
        self.upstream_thread_id = thread_id.map(str::to_string);
        self.updated_at = now();
    }

    pub(crate) fn replace_price_catalog(&mut self, catalog: Option<PriceCatalog>) {
        self.usage = UsageLedger::from_entries(catalog, self.usage.entries().to_vec());
        self.updated_at = now();
    }

    pub(crate) fn resume(&mut self) {
        self.owner_pid = std::process::id();
        self.set_status(SessionStatus::Active);
    }

    pub(crate) fn add_related_session(&mut self, related: RelatedSession) {
        if self
            .related_sessions
            .iter()
            .any(|existing| existing.session_id == related.session_id)
        {
            return;
        }
        self.related_sessions.push(related);
        self.updated_at = now();
    }

    pub(crate) fn set_handoff(&mut self, handoff: Value) {
        self.handoff = Some(handoff);
        self.updated_at = now();
    }

    pub(crate) fn capture_handoff(&mut self, text: &str) -> bool {
        let Some(start) = text.rfind(HANDOFF_OPEN) else {
            return false;
        };
        let remainder = &text[start + HANDOFF_OPEN.len()..];
        let Some((body, trailing)) = remainder.split_once(HANDOFF_CLOSE) else {
            return false;
        };
        let fenced = has_terminal_handoff_fence(&text[..start], trailing);
        if !trailing.trim().is_empty() && !fenced {
            return false;
        }
        let Ok(mut handoff) = serde_json::from_str::<Value>(body.trim()) else {
            return false;
        };
        let Some(object) = handoff.as_object_mut() else {
            return false;
        };
        if object.get("format").and_then(Value::as_str) != Some("agul/handoff/v1")
            || !matches!(
                object.get("status").and_then(Value::as_str),
                Some("completed" | "blocked" | "failed")
            )
            || object.get("summary").and_then(Value::as_str).is_none()
        {
            return false;
        }
        if object.get("verification").and_then(Value::as_str) == Some("required") {
            object.insert(
                "verification".to_string(),
                Value::Array(vec![Value::String("required".to_string())]),
            );
        }
        if ["evidence", "changes", "verification", "risks", "next_steps"]
            .into_iter()
            .any(|field| object.get(field).is_some_and(|value| !value.is_array()))
        {
            return false;
        }
        self.set_handoff(handoff);
        true
    }

    #[cfg(test)]
    pub(crate) fn append_turn(&mut self, turn: VisibleTurn) {
        self.turns.push(turn);
        self.updated_at = now();
    }

    pub(crate) fn begin_turn(
        &mut self,
        visible_user: String,
        model_input: String,
        native_history: Option<Vec<Message>>,
    ) {
        self.resume();
        self.native_history = native_history;
        if let Some(interrupted) = self.pending_turn.take() {
            self.turns.push(VisibleTurn {
                user: interrupted.visible_user,
                assistant: INTERRUPTED_TURN_NOTE.to_string(),
            });
        }
        self.pending_turn = Some(PendingTurn {
            visible_user,
            model_input,
        });
        self.updated_at = now();
    }

    pub(crate) fn finish_turn(
        &mut self,
        assistant: String,
        native_history: Option<Vec<Message>>,
    ) -> bool {
        let Some(completed) = self.pending_turn.take() else {
            return false;
        };
        self.native_history = native_history;
        self.turns.push(VisibleTurn {
            user: completed.visible_user,
            assistant,
        });
        self.updated_at = now();
        true
    }

    pub(crate) fn settle_interrupted_turn(&mut self) -> bool {
        if !matches!(
            self.status,
            SessionStatus::Active | SessionStatus::Interrupted
        ) {
            return false;
        }
        let changed = self.status != SessionStatus::Interrupted || self.pending_turn.is_some();
        if !changed {
            return false;
        }
        if let Some(interrupted) = self.pending_turn.take() {
            self.turns.push(VisibleTurn {
                user: interrupted.visible_user,
                assistant: INTERRUPTED_TURN_NOTE.to_string(),
            });
        }
        self.set_status(SessionStatus::Interrupted);
        true
    }

    pub(crate) fn pending_model_input(&self) -> Option<&str> {
        self.pending_turn
            .as_ref()
            .map(|pending| pending.model_input.as_str())
    }

    pub(crate) fn pending_visible_user(&self) -> Option<&str> {
        self.pending_turn
            .as_ref()
            .map(|pending| pending.visible_user.as_str())
    }

    pub(crate) fn has_resumable_history(&self) -> bool {
        !self.turns.is_empty() || self.summarized_turns > 0 || self.pending_turn.is_some()
    }

    pub(crate) fn native_history(&self) -> Option<&[Message]> {
        self.native_history.as_deref()
    }

    pub(crate) fn native_config(&self) -> Option<&NativeSessionConfig> {
        self.native_config.as_ref()
    }

    pub(crate) fn set_native_config(&mut self, config: Option<NativeSessionConfig>) {
        self.native_config = config;
        self.updated_at = now();
    }

    pub(crate) fn set_native_history(&mut self, history: Option<Vec<Message>>) {
        self.native_history = history;
        self.updated_at = now();
    }

    pub(crate) fn clear(&mut self) {
        self.summary = None;
        self.summarized_turns = 0;
        self.turns.clear();
        self.native_history = None;
        self.pending_turn = None;
        self.updated_at = now();
    }

    pub(crate) fn compaction_source(&self, retain_recent: usize) -> &[VisibleTurn] {
        let count = self.turns.len().saturating_sub(retain_recent);
        &self.turns[..count]
    }

    /// Apply a completed semantic summary. The provider call happens before
    /// this method, so a failed compaction leaves the visible turns unchanged.
    pub(crate) fn commit_compaction(&mut self, retain_recent: usize, summary: String) -> usize {
        let count = self.turns.len().saturating_sub(retain_recent);
        if count == 0 {
            return 0;
        }
        let recent = self.turns.split_off(count);
        self.turns = recent;
        self.summarized_turns = self.summarized_turns.saturating_add(count as u64);
        self.summary = Some(match self.summary.take() {
            Some(previous) if !previous.trim().is_empty() => {
                format!("{}\n\n{}", previous.trim(), summary.trim())
            }
            _ => summary.trim().to_string(),
        });
        self.native_history = None;
        self.updated_at = now();
        count
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) engine: SessionEngine,
    pub(crate) subscription_quota: bool,
    pub(crate) workspace: PathBuf,
    pub(crate) source: SessionSource,
    pub(crate) status: SessionStatus,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) turns: usize,
    pub(crate) summarized_turns: u64,
    pub(crate) pending: bool,
    pub(crate) preview: Option<String>,
    pub(crate) in_use: bool,
    pub(crate) related_sessions: usize,
    pub(crate) usage: Option<UsageSummary>,
}

impl SessionInfo {
    fn is_resumable_chat(&self) -> bool {
        self.source == SessionSource::Chat
            && self.parent_session_id.is_none()
            && !self.in_use
            && (self.turns > 0 || self.summarized_turns > 0 || self.pending)
    }
}

fn session_preview(value: &str) -> Option<String> {
    const MAX_CHARS: usize = 240;

    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return None;
    }
    let mut characters = compact.chars();
    let preview = characters.by_ref().take(MAX_CHARS).collect::<String>();
    Some(if characters.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    })
}

fn has_terminal_handoff_fence(prefix: &str, trailing: &str) -> bool {
    let Some(prefix) = prefix
        .strip_suffix("\r\n")
        .or_else(|| prefix.strip_suffix('\n'))
    else {
        return false;
    };
    let Some(fence) = prefix.lines().next_back() else {
        return false;
    };
    let Some(language) = fence.trim().strip_prefix("```") else {
        return false;
    };
    let valid_opening_fence = language
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if !valid_opening_fence {
        return false;
    }

    let Some(trailing) = trailing
        .strip_prefix("\r\n")
        .or_else(|| trailing.strip_prefix('\n'))
    else {
        return false;
    };
    let (closing_fence, remainder) = trailing
        .split_once('\n')
        .map_or((trailing, ""), |(line, remainder)| (line, remainder));
    closing_fence.trim_end_matches('\r').trim() == "```" && remainder.trim().is_empty()
}

#[derive(Clone, Debug)]
pub(crate) struct SessionStore {
    sessions_root: PathBuf,
    traces_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsageDetail {
    Omit,
    Aggregate,
}

#[derive(Debug)]
pub(crate) struct SessionLease {
    _file: File,
}

pub(crate) struct TraceAppender {
    session_id: String,
    path: PathBuf,
    file: File,
}

impl SessionStore {
    pub(crate) fn discover(override_root: Option<&Path>) -> Result<Self, SessionError> {
        let state_root = override_root
            .map(Path::to_path_buf)
            .or_else(default_state_root)
            .ok_or_else(|| SessionError::new("could not determine the user state directory"))?;
        let sessions_root = state_root.join("sessions");
        let traces_root = state_root.join("traces");
        for root in [&sessions_root, &traces_root] {
            fs::create_dir_all(root).map_err(|error| {
                SessionError::new(format!("could not create {}: {error}", root.display()))
            })?;
        }
        Ok(Self {
            sessions_root,
            traces_root,
        })
    }

    pub(crate) fn save(&self, session: &ChatSession) -> Result<(), SessionError> {
        let path = self.session_path(&session.id)?;
        let document = StoredSession::from_session(session);
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| SessionError::new(format!("could not serialize session: {error}")))?;
        replace_file(&path, &bytes).map_err(|error| {
            SessionError::new(format!("could not save {}: {error}", path.display()))
        })
    }

    pub(crate) fn settle_interrupted_related_sessions(
        &self,
        parent_session_id: &str,
        related_sessions: &[RelatedSession],
    ) -> Result<(), SessionError> {
        let mut errors = Vec::new();
        for related in related_sessions
            .iter()
            .filter(|related| related.relation == "delegated")
        {
            let mut child = match self.load(&related.session_id, None) {
                Ok(child) => child,
                Err(error) => {
                    errors.push(format!("{}: {error}", related.session_id));
                    continue;
                }
            };
            if child.source != SessionSource::Ari
                || child.attribution.parent_session_id.as_deref() != Some(parent_session_id)
            {
                continue;
            }
            if let Err(error) = self.persist_interrupted_session(&mut child, "parent_stopped") {
                errors.push(format!("{}: {error}", related.session_id));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SessionError::new(format!(
                "could not settle every delegated session: {}",
                errors.join("; ")
            )))
        }
    }

    pub(crate) fn load(
        &self,
        id: &str,
        catalog: Option<PriceCatalog>,
    ) -> Result<ChatSession, SessionError> {
        let path = self.session_path(id)?;
        let bytes = fs::read(&path).map_err(|error| {
            SessionError::new(format!("could not read {}: {error}", path.display()))
        })?;
        let stored: StoredSession = serde_json::from_slice(&bytes).map_err(|error| {
            SessionError::new(format!("could not parse {}: {error}", path.display()))
        })?;
        let mut session = stored.into_session(catalog)?;
        if let Some(trace_seq) = self.last_trace_seq(&session.id)? {
            session.trace_seq = session.trace_seq.max(trace_seq);
        }
        let owner_exited = owner_exited_while_active(&session);
        let delegated_ari =
            session.source == SessionSource::Ari && session.attribution.parent_session_id.is_some();
        if delegated_ari && (owner_exited || session.status == SessionStatus::Interrupted) {
            let reason = if owner_exited {
                "owner_exited"
            } else {
                "interrupted_recovery"
            };
            self.persist_interrupted_session(&mut session, reason)?;
        } else if owner_exited {
            session.set_status(SessionStatus::Interrupted);
        }
        Ok(session)
    }

    fn persist_interrupted_session(
        &self,
        session: &mut ChatSession,
        reason: &str,
    ) -> Result<(), SessionError> {
        if !matches!(
            session.status,
            SessionStatus::Active | SessionStatus::Interrupted
        ) {
            return Ok(());
        }
        let state_changed = session.settle_interrupted_turn();
        let unfinished = self.unfinished_operation_ids(&session.id)?;
        for operation_id in &unfinished {
            self.append_trace(
                session,
                operation_id,
                "operation_stopped",
                serde_json::json!({"reason": reason}),
            )?;
        }
        if state_changed || !unfinished.is_empty() {
            self.save(session)?;
        }
        Ok(())
    }

    fn unfinished_operation_ids(&self, id: &str) -> Result<Vec<String>, SessionError> {
        let mut unfinished = Vec::new();
        for line in self.read_trace(id)?.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(operation_id) = event.get("operation_id").and_then(Value::as_str) else {
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("operation_started") => {
                    if !unfinished.iter().any(|open| open == operation_id) {
                        unfinished.push(operation_id.to_string());
                    }
                }
                Some("operation_completed" | "operation_failed" | "operation_stopped") => {
                    unfinished.retain(|open| open != operation_id);
                }
                _ => {}
            }
        }
        Ok(unfinished)
    }

    pub(crate) fn session_engine(&self, id: &str) -> Result<SessionEngine, SessionError> {
        let path = self.session_path(id)?;
        let bytes = fs::read(&path).map_err(|error| {
            SessionError::new(format!("could not read {}: {error}", path.display()))
        })?;
        let stored: StoredSession = serde_json::from_slice(&bytes).map_err(|error| {
            SessionError::new(format!("could not parse {}: {error}", path.display()))
        })?;
        if stored.schema != SESSION_SCHEMA {
            return Err(SessionError::new(format!(
                "unsupported session format: {}",
                stored.schema
            )));
        }
        Ok(stored.engine)
    }

    pub(crate) fn session_native_config(
        &self,
        id: &str,
    ) -> Result<Option<NativeSessionConfig>, SessionError> {
        let path = self.session_path(id)?;
        let bytes = fs::read(&path).map_err(|error| {
            SessionError::new(format!("could not read {}: {error}", path.display()))
        })?;
        let stored: StoredSession = serde_json::from_slice(&bytes).map_err(|error| {
            SessionError::new(format!("could not parse {}: {error}", path.display()))
        })?;
        if stored.schema != SESSION_SCHEMA {
            return Err(SessionError::new(format!(
                "unsupported session format: {}",
                stored.schema
            )));
        }
        Ok(stored.native_config)
    }

    pub(crate) fn list(&self) -> Result<Vec<SessionInfo>, SessionError> {
        self.list_sessions(None, UsageDetail::Aggregate)
    }

    fn list_sessions(
        &self,
        workspace: Option<&Path>,
        usage_detail: UsageDetail,
    ) -> Result<Vec<SessionInfo>, SessionError> {
        let workspace = workspace.map(canonical_workspace);
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_root).map_err(|error| {
            SessionError::new(format!(
                "could not list {}: {error}",
                self.sessions_root.display()
            ))
        })? {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.extension().is_none_or(|value| value != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if document.get("schema").and_then(Value::as_str) != Some(SESSION_SCHEMA) {
                continue;
            }
            if let Some(workspace) = workspace.as_deref() {
                let Some(stored_workspace) = document.get("workspace").and_then(Value::as_str)
                else {
                    continue;
                };
                if !same_workspace(Path::new(stored_workspace), workspace) {
                    continue;
                }
            }
            let stored = match serde_json::from_value::<StoredSession>(document) {
                Ok(stored) => stored,
                Err(error) if workspace.is_some() => {
                    return Err(SessionError::new(format!(
                        "could not parse {}: {error}",
                        path.display()
                    )));
                }
                Err(_) => continue,
            };
            let owner_alive = process_is_alive(stored.owner_pid);
            let status = if stored.status == SessionStatus::Active && !owner_alive {
                SessionStatus::Interrupted
            } else {
                stored.status
            };
            let in_use = owner_alive
                && matches!(
                    status,
                    SessionStatus::Active | SessionStatus::Failed | SessionStatus::Interrupted
                );
            let preview = stored
                .pending_turn
                .as_ref()
                .map(|pending| pending.visible_user.as_str())
                .or_else(|| stored.turns.last().map(|turn| turn.user.as_str()))
                .or(stored.summary.as_deref())
                .and_then(session_preview);
            let subscription_quota = stored.engine == SessionEngine::Codex
                || stored.native_config.as_ref().is_some_and(|config| {
                    config
                        .preset
                        .or_else(|| {
                            NativeConnectionPreset::from_official_endpoint(&config.base_url)
                        })
                        .is_some_and(NativeConnectionPreset::is_subscription)
                });
            let mut info = SessionInfo {
                id: stored.id,
                model: stored.model,
                engine: stored.engine,
                subscription_quota,
                workspace: stored.workspace,
                source: stored.source,
                status,
                parent_session_id: stored.attribution.parent_session_id,
                created_at: stored.created_at,
                updated_at: stored.updated_at,
                turns: stored.turns.len(),
                summarized_turns: stored.summarized_turns,
                pending: stored.pending_turn.is_some(),
                preview,
                in_use,
                related_sessions: stored.related_sessions.len(),
                usage: None,
            };
            if workspace.is_some() && !info.is_resumable_chat() {
                continue;
            }
            if usage_detail == UsageDetail::Aggregate {
                info.usage = Some(
                    self.load(&info.id, None)
                        .and_then(|session| self.aggregate_usage(&session))
                        .unwrap_or_else(|_| {
                            UsageLedger::from_entries(None, stored.usage)
                                .summary()
                                .clone()
                        }),
                );
            }
            sessions.push(info);
        }
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(sessions)
    }

    pub(crate) fn resumable_chats(
        &self,
        workspace: &Path,
    ) -> Result<Vec<SessionInfo>, SessionError> {
        self.list_sessions(Some(workspace), UsageDetail::Omit)
    }

    pub(crate) fn lease_session(&self, id: &str) -> Result<SessionLease, SessionError> {
        let path = self.session_lock_path(id)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                SessionError::new(format!("could not open {}: {error}", path.display()))
            })?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => SessionError::new(format!(
                "session {id} is already open in another Agul process"
            )),
            std::fs::TryLockError::Error(error) => {
                SessionError::new(format!("could not lock {}: {error}", path.display()))
            }
        })?;
        Ok(SessionLease { _file: file })
    }

    fn session_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(id)?;
        Ok(self.sessions_root.join(format!("{id}.json")))
    }

    fn session_lock_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(id)?;
        Ok(self.sessions_root.join(format!("{id}.lock")))
    }

    fn trace_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(id)?;
        Ok(self.traces_root.join(format!("{id}.ndjson")))
    }

    pub(crate) fn append_trace(
        &self,
        session: &mut ChatSession,
        operation_id: &str,
        kind: &str,
        data: Value,
    ) -> Result<u64, SessionError> {
        validate_trace_fields(operation_id, kind)?;
        self.trace_appender(session)?
            .append(session, operation_id, kind, data)
    }

    pub(crate) fn trace_appender(
        &self,
        session: &ChatSession,
    ) -> Result<TraceAppender, SessionError> {
        let path = self.trace_path(&session.id)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                SessionError::new(format!("could not open {}: {error}", path.display()))
            })?;
        if file.metadata().is_ok_and(|metadata| metadata.len() > 0) {
            file.seek(SeekFrom::End(-1)).map_err(|error| {
                SessionError::new(format!("could not inspect {}: {error}", path.display()))
            })?;
            let mut tail = [0_u8; 1];
            file.read_exact(&mut tail).map_err(|error| {
                SessionError::new(format!("could not inspect {}: {error}", path.display()))
            })?;
            if tail[0] != b'\n' {
                file.write_all(b"\n").map_err(|error| {
                    SessionError::new(format!("could not repair {}: {error}", path.display()))
                })?;
            }
        }
        Ok(TraceAppender {
            session_id: session.id.clone(),
            path,
            file,
        })
    }

    pub(crate) fn begin_trace_operation(
        &self,
        session: &mut ChatSession,
        kind: &str,
        data: Value,
    ) -> Result<String, SessionError> {
        let operation_id = format!("{kind}-{}", session.trace_seq.saturating_add(1));
        self.append_trace(session, &operation_id, "operation_started", data)?;
        Ok(operation_id)
    }

    pub(crate) fn read_trace(&self, id: &str) -> Result<String, SessionError> {
        let path = self.trace_path(id)?;
        if !path.exists() {
            return Ok(String::new());
        }
        fs::read(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|error| {
                SessionError::new(format!("could not read {}: {error}", path.display()))
            })
    }

    fn last_trace_seq(&self, id: &str) -> Result<Option<u64>, SessionError> {
        let trace = self.read_trace(id)?;
        Ok(trace.lines().rev().find_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()?
                .get("seq")?
                .as_u64()
        }))
    }

    pub(crate) fn aggregate_usage(
        &self,
        session: &ChatSession,
    ) -> Result<UsageSummary, SessionError> {
        let mut visited = HashSet::from([session.id.clone()]);
        self.aggregate_usage_inner(session, &mut visited)
    }

    pub(crate) fn aggregate_price_catalogs(
        &self,
        session: &ChatSession,
    ) -> Result<Vec<(String, String)>, SessionError> {
        let mut visited = HashSet::from([session.id.clone()]);
        let mut catalogs = Vec::new();
        self.collect_price_catalogs(session, &mut visited, &mut catalogs)?;
        catalogs.sort();
        catalogs.dedup();
        Ok(catalogs)
    }

    fn aggregate_usage_inner(
        &self,
        session: &ChatSession,
        visited: &mut HashSet<String>,
    ) -> Result<UsageSummary, SessionError> {
        let mut summary = session.usage.summary().clone();
        for related in &session.related_sessions {
            if related.relation != "delegated" || !visited.insert(related.session_id.clone()) {
                continue;
            }
            let child = self.load(&related.session_id, None)?;
            summary.merge(&self.aggregate_usage_inner(&child, visited)?);
        }
        Ok(summary)
    }

    fn collect_price_catalogs(
        &self,
        session: &ChatSession,
        visited: &mut HashSet<String>,
        catalogs: &mut Vec<(String, String)>,
    ) -> Result<(), SessionError> {
        catalogs.extend(session.usage.entries().iter().filter_map(|entry| {
            entry
                .price_ref
                .as_ref()
                .map(|price| (price.catalog_id.clone(), price.catalog_version.clone()))
        }));
        for related in &session.related_sessions {
            if related.relation != "delegated" || !visited.insert(related.session_id.clone()) {
                continue;
            }
            let child = self.load(&related.session_id, None)?;
            self.collect_price_catalogs(&child, visited, catalogs)?;
        }
        Ok(())
    }
}

fn canonical_workspace(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_workspace(left: &Path, canonical_right: &Path) -> bool {
    let left = canonical_workspace(left);
    let right = canonical_right;
    if left == right {
        return true;
    }
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

impl TraceAppender {
    pub(crate) fn append(
        &mut self,
        session: &mut ChatSession,
        operation_id: &str,
        kind: &str,
        data: Value,
    ) -> Result<u64, SessionError> {
        validate_trace_fields(operation_id, kind)?;
        if session.id != self.session_id {
            return Err(SessionError::new(
                "trace appender belongs to a different session",
            ));
        }
        let next_seq = session.trace_seq.saturating_add(1);
        let event = StoredTraceEvent {
            format: TRACE_EVENT_FORMAT,
            seq: next_seq,
            timestamp: now(),
            operation_id,
            kind,
            data,
        };
        let mut bytes = serde_json::to_vec(&event)
            .map_err(|error| SessionError::new(format!("could not serialize trace: {error}")))?;
        bytes.push(b'\n');
        self.file.write_all(&bytes).map_err(|error| {
            SessionError::new(format!("could not append {}: {error}", self.path.display()))
        })?;
        session.trace_seq = next_seq;
        Ok(next_seq)
    }
}

fn validate_trace_fields(operation_id: &str, kind: &str) -> Result<(), SessionError> {
    if operation_id.trim().is_empty() || kind.trim().is_empty() {
        return Err(SessionError::new(
            "trace operation_id and type must not be empty",
        ));
    }
    Ok(())
}

fn validate_session_id(id: &str) -> Result<(), SessionError> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(SessionError::new(
            "session id contains unsupported characters",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct StoredTraceEvent<'a> {
    format: &'static str,
    seq: u64,
    timestamp: u64,
    operation_id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    data: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSession {
    schema: String,
    id: String,
    workspace: PathBuf,
    model: String,
    engine: SessionEngine,
    upstream_thread_id: Option<String>,
    source: SessionSource,
    status: SessionStatus,
    owner_pid: u32,
    attribution: SessionAttribution,
    related_sessions: Vec<RelatedSession>,
    handoff: Option<Value>,
    created_at: u64,
    updated_at: u64,
    summarized_turns: u64,
    summary: Option<String>,
    turns: Vec<VisibleTurn>,
    native_config: Option<NativeSessionConfig>,
    native_history: Option<Vec<Message>>,
    pending_turn: Option<PendingTurn>,
    usage: Vec<UsageEntry>,
    trace_seq: u64,
}

impl StoredSession {
    fn from_session(session: &ChatSession) -> Self {
        Self {
            schema: SESSION_SCHEMA.to_string(),
            id: session.id.clone(),
            workspace: session.workspace.clone(),
            model: session.model.clone(),
            engine: session.engine,
            upstream_thread_id: session.upstream_thread_id.clone(),
            source: session.source,
            status: session.status,
            owner_pid: session.owner_pid,
            attribution: session.attribution.clone(),
            related_sessions: session.related_sessions.clone(),
            handoff: session.handoff.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            summarized_turns: session.summarized_turns,
            summary: session.summary.clone(),
            turns: session.turns.clone(),
            native_config: session.native_config.clone(),
            native_history: session.native_history.clone(),
            pending_turn: session.pending_turn.clone(),
            usage: session.usage.entries().to_vec(),
            trace_seq: session.trace_seq,
        }
    }

    fn into_session(self, catalog: Option<PriceCatalog>) -> Result<ChatSession, SessionError> {
        if self.schema != SESSION_SCHEMA {
            return Err(SessionError::new(format!(
                "unsupported session format: {}",
                self.schema
            )));
        }
        Ok(ChatSession {
            id: self.id,
            workspace: self.workspace,
            model: self.model,
            engine: self.engine,
            upstream_thread_id: self.upstream_thread_id,
            source: self.source,
            status: self.status,
            owner_pid: self.owner_pid,
            attribution: self.attribution,
            related_sessions: self.related_sessions,
            handoff: self.handoff,
            created_at: self.created_at,
            updated_at: self.updated_at,
            summarized_turns: self.summarized_turns,
            summary: self.summary,
            turns: self.turns,
            native_config: self.native_config,
            native_history: self.native_history,
            usage: UsageLedger::from_entries(catalog, self.usage),
            trace_seq: self.trace_seq,
            pending_turn: self.pending_turn,
        })
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{sequence}", std::process::id())
}

fn owner_exited_while_active(session: &ChatSession) -> bool {
    session.status == SessionStatus::Active && !process_is_alive(session.owner_pid)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is checked before use and closed exactly once.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let succeeded = GetExitCodeProcess(handle, &mut exit_code) != 0;
        CloseHandle(handle);
        succeeded && exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }

    // Signal zero performs existence/permission checking without sending a signal.
    unsafe { kill(pid, 0) == 0 }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn default_state_root() -> Option<PathBuf> {
    if let Some(path) = env::var_os("LOCALAPPDATA").or_else(|| env::var_os("APPDATA")) {
        return Some(PathBuf::from(path).join("Agul"));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(path).join("agul"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/agul"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug)]
pub(crate) struct SessionError(String);

impl SessionError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::super::provider::{Completion, ToolCall};
    use super::*;

    #[test]
    fn round_trips_visible_turns_native_history_and_usage_entries() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(
            root.path().to_path_buf(),
            "model",
            Some(PriceCatalog::builtin_deepseek_usd()),
        );
        let native_config = NativeSessionConfig {
            preset: Some(NativeConnectionPreset::Deepseek),
            provider: Some(NativeProvider::Deepseek),
            base_url: "https://api.deepseek.com".to_string(),
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            reasoning_effort: Some("high".to_string()),
        };
        session.set_native_config(Some(native_config.clone()));
        session.append_turn(VisibleTurn {
            user: "fix it".to_string(),
            assistant: "done".to_string(),
        });
        let native_history = vec![
            Message::system("stable system prefix"),
            Message::user("expanded skill input"),
            Message::assistant(&Completion {
                response_id: Some("response-tool".to_string()),
                reported_model: Some("model".to_string()),
                provider_created_at: None,
                content: None,
                reasoning: Some("retain this tool-call reasoning".to_string()),
                promoted_reasoning: false,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "Cargo.toml"}),
                }],
                usage: None,
            }),
            Message::tool("call-1", "exact tool result"),
            Message::assistant_text("done"),
        ];
        session.begin_turn(
            "use @skill:review".to_string(),
            "use @skill:review\n\nActivated Skills:\nreview body".to_string(),
            Some(native_history.clone()),
        );
        store.save(&session).unwrap();
        let loaded = store
            .load(&session.id, Some(PriceCatalog::builtin_deepseek_usd()))
            .unwrap();
        assert_eq!(loaded.turns, session.turns);
        assert_eq!(loaded.native_config(), Some(&native_config));
        assert_eq!(
            store.session_native_config(&session.id).unwrap(),
            Some(native_config)
        );
        assert_eq!(loaded.native_history(), Some(native_history.as_slice()));
        assert_eq!(loaded.pending_visible_user(), Some("use @skill:review"));
        assert!(
            loaded
                .pending_model_input()
                .unwrap()
                .contains("review body")
        );
        assert_eq!(loaded.usage.entries(), session.usage.entries());
        assert_eq!(loaded.source, SessionSource::Chat);
        assert_eq!(loaded.status, SessionStatus::Active);
        assert_eq!(loaded.owner_pid, std::process::id());
    }

    #[test]
    fn creates_unique_session_ids_within_one_process() {
        let first = ChatSession::new(PathBuf::from("."), "model", None);
        let second = ChatSession::new(PathBuf::from("."), "model", None);
        assert_ne!(first.id, second.id);
    }

    #[cfg(unix)]
    #[test]
    fn unix_process_liveness_rejects_non_pid_values() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(i32::MAX as u32 + 1));
        assert!(!process_is_alive(u32::MAX));
    }

    #[test]
    fn atomically_replaces_an_existing_session_file() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "model", None);
        session.append_turn(VisibleTurn {
            user: "first".to_string(),
            assistant: "saved".to_string(),
        });
        store.save(&session).unwrap();

        session.append_turn(VisibleTurn {
            user: "second".to_string(),
            assistant: "replaced".to_string(),
        });
        store.save(&session).unwrap();

        let loaded = store.load(&session.id, None).unwrap();
        assert_eq!(loaded.turns, session.turns);
        assert_eq!(fs::read_dir(&store.sessions_root).unwrap().count(), 1);
    }

    #[test]
    fn session_lease_prevents_two_processes_from_resuming_the_same_chat() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let session = ChatSession::new(root.path().to_path_buf(), "model", None);

        let first = store.lease_session(&session.id).unwrap();
        let error = store.lease_session(&session.id).unwrap_err().to_string();
        assert!(error.contains("already open"));

        drop(first);
        store.lease_session(&session.id).unwrap();
    }

    #[test]
    fn session_list_marks_glm_coding_as_subscription_quota() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "glm-4.7", None);
        session.set_native_config(Some(NativeSessionConfig {
            preset: Some(NativeConnectionPreset::GlmCoding),
            provider: Some(NativeProvider::Glm),
            base_url: "https://proxy.example/v1".to_string(),
            api_key_env: Some("GLM_API_KEY".to_string()),
            reasoning_effort: None,
        }));
        store.save(&session).unwrap();

        let listed = store
            .list()
            .unwrap()
            .into_iter()
            .find(|info| info.id == session.id)
            .unwrap();

        assert!(listed.subscription_quota);
    }

    #[test]
    fn resumable_chats_skip_children_live_sessions_and_empty_shells() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(root.path()).unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();

        let mut completed = ChatSession::new(workspace.clone(), "completed", None);
        completed.append_turn(VisibleTurn {
            user: "older work".to_string(),
            assistant: "done".to_string(),
        });
        completed
            .usage
            .record(super::super::billing::ResponseUsage {
                purpose: super::super::billing::UsagePurpose::Chat,
                provider: "test".to_string(),
                origin: "http://localhost".to_string(),
                response_id: None,
                observed_at_unix_seconds: 1,
                observation_time_source: super::super::billing::ObservationTimeSource::Host,
                reported_model: "completed".to_string(),
                usage: None,
            });
        completed.status = SessionStatus::Completed;
        completed.created_at = 10;
        completed.updated_at = 10;

        let mut pending = ChatSession::new(workspace.clone(), "pending", None);
        pending.begin_turn("latest repair".to_string(), "expanded".to_string(), None);
        pending.status = SessionStatus::Failed;
        pending.owner_pid = u32::MAX;
        pending.created_at = 20;
        pending.updated_at = 20;

        let mut empty = ChatSession::new(workspace.clone(), "empty", None);
        empty.status = SessionStatus::Completed;
        empty.created_at = 30;
        empty.updated_at = 30;

        let mut live = ChatSession::new(workspace.clone(), "live", None);
        live.append_turn(VisibleTurn {
            user: "still open".to_string(),
            assistant: "waiting".to_string(),
        });
        live.status = SessionStatus::Failed;
        live.owner_pid = std::process::id();
        live.created_at = 40;
        live.updated_at = 40;

        let mut child = ChatSession::new(workspace.clone(), "child", None);
        child.append_turn(VisibleTurn {
            user: "delegated".to_string(),
            assistant: "done".to_string(),
        });
        child.attribution.parent_session_id = Some(completed.id.clone());
        child.status = SessionStatus::Completed;

        let mut ari = ChatSession::new_ari(
            workspace.clone(),
            "ari",
            None,
            SessionAttribution::default(),
        );
        ari.append_turn(VisibleTurn {
            user: "worker".to_string(),
            assistant: "done".to_string(),
        });
        ari.status = SessionStatus::Completed;

        let mut elsewhere =
            ChatSession::new(fs::canonicalize(other.path()).unwrap(), "elsewhere", None);
        elsewhere.append_turn(VisibleTurn {
            user: "other workspace".to_string(),
            assistant: "done".to_string(),
        });
        elsewhere.status = SessionStatus::Completed;

        for session in [
            &completed, &pending, &empty, &live, &child, &ari, &elsewhere,
        ] {
            store.save(session).unwrap();
        }

        let resumable = store.resumable_chats(&workspace).unwrap();
        assert_eq!(
            resumable
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            [pending.id.as_str(), completed.id.as_str()]
        );
        assert_eq!(resumable[0].preview.as_deref(), Some("latest repair"));
        assert!(resumable[0].pending);
        assert!(resumable.iter().all(|session| session.usage.is_none()));
    }

    #[test]
    fn resumable_chat_order_is_stable_when_timestamps_match() {
        let root = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(root.path()).unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut sessions = (0..2)
            .map(|index| {
                let mut session = ChatSession::new(workspace.clone(), "model", None);
                session.append_turn(VisibleTurn {
                    user: format!("work {index}"),
                    assistant: "done".to_string(),
                });
                session.status = SessionStatus::Completed;
                session.created_at = 1;
                session.updated_at = 1;
                session
            })
            .collect::<Vec<_>>();
        for session in &sessions {
            store.save(session).unwrap();
        }
        sessions.sort_by(|left, right| right.id.cmp(&left.id));

        let resumable = store.resumable_chats(&workspace).unwrap();
        assert_eq!(resumable[0].id, sessions[0].id);
        assert_eq!(resumable[1].id, sessions[1].id);
    }

    #[test]
    fn resumable_chat_discovery_ignores_legacy_and_reports_current_v5_damage() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let workspace = fs::canonicalize(root.path()).unwrap();
        let mut valid = ChatSession::new(workspace.clone(), "valid", None);
        valid.append_turn(VisibleTurn {
            user: "keep working".to_string(),
            assistant: "done".to_string(),
        });
        valid.status = SessionStatus::Completed;
        store.save(&valid).unwrap();
        fs::write(
            root.path().join("sessions/legacy.json"),
            serde_json::to_vec(&serde_json::json!({"schema": "agul/chat-session/v3"})).unwrap(),
        )
        .unwrap();
        fs::write(root.path().join("sessions/garbled.json"), b"{").unwrap();
        fs::write(
            root.path().join("sessions/unattributed.json"),
            serde_json::to_vec(&serde_json::json!({"schema": SESSION_SCHEMA})).unwrap(),
        )
        .unwrap();
        fs::write(
            root.path().join("sessions/other.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": SESSION_SCHEMA,
                "workspace": other.path()
            }))
            .unwrap(),
        )
        .unwrap();
        let resumable = store.resumable_chats(root.path()).unwrap();
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].id, valid.id);

        fs::write(
            root.path().join("sessions/broken.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": SESSION_SCHEMA,
                "workspace": root.path()
            }))
            .unwrap(),
        )
        .unwrap();

        let error = store.resumable_chats(root.path()).unwrap_err().to_string();
        assert!(error.contains("could not parse"));
        assert!(error.contains("broken.json"));
    }

    #[test]
    fn persists_ari_attribution_relations_and_handoff() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut parent = ChatSession::new(root.path().to_path_buf(), "master", None);
        let mut child = ChatSession::new_ari(
            root.path().to_path_buf(),
            "worker",
            None,
            SessionAttribution {
                parent_session_id: Some(parent.id.clone()),
                delegation_id: Some("delegation-1".to_string()),
                task_id: Some("task-1".to_string()),
                specialist_id: Some("repo-scout".to_string()),
                pool_id: Some("local".to_string()),
            },
        );
        child.set_handoff(serde_json::json!({
            "format": "agul/handoff/v1",
            "summary": "done"
        }));
        parent.add_related_session(RelatedSession {
            relation: "delegated".to_string(),
            session_id: child.id.clone(),
            delegation_id: Some("delegation-1".to_string()),
            task_id: Some("task-1".to_string()),
        });
        store.save(&child).unwrap();
        store.save(&parent).unwrap();

        let loaded_child = store.load(&child.id, None).unwrap();
        let loaded_parent = store.load(&parent.id, None).unwrap();
        assert_eq!(loaded_child.source, SessionSource::Ari);
        assert_eq!(
            loaded_child.attribution.specialist_id.as_deref(),
            Some("repo-scout")
        );
        assert_eq!(loaded_child.handoff, child.handoff);
        assert_eq!(loaded_parent.related_sessions, parent.related_sessions);
    }

    #[test]
    fn captures_an_exact_versioned_handoff_block() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(session.capture_handoff(
            "done\n<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"ok\"}</agul-handoff>"
        ));
        assert_eq!(session.handoff.as_ref().unwrap()["status"], "completed");
        assert!(
            !session.capture_handoff("<agul-handoff format=\"agul/handoff/v0\">{}</agul-handoff>")
        );
        assert!(!session.capture_handoff(
            "<agul-handoff format=\"agul/handoff/v1\">{\"status\":\"completed\",\"summary\":\"missing format\"}</agul-handoff>"
        ));
        assert!(!session.capture_handoff(
            "<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"not final\"}</agul-handoff> trailing"
        ));

        let mut fenced = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(fenced.capture_handoff(
            "Finished.\n```text\n<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"fenced\",\"verification\":[]}</agul-handoff>\n```"
        ));
        assert_eq!(fenced.handoff.as_ref().unwrap()["summary"], "fenced");

        let mut opening_fence_on_same_line = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(!opening_fence_on_same_line.capture_handoff(
            "```text<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"not fenced\"}</agul-handoff>\n```"
        ));

        let mut closing_fence_on_same_line = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(!closing_fence_on_same_line.capture_handoff(
            "```text\n<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"not fenced\"}</agul-handoff>```"
        ));

        let mut scalar_verification = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(scalar_verification.capture_handoff(
            "<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"needs verification\",\"verification\":\"required\"}</agul-handoff>"
        ));
        assert_eq!(
            scalar_verification.handoff.as_ref().unwrap()["verification"],
            serde_json::json!(["required"])
        );

        let mut unknown_scalar_verification = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(!unknown_scalar_verification.capture_handoff(
            "<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"invalid verification\",\"verification\":\"garbage\"}</agul-handoff>"
        ));
        assert!(unknown_scalar_verification.handoff.is_none());

        let mut schema_invalid = ChatSession::new(PathBuf::from("."), "model", None);
        assert!(!schema_invalid.capture_handoff(
            "<agul-handoff format=\"agul/handoff/v1\">{\"format\":\"agul/handoff/v1\",\"status\":\"completed\",\"summary\":\"wrong optional type\",\"verification\":{\"state\":\"required\"}}</agul-handoff>"
        ));
        assert!(schema_invalid.handoff.is_none());
    }

    #[test]
    fn appends_ordered_trace_events() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut session = ChatSession::new(root.path().to_path_buf(), "model", None);
        store.save(&session).unwrap();
        let mut appender = store.trace_appender(&session).unwrap();
        assert_eq!(
            appender
                .append(
                    &mut session,
                    "operation-1",
                    "assistant_delta",
                    serde_json::json!({"text": "hi"}),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            appender
                .append(
                    &mut session,
                    "operation-1",
                    "usage",
                    serde_json::json!({"tokens": 3}),
                )
                .unwrap(),
            2
        );
        // An operation-scoped appender stays write-through: live trace readers see
        // every event before the handle is dropped, just as with one open per event.
        let events = store.read_trace(&session.id).unwrap();
        let lines = events.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"seq\":1"));
        assert!(lines[1].contains("\"seq\":2"));
        drop(appender);

        let trace_path = store.trace_path(&session.id).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&trace_path)
            .unwrap()
            .write_all(b"{\"format\":")
            .unwrap();

        let mut recovered = store.load(&session.id, None).unwrap();
        assert_eq!(
            store
                .append_trace(
                    &mut recovered,
                    "operation-2",
                    "text",
                    serde_json::json!({"text": "again"}),
                )
                .unwrap(),
            3
        );
        let valid_sequences = store
            .read_trace(&session.id)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|event| event["seq"].as_u64())
            .collect::<Vec<_>>();
        assert_eq!(valid_sequences, vec![1, 2, 3]);
    }

    #[test]
    fn aggregates_each_delegated_session_once() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut parent = ChatSession::new(root.path().to_path_buf(), "master", None);
        let mut child = ChatSession::new_ari(
            root.path().to_path_buf(),
            "worker",
            None,
            SessionAttribution {
                parent_session_id: Some(parent.id.clone()),
                ..SessionAttribution::default()
            },
        );
        for session in [&mut parent, &mut child] {
            session.usage.record(super::super::billing::ResponseUsage {
                purpose: super::super::billing::UsagePurpose::Chat,
                provider: "test".to_string(),
                origin: "http://localhost".to_string(),
                response_id: None,
                observed_at_unix_seconds: 1,
                observation_time_source: super::super::billing::ObservationTimeSource::Host,
                reported_model: "model".to_string(),
                usage: Some(super::super::billing::TokenUsage {
                    input_tokens: 2,
                    output_tokens: 1,
                    total_tokens: Some(3),
                    ..super::super::billing::TokenUsage::default()
                }),
            });
        }
        parent.add_related_session(RelatedSession {
            relation: "delegated".to_string(),
            session_id: child.id.clone(),
            delegation_id: None,
            task_id: None,
        });
        child.add_related_session(RelatedSession {
            relation: "delegated".to_string(),
            session_id: parent.id.clone(),
            delegation_id: None,
            task_id: None,
        });
        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        let summary = store.aggregate_usage(&parent).unwrap();
        assert_eq!(summary.responses, 2);
        assert_eq!(summary.total_tokens, 6);
    }

    #[test]
    fn rejects_previous_session_schema() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let session = ChatSession::new(root.path().to_path_buf(), "model", None);
        store.save(&session).unwrap();
        let path = root
            .path()
            .join("sessions")
            .join(format!("{}.json", session.id));
        let previous = fs::read_to_string(&path)
            .unwrap()
            .replace(SESSION_SCHEMA, "agul/chat-session/v4");
        fs::write(path, previous).unwrap();
        let error = store.load(&session.id, None).unwrap_err().to_string();
        assert!(error.contains("unsupported session format: agul/chat-session/v4"));
    }

    #[test]
    fn pending_context_stays_out_of_visible_compaction_history() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        session.begin_turn(
            "first visible".to_string(),
            "first expanded".to_string(),
            None,
        );
        assert!(session.compaction_source(0).is_empty());

        session.begin_turn(
            "second visible".to_string(),
            "second expanded".to_string(),
            None,
        );
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].user, "first visible");
        assert_eq!(session.turns[0].assistant, INTERRUPTED_TURN_NOTE);
        assert_eq!(session.pending_model_input(), Some("second expanded"));

        assert!(session.finish_turn("done".to_string(), None));
        assert_eq!(session.turns[1].user, "second visible");
        assert_eq!(session.turns[1].assistant, "done");
        assert_eq!(session.pending_model_input(), None);
    }

    #[test]
    fn parent_cancellation_settles_a_delegated_ari_child_without_touching_parent_pending() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut parent = ChatSession::new(root.path().to_path_buf(), "parent", None);
        parent.begin_turn(
            "steered parent".to_string(),
            "expanded parent".to_string(),
            None,
        );
        let mut child = ChatSession::new_ari(
            root.path().to_path_buf(),
            "child",
            None,
            SessionAttribution {
                parent_session_id: Some(parent.id.clone()),
                delegation_id: Some("delegation-1".to_string()),
                task_id: Some("inspect".to_string()),
                specialist_id: Some("repository-scout".to_string()),
                pool_id: Some("local".to_string()),
            },
        );
        child.begin_turn(
            "inspect the failure".to_string(),
            "inspect the failure".to_string(),
            None,
        );
        let child_id = child.id.clone();
        let send_operation = store
            .begin_trace_operation(
                &mut child,
                "send",
                serde_json::json!({"input": "inspect the failure"}),
            )
            .unwrap();
        let nested_operation = store
            .begin_trace_operation(&mut child, "nested", serde_json::json!({}))
            .unwrap();
        store.save(&child).unwrap();
        let related = [RelatedSession {
            relation: "delegated".to_string(),
            session_id: child_id.clone(),
            delegation_id: Some("delegation-1".to_string()),
            task_id: Some("inspect".to_string()),
        }];

        store
            .settle_interrupted_related_sessions(&parent.id, &related)
            .unwrap();

        let settled = store.load(&child_id, None).unwrap();
        assert_eq!(settled.status, SessionStatus::Interrupted);
        assert_eq!(settled.pending_visible_user(), None);
        assert_eq!(settled.turns.len(), 1);
        assert_eq!(settled.turns[0].user, "inspect the failure");
        assert_eq!(settled.turns[0].assistant, INTERRUPTED_TURN_NOTE);
        assert_eq!(parent.pending_visible_user(), Some("steered parent"));
        let trace = store
            .read_trace(&child_id)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for operation_id in [&send_operation, &nested_operation] {
            let terminal = trace
                .iter()
                .filter(|event| event["operation_id"] == operation_id.as_str())
                .filter(|event| event["type"] == "operation_stopped")
                .collect::<Vec<_>>();
            assert_eq!(terminal.len(), 1);
            assert_eq!(terminal[0]["data"]["reason"], "parent_stopped");
        }

        store
            .settle_interrupted_related_sessions(&parent.id, &related)
            .unwrap();
        assert_eq!(store.load(&child_id, None).unwrap().turns.len(), 1);
        assert_eq!(
            store
                .read_trace(&child_id)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| event["type"] == "operation_stopped")
                .count(),
            2
        );
    }

    #[test]
    fn loading_a_dead_delegated_ari_child_persists_its_interrupted_turn() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let mut child = ChatSession::new_ari(
            root.path().to_path_buf(),
            "child",
            None,
            SessionAttribution {
                parent_session_id: Some("parent-1".to_string()),
                ..SessionAttribution::default()
            },
        );
        child.begin_turn(
            "unfinished child task".to_string(),
            "expanded child task".to_string(),
            None,
        );
        child.owner_pid = i32::MAX as u32;
        let child_id = child.id.clone();
        let operation_id = store
            .begin_trace_operation(
                &mut child,
                "send",
                serde_json::json!({"input": "unfinished child task"}),
            )
            .unwrap();
        store.save(&child).unwrap();

        let loaded = store.load(&child_id, None).unwrap();
        assert_eq!(loaded.status, SessionStatus::Interrupted);
        assert_eq!(loaded.pending_visible_user(), None);
        assert_eq!(loaded.turns.len(), 1);

        let path = root
            .path()
            .join("sessions")
            .join(format!("{child_id}.json"));
        let persisted: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["status"], "interrupted");
        assert_eq!(persisted["pending_turn"], Value::Null);
        let stopped = store
            .read_trace(&child_id)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| {
                event["operation_id"] == operation_id && event["type"] == "operation_stopped"
            })
            .unwrap();
        assert_eq!(stopped["data"]["reason"], "owner_exited");

        let trace_path = store.trace_path(&child_id).unwrap();
        let trace_without_terminal = store
            .read_trace(&child_id)
            .unwrap()
            .lines()
            .filter(|line| !line.contains("operation_stopped"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(trace_path, trace_without_terminal).unwrap();

        let loaded_again = store.load(&child_id, None).unwrap();
        assert_eq!(loaded_again.turns.len(), 1);
        assert_eq!(loaded_again.turns[0].user, "unfinished child task");
        assert_eq!(loaded_again.turns[0].assistant, INTERRUPTED_TURN_NOTE);
        let repaired_trace = store.read_trace(&child_id).unwrap();
        assert_eq!(repaired_trace.matches("operation_stopped").count(), 1);
        let repaired_terminal = repaired_trace
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["type"] == "operation_stopped")
            .unwrap();
        assert_eq!(repaired_terminal["data"]["reason"], "interrupted_recovery");
    }

    #[test]
    fn related_session_settlement_continues_after_errors_and_reports_all_of_them() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::discover(Some(root.path())).unwrap();
        let parent_id = "parent-1";
        let mut child = ChatSession::new_ari(
            root.path().to_path_buf(),
            "child",
            None,
            SessionAttribution {
                parent_session_id: Some(parent_id.to_string()),
                ..SessionAttribution::default()
            },
        );
        child.begin_turn("work".to_string(), "work".to_string(), None);
        let child_id = child.id.clone();
        store
            .begin_trace_operation(&mut child, "send", serde_json::json!({"input": "work"}))
            .unwrap();
        store.save(&child).unwrap();
        let related = [
            RelatedSession {
                relation: "delegated".to_string(),
                session_id: "missing-first".to_string(),
                delegation_id: None,
                task_id: None,
            },
            RelatedSession {
                relation: "delegated".to_string(),
                session_id: child_id.clone(),
                delegation_id: None,
                task_id: None,
            },
            RelatedSession {
                relation: "delegated".to_string(),
                session_id: "missing-last".to_string(),
                delegation_id: None,
                task_id: None,
            },
        ];

        let error = store
            .settle_interrupted_related_sessions(parent_id, &related)
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing-first"), "{error}");
        assert!(error.contains("missing-last"), "{error}");
        let settled = store.load(&child_id, None).unwrap();
        assert_eq!(settled.status, SessionStatus::Interrupted);
        assert_eq!(settled.pending_visible_user(), None);
        assert_eq!(
            store
                .read_trace(&child_id)
                .unwrap()
                .matches("operation_stopped")
                .count(),
            1
        );
    }

    #[test]
    fn compaction_changes_history_only_when_committed() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        for index in 0..6 {
            session.append_turn(VisibleTurn {
                user: format!("u{index}"),
                assistant: format!("a{index}"),
            });
        }
        session.set_native_history(Some(vec![Message::system("old prefix")]));
        assert_eq!(session.compaction_source(4).len(), 2);
        assert_eq!(session.turns.len(), 6);
        assert_eq!(session.commit_compaction(4, "summary".to_string()), 2);
        assert_eq!(session.turns.len(), 4);
        assert_eq!(session.summary.as_deref(), Some("summary"));
        assert_eq!(session.native_history(), None);
    }
}
