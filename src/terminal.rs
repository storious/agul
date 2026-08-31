pub(crate) fn plain_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\n' | '\t' => vec![character],
            character if character.is_control() || is_format_control(character) => {
                character.escape_default().collect()
            }
            character => vec![character],
        })
        .collect()
}

fn is_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{206f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_lines_but_neutralizes_terminal_controls() {
        let text = plain_text("hello\n\u{1b}]0;title\u{7}\u{202e}");
        assert!(text.starts_with("hello\n"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
    }
}
