use textwrap::{Options, WordSplitter};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::state::{TranscriptKind, TranscriptLine};
use super::text::TranscriptText;

/// A wrapped, virtual viewport over the current chat transcript.
///
/// Completed logical lines are cached until the terminal width changes. The
/// live line is wrapped independently, so streaming a token never rebuilds the
/// whole conversation.
#[derive(Debug, Default)]
pub(super) struct TranscriptViewport {
    wrapped: Vec<TranscriptLine>,
    logical_starts: Vec<usize>,
    wrapped_logical_lines: usize,
    width: usize,
    scroll_from_bottom: usize,
    max_scroll: usize,
    viewport_height: usize,
    previous_total_rows: usize,
}

impl TranscriptViewport {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn visible_lines(
        &mut self,
        completed: &[TranscriptLine],
        live: &TranscriptText,
        live_kind: TranscriptKind,
        width: u16,
        height: u16,
    ) -> Vec<TranscriptLine> {
        let width = usize::from(width);
        let height = usize::from(height);
        let width_changed = width != self.width;
        let resize_anchor = width_changed.then(|| self.reading_anchor()).flatten();
        if width_changed || completed.len() < self.wrapped_logical_lines {
            self.wrapped.clear();
            self.logical_starts.clear();
            self.wrapped_logical_lines = 0;
        }
        if width > 0 {
            for line in &completed[self.wrapped_logical_lines..] {
                self.logical_starts.push(self.wrapped.len());
                self.wrapped.extend(wrap_transcript_line(line, width));
            }
        }
        self.wrapped_logical_lines = completed.len();
        self.width = width;

        let live = if live.is_empty() || width == 0 {
            Vec::new()
        } else {
            wrap_transcript_line(&TranscriptLine::styled(live_kind, live.clone()), width)
        };
        let total_rows = self.wrapped.len().saturating_add(live.len());

        // scroll_from_bottom is normally a stable reading anchor. When new
        // rows arrive at the same width, grow the offset by the same amount so
        // a user reading older output is not pulled toward the live tail.
        if !width_changed && self.scroll_from_bottom > 0 && total_rows > self.previous_total_rows {
            self.scroll_from_bottom = self
                .scroll_from_bottom
                .saturating_add(total_rows - self.previous_total_rows);
        }

        self.viewport_height = height;
        self.max_scroll = total_rows.saturating_sub(height);
        self.scroll_from_bottom = resize_anchor
            .and_then(|anchor| self.offset_for_anchor(anchor, completed.len(), live.len(), height))
            .unwrap_or(self.scroll_from_bottom)
            .min(self.max_scroll);
        self.previous_total_rows = total_rows;

        let visible_rows = height.min(total_rows);
        let end = total_rows.saturating_sub(self.scroll_from_bottom);
        let start = end.saturating_sub(visible_rows);
        (start..end)
            .filter_map(|row| {
                if row < self.wrapped.len() {
                    self.wrapped.get(row).cloned()
                } else {
                    live.get(row - self.wrapped.len()).cloned()
                }
            })
            .collect()
    }

    fn reading_anchor(&self) -> Option<ReadingAnchor> {
        if self.scroll_from_bottom == 0
            || self.previous_total_rows == 0
            || self.viewport_height == 0
        {
            return None;
        }
        let visible_rows = self.viewport_height.min(self.previous_total_rows);
        let end = self
            .previous_total_rows
            .saturating_sub(self.scroll_from_bottom);
        let top = end.saturating_sub(visible_rows);
        let (logical_line, wrapped_row) = if top < self.wrapped.len() {
            let logical_line = self
                .logical_starts
                .partition_point(|start| *start <= top)
                .saturating_sub(1);
            (logical_line, top - self.logical_starts[logical_line])
        } else {
            (self.wrapped_logical_lines, top - self.wrapped.len())
        };
        Some(ReadingAnchor {
            logical_line,
            wrapped_row,
            width: self.width.max(1),
        })
    }

    fn offset_for_anchor(
        &self,
        anchor: ReadingAnchor,
        completed_lines: usize,
        live_rows: usize,
        viewport_height: usize,
    ) -> Option<usize> {
        let total_rows = self.wrapped.len().saturating_add(live_rows);
        let (start, end) = if anchor.logical_line < completed_lines {
            let start = *self.logical_starts.get(anchor.logical_line)?;
            let end = self
                .logical_starts
                .get(anchor.logical_line + 1)
                .copied()
                .unwrap_or(self.wrapped.len());
            (start, end)
        } else if anchor.logical_line == completed_lines && live_rows > 0 {
            (self.wrapped.len(), total_rows)
        } else {
            return None;
        };
        let rows = end.saturating_sub(start).max(1);
        let scaled_row = anchor
            .wrapped_row
            .saturating_mul(anchor.width)
            .checked_div(self.width.max(1))
            .unwrap_or(0)
            .min(rows - 1);
        let top = start.saturating_add(scaled_row);
        let visible_rows = viewport_height.min(total_rows);
        let end = top.saturating_add(visible_rows).min(total_rows);
        Some(total_rows.saturating_sub(end))
    }

