use std::borrow::Cow;

use crate::runtime::skill_references;

pub(super) const CHAT_COMMANDS: &[(&str, &str, ChatCommand)] = &[
    ("/help", "commands", ChatCommand::Help),
    ("/status", "status", ChatCommand::Status),
    ("/skills", "skills", ChatCommand::Skills),
    ("/usage", "tokens", ChatCommand::Usage),
    ("/cost", "billing", ChatCommand::Cost),
    ("/compact", "semantic", ChatCommand::Compact),
    ("/sessions", "sessions", ChatCommand::Sessions),
    ("/clear", "history", ChatCommand::Clear),
    ("/stop", "stop turn", ChatCommand::Stop),
    ("/exit", "close", ChatCommand::Exit),
    ("/quit", "close", ChatCommand::Exit),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatCommand {
    Help,
    Status,
    Skills,
    Usage,
    Cost,
    Compact,
    Sessions,
    Clear,
    Stop,
    Exit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ParsedMessage<'a> {
    Empty,
    Command(ChatCommand),
    UnknownCommand(&'a str),
    UserText {
        text: Cow<'a, str>,
        skills: Vec<String>,
    },
}

pub(super) fn parse_message(input: &str) -> ParsedMessage<'_> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return ParsedMessage::Empty;
    }
    let leading = input.len() - input.trim_start().len();
    let body = &input[leading..];
    if let Some(rest) = body.strip_prefix("//") {
        let text = format!("{}/{rest}", &input[..leading]);
        return ParsedMessage::UserText {
            text: Cow::Owned(unescape_at(&text).into_owned()),
            skills: Vec::new(),
        };
    }
    if body.starts_with('/') {
        let command = CHAT_COMMANDS
            .iter()
            .find_map(|(name, _, command)| (*name == trimmed).then_some(*command));
        return command
            .map(ParsedMessage::Command)
            .unwrap_or(ParsedMessage::UnknownCommand(trimmed));
    }
    ParsedMessage::UserText {
        text: unescape_at(input),
        skills: skill_references(input),
    }
}

pub(super) fn parse_user_text(input: &str) -> (Cow<'_, str>, Vec<String>) {
    (unescape_at(input), skill_references(input))
}

fn unescape_at(input: &str) -> Cow<'_, str> {
    if input.contains("@@") {
        Cow::Owned(input.replace("@@", "@"))
    } else {
        Cow::Borrowed(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_skills_and_escapes() {
        assert_eq!(
            parse_message("/cost"),
            ParsedMessage::Command(ChatCommand::Cost)
        );
        assert_eq!(
            parse_message("/stop"),
            ParsedMessage::Command(ChatCommand::Stop)
        );
        let ParsedMessage::UserText { text, skills } = parse_message("use @skill:review") else {
            panic!("user text");
        };
        assert_eq!(text, "use @skill:review");
        assert_eq!(skills, ["review"]);
        let ParsedMessage::UserText { text, skills } = parse_message("//cost @@skill:review")
        else {
            panic!("escaped text");
        };
        assert_eq!(text, "/cost @skill:review");
        assert!(skills.is_empty());
    }
}
