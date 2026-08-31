use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::terminal::plain_text;

use super::text::TranscriptText;

const DEFAULT_WIDTH: usize = 76;
const MAX_WIDTH: usize = 240;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) enum WorkbenchPhase {
    #[default]
    Ready,
    Thinking,
    ToolRunning,
    Failed,
}

impl WorkbenchPhase {
    pub(in crate::commands::chat) const fn marker(self) -> &'static str {
        match self {
            Self::Ready => "●",
            Self::Thinking => "◌",
            Self::ToolRunning => "◆",
            Self::Failed => "!",
        }
    }

    pub(in crate::commands::chat) const fn composer_placeholder(self) -> &'static str {
        match self {
            Self::Ready | Self::Failed => "Message · / commands · @ skills",
            Self::Thinking | Self::ToolRunning => "Steer · /stop",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) enum TranscriptKind {
    User,
    Assistant,
    Reasoning,
    Activity,
    #[default]
    Notice,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) struct TranscriptLine {
    pub(in crate::commands::chat) kind: TranscriptKind,
    content: TranscriptText,
}

impl TranscriptLine {
    pub(in crate::commands::chat) fn new(kind: TranscriptKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            content: TranscriptText::plain_untrusted(&text.into()),
        }
    }

    pub(in crate::commands::chat) const fn styled(
        kind: TranscriptKind,
        content: TranscriptText,
    ) -> Self {
        Self { kind, content }
    }

    pub(in crate::commands::chat) fn text(&self) -> &str {
        self.content.as_str()
    }

    pub(in crate::commands::chat) const fn content(&self) -> &TranscriptText {
        &self.content
    }

    pub(in crate::commands::chat) fn into_parts(self) -> (TranscriptKind, TranscriptText) {
        (self.kind, self.content)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::commands::chat) struct WorkbenchStatus {
    pub(in crate::commands::chat) terminal_width: usize,
    pub(in crate::commands::chat) model: String,
    pub(in crate::commands::chat) reasoning_effort: String,
    pub(in crate::commands::chat) phase: WorkbenchPhase,
    pub(in crate::commands::chat) rounds: u32,
    pub(in crate::commands::chat) tool_calls: u32,
    pub(in crate::commands::chat) input_tokens: u64,
    pub(in crate::commands::chat) output_tokens: u64,
    pub(in crate::commands::chat) context_used_tokens: u64,
    pub(in crate::commands::chat) context_window_tokens: Option<u64>,
    pub(in crate::commands::chat) cache_hit_percent: Option<f64>,
    pub(in crate::commands::chat) cost: Option<String>,
    pub(in crate::commands::chat) elapsed: Option<Duration>,
}

impl Default for WorkbenchStatus {
    fn default() -> Self {
        Self {
            terminal_width: DEFAULT_WIDTH,
            model: String::new(),
            reasoning_effort: String::new(),
            phase: WorkbenchPhase::Ready,
            rounds: 0,
            tool_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            context_used_tokens: 0,
            context_window_tokens: None,
            cache_hit_percent: None,
            cost: None,
            elapsed: None,
        }
    }
}