    pub(super) fn page_up(&mut self) {
        let rows = self.viewport_height.saturating_sub(1).max(1);
        self.scroll_up(rows);
    }

    pub(super) fn page_down(&mut self) {
        let rows = self.viewport_height.saturating_sub(1).max(1);
        self.scroll_down(rows);
    }

    fn scroll_up(&mut self, rows: usize) {
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .saturating_add(rows)
            .min(self.max_scroll);
    }

    fn scroll_down(&mut self, rows: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(rows);
    }

    #[cfg(test)]
    fn offset(&self) -> usize {
        self.scroll_from_bottom
    }
}

#[derive(Clone, Copy, Debug)]
struct ReadingAnchor {
    logical_line: usize,
    wrapped_row: usize,
    width: usize,
}

/// Wrap at Unicode line-break opportunities and fall back to grapheme-aware
/// splitting for long paths, hashes, and other unbroken text.
pub(super) fn wrap_display_lines(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut wrapped = Vec::new();
    for logical_line in value.split('\n') {
        let logical_line = logical_line.strip_suffix('\r').unwrap_or(logical_line);
        let expanded = logical_line.replace('\t', "    ");
        if expanded.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let indent = if expanded.starts_with("◌ ") {
            "  "
        } else {
            ""
        };
        let options = Options::new(width)
            .break_words(false)
            .word_splitter(WordSplitter::NoHyphenation)
            .subsequent_indent(indent);
        for line in textwrap::wrap(&expanded, options) {
            push_grapheme_wrapped(line.as_ref(), width, &mut wrapped);
        }
    }
    wrapped
}

fn wrap_transcript_line(line: &TranscriptLine, width: usize) -> Vec<TranscriptLine> {
    if width == 0 {
        return Vec::new();
    }
    let expanded = line.content().expanded_tabs();
    if expanded.is_empty() {
        return vec![TranscriptLine::styled(line.kind, expanded)];
    }

    let indent = if expanded.as_str().starts_with("◌ ") {
        "  "
    } else {
        ""
    };
    let options = Options::new(width)
        .break_words(false)
        .word_splitter(WordSplitter::NoHyphenation)
        .subsequent_indent(indent);
    let mut wrapped = Vec::new();
    let mut cursor = 0;
    for (index, displayed) in textwrap::wrap(expanded.as_str(), options)
        .into_iter()
        .enumerate()
    {
        let prefix = if index > 0 { indent } else { "" };
        let Some(content) = displayed.strip_prefix(prefix) else {
            debug_assert!(false, "textwrap omitted the configured continuation indent");
            return wrap_display_lines(expanded.as_str(), width)
                .into_iter()
                .map(|text| TranscriptLine::new(line.kind, text))
                .collect();
        };
        let Some(relative_start) = expanded.as_str()[cursor..].find(content) else {
            debug_assert!(false, "textwrap output no longer maps to its source line");
            return wrap_display_lines(expanded.as_str(), width)
                .into_iter()
                .map(|text| TranscriptLine::new(line.kind, text))
                .collect();
        };
        let start = cursor.saturating_add(relative_start);
        let end = start
            .saturating_add(content.len())
            .min(expanded.as_str().len());
        cursor = end;
        push_styled_grapheme_wrapped(
            expanded.slice(start..end),
            prefix,
            width,
            line.kind,
            &mut wrapped,
        );
    }
    wrapped
}

