//! Small terminal-only messages that live outside the streaming presenter.

use std::io::{self, Write};

use super::theme::{Tone, paint};
use super::workbench::truncate_cells;

const DEFAULT_WIDTH: usize = 76;
const MIN_WIDTH: usize = 20;
const MAX_WIDTH: usize = 240;

pub(super) fn goodbye(color: bool, saved: bool, continue_available: bool) -> io::Result<()> {
    let width = terminal_width();
    let summary = close_summary(saved, continue_available, width.saturating_sub(4));
    let stderr = io::stderr();
    let mut output = stderr.lock();
    write_goodbye(&mut output, color, &summary)
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

fn close_summary(saved: bool, continue_available: bool, max_cells: usize) -> String {
    let summary = match (saved, continue_available) {
        (true, true) => "closed · saved · ↩ agul chat --continue",
        (true, false) => "closed · saved",
        (false, _) => "closed · ephemeral",
    };
    truncate_cells(summary, max_cells)
}

fn write_goodbye(output: &mut dyn Write, color: bool, summary: &str) -> io::Result<()> {
    writeln!(output)?;
    writeln!(
        output,
        " {} {}",
        paint(color, Tone::Success, "✓"),
        paint(color, Tone::Focus, summary)
    )
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn close_summary_distinguishes_saved_and_ephemeral_chats() {
        assert_eq!(close_summary(false, false, 40), "closed · ephemeral");
        assert_eq!(close_summary(true, false, 40), "closed · saved");
        assert_eq!(
            close_summary(true, true, 60),
            "closed · saved · ↩ agul chat --continue"
        );
    }

    #[test]
    fn close_summary_is_width_bounded() {
        assert!(UnicodeWidthStr::width(close_summary(true, true, 20).as_str()) <= 20);
    }

    #[test]
    fn goodbye_is_one_compact_line_after_the_composer() {
        let mut output = Vec::new();
        write_goodbye(
            &mut output,
            false,
            "closed · saved · ↩ agul chat --continue",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\n ✓ closed · saved · ↩ agul chat --continue\n"
        );
    }
}