impl WorkbenchStatus {
    pub(in crate::commands::chat) fn render_status_bar(&self, terminal_width: usize) -> String {
        let width = terminal_width.min(MAX_WIDTH);
        let identity = self.render_identity();
        let mut counts = Vec::new();
        if self.rounds > 0 {
            counts.push(format!("turns {}", self.rounds));
            counts.push(format!("tools {}", self.tool_calls));
        }
        let tokens = (self.input_tokens > 0 || self.output_tokens > 0).then(|| {
            format!(
                "↑{} ↓{}",
                format_tokens(self.input_tokens),
                format_tokens(self.output_tokens)
            )
        });
        let context = self
            .context_window_tokens
            .filter(|window| *window > 0)
            .map(|window| {
                let percent = self.context_used_tokens as f64 * 100.0 / window as f64;
                format!(
                    "ctx {}/{} {:.1}%",
                    format_tokens(self.context_used_tokens),
                    format_context_window(window),
                    percent
                )
            });
        let cache = self
            .cache_hit_percent
            .filter(|value| value.is_finite())
            .map(|percent| format!("KV {:.1}%", percent.clamp(0.0, 100.0)));
        let cost = self
            .cost
            .as_deref()
            .filter(|cost| !cost.is_empty())
            .map(plain_text);
        let elapsed = self.elapsed.map(format_duration);

        let mut full = counts.clone();
        full.extend(tokens);
        full.extend(context.clone());
        full.extend(cache.clone());
        full.extend(cost.clone());
        full.extend(elapsed);
        if let Some(line) = status_anchors(&full, &identity, width) {
            return line;
        }

        // At ordinary terminal widths the durable telemetry wins over request
        // token and latency detail. Model identity stays right-aligned, while
        // context, KV, and cost remain visible for as long as space permits.
        let mut durable = counts;
        durable.extend(context.clone());
        durable.extend(cache.clone());
        durable.extend(cost.clone());
        if let Some(line) = status_anchors(&durable, &identity, width) {
            return line;
        }
        let essentials = context
            .clone()
            .into_iter()
            .chain(cache.clone())
            .chain(cost.clone())
            .collect::<Vec<_>>();
        if let Some(line) = status_anchors(&essentials, &identity, width) {
            return line;
        }
        if let Some(context) = context
            && let Some(line) = status_anchors(&[context], &identity, width)
        {
            return line;
        }
        let billing = cache.into_iter().chain(cost).collect::<Vec<_>>();
        status_anchors(&billing, &identity, width).unwrap_or_else(|| align_right(&identity, width))
    }

    pub(in crate::commands::chat) fn render_identity(&self) -> String {
        identity(self.phase, &self.model, &self.reasoning_effort)
    }

    pub(in crate::commands::chat) fn clear_request(&mut self) {
        self.rounds = 0;
        self.tool_calls = 0;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cache_hit_percent = None;
        self.cost = None;
        self.elapsed = None;
    }

    pub(in crate::commands::chat) fn reset_context(&mut self) {
        self.context_used_tokens = 0;
    }
}

#[derive(Clone, Debug)]
pub(in crate::commands::chat) enum WorkbenchEvent {
    StatusChanged(WorkbenchStatus),
    OperationStarted,
    OutputFragment {
        kind: TranscriptKind,
        content: TranscriptText,
    },
    OutputLine {
        kind: TranscriptKind,
        content: TranscriptText,
    },
    PromoteReasoning {
        content: TranscriptText,
    },
    InputSubmitted {
        id: u64,
        lines: Vec<String>,
    },
    FinishOutputLine,
    TranscriptCleared,
    OperationFinished,
}

#[derive(Debug, Default)]
pub(in crate::commands::chat) struct WorkbenchModel {
    pub(in crate::commands::chat) status: WorkbenchStatus,
    transcript: Vec<TranscriptLine>,
    live_output: TranscriptText,
    live_kind: TranscriptKind,
    pending_cr: bool,
    operation_finished: bool,
}

impl WorkbenchModel {
    pub(in crate::commands::chat) fn new(status: WorkbenchStatus) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// Apply one ordered runtime event. A partial line remains live while
    /// completed lines stay in the full-screen transcript.
    pub(in crate::commands::chat) fn apply(&mut self, event: WorkbenchEvent) {
        let mut completed = Vec::new();
        match event {
            WorkbenchEvent::StatusChanged(status) => self.status = status,
            WorkbenchEvent::OperationStarted => self.begin_operation(),
            WorkbenchEvent::OutputFragment { kind, content } => {
                self.push_fragment(kind, &content, &mut completed);
            }
            WorkbenchEvent::OutputLine { kind, content } => {
                self.finish_output_line(&mut completed);
                self.push_group_gap(kind, &mut completed);
                push_text_lines(kind, &content, &mut completed);
                self.live_kind = kind;
            }
            WorkbenchEvent::PromoteReasoning { content } => {
                self.promote_reasoning(&content, &mut completed);
            }
            WorkbenchEvent::InputSubmitted { lines, .. } => {
                self.finish_output_line(&mut completed);
                completed.extend(user_message_lines(lines));
                self.live_kind = TranscriptKind::User;
            }
            WorkbenchEvent::FinishOutputLine => self.finish_output_line(&mut completed),
            WorkbenchEvent::TranscriptCleared => {
                self.transcript.clear();
                self.live_output.clear();
                self.live_kind = TranscriptKind::Notice;
                self.pending_cr = false;
            }
            WorkbenchEvent::OperationFinished => {
                self.finish_output_line(&mut completed);
                self.operation_finished = true;
            }
        }
        self.transcript.extend(completed);
    }

