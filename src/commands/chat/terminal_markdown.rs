use std::sync::OnceLock;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::terminal::plain_text as escape_terminal_controls;

use super::workbench::{TranscriptPaint, TranscriptStyle, TranscriptText, TranscriptTone};

const MAX_PENDING_LINE_BYTES: usize = 64 * 1024;
const CODE_THEME: &str = "base16-ocean.dark";

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<ThemeSet> = OnceLock::new();

pub(super) struct TerminalMarkdown {
    styled: bool,
    pending: String,
    plain_until_newline: bool,
    fence: Option<CodeFence>,
    emitted_text: bool,
    ends_with_newline: bool,
}

struct CodeFence {
    marker: char,
    width: usize,
    highlighter: Option<HighlightLines<'static>>,
}

impl TerminalMarkdown {
    pub(super) fn new(styled: bool) -> Self {
        Self {
            styled,
            pending: String::new(),
            plain_until_newline: false,
            fence: None,
            emitted_text: false,
            ends_with_newline: false,
        }
    }

    pub(super) fn render_complete(markdown: &str, styled: bool) -> TranscriptText {
        let mut renderer = Self::new(styled);
        let mut rendered = renderer.push_untrusted(markdown);
        rendered.append(renderer.finish_response());
        rendered
    }

    pub(super) fn push_untrusted(&mut self, delta: &str) -> TranscriptText {
        if delta.is_empty() {
            return TranscriptText::default();
        }
        self.pending.push_str(&escape_terminal_controls(delta));
        let mut rendered = TranscriptText::default();
        while let Some(newline) = self.pending.find('\n') {
            let mut line: String = self.pending.drain(..=newline).collect();
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            if self.plain_until_newline {
                rendered.push_safe(&escape_terminal_controls(&line), TranscriptStyle::default());
                rendered.push_safe("\n", TranscriptStyle::default());
                self.plain_until_newline = false;
            } else {
                rendered.append(self.render_line_or_plain(&line));
                rendered.push_safe("\n", TranscriptStyle::default());
            }
            self.emitted_text = true;
            self.ends_with_newline = true;
        }
        if self.pending.len() > MAX_PENDING_LINE_BYTES {
            rendered.push_safe(
                &escape_terminal_controls(&self.pending),
                TranscriptStyle::default(),
            );
            self.pending.clear();
            self.plain_until_newline = true;
            self.emitted_text = true;
            self.ends_with_newline = false;
        }
        rendered
    }

    pub(super) fn finish_response(&mut self) -> TranscriptText {
        let mut rendered = TranscriptText::default();
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            if self.plain_until_newline {
                rendered.push_safe(
                    &escape_terminal_controls(&pending),
                    TranscriptStyle::default(),
                );
            } else {
                rendered.append(self.render_line_or_plain(&pending));
            }
            self.emitted_text = true;
            self.ends_with_newline = false;
        }
        self.plain_until_newline = false;
        if self.fence.take().is_some() {
            if self.emitted_text && !self.ends_with_newline {
                rendered.push_safe("\n", TranscriptStyle::default());
            }
            rendered.push_safe("  ╰─", TranscriptStyle::tone(TranscriptTone::Muted));
            self.ends_with_newline = false;
            self.emitted_text = true;
        }
        if self.emitted_text && !self.ends_with_newline {
            rendered.push_safe("\n", TranscriptStyle::default());
        }
        self.emitted_text = false;
        self.ends_with_newline = false;
        rendered
    }

    fn render_line_or_plain(&mut self, line: &str) -> TranscriptText {
        if line.len() <= MAX_PENDING_LINE_BYTES {
            return self.render_line(line);
        }
        if self.fence.is_some() {
            let mut rendered = TranscriptText::styled_untrusted(
                "  │ ",
                TranscriptStyle::tone(TranscriptTone::Muted),
            );
            rendered.push_safe(&escape_terminal_controls(line), TranscriptStyle::default());
            rendered
        } else {
            TranscriptText::plain_untrusted(line)
        }
    }

    fn render_line(&mut self, line: &str) -> TranscriptText {
        let styled = self.styled;
        if let Some(fence) = self.fence.as_mut() {
            if is_closing_fence(line, fence.marker, fence.width) {
                self.fence = None;
                return TranscriptText::styled_untrusted(
                    "  ╰─",
                    TranscriptStyle::tone(TranscriptTone::Muted),
                );
            }
            let mut rendered = TranscriptText::styled_untrusted(
                "  │ ",
                TranscriptStyle::tone(TranscriptTone::Muted),
            );
            rendered.append(render_code_line(line, fence, styled));
            return rendered;
        }
        if let Some(opening) = parse_opening_fence(line) {
            let language = opening.language.clone();
            self.fence = Some(CodeFence {
                marker: opening.marker,
                width: opening.width,
                highlighter: code_highlighter(&language, self.styled),
            });
            let label = if language.is_empty() {
                "code".to_string()
            } else {
                format!("code · {}", escape_terminal_controls(&language))
            };
            let mut rendered = TranscriptText::styled_untrusted(
                "  ╭─",
                TranscriptStyle::tone(TranscriptTone::Muted),
            );
            rendered.push_safe(" ", TranscriptStyle::default());
            rendered.push_untrusted(&label, TranscriptStyle::tone(TranscriptTone::CodeLabel));
            return rendered;
        }
        render_markdown_line(line, self.styled)
    }
}