fn push_styled_grapheme_wrapped(
    content: TranscriptText,
    prefix: &str,
    width: usize,
    kind: TranscriptKind,
    output: &mut Vec<TranscriptLine>,
) {
    let prefix_width = UnicodeWidthStr::width(prefix);
    if prefix_width.saturating_add(UnicodeWidthStr::width(content.as_str())) <= width {
        output.push(TranscriptLine::styled(kind, content.prepend_plain(prefix)));
        return;
    }

    let mut chunk_start = 0;
    let mut chunk_prefix = prefix;
    let mut used = prefix_width;
    for (offset, grapheme) in content.as_str().grapheme_indices(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if cells > width {
            push_styled_chunk(&content, chunk_start..offset, chunk_prefix, kind, output);
            output.push(TranscriptLine::styled(
                kind,
                TranscriptText::styled_untrusted("…", content.style_at(offset)),
            ));
            chunk_start = offset.saturating_add(grapheme.len());
            chunk_prefix = "";
            used = 0;
            continue;
        }
        if cells > 0 && used.saturating_add(cells) > width {
            push_styled_chunk(&content, chunk_start..offset, chunk_prefix, kind, output);
            chunk_start = offset;
            chunk_prefix = "";
            used = 0;
        }
        used = used.saturating_add(cells);
    }
    push_styled_chunk(
        &content,
        chunk_start..content.as_str().len(),
        chunk_prefix,
        kind,
        output,
    );
}

fn push_styled_chunk(
    content: &TranscriptText,
    bytes: std::ops::Range<usize>,
    prefix: &str,
    kind: TranscriptKind,
    output: &mut Vec<TranscriptLine>,
) {
    if bytes.is_empty() && prefix.is_empty() {
        return;
    }
    output.push(TranscriptLine::styled(
        kind,
        content.slice(bytes).prepend_plain(prefix),
    ));
}