    pub(in crate::commands::chat) fn transcript(&self) -> &[TranscriptLine] {
        &self.transcript
    }

    pub(in crate::commands::chat) const fn live_output(&self) -> &TranscriptText {
        &self.live_output
    }

    pub(in crate::commands::chat) const fn live_kind(&self) -> TranscriptKind {
        self.live_kind
    }

    pub(in crate::commands::chat) fn take_operation_finished(&mut self) -> bool {
        std::mem::take(&mut self.operation_finished)
    }

    pub(in crate::commands::chat) fn begin_operation(&mut self) {
        self.operation_finished = false;
    }

    fn push_fragment(
        &mut self,
        kind: TranscriptKind,
        fragment: &TranscriptText,
        completed: &mut Vec<TranscriptLine>,
    ) {
        if self.live_kind != kind {
            self.finish_output_line(completed);
            self.push_group_gap(kind, completed);
            self.live_kind = kind;
        }
        let mut start = 0;
        if self.pending_cr {
            if fragment.as_str().starts_with('\n') {
                completed.push(TranscriptLine::styled(
                    self.live_kind,
                    std::mem::take(&mut self.live_output),
                ));
                self.pending_cr = false;
                start = 1;
            } else if fragment.is_empty() {
                return;
            } else {
                self.live_output
                    .append(TranscriptText::plain_untrusted("\r"));
                self.pending_cr = false;
            }
        }

        for segment in fragment.as_str()[start..].split_inclusive('\n') {
            let segment_start = start;
            start = start.saturating_add(segment.len());
            if let Some(line) = segment.strip_suffix('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                let line_end = segment_start.saturating_add(line.len());
                self.live_output
                    .append(fragment.slice(segment_start..line_end));
                completed.push(TranscriptLine::styled(
                    self.live_kind,
                    std::mem::take(&mut self.live_output),
                ));
            } else if let Some(partial) = segment.strip_suffix('\r') {
                let partial_end = segment_start.saturating_add(partial.len());
                self.live_output
                    .append(fragment.slice(segment_start..partial_end));
                self.pending_cr = true;
            } else {
                self.live_output
                    .append(fragment.slice(segment_start..start));
            }
        }
    }

    fn finish_output_line(&mut self, completed: &mut Vec<TranscriptLine>) {
        if self.pending_cr {
            self.live_output
                .append(TranscriptText::plain_untrusted("\r"));
            self.pending_cr = false;
        }
        if self.live_output.is_empty() {
            return;
        }
        completed.push(TranscriptLine::styled(
            self.live_kind,
            std::mem::take(&mut self.live_output),
        ));
    }

    fn promote_reasoning(&mut self, content: &TranscriptText, completed: &mut Vec<TranscriptLine>) {
        if self.live_kind == TranscriptKind::Reasoning {
            self.live_output.clear();
            self.pending_cr = false;
        }
        while self
            .transcript
            .last()
            .is_some_and(|line| line.kind == TranscriptKind::Reasoning)
        {
            self.transcript.pop();
        }
        self.push_fragment(TranscriptKind::Assistant, content, completed);
    }

    fn push_group_gap(&self, next: TranscriptKind, completed: &mut Vec<TranscriptLine>) {
        let previous = completed.last().or_else(|| self.transcript.last());
        if needs_group_gap(previous, next) {
            completed.push(TranscriptLine::new(TranscriptKind::Notice, ""));
        }
    }
}

pub(in crate::commands::chat) fn user_message_lines(lines: Vec<String>) -> Vec<TranscriptLine> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut message = vec![TranscriptLine::new(TranscriptKind::User, "")];
    for line in lines {
        push_safe_lines(TranscriptKind::User, &line, &mut message);
    }
    message.push(TranscriptLine::new(TranscriptKind::User, ""));
    message
}