struct OpeningFence {
    marker: char,
    width: usize,
    language: String,
}

fn parse_opening_fence(line: &str) -> Option<OpeningFence> {
    let trimmed = line.trim_start();
    if line.len().saturating_sub(trimmed.len()) > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let width = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    if width < 3 {
        return None;
    }
    let remainder = trimmed.get(width..)?.trim();
    let language = remainder
        .split_whitespace()
        .next()
        .filter(|token| {
            token.len() <= 32
                && token.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "+#-_".contains(character)
                })
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some(OpeningFence {
        marker,
        width,
        language,
    })
}

fn is_closing_fence(line: &str, marker: char, opening_width: usize) -> bool {
    let trimmed = line.trim();
    let width = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    width >= opening_width && trimmed.chars().all(|character| character == marker)
}

fn code_highlighter(language: &str, styled: bool) -> Option<HighlightLines<'static>> {
    if !styled || language.is_empty() {
        return None;
    }
    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let syntax = syntaxes
        .find_syntax_by_token(language)
        .or_else(|| syntaxes.find_syntax_by_extension(language))?;
    let theme = themes.themes.get(CODE_THEME)?;
    Some(HighlightLines::new(syntax, theme))
}

fn render_code_line(line: &str, fence: &mut CodeFence, styled: bool) -> TranscriptText {
    if !styled {
        return TranscriptText::plain_untrusted(line);
    }
    let Some(highlighter) = fence.highlighter.as_mut() else {
        return TranscriptText::styled_untrusted(
            line,
            TranscriptStyle::tone(TranscriptTone::CodeText),
        );
    };
    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let with_newline = format!("{line}\n");
    let Ok(regions) = highlighter.highlight_line(&with_newline, syntaxes) else {
        return TranscriptText::styled_untrusted(
            line,
            TranscriptStyle::tone(TranscriptTone::CodeText),
        );
    };
    let mut rendered = TranscriptText::default();
    for (style, region) in regions {
        let region = region.strip_suffix('\n').unwrap_or(region);
        if region.is_empty() {
            continue;
        }
        rendered.push_untrusted(region, syntect_style(style));
    }
    rendered
}

fn syntect_style(style: SyntectStyle) -> TranscriptStyle {
    let mut transcript =
        TranscriptStyle::rgb(style.foreground.r, style.foreground.g, style.foreground.b);
    transcript.bold = style.font_style.contains(FontStyle::BOLD);
    transcript.italic = style.font_style.contains(FontStyle::ITALIC);
    transcript.underline = style.font_style.contains(FontStyle::UNDERLINE);
    transcript
}

#[derive(Default)]
struct InlineStyle {
    heading: bool,
    strong: bool,
    emphasis: bool,
    strike: bool,
    code_block: bool,
    links: Vec<String>,
    images: Vec<String>,
    ordered_item: Option<u64>,
}

