use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime::{ChatSession, Project, SessionInfo, SessionStatus, SessionStore};
use crate::terminal::plain_text;

use super::ChatArgs;
use super::workbench::{SessionChoice, select_session};

pub(super) enum SessionRequest {
    New,
    Resume(String),
    Cancelled,
}

pub(super) fn resolve_session_request(
    args: &ChatArgs,
    store: Option<&SessionStore>,
) -> Result<SessionRequest, Box<dyn std::error::Error>> {
    if let Some(id) = &args.session {
        return Ok(SessionRequest::Resume(id.clone()));
    }
    if !args.continue_session && !args.resume {
        return Ok(SessionRequest::New);
    }
    if args.continue_session && args.resume {
        return Err("--continue and --resume cannot be used together".into());
    }

    let store = store.ok_or("session history is disabled")?;
    let workspace = Project::canonical_workspace(&args.workspace)?;
    let sessions = store.resumable_chats(&workspace)?;
    if sessions.is_empty() {
        return Err(format!(
            "no resumable chats in {}",
            plain_text(&workspace.display().to_string())
        )
        .into());
    }

    if args.continue_session {
        return Ok(SessionRequest::Resume(sessions[0].id.clone()));
    }

    let choices = sessions.iter().map(session_choice).collect();
    let color = !args.no_color && env::var_os("NO_COLOR").is_none();
    Ok(match select_session(choices, color)? {
        Some(id) => SessionRequest::Resume(id),
        None => SessionRequest::Cancelled,
    })
}

fn session_choice(session: &SessionInfo) -> SessionChoice {
    let turns = session_turns(session.turns, session.summarized_turns);
    let label = format!(
        "{} · {} · {} · {turns}",
        session_age(session.updated_at),
        plain_text(&session.model),
        session_status(session.status),
    );
    let description = session
        .preview
        .as_deref()
        .map(plain_text)
        .filter(|preview| !preview.is_empty())
        .unwrap_or_else(|| plain_text(&session.workspace.display().to_string()));
    SessionChoice::new(
        session.id.clone(),
        label,
        description,
        format!("{} {}", session.workspace.display(), session.status),
    )
}

pub(super) const fn session_status(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Active => "●",
        SessionStatus::Completed => "✓",
        SessionStatus::Failed => "!",
        SessionStatus::Cancelled => "■",
        SessionStatus::Interrupted => "↯",
    }
}

pub(super) fn session_turns(turns: usize, summarized_turns: u64) -> String {
    let noun = if (turns as u64).saturating_add(summarized_turns) == 1 {
        "turn"
    } else {
        "turns"
    };
    if summarized_turns == 0 {
        format!("{turns} {noun}")
    } else {
        format!("{turns}+{summarized_turns} {noun}")
    }
}

pub(super) fn session_age(updated_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(updated_at);
    match age {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", age / 60),
        3_600..=86_399 => format!("{}h", age / 3_600),
        _ => format!("{}d", age / 86_400),
    }
}

pub(super) fn resumed_line(session: &ChatSession) -> String {
    let turns = session_turns(session.turns.len(), session.summarized_turns);
    format!("↳ resumed · {turns}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn resumed_chat_uses_one_compact_history_boundary() {
        let mut session = ChatSession::new(PathBuf::from("."), "model", None);
        let user = format!("继续修复\u{1b}[31m {}", "界".repeat(100));
        session.begin_turn(user.clone(), user, None);
        assert!(session.finish_turn("done".to_string(), None));

        let line = resumed_line(&session);
        assert_eq!(line, "↳ resumed · 1 turn");
        assert!(!line.contains(&session.id));
        assert!(UnicodeWidthStr::width(line.as_str()) < 40);
    }

    #[test]
    fn session_turn_count_uses_normal_english_pluralization() {
        assert_eq!(session_turns(1, 0), "1 turn");
        assert_eq!(session_turns(0, 1), "0+1 turn");
        assert_eq!(session_turns(2, 0), "2 turns");
    }
}