pub(in crate::commands::chat) fn push_group_gap(
    lines: &mut Vec<TranscriptLine>,
    next: TranscriptKind,
) {
    if needs_group_gap(lines.last(), next) {
        lines.push(TranscriptLine::new(TranscriptKind::Notice, ""));
    }
}

fn needs_group_gap(previous: Option<&TranscriptLine>, next: TranscriptKind) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let changes_group = matches!(
        (previous.kind, next),
        (
            TranscriptKind::User,
            TranscriptKind::Reasoning | TranscriptKind::Assistant | TranscriptKind::Activity
        ) | (
            TranscriptKind::Reasoning,
            TranscriptKind::Assistant | TranscriptKind::Activity
        ) | (
            TranscriptKind::Activity,
            TranscriptKind::Reasoning | TranscriptKind::Assistant
        ) | (TranscriptKind::Assistant, TranscriptKind::Activity)
    );
    changes_group && (previous.kind == TranscriptKind::User || !previous.text().is_empty())
}

fn push_safe_lines(kind: TranscriptKind, text: &str, completed: &mut Vec<TranscriptLine>) {
    completed.extend(text.split('\n').map(|line| {
        TranscriptLine::new(kind, plain_text(line.strip_suffix('\r').unwrap_or(line)))
    }));
}

fn push_text_lines(
    kind: TranscriptKind,
    content: &TranscriptText,
    completed: &mut Vec<TranscriptLine>,
) {
    completed.extend(
        content
            .lines_with_trailing_empty()
            .into_iter()
            .map(|line| TranscriptLine::styled(kind, line)),
    );
}

fn identity(phase: WorkbenchPhase, model: &str, effort: &str) -> String {
    let mut value = phase.marker().to_string();
    if !model.trim().is_empty() {
        value.push_str("  ");
        value.push_str(&plain_text(model.trim()));
    }
    if !effort.trim().is_empty() {
        value.push_str(" • ");
        value.push_str(&plain_text(effort.trim()));
    }
    value
}

fn status_anchors(fields: &[String], identity: &str, width: usize) -> Option<String> {
    if fields.is_empty() {
        return Some(align_right(identity, width));
    }
    let telemetry = fields.join(" · ");
    let telemetry_width = UnicodeWidthStr::width(telemetry.as_str());
    let minimum_gap = 2;
    let identity_width = UnicodeWidthStr::width(identity);
    let content_width = telemetry_width + minimum_gap + identity_width;
    if content_width > width {
        return None;
    }
    let padding = width - telemetry_width - identity_width;
    Some(format!("{telemetry}{}{identity}", " ".repeat(padding)))
}

fn align_right(value: &str, width: usize) -> String {
    let value = truncate_cells(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{}{value}", " ".repeat(padding))
}

pub(in crate::commands::chat) fn truncate_cells(value: &str, max_cells: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_cells {
        return value.to_string();
    }
    if max_cells <= 3 {
        return ".".repeat(max_cells);
    }
    let budget = max_cells - 3;
    let mut used = 0;
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > budget {
            break;
        }
        output.push_str(grapheme);
        used += width;
    }
    output.push_str("...");
    output
}

pub(in crate::commands::chat) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else {
        format_tokens(tokens)
    }
}