fn render_markdown_line(line: &str, styled: bool) -> TranscriptText {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_GFM);
    let mut rendered = TranscriptText::default();
    let mut style = InlineStyle::default();
    for event in Parser::new_ext(line, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => style.heading = true,
                Tag::BlockQuote(_) => {
                    rendered.push_safe("│ ", TranscriptStyle::tone(TranscriptTone::Muted))
                }
                Tag::CodeBlock(_) => style.code_block = true,
                Tag::List(start) => style.ordered_item = start,
                Tag::Item => {
                    let marker_style = if styled {
                        TranscriptStyle::tone(TranscriptTone::Accent)
                    } else {
                        TranscriptStyle::default()
                    };
                    if let Some(number) = style.ordered_item.as_mut() {
                        rendered.push_safe(&format!("{}. ", *number), marker_style);
                        *number = number.saturating_add(1);
                    } else {
                        rendered.push_safe("• ", marker_style);
                    }
                }
                Tag::Emphasis => style.emphasis = true,
                Tag::Strong => style.strong = true,
                Tag::Strikethrough => style.strike = true,
                Tag::Link { dest_url, .. } => style.links.push(dest_url.into_string()),
                Tag::Image { dest_url, .. } => {
                    rendered.push_safe("image: ", TranscriptStyle::tone(TranscriptTone::Muted));
                    style.images.push(dest_url.into_string());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => style.heading = false,
                TagEnd::CodeBlock => style.code_block = false,
                TagEnd::List(_) => style.ordered_item = None,
                TagEnd::Emphasis => style.emphasis = false,
                TagEnd::Strong => style.strong = false,
                TagEnd::Strikethrough => style.strike = false,
                TagEnd::Link => append_destination(&mut rendered, style.links.pop()),
                TagEnd::Image => append_destination(&mut rendered, style.images.pop()),
                _ => {}
            },
            Event::Text(text) => append_inline(&mut rendered, &text, &style, styled, false),
            Event::Code(code) => append_inline(&mut rendered, &code, &style, styled, true),
            Event::InlineMath(math) => {
                append_inline(&mut rendered, &format!("${math}$"), &style, styled, true);
            }
            Event::DisplayMath(math) => {
                append_inline(&mut rendered, &format!("$${math}$$"), &style, styled, true);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                append_inline(&mut rendered, &html, &style, styled, false);
            }
            Event::FootnoteReference(reference) => {
                rendered.push_untrusted(
                    &format!("[^{reference}]"),
                    TranscriptStyle::tone(TranscriptTone::Muted),
                );
            }
            Event::SoftBreak => rendered.push_safe(" ", TranscriptStyle::default()),
            Event::HardBreak => rendered.push_safe("\n", TranscriptStyle::default()),
            Event::Rule => rendered.push_safe(
                &"─".repeat(32),
                TranscriptStyle::tone(TranscriptTone::Muted),
            ),
            Event::TaskListMarker(checked) => {
                let marker_style = if styled {
                    TranscriptStyle::tone(if checked {
                        TranscriptTone::Success
                    } else {
                        TranscriptTone::Accent
                    })
                } else {
                    TranscriptStyle::default()
                };
                rendered.push_safe(if checked { "☑ " } else { "☐ " }, marker_style);
            }
        }
    }
    rendered
}

fn append_destination(rendered: &mut TranscriptText, destination: Option<String>) {
    if let Some(destination) = destination.filter(|destination| !destination.is_empty()) {
        rendered.push_safe(" ", TranscriptStyle::default());
        rendered.push_untrusted(
            &format!("<{destination}>"),
            TranscriptStyle::tone(TranscriptTone::Link)
                .dim()
                .underline(),
        );
    }
}

