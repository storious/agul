use std::ops::Range;

use crate::terminal::plain_text;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) enum TranscriptPaint {
    #[default]
    Default,
    Tone(TranscriptTone),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) enum TranscriptTone {
    Accent,
    Label,
    Muted,
    Success,
    Error,
    CodeLabel,
    CodeText,
    InlineCode,
    Link,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) struct TranscriptStyle {
    pub(in crate::commands::chat) paint: TranscriptPaint,
    pub(in crate::commands::chat) bold: bool,
    pub(in crate::commands::chat) italic: bool,
    pub(in crate::commands::chat) dim: bool,
    pub(in crate::commands::chat) underline: bool,
    pub(in crate::commands::chat) strike: bool,
}

impl TranscriptStyle {
    pub(in crate::commands::chat) const fn tone(tone: TranscriptTone) -> Self {
        Self {
            paint: TranscriptPaint::Tone(tone),
            ..Self::plain()
        }
    }

    pub(in crate::commands::chat) const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            paint: TranscriptPaint::Rgb(red, green, blue),
            ..Self::plain()
        }
    }

    pub(in crate::commands::chat) const fn plain() -> Self {
        Self {
            paint: TranscriptPaint::Default,
            bold: false,
            italic: false,
            dim: false,
            underline: false,
            strike: false,
        }
    }

    pub(in crate::commands::chat) const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub(in crate::commands::chat) const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub(in crate::commands::chat) const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub(in crate::commands::chat) const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub(in crate::commands::chat) const fn strike(mut self) -> Self {
        self.strike = true;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StyleRange {
    bytes: Range<usize>,
    style: TranscriptStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) struct TranscriptText {
    text: String,
    styles: Vec<StyleRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct TranscriptSegment<'a> {
    pub(in crate::commands::chat) text: &'a str,
    pub(in crate::commands::chat) style: TranscriptStyle,
}

impl TranscriptText {
    pub(in crate::commands::chat) fn plain_untrusted(value: &str) -> Self {
        Self {
            text: plain_text(value),
            styles: Vec::new(),
        }
    }

    pub(in crate::commands::chat) fn styled_untrusted(value: &str, style: TranscriptStyle) -> Self {
        let mut text = Self::default();
        text.push_untrusted(value, style);
        text
    }

    pub(in crate::commands::chat) fn stream_untrusted(value: &str, style: TranscriptStyle) -> Self {
        let mut text = Self::default();
        let mut start = 0;
        for (index, character) in value.char_indices() {
            if character != '\r' {
                continue;
            }
            text.push_untrusted(&value[start..index], style);
            text.push_safe("\r", style);
            start = index + character.len_utf8();
        }
        text.push_untrusted(&value[start..], style);
        text
    }

    pub(in crate::commands::chat) fn as_str(&self) -> &str {
        &self.text
    }

    pub(in crate::commands::chat) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(in crate::commands::chat) fn clear(&mut self) {
        self.text.clear();
        self.styles.clear();
    }

    pub(in crate::commands::chat) fn push_untrusted(
        &mut self,
        value: &str,
        style: TranscriptStyle,
    ) {
        self.push_safe(&plain_text(value), style);
    }

    pub(in crate::commands::chat) fn push_safe(&mut self, value: &str, style: TranscriptStyle) {
        if value.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(value);
        if style == TranscriptStyle::default() {
            return;
        }
        let end = self.text.len();
        if let Some(last) = self.styles.last_mut()
            && last.bytes.end == start
            && last.style == style
        {
            last.bytes.end = end;
        } else {
            self.styles.push(StyleRange {
                bytes: start..end,
                style,
            });
        }
    }

    pub(in crate::commands::chat) fn append(&mut self, other: Self) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        for range in other.styles {
            let start = offset.saturating_add(range.bytes.start);
            let end = offset.saturating_add(range.bytes.end);
            if let Some(last) = self.styles.last_mut()
                && last.bytes.end == start
                && last.style == range.style
            {
                last.bytes.end = end;
            } else {
                self.styles.push(StyleRange {
                    bytes: start..end,
                    style: range.style,
                });
            }
        }
    }

    pub(in crate::commands::chat) fn slice(&self, bytes: Range<usize>) -> Self {
        let start = bytes.start.min(self.text.len());
        let end = bytes.end.min(self.text.len()).max(start);
        let mut sliced = Self {
            text: self.text[start..end].to_string(),
            styles: Vec::new(),
        };
        for range in &self.styles {
            let intersection_start = range.bytes.start.max(start);
            let intersection_end = range.bytes.end.min(end);
            if intersection_start < intersection_end {
                sliced.styles.push(StyleRange {
                    bytes: intersection_start - start..intersection_end - start,
                    style: range.style,
                });
            }
        }
        sliced
    }

    pub(in crate::commands::chat) fn prepend_plain(self, prefix: &str) -> Self {
        if prefix.is_empty() {
            return self;
        }
        let mut prefixed = Self::plain_untrusted(prefix);
        prefixed.append(self);
        prefixed
    }

    pub(in crate::commands::chat) fn expanded_tabs(&self) -> Self {
        if !self.text.contains('\t') {
            return self.clone();
        }
        let mut expanded = Self::default();
        for segment in self.segments() {
            expanded.push_safe(&segment.text.replace('\t', "    "), segment.style);
        }
        expanded
    }

    pub(in crate::commands::chat) fn lines(&self) -> Vec<Self> {
        self.split_lines(false)
    }

    pub(in crate::commands::chat) fn lines_with_trailing_empty(&self) -> Vec<Self> {
        self.split_lines(true)
    }

    fn split_lines(&self, include_trailing_empty: bool) -> Vec<Self> {
        let mut lines = Vec::new();
        let mut start: usize = 0;
        for segment in self.text.split_inclusive('\n') {
            let without_newline = segment.strip_suffix('\n').unwrap_or(segment);
            let line = without_newline
                .strip_suffix('\r')
                .unwrap_or(without_newline);
            let end = start.saturating_add(line.len());
            lines.push(self.slice(start..end));
            start = start.saturating_add(segment.len());
        }
        if include_trailing_empty && (self.text.is_empty() || self.text.ends_with('\n')) {
            lines.push(Self::default());
        }
        lines
    }

    pub(in crate::commands::chat) fn style_at(&self, byte: usize) -> TranscriptStyle {
        self.styles
            .iter()
            .find(|range| range.bytes.contains(&byte))
            .map_or_else(TranscriptStyle::default, |range| range.style)
    }

    pub(in crate::commands::chat) fn segments(&self) -> Vec<TranscriptSegment<'_>> {
        let mut segments = Vec::new();
        let mut cursor = 0;
        for range in &self.styles {
            if cursor < range.bytes.start {
                segments.push(TranscriptSegment {
                    text: &self.text[cursor..range.bytes.start],
                    style: TranscriptStyle::default(),
                });
            }
            segments.push(TranscriptSegment {
                text: &self.text[range.bytes.clone()],
                style: range.style,
            });
            cursor = range.bytes.end;
        }
        if cursor < self.text.len() {
            segments.push(TranscriptSegment {
                text: &self.text[cursor..],
                style: TranscriptStyle::default(),
            });
        }
        segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_rebase_and_preserve_overlapping_styles() {
        let mut text = TranscriptText::plain_untrusted("start ");
        text.push_untrusted(
            "styled",
            TranscriptStyle::tone(TranscriptTone::Label).bold(),
        );
        text.push_untrusted(" end", TranscriptStyle::default());

        let sliced = text.slice(3..10);
        assert_eq!(sliced.as_str(), "rt styl");
        assert_eq!(sliced.segments().len(), 2);
        assert_eq!(sliced.segments()[1].text, "styl");
        assert!(sliced.segments()[1].style.bold);
    }

    #[test]
    fn expanding_tabs_keeps_their_style() {
        let text = TranscriptText::styled_untrusted(
            "\tcode",
            TranscriptStyle::tone(TranscriptTone::CodeText),
        );
        let expanded = text.expanded_tabs();
        assert_eq!(expanded.as_str(), "    code");
        assert_eq!(expanded.segments().len(), 1);
        assert_eq!(
            expanded.segments()[0].style.paint,
            TranscriptPaint::Tone(TranscriptTone::CodeText)
        );
    }

    #[test]
    fn line_splitting_makes_trailing_empty_rows_explicit_only_when_requested() {
        let text = TranscriptText::plain_untrusted("one\ntwo\n");
        assert_eq!(
            text.lines()
                .iter()
                .map(TranscriptText::as_str)
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(
            text.lines_with_trailing_empty()
                .iter()
                .map(TranscriptText::as_str)
                .collect::<Vec<_>>(),
            ["one", "two", ""]
        );
        assert!(TranscriptText::default().lines().is_empty());
        assert_eq!(
            TranscriptText::default().lines_with_trailing_empty().len(),
            1
        );
    }
}