pub(in crate::commands::chat) fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(kind: TranscriptKind, text: &str) -> WorkbenchEvent {
        WorkbenchEvent::OutputFragment {
            kind,
            content: TranscriptText::stream_untrusted(text, Default::default()),
        }
    }

    fn line(kind: TranscriptKind, text: &str) -> WorkbenchEvent {
        WorkbenchEvent::OutputLine {
            kind,
            content: TranscriptText::stream_untrusted(text, Default::default()),
        }
    }

    fn transcript_text(model: &WorkbenchModel) -> Vec<&str> {
        model
            .transcript()
            .iter()
            .map(TranscriptLine::text)
            .collect()
    }

    #[test]
    fn status_is_one_compact_row() {
        let status = WorkbenchStatus {
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "high".to_string(),
            rounds: 2,
            tool_calls: 1,
            input_tokens: 3_200,
            output_tokens: 103,
            context_used_tokens: 3_200,
            context_window_tokens: Some(1_000_000),
            cache_hit_percent: Some(96.5),
            cost: Some("$0.001".to_string()),
            elapsed: Some(Duration::from_millis(2_500)),
            ..WorkbenchStatus::default()
        };
        let line = status.render_status_bar(120);
        assert!(line.ends_with("●  deepseek-v4-flash • high"));
        assert!(line.contains("turns 2 · tools 1"));
        assert!(line.contains("↑3.2k ↓103"));
        assert!(line.contains("ctx 3.2k/1.00M 0.3%"));
        assert!(line.contains("KV 96.5%"));
        assert!(line.contains("$0.001"));
        assert!(line.find("turns 2").unwrap() < line.find('●').unwrap());
        assert!(!line.contains("coverage"));
        assert!(!line.contains("estimate"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn ordinary_width_keeps_kv_and_cost_visible() {
        let status = WorkbenchStatus {
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "auto".to_string(),
            rounds: 8,
            tool_calls: 14,
            input_tokens: 117_700,
            output_tokens: 5_800,
            cache_hit_percent: Some(79.9),
            cost: Some("$0.010".to_string()),
            elapsed: Some(Duration::from_millis(62_700)),
            ..WorkbenchStatus::default()
        };
        let line = status.render_status_bar(76);
        assert!(line.contains("turns 8 · tools 14"));
        assert!(line.contains("KV 79.9%"));
        assert!(line.contains("$0.010"));
        assert!(line.ends_with("●  deepseek-v4-flash • auto"), "{line:?}");
        assert!(UnicodeWidthStr::width(line.as_str()) <= 76);
    }

    #[test]
    fn singular_counts_keep_stable_labels_and_zero_tools() {
        let status = WorkbenchStatus {
            rounds: 1,
            ..WorkbenchStatus::default()
        };
        let line = status.render_status_bar(76);
        assert!(line.contains("turns 1 · tools 0"));
    }

    #[test]
    fn compact_status_keeps_context_before_request_detail() {
        let status = WorkbenchStatus {
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "auto".to_string(),
            rounds: 8,
            tool_calls: 14,
            input_tokens: 117_700,
            output_tokens: 5_800,
            context_used_tokens: 117_700,
            context_window_tokens: Some(1_000_000),
            cache_hit_percent: Some(79.9),
            cost: Some("$0.010".to_string()),
            elapsed: Some(Duration::from_millis(62_700)),
            ..WorkbenchStatus::default()
        };

        let line = status.render_status_bar(76);

        assert!(line.contains("ctx 117.7k/1.00M 11.8%"), "{line:?}");
        assert!(line.contains("KV 79.9%"), "{line:?}");
        assert!(line.contains("$0.010"), "{line:?}");
        assert!(!line.contains("↑117.7k"), "{line:?}");
        assert!(UnicodeWidthStr::width(line.as_str()) <= 76);
    }

    #[test]
    fn zero_context_window_is_not_rendered() {
        let status = WorkbenchStatus {
            context_used_tokens: 100,
            context_window_tokens: Some(0),
            ..WorkbenchStatus::default()
        };

        assert!(!status.render_status_bar(76).contains("ctx"));
    }

    #[test]
    fn narrow_status_keeps_identity() {
        let status = WorkbenchStatus {
            model: "a-very-long-model-name".to_string(),
            reasoning_effort: "max".to_string(),
            input_tokens: 100,
            output_tokens: 10,
            ..WorkbenchStatus::default()
        };
        let line = status.render_status_bar(20);
        assert!(line.trim_start().starts_with('●'), "{line:?}");
        assert!(UnicodeWidthStr::width(line.as_str()) <= 20);
    }

    #[test]
    fn very_narrow_status_uses_the_real_terminal_width() {
        let status = WorkbenchStatus {
            model: "glm".to_string(),
            reasoning_effort: "high".to_string(),
            ..WorkbenchStatus::default()
        };
        let line = status.render_status_bar(10);
        assert!(!line.trim().is_empty(), "{line:?}");
        assert!(UnicodeWidthStr::width(line.as_str()) <= 10);
    }

    #[test]
    fn fragments_only_commit_real_lines_and_keep_the_partial_tail() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Reasoning, "思考🙂"));
        assert!(model.transcript().is_empty());
        assert_eq!(model.live_output().as_str(), "思考🙂");

        model.apply(fragment(TranscriptKind::Reasoning, "中\nanswer"));
        assert_eq!(transcript_text(&model), ["思考🙂中"]);
        assert_eq!(model.live_output().as_str(), "answer");
    }

    #[test]
    fn multiline_fragments_preserve_blank_lines_crlf_and_the_partial_tail() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Assistant, "first line\r"));
        assert_eq!(model.live_output().as_str(), "first line");

        model.apply(fragment(TranscriptKind::Assistant, "\n\nthird\ntail🙂"));

        assert_eq!(transcript_text(&model), ["first line", "", "third"]);
        assert_eq!(model.live_output().as_str(), "tail🙂");
    }

    #[test]
    fn many_single_line_fragments_append_without_rebuilding_the_live_tail() {
        let mut model = WorkbenchModel::default();

        for _ in 0..1_000 {
            model.apply(fragment(TranscriptKind::Assistant, "token🙂"));
        }

        assert!(model.transcript().is_empty());
        assert_eq!(model.live_output().as_str(), "token🙂".repeat(1_000));
    }

    #[test]
    fn styled_fragments_keep_spans_across_completed_and_live_lines() {
        let style =
            super::super::text::TranscriptStyle::tone(super::super::text::TranscriptTone::Label)
                .bold();
        let mut model = WorkbenchModel::default();
        model.apply(WorkbenchEvent::OutputFragment {
            kind: TranscriptKind::Assistant,
            content: TranscriptText::stream_untrusted("first\nsecond", style),
        });

        assert_eq!(model.transcript()[0].text(), "first");
        assert_eq!(model.live_output().as_str(), "second");
        assert!(model.transcript()[0].content().segments()[0].style.bold);
        assert!(model.live_output().segments()[0].style.bold);
    }

    #[test]
    fn complete_lines_normalize_crlf_before_terminal_escaping() {
        let mut model = WorkbenchModel::default();

        model.apply(line(TranscriptKind::Assistant, "first\r\nsecond"));
        assert_eq!(transcript_text(&model), ["first", "second"]);
    }

    #[test]
    fn finish_event_flushes_the_last_partial_before_waking_input() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Assistant, "last"));
        model.apply(WorkbenchEvent::OperationFinished);
        assert_eq!(transcript_text(&model), ["last"]);
        assert!(model.live_output().is_empty());
        assert!(model.take_operation_finished());
        assert!(!model.take_operation_finished());
    }

    #[test]
    fn output_line_never_merges_with_a_stream_tail() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Assistant, "answer"));
        model.apply(line(TranscriptKind::Activity, "◆ tool"));
        assert_eq!(transcript_text(&model), ["answer", "", "◆ tool"]);
        assert_eq!(model.transcript()[0].kind, TranscriptKind::Assistant);
        assert_eq!(model.transcript()[1].kind, TranscriptKind::Notice);
        assert_eq!(model.transcript()[2].kind, TranscriptKind::Activity);
        assert!(model.live_output().is_empty());
    }

    #[test]
    fn submitted_input_keeps_channel_order_with_stream_output() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Assistant, "answer"));
        model.apply(WorkbenchEvent::InputSubmitted {
            id: 1,
            lines: vec!["❯ /stop".to_string()],
        });
        assert_eq!(transcript_text(&model), ["answer", "", "❯ /stop", ""]);
        assert_eq!(model.transcript()[1].kind, TranscriptKind::User);
        assert_eq!(model.transcript()[2].kind, TranscriptKind::User);
        assert_eq!(model.transcript()[3].kind, TranscriptKind::User);
        assert!(model.live_output().is_empty());
    }

    #[test]
    fn semantic_group_transitions_insert_exactly_one_neutral_gap() {
        let mut model = WorkbenchModel::default();
        model.apply(line(TranscriptKind::Reasoning, "thinking"));
        model.apply(line(TranscriptKind::Activity, "◆ shell  cargo test"));
        model.apply(line(TranscriptKind::Activity, "✓ shell  cargo test"));
        model.apply(line(TranscriptKind::Assistant, "answer"));

        assert_eq!(
            transcript_text(&model),
            [
                "thinking",
                "",
                "◆ shell  cargo test",
                "✓ shell  cargo test",
                "",
                "answer"
            ]
        );
        assert_eq!(model.transcript()[1].kind, TranscriptKind::Notice);
        assert_eq!(model.transcript()[4].kind, TranscriptKind::Notice);
    }

    #[test]
    fn existing_neutral_blank_suppresses_a_second_group_gap() {
        let mut model = WorkbenchModel::default();
        model.apply(line(TranscriptKind::Reasoning, "thinking\n"));
        model.apply(line(TranscriptKind::Assistant, "answer"));

        assert_eq!(transcript_text(&model), ["thinking", "", "answer"]);
    }

    #[test]
    fn user_card_padding_and_response_spacer_have_distinct_backgrounds() {
        let mut model = WorkbenchModel::default();
        model.apply(WorkbenchEvent::InputSubmitted {
            id: 1,
            lines: vec!["❯ hello".to_string()],
        });
        model.apply(line(TranscriptKind::Reasoning, "thinking"));

        assert_eq!(transcript_text(&model), ["", "❯ hello", "", "", "thinking"]);
        assert_eq!(model.transcript()[0].kind, TranscriptKind::User);
        assert_eq!(model.transcript()[2].kind, TranscriptKind::User);
        assert_eq!(model.transcript()[3].kind, TranscriptKind::Notice);
    }

    #[test]
    fn changing_stream_kind_finishes_the_previous_visual_line() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Reasoning, "thinking"));
        model.apply(fragment(TranscriptKind::Assistant, "answer"));

        assert_eq!(transcript_text(&model), ["thinking", ""]);
        assert_eq!(model.transcript()[0].kind, TranscriptKind::Reasoning);
        assert_eq!(model.transcript()[1].kind, TranscriptKind::Notice);
        assert_eq!(model.live_output().as_str(), "answer");
        assert_eq!(model.live_kind(), TranscriptKind::Assistant);
    }

    #[test]
    fn promoted_reasoning_replaces_the_streamed_block_without_duplicate_text() {
        let mut model = WorkbenchModel::default();
        model.apply(fragment(TranscriptKind::Reasoning, "◌ **final"));
        model.apply(fragment(
            TranscriptKind::Reasoning,
            " answer**\nsecond line",
        ));

        assert_eq!(transcript_text(&model), ["◌ **final answer**"]);
        assert_eq!(model.live_kind(), TranscriptKind::Reasoning);
        assert_eq!(model.live_output().as_str(), "second line");

        let mut promoted = TranscriptText::styled_untrusted(
            "final answer",
            super::super::text::TranscriptStyle::plain().bold(),
        );
        promoted.push_safe("\nsecond line\n", Default::default());
        model.apply(WorkbenchEvent::PromoteReasoning { content: promoted });

        assert_eq!(transcript_text(&model), ["final answer", "second line"]);
        assert!(
            model
                .transcript()
                .iter()
                .all(|line| line.kind == TranscriptKind::Assistant)
        );
        assert_eq!(
            model
                .transcript()
                .iter()
                .filter(|line| line.text().contains("final answer"))
                .count(),
            1
        );
        assert!(model.transcript()[0].content().segments()[0].style.bold);
        assert!(model.live_output().is_empty());
    }

    #[test]
    fn clearing_the_session_also_clears_the_visible_transcript() {
        let mut model = WorkbenchModel::default();
        model.apply(line(TranscriptKind::Assistant, "old answer"));
        model.apply(fragment(TranscriptKind::Reasoning, "partial"));

        model.apply(WorkbenchEvent::TranscriptCleared);

        assert!(model.transcript().is_empty());
        assert!(model.live_output().is_empty());
    }
}
