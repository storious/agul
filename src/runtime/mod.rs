mod atomic_file;
pub(crate) mod billing;
mod cancellation;
mod chat_session;
mod codex;
mod direct_chat;
mod engine;
mod host_tools;
mod plugin;
mod process;
mod project;
mod provider;
mod tools;
mod usage;

pub(crate) const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub(crate) const DEEPSEEK_DEFAULT_MODEL: &str = "deepseek-v4-flash";
pub(crate) const DEEPSEEK_DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
pub(crate) const GLM_DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";
pub(crate) const GLM_DEFAULT_MODEL: &str = "glm-5.3";
pub(crate) const GLM_DEFAULT_API_KEY_ENV: &str = "GLM_API_KEY";
pub(crate) const GLM_CODING_DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";
pub(crate) const GLM_CODING_DEFAULT_MODEL: &str = "glm-4.7";
pub(crate) const AGUL_LAUNCH_FORMAT: &str = "agul/launch/v2";
pub(crate) const AGUL_PLUGIN_FORMAT: &str = "agul/plugin/v2";
/// Carries an explicit state root through plugin-managed Agul process trees.
pub(crate) const AGUL_STATE_DIR_ENV: &str = "AGUL_STATE_DIR";
pub(crate) const DEFAULT_MAX_ROUNDS: u32 = 32;
pub(crate) const DEFAULT_MAX_TOOL_CALLS: u32 = 128;
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 16_384;
pub(crate) const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

pub(crate) use billing::{
    PriceCatalog, PriceCatalogStore, PriceSelection, PricingStatus, UsagePurpose, UsageSummary,
    format_femto_amount_3dp,
};
pub(crate) use cancellation::TurnCancellation;
#[cfg(test)]
pub(crate) use chat_session::SessionSource;
pub(crate) use chat_session::{
    ChatSession, INTERRUPTED_TURN_NOTE, NativeSessionConfig, RelatedSession, SESSION_SCHEMA,
    SessionAttribution, SessionEngine, SessionInfo, SessionStatus, SessionStore, TraceAppender,
};
pub(crate) use codex::{CodexAppServer, CodexChatConfig, CodexLogin};
pub(crate) use direct_chat::{ChatConfig, ChatError, ChatEvent, ResponseObservation, TurnOutcome};
pub(crate) use engine::ChatEngine;
#[cfg(test)]
pub(crate) use plugin::process_tree_command_fixture;
pub(crate) use plugin::{PluginCallContext, PluginEvent, PluginExecutionError, PluginTerminal};
#[cfg(test)]
pub(crate) use process::{FixtureProcess, process_test_lock};
pub(crate) use project::{Project, is_skill_reference_char, skill_references};
pub(crate) use provider::ProviderConfig;
#[cfg(test)]
pub(crate) use provider::Usage;
pub(crate) use usage::{NativeConnectionPreset, NativeProvider, ProviderIdentity};