fn push_grapheme_wrapped(value: &str, width: usize, output: &mut Vec<String>) {
    let mut line = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if cells > width {
            if !line.is_empty() {
                output.push(std::mem::take(&mut line));
                used = 0;
            }
            output.push("…".to_string());
            continue;
        }
        if cells > 0 && used + cells > width {
            output.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push_str(grapheme);
        used += cells;
    }
    output.push(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: impl IntoIterator<Item = String>) -> Vec<TranscriptLine> {
        values
            .into_iter()
            .map(|text| TranscriptLine::new(TranscriptKind::Notice, text))
            .collect()
    }

    fn visible(
        viewport: &mut TranscriptViewport,
        completed: &[TranscriptLine],
        width: u16,
        height: u16,
    ) -> Vec<String> {
        viewport
            .visible_lines(
                completed,
                &TranscriptText::default(),
                TranscriptKind::Notice,
                width,
                height,
            )
            .into_iter()
            .map(|line| line.text().to_string())
            .collect()
    }

    #[test]
    fn display_wrap_keeps_cjk_and_emoji_graphemes_whole() {
        assert_eq!(wrap_display_lines("中文🙂ab", 4), ["中文", "🙂ab"]);
        assert_eq!(wrap_display_lines("e\u{301}🙂x", 3), ["e\u{301}🙂", "x"]);
        assert_eq!(wrap_display_lines("甲\n\n乙", 4), ["甲", "", "乙"]);
    }

    #[test]
    fn display_wrap_prefers_words_and_indents_reasoning_continuations() {
        let lines = wrap_display_lines("◌ The tools are not needed.", 14);
        assert!(lines.len() > 1);
        assert!(lines[1..].iter().all(|line| line.starts_with("  ")));
        assert!(lines.iter().any(|line| line.contains("needed.")));
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 14)
        );
    }

    #[test]
    fn wrapping_preserves_completed_and_live_message_kinds() {
        let completed = [TranscriptLine::new(
            TranscriptKind::Reasoning,
            "◌ long reasoning line",
        )];
        let mut viewport = TranscriptViewport::default();
        let visible = viewport.visible_lines(
            &completed,
            &TranscriptText::plain_untrusted("final answer"),
            TranscriptKind::Assistant,
            10,
            20,
        );

        assert!(
            visible
                .iter()
                .filter(|line| line.text().contains("reason") || line.text().starts_with("◌"))
                .all(|line| line.kind == TranscriptKind::Reasoning)
        );
        assert!(
            visible
                .iter()
                .filter(|line| line.text().contains("final") || line.text().contains("answer"))
                .all(|line| line.kind == TranscriptKind::Assistant)
        );
    }

    #[test]
    fn wrapping_preserves_styles_for_cjk_emoji_and_long_tokens() {
        let style =
            super::super::text::TranscriptStyle::tone(super::super::text::TranscriptTone::Label)
                .bold();
        let line = TranscriptLine::styled(
            TranscriptKind::Assistant,
            TranscriptText::styled_untrusted("中文🙂abcdefgh", style),
        );

        let wrapped = wrap_transcript_line(&line, 4);
        assert!(wrapped.len() > 2);
        assert!(wrapped.iter().all(|line| {
            UnicodeWidthStr::width(line.text()) <= 4
                && line
                    .content()
                    .segments()
                    .iter()
                    .all(|segment| segment.style == style)
        }));
    }

    #[test]
    fn wrapping_preserves_mixed_styles_across_repeated_words_and_wide_text() {
        use super::super::text::{TranscriptPaint, TranscriptStyle, TranscriptTone};

        let label = TranscriptStyle::tone(TranscriptTone::Label).bold();
        let detail = TranscriptStyle::tone(TranscriptTone::Muted);
        let code = TranscriptStyle::tone(TranscriptTone::InlineCode);
        let mut content = TranscriptText::default();
        content.push_untrusted("◆ shell ", label);
        content.push_untrusted("same 中文🙂 same ", detail);
        content.push_untrusted("cargo test", code);
        let expected = content.as_str().to_string();

        let wrapped = wrap_transcript_line(
            &TranscriptLine::styled(TranscriptKind::Activity, content),
            9,
        );
        let visible = wrapped.iter().map(TranscriptLine::text).collect::<String>();
        let without_spaces = |text: &str| {
            text.chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        };
        assert_eq!(without_spaces(&visible), without_spaces(&expected));
        assert!(wrapped.iter().any(|line| {
            line.content().segments().iter().any(|segment| {
                segment.text.contains("shell")
                    && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::Label)
            })
        }));
        assert!(wrapped.iter().any(|line| {
            line.content().segments().iter().any(|segment| {
                segment.text.contains("中文")
                    && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::Muted)
            })
        }));
        assert!(wrapped.iter().any(|line| {
            line.content().segments().iter().any(|segment| {
                segment.text.contains("cargo")
                    && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::InlineCode)
            })
        }));
    }

    #[test]
    fn follows_the_tail_until_the_user_scrolls() {
        let completed = lines((0..12).map(|row| format!("row {row}")));
        let mut viewport = TranscriptViewport::default();
        assert_eq!(
            visible(&mut viewport, &completed, 20, 4),
            ["row 8", "row 9", "row 10", "row 11"]
        );

        viewport.page_up();
        assert_eq!(viewport.offset(), 3);
        assert_eq!(
            visible(&mut viewport, &completed, 20, 4),
            ["row 5", "row 6", "row 7", "row 8"]
        );

        viewport.page_down();
        assert_eq!(viewport.offset(), 0);
    }

    #[test]
    fn streaming_does_not_move_a_scrolled_reading_position() {
        let mut completed = lines((0..12).map(|row| format!("row {row}")));
        let mut viewport = TranscriptViewport::default();
        visible(&mut viewport, &completed, 20, 4);
        viewport.page_up();
        let before = visible(&mut viewport, &completed, 20, 4);

        completed.push(TranscriptLine::new(TranscriptKind::Notice, "row 12"));
        let after = visible(&mut viewport, &completed, 20, 4);
        assert_eq!(before, after);
        assert_eq!(viewport.offset(), 4);
    }

    #[test]
    fn resize_rewraps_and_clamps_the_scroll_offset() {
        let completed = lines(["one two three four five six".to_string()]);
        let mut viewport = TranscriptViewport::default();
        visible(&mut viewport, &completed, 8, 2);
        viewport.page_up();
        assert!(viewport.offset() > 0);

        let visible = visible(&mut viewport, &completed, 40, 20);
        assert_eq!(viewport.offset(), 0);
        assert_eq!(visible, ["one two three four five six"]);
    }

    #[test]
    fn explicit_reset_replaces_same_length_content() {
        let mut viewport = TranscriptViewport::default();
        assert_eq!(
            visible(&mut viewport, &lines(["old".to_string()]), 20, 4),
            ["old"]
        );

        viewport.reset();
        assert_eq!(
            visible(&mut viewport, &lines(["new".to_string()]), 20, 4),
            ["new"]
        );
    }

    #[test]
    fn detached_resize_keeps_the_same_logical_line_at_the_top() {
        let completed = lines((0..12).map(|row| format!("ROW{row:02} ").repeat(8)));
        let mut viewport = TranscriptViewport::default();
        visible(&mut viewport, &completed, 24, 5);
        viewport.page_up();
        viewport.page_up();
        let before = visible(&mut viewport, &completed, 24, 5);
        let marker = before[0][..5].to_string();

        let narrow = visible(&mut viewport, &completed, 12, 5);
        assert!(narrow[0].contains(&marker), "{marker:?} not in {narrow:?}");

        let wide = visible(&mut viewport, &completed, 40, 5);
        assert!(wide[0].contains(&marker), "{marker:?} not in {wide:?}");
        assert!(viewport.offset() > 0, "resize must stay detached");
    }
}
