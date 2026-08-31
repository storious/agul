//! Semantic colors for chat output and the full-screen workbench.
//!
//! The theme is deliberately presentation-only. It never emits color when the
//! rich terminal gate (or `NO_COLOR`) disabled it, and it never owns terminal
//! mode, cursor movement, or runtime state.

use crate::terminal::plain_text as escape_terminal_controls;

use super::workbench::{TranscriptPaint, TranscriptStyle, TranscriptText, TranscriptTone};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tone {
    Focus,
    Success,
}

impl Tone {
    pub(super) const fn sgr(self) -> &'static str {
        match self {
            // Blue carries focus, navigation, and ordinary successful state.
            Self::Focus | Self::Success => "1;38;5;75",
        }
    }
}

pub(super) fn paint(color: bool, tone: Tone, text: &str) -> String {
    let safe = escape_terminal_controls(text);
    if color {
        format!("\u{1b}[{}m{safe}\u{1b}[0m", tone.sgr())
    } else {
        safe
    }
}

pub(super) fn render_transcript_text(color: bool, text: &TranscriptText) -> String {
    let mut rendered = String::new();
    for segment in text.segments() {
        let codes = transcript_sgr(segment.style);
        let safe = escape_terminal_controls(segment.text);
        if color && !codes.is_empty() {
            rendered.push_str("\u{1b}[");
            rendered.push_str(&codes.join(";"));
            rendered.push('m');
            rendered.push_str(&safe);
            rendered.push_str("\u{1b}[0m");
        } else {
            rendered.push_str(&safe);
        }
    }
    rendered
}

fn transcript_sgr(style: TranscriptStyle) -> Vec<String> {
    let mut codes = match style.paint {
        TranscriptPaint::Default => Vec::new(),
        TranscriptPaint::Tone(tone) => transcript_tone_sgr(tone)
            .iter()
            .map(ToString::to_string)
            .collect(),
        TranscriptPaint::Rgb(red, green, blue) => {
            vec![format!("38;2;{red};{green};{blue}")]
        }
    };
    if style.bold {
        codes.push("1".to_string());
    }
    if style.italic {
        codes.push("3".to_string());
    }
    if style.dim {
        codes.push("2".to_string());
    }
    if style.underline {
        codes.push("4".to_string());
    }
    if style.strike {
        codes.push("9".to_string());
    }
    codes
}

const fn transcript_tone_sgr(tone: TranscriptTone) -> &'static [&'static str] {
    match tone {
        TranscriptTone::Accent => &["38;5;75"],
        TranscriptTone::Success => &["38;5;78"],
        TranscriptTone::Label | TranscriptTone::CodeLabel => &["38;5;220"],
        TranscriptTone::Muted => &["38;5;245"],
        TranscriptTone::Error => &["38;5;203"],
        TranscriptTone::CodeText => &["38;5;252"],
        TranscriptTone::InlineCode => &["38;5;222", "48;5;236"],
        TranscriptTone::Link => &["38;5;75"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_keeps_text_and_removes_all_sgr() {
        assert_eq!(paint(false, Tone::Focus, "agul"), "agul");
    }

    #[test]
    fn themed_text_escapes_untrusted_terminal_controls() {
        let rendered = paint(true, Tone::Focus, "x\u{1b}]0;owned\u{7}\u{202e}");
        assert!(rendered.starts_with("\u{1b}[1;38;5;75m"));
        assert!(rendered.ends_with("\u{1b}[0m"));
        assert_eq!(rendered.matches('\u{1b}').count(), 2);
        assert!(rendered.contains("\\u{1b}"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{202e}"));
    }

    #[test]
    fn structured_transcript_text_becomes_ansi_only_at_the_plain_terminal_edge() {
        let text = TranscriptText::styled_untrusted(
            "tool",
            TranscriptStyle::tone(TranscriptTone::Label).bold(),
        );
        assert_eq!(render_transcript_text(false, &text), "tool");
        let colored = render_transcript_text(true, &text);
        assert!(colored.starts_with("\u{1b}[38;5;220;1m"));
        assert!(colored.ends_with("\u{1b}[0m"));
    }

    #[test]
    fn plain_terminal_edge_neutralizes_controls_preserved_for_stream_framing() {
        let text = TranscriptText::stream_untrusted(
            "before\rafter\u{1b}",
            TranscriptStyle::tone(TranscriptTone::Muted),
        );
        let rendered = render_transcript_text(true, &text);
        assert!(!rendered.contains('\r'));
        assert_eq!(rendered.matches('\u{1b}').count(), 2);
        assert!(rendered.contains("before\\rafter\\u{1b}"));
    }
}
