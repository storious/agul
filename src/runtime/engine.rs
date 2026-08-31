use std::path::PathBuf;

use super::TurnCancellation;
use super::codex::{CodexChat, CodexChatConfig};
use super::direct_chat::{
    ChatConfig, ChatError, ChatEvent, CompactionOutcome, DirectChat, TurnOutcome, VisibleTurn,
};
use super::project::Project;
use super::provider::Message;

pub(crate) enum ChatEngine {
    Native(DirectChat),
    Codex(CodexChat),
}

impl ChatEngine {
    pub(crate) fn native(
        project: &Project,
        config: ChatConfig,
        session_id: impl Into<String>,
        state_dir: Option<PathBuf>,
    ) -> Result<Self, ChatError> {
        DirectChat::new(project, config, session_id, state_dir).map(Self::Native)
    }

    pub(crate) fn codex(project: &Project, config: CodexChatConfig) -> Result<Self, ChatError> {
        CodexChat::new(project, config).map(Self::Codex)
    }

    pub(crate) fn model(&self) -> &str {
        match self {
            Self::Native(chat) => chat.model(),
            Self::Codex(chat) => chat.model(),
        }
    }

    pub(crate) fn endpoint(&self) -> &str {
        match self {
            Self::Native(chat) => chat.endpoint(),
            Self::Codex(chat) => chat.endpoint(),
        }
    }

    pub(crate) fn thread_id(&self) -> Option<&str> {
        match self {
            Self::Native(_) => None,
            Self::Codex(chat) => Some(chat.thread_id()),
        }
    }

    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        match self {
            Self::Native(chat) => chat.reasoning_effort(),
            Self::Codex(chat) => chat.reasoning_effort(),
        }
    }

    pub(crate) fn context_window(&self) -> Option<u64> {
        match self {
            Self::Native(chat) => chat.context_window(),
            Self::Codex(_) => None,
        }
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        match self {
            Self::Native(chat) => chat.tool_names(),
            Self::Codex(_) => Vec::new(),
        }
    }

    pub(crate) const fn is_codex(&self) -> bool {
        matches!(self, Self::Codex(_))
    }

    pub(crate) fn reset(&mut self) -> Result<(), ChatError> {
        match self {
            Self::Native(chat) => {
                chat.reset();
                Ok(())
            }
            Self::Codex(chat) => chat.reset(),
        }
    }

    pub(crate) fn native_history(&self) -> Option<Vec<Message>> {
        match self {
            Self::Native(chat) => Some(chat.history()),
            Self::Codex(_) => None,
        }
    }

    pub(crate) fn restore(
        &mut self,
        summary: Option<&str>,
        turns: &[VisibleTurn],
        native_history: Option<&[Message]>,
    ) {
        match self {
            Self::Native(chat) => chat.restore(summary, turns, native_history),
            Self::Codex(chat) => chat.restore(summary, turns),
        }
    }

    pub(crate) fn restore_interrupted(&mut self, model_input: &str, assistant_note: &str) {
        match self {
            Self::Native(chat) => chat.restore_interrupted(model_input, assistant_note),
            Self::Codex(chat) => chat.restore_interrupted(model_input, assistant_note),
        }
    }

    pub(crate) fn send(
        &mut self,
        input: impl Into<String>,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<TurnOutcome, ChatError> {
        self.send_cancellable(input, &TurnCancellation::default(), on_event)
    }

    pub(crate) fn send_cancellable(
        &mut self,
        input: impl Into<String>,
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<TurnOutcome, ChatError> {
        let input = input.into();
        match self {
            Self::Native(chat) => chat.send_cancellable(input, cancellation, on_event),
            Self::Codex(chat) => chat.send_cancellable(input, cancellation, on_event),
        }
    }

    pub(crate) fn compact(
        &self,
        turns: &[VisibleTurn],
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<CompactionOutcome, ChatError> {
        self.compact_cancellable(turns, &TurnCancellation::default(), on_event)
    }

    pub(crate) fn compact_cancellable(
        &self,
        turns: &[VisibleTurn],
        cancellation: &TurnCancellation,
        on_event: &mut dyn FnMut(ChatEvent<'_>) -> Result<(), ChatError>,
    ) -> Result<CompactionOutcome, ChatError> {
        match self {
            Self::Native(chat) => chat.compact_cancellable(turns, cancellation, on_event),
            Self::Codex(_) => Err(ChatError::new(
                "manual visible-turn compaction is not available in the Codex engine yet",
            )),
        }
    }
}