fn append_inline(
    rendered: &mut TranscriptText,
    text: &str,
    style: &InlineStyle,
    styled: bool,
    inline_code: bool,
) {
    if !styled {
        if inline_code || style.code_block {
            rendered.push_safe("`", TranscriptStyle::default());
            rendered.push_untrusted(text, TranscriptStyle::default());
            rendered.push_safe("`", TranscriptStyle::default());
        } else {
            rendered.push_untrusted(text, TranscriptStyle::default());
        }
        return;
    }

    let mut transcript = TranscriptStyle::default();
    if style.heading {
        transcript.paint = TranscriptPaint::Tone(TranscriptTone::Label);
        transcript.bold = true;
    }
    if style.strong {
        transcript.bold = true;
    }
    if style.emphasis {
        transcript.italic = true;
    }
    if style.strike {
        transcript = transcript.strike();
    }
    if inline_code || style.code_block {
        transcript.paint = TranscriptPaint::Tone(TranscriptTone::InlineCode);
    } else if !style.links.is_empty() {
        transcript.paint = TranscriptPaint::Tone(TranscriptTone::Link);
        transcript.underline = true;
    }
    rendered.push_untrusted(text, transcript);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_structure_is_visible_without_color() {
        let output =
            TerminalMarkdown::render_complete("# Title\n\n- **bold** and `code`\n> note\n", false);
        assert!(output.as_str().contains("Title"));
        assert!(output.as_str().contains("• bold and `code`"));
        assert!(output.as_str().contains("│ note"));
        assert!(!output.as_str().contains('\u{1b}'));
    }

    #[test]
    fn markdown_keeps_heading_strong_emphasis_and_inline_code_styles() {
        let output =
            TerminalMarkdown::render_complete("# Title\n**bold** and *italic* and `code`\n", true);
        let segments = output.segments();
        assert!(segments.iter().any(|segment| {
            segment.text == "Title"
                && segment.style.bold
                && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::Label)
        }));
        assert!(
            segments
                .iter()
                .any(|segment| segment.text == "bold" && segment.style.bold)
        );
        assert!(
            segments
                .iter()
                .any(|segment| segment.text == "italic" && segment.style.italic)
        );
        assert!(segments.iter().any(|segment| {
            segment.text == "code"
                && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::InlineCode)
        }));
    }

    #[test]
    fn unsafe_terminal_controls_and_decoded_entities_remain_visible() {
        let output = TerminalMarkdown::render_complete(
            "**x\u{1b}[31m** [link](https://example.test/\u{202e}) &#x1b;",
            true,
        );
        assert!(!output.as_str().contains('\u{1b}'));
        assert!(output.as_str().contains("\\u{1b}"));
        assert!(output.as_str().contains("\\u{202e}"));
    }

    #[test]
    fn streaming_delta_boundaries_preserve_fences_and_reset_each_response() {
        let mut renderer = TerminalMarkdown::new(false);
        let mut output = renderer.push_untrusted("```ru");
        output.append(renderer.push_untrusted("st\nfn main() {\n}"));
        output.append(renderer.push_untrusted("\n```"));
        output.append(renderer.finish_response());
        assert!(output.as_str().contains("code · rust"));
        assert!(output.as_str().contains("  │ fn main() {"));
        assert!(output.as_str().contains("  ╰─"));

        let next = renderer.push_untrusted("plain\n");
        assert_eq!(next.as_str(), "plain\n");
    }

    #[test]
    fn rust_code_uses_syntect_rgb_and_unknown_language_falls_back() {
        let rust = TerminalMarkdown::render_complete("```rust\nfn main() {}\n```", true);
        assert!(
            rust.segments()
                .iter()
                .any(|segment| matches!(segment.style.paint, TranscriptPaint::Rgb(_, _, _)))
        );

        let unknown = TerminalMarkdown::render_complete("```not-a-language\nvalue\n```", true);
        assert!(unknown.segments().iter().any(|segment| {
            segment.text == "value"
                && segment.style.paint == TranscriptPaint::Tone(TranscriptTone::CodeText)
        }));
    }

    #[test]
    fn complete_oversized_lines_never_enter_markdown_or_syntax_parsers() {
        let long_code = "x".repeat(MAX_PENDING_LINE_BYTES + 1);
        let output =
            TerminalMarkdown::render_complete(&format!("```rust\n{long_code}\n```\n"), true);
        output
            .as_str()
            .lines()
            .find(|line| line.contains(&long_code))
            .expect("oversized code line should remain visible");
        assert!(output.segments().iter().all(|segment| {
            !segment.text.contains(&long_code)
                || !matches!(segment.style.paint, TranscriptPaint::Rgb(_, _, _))
        }));

        let long_markdown = format!("**{}**\n", "y".repeat(MAX_PENDING_LINE_BYTES + 1));
        let plain = TerminalMarkdown::render_complete(&long_markdown, false);
        assert_eq!(plain.as_str(), long_markdown);
    }
}
