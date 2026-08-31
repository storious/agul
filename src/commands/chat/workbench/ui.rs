use std::io::{self, Stdout, Write};

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};
use ratatui::{Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use super::state::{
    TranscriptKind, TranscriptLine, WorkbenchPhase, WorkbenchStatus, truncate_cells,
};
use super::text::{TranscriptPaint, TranscriptStyle, TranscriptText, TranscriptTone};
use super::transcript::{TranscriptViewport, wrap_display_lines};
use crate::terminal::plain_text;

const COMPOSER_CHROME_HEIGHT: u16 = 2;
const STATUS_HEIGHT: u16 = 1;
const MIN_COMPOSER_CONTENT_ROWS: u16 = 1;
const MIN_COMPOSER_CONTENT_CAP: u16 = 5;
const MAX_COMPOSER_CONTENT_ROWS: u16 = 10;
const MAX_CANDIDATE_ROWS: u16 = 5;
const MIN_FRAMED_COMPOSER_WIDTH: u16 = 5;
const COMPACT_HORIZONTAL_GUTTER: u16 = 1;
const COMFORTABLE_HORIZONTAL_GUTTER: u16 = 2;
const VERTICAL_GUTTER: u16 = 1;
const MIN_GUTTERED_HEIGHT: u16 = 10;
const MIN_FULL_GUTTERED_HEIGHT: u16 = MIN_GUTTERED_HEIGHT + 1;
const COMFORTABLE_TRANSCRIPT_BOTTOM_GUTTER: u16 = 2;
const COMPACT_TRANSCRIPT_BOTTOM_GUTTER: u16 = 1;
const MIN_COMFORTABLE_HEIGHT: u16 = 18;
const MIN_TRANSCRIPT_ROWS: u16 = 4;
pub(in crate::commands::chat) const WORKBENCH_BLUE: Color = Color::Rgb(66, 142, 255);
pub(in crate::commands::chat) const WORKBENCH_BLACK: Color = Color::Rgb(10, 13, 18);
pub(in crate::commands::chat) const WORKBENCH_GOLD: Color = Color::Rgb(232, 184, 74);
const WORKBENCH_PANEL: Color = Color::Rgb(15, 20, 29);
const WORKBENCH_USER_BG: Color = Color::Rgb(24, 32, 46);
const WORKBENCH_TEXT: Color = Color::Rgb(222, 226, 234);
const WORKBENCH_MUTED: Color = Color::Rgb(132, 145, 164);

type FullscreenTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(in crate::commands::chat) fn style_editor(
    editor: &mut TextArea<'_>,
    color: bool,
    placeholder: &str,
) {
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_placeholder_text(placeholder);
    editor.set_selection_style(Style::default().add_modifier(Modifier::BOLD));
    if !color {
        return;
    }
    editor.set_style(Style::default().fg(WORKBENCH_TEXT).bg(WORKBENCH_PANEL));
    editor.set_cursor_style(
        Style::default()
            .fg(WORKBENCH_GOLD)
            .bg(WORKBENCH_PANEL)
            .add_modifier(Modifier::REVERSED),
    );
    editor.set_selection_style(
        Style::default()
            .fg(WORKBENCH_BLACK)
            .bg(WORKBENCH_BLUE)
            .add_modifier(Modifier::BOLD),
    );
    editor.set_placeholder_style(Style::default().fg(WORKBENCH_MUTED).bg(WORKBENCH_PANEL));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct CandidateItem<'a> {
    pub(in crate::commands::chat) value: &'a str,
    pub(in crate::commands::chat) description: Option<&'a str>,
}

impl<'a> CandidateItem<'a> {
    pub(in crate::commands::chat) const fn new(
        value: &'a str,
        description: Option<&'a str>,
    ) -> Self {
        Self { value, description }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::commands::chat) struct CandidateView<'a> {
    items: &'a [CandidateItem<'a>],
    selected: usize,
}

impl<'a> CandidateView<'a> {
    pub(in crate::commands::chat) const fn new(
        items: &'a [CandidateItem<'a>],
        selected: usize,
    ) -> Self {
        Self { items, selected }
    }

    pub(in crate::commands::chat) const fn empty() -> Self {
        Self {
            items: &[],
            selected: 0,
        }
    }

    fn visible(self, row_limit: u16) -> impl Iterator<Item = (usize, &'a CandidateItem<'a>)> {
        let visible = usize::from(row_limit)
            .min(usize::from(MAX_CANDIDATE_ROWS))
            .min(self.items.len());
        let selected = self.selected.min(self.items.len().saturating_sub(1));
        let max_start = self.items.len().saturating_sub(visible);
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(max_start);
        self.items.iter().enumerate().skip(start).take(visible)
    }
}

/// Owns the alternate-screen chat session and restores the original shell on
/// every exit path.
pub(in crate::commands::chat) struct WorkbenchTerminal {
    terminal: FullscreenTerminal,
    terminal_size: (u16, u16),
    editor_area: Option<Size>,
    transcript: TranscriptViewport,
    palette: Palette,
    restored: bool,
}

impl WorkbenchTerminal {
    pub(in crate::commands::chat) fn new(color: bool) -> io::Result<Self> {
        enable_raw_mode()?;

        let mut output = io::stdout();
        if let Err(error) = enter_workbench_screen(&mut output) {
            let _ = restore_workbench_screen(&mut io::stdout());
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(output);
        let mut terminal = match Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore_workbench_screen(&mut io::stdout());
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        let size = match terminal.size() {
            Ok(size) => size,
            Err(error) => {
                let _ = restore_workbench_screen(terminal.backend_mut());
                let _ = disable_raw_mode();
                return Err(error);
            }
        };
        Ok(Self {
            terminal,
            terminal_size: (size.width, size.height),
            editor_area: None,
            transcript: TranscriptViewport::default(),
            palette: Palette::new(color),
            restored: false,
        })
    }

    pub(in crate::commands::chat) fn draw(
        &mut self,
        status: &WorkbenchStatus,
        transcript: &[TranscriptLine],
        live_tail: &TranscriptText,
        live_kind: TranscriptKind,
        editor: &mut TextArea<'_>,
        candidates: CandidateView<'_>,
    ) -> io::Result<()> {
        set_composer_phase(editor, status.phase);
        self.draw_screen(
            transcript,
            live_tail,
            live_kind,
            editor,
            candidates,
            DockFooter::Status(status),
        )
    }

    pub(in crate::commands::chat) fn draw_picker(
        &mut self,
        title: &str,
        hint: &str,
        editor: &mut TextArea<'_>,
        candidates: CandidateView<'_>,
    ) -> io::Result<()> {
        // Picker titles and result counts replace one another rather than
        // append, so they cannot use the chat transcript's append-only cache.
        self.transcript.reset();
        self.draw_screen(
            &[TranscriptLine::new(TranscriptKind::Notice, title)],
            &TranscriptText::default(),
            TranscriptKind::Notice,
            editor,
            candidates,
            DockFooter::Hint(hint),
        )
    }

    fn draw_screen(
        &mut self,
        transcript: &[TranscriptLine],
        live_tail: &TranscriptText,
        live_kind: TranscriptKind,
        editor: &mut TextArea<'_>,
        candidates: CandidateView<'_>,
        footer: DockFooter<'_>,
    ) -> io::Result<()> {
        self.sync_size()?;
        let geometry = workbench_geometry(self.terminal_size.1, editor, self.terminal_size.0);
        let editor_area = editor_area_size(self.terminal_size.0, geometry);
        let reveal_all = editor_area_grew(self.editor_area, editor_area)
            && editor_visual_rows(editor, self.terminal_size.0, geometry.framed)
                <= editor_area.height;
        let layout = workbench_layout(
            Rect::new(0, 0, self.terminal_size.0, self.terminal_size.1),
            geometry,
            candidates,
        );
        let visible = self.transcript.visible_lines(
            transcript,
            live_tail,
            live_kind,
            layout.transcript.width,
            layout.transcript.height,
        );
        let palette = self.palette;
        self.terminal.draw(|frame| {
            render_workbench(
                frame, &visible, editor, candidates, geometry, palette, footer,
            );
        })?;
        if reveal_all && scroll_editor_to_top(editor) {
            self.terminal.draw(|frame| {
                render_workbench(
                    frame, &visible, editor, candidates, geometry, palette, footer,
                );
            })?;
        }
        self.editor_area = Some(editor_area);
        Ok(())
    }

    pub(in crate::commands::chat) fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        let size = (width.max(1), height.max(1));
        if size == self.terminal_size {
            return Ok(());
        }
        self.terminal.resize(Rect::new(0, 0, size.0, size.1))?;
        self.terminal.clear()?;
        self.terminal_size = size;
        Ok(())
    }

    pub(in crate::commands::chat) fn page_up(&mut self) {
        self.transcript.page_up();
    }

    pub(in crate::commands::chat) fn page_down(&mut self) {
        self.transcript.page_down();
    }

    pub(in crate::commands::chat) fn reset_transcript(&mut self) {
        self.transcript.reset();
    }

    fn sync_size(&mut self) -> io::Result<()> {
        let size = self.terminal.size()?;
        self.resize(size.width, size.height)
    }

    /// Restore the shell explicitly so cleanup errors can be returned on the
    /// successful exit path. Drop remains the fallback for every other path.
    pub(in crate::commands::chat) fn finish(mut self) -> io::Result<()> {
        self.restore()
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let mut first_error = None;
        remember_error(
            &mut first_error,
            restore_workbench_screen(self.terminal.backend_mut()),
        );
        remember_error(&mut first_error, disable_raw_mode());

        if let Some(error) = first_error {
            Err(error)
        } else {
            self.restored = true;
            Ok(())
        }
    }
}

fn enter_workbench_screen(output: &mut impl Write) -> io::Result<()> {
    execute!(output, EnterAlternateScreen, EnableBracketedPaste)
}

fn restore_workbench_screen(output: &mut impl Write) -> io::Result<()> {
    let mut first_error = None;
    remember_error(
        &mut first_error,
        execute!(&mut *output, DisableBracketedPaste),
    );
    remember_error(
        &mut first_error,
        execute!(&mut *output, LeaveAlternateScreen),
    );
    remember_error(&mut first_error, execute!(&mut *output, Show));
    first_error.map_or(Ok(()), Err)
}

impl Drop for WorkbenchTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn remember_error(slot: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

#[derive(Clone, Copy)]
struct Palette {
    color: bool,
}

impl Palette {
    const fn new(color: bool) -> Self {
        Self { color }
    }

    fn border(self, phase: WorkbenchPhase) -> Style {
        match phase {
            WorkbenchPhase::Ready => {
                self.style(Some(WORKBENCH_BLUE), Some(WORKBENCH_PANEL), Modifier::DIM)
            }
            WorkbenchPhase::Thinking => self.style(
                Some(WORKBENCH_GOLD),
                Some(WORKBENCH_PANEL),
                Modifier::empty(),
            ),
            WorkbenchPhase::ToolRunning => {
                self.style(Some(WORKBENCH_BLUE), Some(WORKBENCH_PANEL), Modifier::BOLD)
            }
            WorkbenchPhase::Failed => self.style(
                Some(Color::Rgb(242, 112, 112)),
                Some(WORKBENCH_PANEL),
                Modifier::DIM,
            ),
        }
    }

    fn title(self) -> Style {
        self.style(Some(WORKBENCH_GOLD), None, Modifier::BOLD)
    }

    fn transcript(self, kind: TranscriptKind) -> Style {
        match kind {
            TranscriptKind::User => self.style(
                Some(WORKBENCH_TEXT),
                Some(WORKBENCH_USER_BG),
                Modifier::empty(),
            ),
            TranscriptKind::Assistant => self.style(Some(WORKBENCH_TEXT), None, Modifier::empty()),
            TranscriptKind::Reasoning => self.style(
                Some(WORKBENCH_MUTED),
                None,
                Modifier::DIM | Modifier::ITALIC,
            ),
            TranscriptKind::Activity => self.style(Some(WORKBENCH_MUTED), None, Modifier::empty()),
            TranscriptKind::Notice => self.style(Some(WORKBENCH_MUTED), None, Modifier::DIM),
        }
    }

    fn transcript_span(self, style: TranscriptStyle) -> Style {
        let (foreground, background) = match style.paint {
            TranscriptPaint::Default => (None, None),
            TranscriptPaint::Tone(TranscriptTone::Accent) => (Some(WORKBENCH_BLUE), None),
            TranscriptPaint::Tone(TranscriptTone::Label | TranscriptTone::CodeLabel) => {
                (Some(WORKBENCH_GOLD), None)
            }
            TranscriptPaint::Tone(TranscriptTone::Muted) => (Some(WORKBENCH_MUTED), None),
            TranscriptPaint::Tone(TranscriptTone::Success) => {
                (Some(Color::Rgb(80, 200, 145)), None)
            }
            TranscriptPaint::Tone(TranscriptTone::Error) => (Some(Color::Rgb(242, 112, 112)), None),
            TranscriptPaint::Tone(TranscriptTone::CodeText) => (Some(WORKBENCH_TEXT), None),
            TranscriptPaint::Tone(TranscriptTone::InlineCode) => {
                (Some(WORKBENCH_GOLD), Some(WORKBENCH_PANEL))
            }
            TranscriptPaint::Tone(TranscriptTone::Link) => (Some(WORKBENCH_BLUE), None),
            TranscriptPaint::Rgb(red, green, blue) => (Some(Color::Rgb(red, green, blue)), None),
        };
        let mut modifier = Modifier::empty();
        if style.bold {
            modifier |= Modifier::BOLD;
        }
        if style.italic {
            modifier |= Modifier::ITALIC;
        }
        if style.dim {
            modifier |= Modifier::DIM;
        }
        if style.underline {
            modifier |= Modifier::UNDERLINED;
        }
        if style.strike {
            modifier |= Modifier::CROSSED_OUT;
        }
        let mut span = Style::default().add_modifier(modifier);
        if self.color {
            if let Some(foreground) = foreground {
                span = span.fg(foreground);
            }
            if let Some(background) = background {
                span = span.bg(background);
            }
        }
        span
    }

    fn user_prompt(self) -> Style {
        self.style(
            Some(WORKBENCH_GOLD),
            Some(WORKBENCH_USER_BG),
            Modifier::BOLD,
        )
    }

    fn canvas(self) -> Style {
        self.style(None, Some(WORKBENCH_BLACK), Modifier::empty())
    }

    fn phase_marker(self, phase: WorkbenchPhase) -> Style {
        let color = match phase {
            WorkbenchPhase::Ready | WorkbenchPhase::ToolRunning => WORKBENCH_BLUE,
            WorkbenchPhase::Thinking => WORKBENCH_GOLD,
            WorkbenchPhase::Failed => Color::Rgb(242, 112, 112),
        };
        self.style(Some(color), None, Modifier::BOLD)
    }

    fn secondary(self) -> Style {
        self.style(Some(WORKBENCH_MUTED), None, Modifier::DIM)
    }

    fn status_model(self) -> Style {
        self.style(Some(WORKBENCH_TEXT), None, Modifier::empty())
    }

    fn status_effort(self) -> Style {
        self.style(Some(WORKBENCH_GOLD), None, Modifier::DIM)
    }

    fn candidate_marker(self) -> Style {
        self.style(Some(WORKBENCH_GOLD), None, Modifier::BOLD)
    }

    fn candidate_value(self, selected: bool) -> Style {
        if selected {
            self.style(Some(WORKBENCH_GOLD), None, Modifier::BOLD)
        } else {
            self.style(Some(WORKBENCH_TEXT), None, Modifier::empty())
        }
    }

    fn prompt(self) -> Style {
        self.style(Some(WORKBENCH_GOLD), Some(WORKBENCH_PANEL), Modifier::BOLD)
    }

    fn composer(self) -> Style {
        self.style(
            Some(WORKBENCH_TEXT),
            Some(WORKBENCH_PANEL),
            Modifier::empty(),
        )
    }

    fn style(
        self,
        foreground: Option<Color>,
        background: Option<Color>,
        modifier: Modifier,
    ) -> Style {
        let mut style = Style::default().add_modifier(modifier);
        if self.color {
            style = style.fg(foreground.unwrap_or(Color::Reset));
            if let Some(background) = background {
                style = style.bg(background);
            }
        }
        style
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkbenchLayout {
    transcript: Rect,
    candidates: Rect,
    composer: Rect,
    status: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkbenchGeometry {
    composer_height: u16,
    framed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VerticalRoom {
    top_gutter: u16,
    status_height: u16,
    above_status: u16,
}

#[derive(Clone, Copy)]
enum DockFooter<'a> {
    Status(&'a WorkbenchStatus),
    Hint(&'a str),
}

impl DockFooter<'_> {
    const fn phase(self) -> WorkbenchPhase {
        match self {
            Self::Status(status) => status.phase,
            Self::Hint(_) => WorkbenchPhase::Ready,
        }
    }
}

fn workbench_geometry(
    terminal_height: u16,
    editor: &TextArea<'_>,
    width: u16,
) -> WorkbenchGeometry {
    let terminal_height = terminal_height.max(1);
    let room = vertical_room(terminal_height);
    let available_height = room.above_status;
    let framed = can_frame_composer(width, available_height);
    let chrome_height = if framed { COMPOSER_CHROME_HEIGHT } else { 0 };
    let screen_content_room = available_height
        .saturating_sub(chrome_height)
        .max(MIN_COMPOSER_CONTENT_ROWS);
    let content_cap = composer_content_cap(terminal_height).min(screen_content_room);
    let content_height =
        editor_visual_rows(editor, width, framed).clamp(MIN_COMPOSER_CONTENT_ROWS, content_cap);
    let desired_composer = content_height.saturating_add(chrome_height);
    let composer_height = desired_composer.min(available_height);

    WorkbenchGeometry {
        composer_height,
        framed,
    }
}

fn editor_visual_rows(editor: &TextArea<'_>, width: u16, framed: bool) -> u16 {
    let editor_width = usize::from(composer_editor_width(width, framed));
    editor
        .lines()
        .iter()
        .map(|line| wrap_display_lines(line, editor_width).len().max(1))
        .fold(0usize, usize::saturating_add)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn editor_area_size(width: u16, geometry: WorkbenchGeometry) -> Size {
    let chrome_height = if geometry.framed {
        COMPOSER_CHROME_HEIGHT
    } else {
        0
    };
    Size::new(
        composer_editor_width(width, geometry.framed),
        geometry.composer_height.saturating_sub(chrome_height),
    )
}

fn composer_editor_width(terminal_width: u16, framed: bool) -> u16 {
    let inset = horizontal_inset(terminal_width);
    let outer_width = terminal_width.saturating_sub(inset.saturating_mul(2));
    let content_width = if framed {
        outer_width.saturating_sub(2)
    } else {
        outer_width
    };
    content_width
        .saturating_sub(prompt_width(content_width))
        .max(1)
}

fn prompt_width(width: u16) -> u16 {
    if width >= 3 {
        2
    } else {
        width.saturating_sub(1)
    }
}

fn can_frame_composer(width: u16, height: u16) -> bool {
    width >= MIN_FRAMED_COMPOSER_WIDTH && height >= 3
}

fn status_height(terminal_height: u16) -> u16 {
    if terminal_height > 1 {
        STATUS_HEIGHT
    } else {
        0
    }
}

fn horizontal_inset(width: u16) -> u16 {
    if width >= 16 {
        COMFORTABLE_HORIZONTAL_GUTTER
    } else if width >= 8 {
        COMPACT_HORIZONTAL_GUTTER
    } else {
        0
    }
}

fn vertical_gutters(height: u16) -> (u16, u16) {
    if height >= MIN_FULL_GUTTERED_HEIGHT {
        (VERTICAL_GUTTER, VERTICAL_GUTTER)
    } else if height >= MIN_GUTTERED_HEIGHT {
        (VERTICAL_GUTTER, 0)
    } else {
        (0, 0)
    }
}

fn vertical_room(height: u16) -> VerticalRoom {
    let (top_gutter, bottom_gutter) = vertical_gutters(height);
    let usable_height = height
        .saturating_sub(top_gutter)
        .saturating_sub(bottom_gutter);
    let status_height = status_height(usable_height);
    VerticalRoom {
        top_gutter,
        status_height,
        above_status: usable_height.saturating_sub(status_height),
    }
}

fn editor_area_grew(previous: Option<Size>, current: Size) -> bool {
    previous
        .is_some_and(|previous| current.width > previous.width || current.height > previous.height)
}

/// Rendering once with the expanded area refreshes TextArea's screen map, but
/// its viewport deliberately keeps the previous top row. When every wrapped
/// row now fits, persistently reveal the top without moving the data cursor.
fn scroll_editor_to_top(editor: &mut TextArea<'_>) -> bool {
    let cursor = editor.cursor();
    let previous = editor.clone();
    editor.scroll((-i16::MAX, 0));
    if editor.cursor() == cursor {
        true
    } else {
        *editor = previous;
        false
    }
}

fn composer_content_cap(terminal_height: u16) -> u16 {
    let proportional = (u32::from(terminal_height) * 3 / 10) as u16;
    proportional.clamp(MIN_COMPOSER_CONTENT_CAP, MAX_COMPOSER_CONTENT_ROWS)
}

fn workbench_layout(
    area: Rect,
    geometry: WorkbenchGeometry,
    candidates: CandidateView<'_>,
) -> WorkbenchLayout {
    let room = vertical_room(area.height);
    let composer_height = room.above_status.min(geometry.composer_height);
    let dock_height = room.above_status.saturating_sub(composer_height);
    let desired_gutter = if area.height >= MIN_COMFORTABLE_HEIGHT {
        COMFORTABLE_TRANSCRIPT_BOTTOM_GUTTER
    } else {
        COMPACT_TRANSCRIPT_BOTTOM_GUTTER
    };
    let transcript_gutter = desired_gutter.min(dock_height.saturating_sub(MIN_TRANSCRIPT_ROWS));
    let transcript_height = dock_height.saturating_sub(transcript_gutter);

    let inset = horizontal_inset(area.width);
    let composer_x = area.x.saturating_add(inset);
    let composer_width = area.width.saturating_sub(inset.saturating_mul(2));
    let content_padding = u16::from(geometry.framed && composer_width > 2);
    let content_x = composer_x.saturating_add(content_padding);
    let content_width = composer_width.saturating_sub(content_padding.saturating_mul(2));
    let transcript_y = area.y.saturating_add(room.top_gutter);

    let transcript = Rect::new(content_x, transcript_y, content_width, transcript_height);
    let candidate_height = u16::try_from(candidates.items.len())
        .unwrap_or(u16::MAX)
        .min(MAX_CANDIDATE_ROWS)
        .min(dock_height);
    let composer_y = transcript_y.saturating_add(dock_height);
    let candidate_y = composer_y.saturating_sub(candidate_height);
    let candidate_area = Rect::new(content_x, candidate_y, content_width, candidate_height);
    let composer = Rect::new(composer_x, composer_y, composer_width, composer_height);
    let status = Rect::new(
        content_x,
        composer.bottom(),
        content_width,
        room.status_height,
    );
    WorkbenchLayout {
        transcript,
        candidates: candidate_area,
        composer,
        status,
    }
}

fn render_workbench(
    frame: &mut ratatui::Frame<'_>,
    visible_transcript: &[TranscriptLine],
    editor: &TextArea<'_>,
    candidates: CandidateView<'_>,
    geometry: WorkbenchGeometry,
    palette: Palette,
    footer: DockFooter<'_>,
) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, palette.canvas());

    let layout = workbench_layout(area, geometry, candidates);
    render_transcript(
        frame.buffer_mut(),
        layout.transcript,
        visible_transcript,
        palette,
    );
    if !candidates.items.is_empty() {
        render_candidates(frame.buffer_mut(), layout.candidates, candidates, palette);
    }
    render_composer(frame, layout.composer, editor, palette, footer.phase());
    match footer {
        DockFooter::Status(status) => {
            render_status(frame.buffer_mut(), layout.status, status, palette);
        }
        DockFooter::Hint(hint) => render_hint(frame.buffer_mut(), layout.status, hint, palette),
    }
}

fn render_transcript(buffer: &mut Buffer, area: Rect, lines: &[TranscriptLine], palette: Palette) {
    if area.is_empty() {
        return;
    }
    let start_y = area.y;
    for (row, line) in lines.iter().enumerate() {
        let y = start_y.saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let base_style = palette.transcript(line.kind);
        let row_area = Rect::new(area.x, y, area.width, 1);
        buffer.set_style(row_area, base_style);
        let mut x = area.x;
        for segment in line.content().segments() {
            if x >= area.right() {
                break;
            }
            let remaining = area.right().saturating_sub(x);
            buffer.set_stringn(
                x,
                y,
                segment.text,
                usize::from(remaining),
                base_style.patch(palette.transcript_span(segment.style)),
            );
            let width = UnicodeWidthStr::width(segment.text)
                .min(usize::from(remaining))
                .try_into()
                .unwrap_or(remaining);
            x = x.saturating_add(width);
        }
        if line.kind == TranscriptKind::User && line.text().starts_with("❯ ") {
            let prompt_width = area.width.min(2);
            buffer.set_stringn(
                area.x,
                y,
                "❯ ",
                usize::from(prompt_width),
                palette.user_prompt(),
            );
        }
    }
}

fn render_status(buffer: &mut Buffer, area: Rect, status: &WorkbenchStatus, palette: Palette) {
    if area.is_empty() {
        return;
    }
    let line = status.render_status_bar(usize::from(area.width));
    buffer.set_stringn(
        area.x,
        area.y,
        &line,
        usize::from(area.width),
        palette.secondary(),
    );

    let identity = status.render_identity();
    let Some(prefix) = line.strip_suffix(&identity) else {
        if let Some(marker) = line.find(status.phase.marker()) {
            paint_segment(
                buffer,
                area,
                UnicodeWidthStr::width(&line[..marker]),
                status.phase.marker(),
                palette.phase_marker(status.phase),
            );
        }
        return;
    };
    let identity_x = UnicodeWidthStr::width(prefix);
    paint_segment(
        buffer,
        area,
        identity_x,
        status.phase.marker(),
        palette.phase_marker(status.phase),
    );

    let model = plain_text(status.model.trim());
    if !model.is_empty()
        && let Some(offset) = identity.find(&model)
    {
        paint_segment(
            buffer,
            area,
            identity_x.saturating_add(UnicodeWidthStr::width(&identity[..offset])),
            &model,
            palette.status_model(),
        );
    }
    let effort = plain_text(status.reasoning_effort.trim());
    if !effort.is_empty()
        && let Some(offset) = identity.rfind(&effort)
    {
        paint_segment(
            buffer,
            area,
            identity_x.saturating_add(UnicodeWidthStr::width(&identity[..offset])),
            &effort,
            palette.status_effort(),
        );
    }
}

fn render_hint(buffer: &mut Buffer, area: Rect, hint: &str, palette: Palette) {
    if area.is_empty() {
        return;
    }
    let hint = truncate_cells(&plain_text(hint), usize::from(area.width));
    buffer.set_stringn(
        area.x,
        area.y,
        hint,
        usize::from(area.width),
        palette.title(),
    );
}

fn render_candidates(
    buffer: &mut Buffer,
    area: Rect,
    candidates: CandidateView<'_>,
    palette: Palette,
) {
    if area.is_empty() {
        return;
    }

    // Candidate rows are an overlay. Reset the covered cells first so their
    // background and modifiers never depend on the transcript underneath.
    buffer.set_style(area, Style::reset());
    buffer.set_style(area, palette.canvas());

    let selected = candidates
        .selected
        .min(candidates.items.len().saturating_sub(1));
    for (row, (index, candidate)) in candidates.visible(area.height).enumerate() {
        let is_selected = index == selected;
        let marker = if is_selected { "› " } else { "  " };
        let value = plain_text(candidate.value);
        let description = candidate.description.map(plain_text);
        let position = is_selected.then(|| format!("{}/{}", index + 1, candidates.items.len()));
        let position_width = position.as_deref().map_or(0, |position| {
            UnicodeWidthStr::width(position).saturating_add(1)
        });
        let left_width = usize::from(area.width).saturating_sub(position_width);
        let value = truncate_cells(&value, left_width.saturating_sub(2));
        let line = description
            .filter(|description| !description.is_empty())
            .map_or_else(
                || format!("{marker}{value}"),
                |description| format!("{marker}{value}  {description}"),
            );
        let line = position.as_deref().map_or_else(
            || truncate_cells(&line, usize::from(area.width)),
            |position| anchored_line(&line, position, usize::from(area.width)),
        );
        let row_area = Rect::new(area.x, area.y + row as u16, area.width, 1);
        buffer.set_stringn(
            area.x,
            area.y + row as u16,
            &line,
            usize::from(area.width),
            palette.secondary(),
        );
        if is_selected {
            paint_segment(buffer, row_area, 0, "›", palette.candidate_marker());
        }
        paint_segment(
            buffer,
            row_area,
            2,
            &value,
            palette.candidate_value(is_selected),
        );
    }
}

fn render_composer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    editor: &TextArea<'_>,
    palette: Palette,
    phase: WorkbenchPhase,
) {
    if area.is_empty() {
        return;
    }

    if !can_frame_composer(area.width, area.height) {
        frame.buffer_mut().set_style(area, palette.composer());
        render_editor_row(frame, area, editor, palette);
        return;
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(palette.border(phase))
        .style(palette.composer());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_editor_row(frame, inner, editor, palette);
}

fn set_composer_phase(editor: &mut TextArea<'_>, phase: WorkbenchPhase) {
    editor.set_placeholder_text(phase.composer_placeholder());
}

fn paint_segment(buffer: &mut Buffer, area: Rect, offset: usize, value: &str, style: Style) {
    let Ok(offset) = u16::try_from(offset) else {
        return;
    };
    if offset >= area.width {
        return;
    }
    buffer.set_stringn(
        area.x.saturating_add(offset),
        area.y,
        value,
        usize::from(area.width.saturating_sub(offset)),
        style,
    );
}

fn render_editor_row(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    editor: &TextArea<'_>,
    palette: Palette,
) {
    if area.is_empty() {
        return;
    }
    let prompt_width = prompt_width(area.width);
    let prompt_area = Rect::new(area.x, area.y, prompt_width, area.height);
    frame.render_widget(
        Line::from(Span::styled("❯ ", palette.prompt())),
        prompt_area,
    );

    let editor_area = Rect::new(
        area.x.saturating_add(prompt_width),
        area.y,
        area.width.saturating_sub(prompt_width),
        area.height,
    );
    if !editor_area.is_empty() {
        frame.render_widget(editor, editor_area);
        if let Some(position) = editor_cursor_position(frame.buffer_mut(), editor_area) {
            frame.set_cursor_position(position);
        }
    }
}

fn anchored_line(left: &str, right: &str, width: usize) -> String {
    let right_width = UnicodeWidthStr::width(right);
    if right_width >= width {
        return truncate_cells(right, width);
    }
    let left_width = width - right_width - 1;
    let left = truncate_cells(left, left_width);
    let padding = width
        .saturating_sub(UnicodeWidthStr::width(left.as_str()))
        .saturating_sub(right_width);
    format!("{left}{}{right}", " ".repeat(padding))
}

fn editor_cursor_position(buffer: &Buffer, area: Rect) -> Option<(u16, u16)> {
    (area.y..area.bottom()).find_map(|row| {
        (area.x..area.right())
            .find(|column| buffer[(*column, row)].modifier.contains(Modifier::REVERSED))
            .map(|column| (column, row))
    })
}

#[cfg(test)]
mod tests {
    use ratatui::backend::{Backend, TestBackend};

    use super::*;

    #[test]
    fn alternate_screen_lifecycle_enables_and_restores_terminal_features_in_order() {
        let mut entered = Vec::new();
        enter_workbench_screen(&mut entered).unwrap();
        assert_eq!(
            String::from_utf8(entered).unwrap(),
            "\u{1b}[?1049h\u{1b}[?2004h"
        );

        let mut restored = Vec::new();
        restore_workbench_screen(&mut restored).unwrap();
        assert_eq!(
            String::from_utf8(restored).unwrap(),
            "\u{1b}[?2004l\u{1b}[?1049l\u{1b}[?25h"
        );
    }

    #[test]
    fn fullscreen_geometry_keeps_transcript_above_the_bottom_dock() {
        let editor = TextArea::default();
        let geometry = workbench_geometry(24, &editor, 80);
        let area = Rect::new(0, 0, 80, 24);
        let layout = workbench_layout(area, geometry, CandidateView::empty());
        assert_eq!(layout.transcript.y, 1);
        assert_eq!(layout.transcript.height, 16);
        assert_eq!(layout.candidates.height, 0);
        assert_eq!(layout.composer.height, 3);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.composer.x, 2);
        assert_eq!(layout.transcript.x, layout.composer.x + 1);
        assert_eq!(layout.status.x, layout.transcript.x);
        assert_eq!(layout.status.right(), layout.composer.right() - 1);
        assert_eq!(layout.transcript.bottom() + 2, layout.composer.y);
        assert_eq!(layout.composer.bottom(), layout.status.y);
        assert_eq!(layout.status.bottom(), area.bottom() - 1);
    }

    #[test]
    fn adding_short_terminal_rows_never_hides_the_last_candidate_row() {
        let editor = TextArea::new((0..5).map(|row| format!("line {row}")).collect());
        let items = [CandidateItem::new("/stop", Some("stop"))];
        let candidates = CandidateView::new(&items, 0);
        let mut previous_capacity = 0;

        for height in 9..=11 {
            let geometry = workbench_geometry(height, &editor, 80);
            let layout = workbench_layout(Rect::new(0, 0, 80, height), geometry, candidates);
            let capacity = layout
                .transcript
                .height
                .saturating_add(layout.candidates.height);
            assert!(
                capacity >= previous_capacity,
                "height {height} lost dock rows"
            );
            assert_eq!(layout.candidates.height, 1);
            assert_eq!(layout.candidates.bottom(), layout.composer.y);
            previous_capacity = capacity;
        }
    }

    #[test]
    fn multiline_and_wrapped_input_grow_with_a_screen_aware_cap() {
        let multiline = TextArea::from(["one", "two", "three", "four"]);
        let geometry = workbench_geometry(24, &multiline, 80);
        assert_eq!(geometry.composer_height, 6);

        let mut wrapped = TextArea::from(["alpha beta gamma delta epsilon"]);
        style_editor(&mut wrapped, false, "");
        let geometry = workbench_geometry(24, &wrapped, 12);
        assert!(geometry.composer_height > 3);

        let many_lines = (0..20).map(|index| index.to_string()).collect::<Vec<_>>();
        let editor = TextArea::new(many_lines);
        let geometry = workbench_geometry(24, &editor, 80);
        assert_eq!(geometry.composer_height, 9);

        let geometry = workbench_geometry(8, &editor, 80);
        assert_eq!(geometry.composer_height, 7);
    }

    #[test]
    fn growing_wrapped_input_keeps_both_ends_visible() {
        let width = 12;
        for (input, start, end) in [
            ("abcdefghijklmnopqrstuvwxyz", "abcdefghij", "uvwxyz"),
            ("alpha beta gamma delta epsilon zeta", "alpha", "zeta"),
        ] {
            let mut editor = TextArea::default();
            style_editor(&mut editor, false, "");
            let mut previous_area = None;
            let input_length = input.chars().count();

            for (index, character) in input.chars().enumerate() {
                editor.insert_char(character);
                let geometry = workbench_geometry(24, &editor, width);
                let editor_area = editor_area_size(width, geometry);
                let reveal_all = editor_area_grew(previous_area, editor_area)
                    && editor_visual_rows(&editor, width, geometry.framed) <= editor_area.height;
                let backend = TestBackend::new(width, 24);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| {
                        render_workbench(
                            frame,
                            &[],
                            &editor,
                            CandidateView::empty(),
                            geometry,
                            Palette::new(false),
                            DockFooter::Status(&WorkbenchStatus::default()),
                        );
                    })
                    .unwrap();
                if reveal_all {
                    let cursor = editor.cursor();
                    assert!(scroll_editor_to_top(&mut editor));
                    assert_eq!(editor.cursor(), cursor);
                    terminal
                        .draw(|frame| {
                            render_workbench(
                                frame,
                                &[],
                                &editor,
                                CandidateView::empty(),
                                geometry,
                                Palette::new(false),
                                DockFooter::Status(&WorkbenchStatus::default()),
                            );
                        })
                        .unwrap();
                }
                previous_area = Some(editor_area);

                if index + 1 == input_length {
                    let buffer = terminal.backend().buffer();
                    let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
                    let editor_x = layout.composer.x.saturating_add(3);
                    let editor_width = layout.composer.width.saturating_sub(4);
                    let visible = (layout.composer.y + 1..layout.composer.bottom() - 1)
                        .map(|row| buffer_span(buffer, editor_x, row, editor_width))
                        .collect::<String>();
                    assert!(visible.contains(start), "visible: {visible:?}");
                    assert!(visible.contains(end), "visible: {visible:?}");
                }
            }
        }
    }

    #[test]
    fn streaming_tail_uses_the_transcript_instead_of_a_four_row_dock() {
        let editor = TextArea::default();
        let live_tail = "one two three four five six seven eight nine ten eleven twelve";
        let geometry = workbench_geometry(24, &editor, 16);
        let visible = wrap_display_lines(live_tail, 16)
            .into_iter()
            .map(|line| TranscriptLine::new(TranscriptKind::Assistant, line))
            .collect::<Vec<_>>();

        let backend = TestBackend::new(16, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &visible,
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(false),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        let non_empty_tail_rows = (layout.transcript.y..layout.transcript.bottom())
            .filter(|row| !buffer_row(buffer, *row).trim().is_empty())
            .count();
        assert!(non_empty_tail_rows > 1);
        assert_eq!(layout.transcript.height, 16);
    }

    #[test]
    fn narrow_cjk_frame_keeps_menu_above_editor_and_status_below_it() {
        let status = WorkbenchStatus {
            model: "超长模型🙂".to_string(),
            reasoning_effort: "high".to_string(),
            ..WorkbenchStatus::default()
        };
        let editor = TextArea::from(["输入🙂中文"]);
        let items = [
            CandidateItem::new("/help", Some("帮助")),
            CandidateItem::new("/stop", Some("停止")),
            CandidateItem::new("/exit", Some("退出")),
            CandidateItem::new("/usage", Some("用量")),
        ];
        let candidates = CandidateView::new(&items, 2);
        let geometry = workbench_geometry(12, &editor, 24);
        let backend = TestBackend::new(24, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &[TranscriptLine::new(
                        TranscriptKind::Reasoning,
                        "思考中🙂思考中🙂",
                    )],
                    &editor,
                    candidates,
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&status),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, candidates);
        let status_row = buffer_row(buffer, layout.status.y);
        let first_editor_row = buffer_row(buffer, layout.composer.y + 1);
        let menu = (layout.candidates.y..layout.candidates.bottom())
            .map(|row| buffer_row(buffer, row))
            .collect::<String>();

        assert!(status_row.contains('●'));
        assert!(!status_row.contains('─'));
        assert!(first_editor_row.contains('❯'));
        assert!(menu.contains("/help"));
        assert!(menu.contains("/stop"));
        assert!(menu.contains("/exit"));
        assert!(menu.contains("/usage"));
        assert!(menu.contains("3/4"));
        assert!(layout.status.y > layout.composer.y);
    }

    #[test]
    fn transcript_roles_have_distinct_low_noise_styles() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let editor = TextArea::default();
        let geometry = workbench_geometry(12, &editor, 60);
        let transcript = [
            TranscriptLine::new(TranscriptKind::User, "❯ user"),
            TranscriptLine::new(TranscriptKind::Reasoning, "◌ thinking"),
            TranscriptLine::new(TranscriptKind::Assistant, "answer"),
            TranscriptLine::new(TranscriptKind::Activity, "◆ tool"),
            TranscriptLine::new(TranscriptKind::Notice, "↳ resumed"),
        ];
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &transcript,
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        let x = layout.transcript.x;
        let y = layout.transcript.y;
        let user_prompt = &buffer[(x, y)];
        let user_text = &buffer[(x + 2, y)];
        let user_tail = &buffer[(layout.transcript.right() - 1, y)];
        let reasoning = &buffer[(x, y + 1)];
        let assistant = &buffer[(x, y + 2)];
        let activity = &buffer[(x, y + 3)];

        assert_eq!(user_prompt.fg, WORKBENCH_GOLD);
        assert_eq!(user_text.fg, WORKBENCH_TEXT);
        assert_eq!(user_text.bg, WORKBENCH_USER_BG);
        assert_eq!(user_tail.bg, WORKBENCH_USER_BG);
        assert_eq!(reasoning.fg, WORKBENCH_MUTED);
        assert!(reasoning.modifier.contains(Modifier::ITALIC));
        assert!(reasoning.modifier.contains(Modifier::DIM));
        assert_eq!(assistant.fg, WORKBENCH_TEXT);
        assert!(!assistant.modifier.contains(Modifier::ITALIC));
        assert_eq!(activity.fg, WORKBENCH_MUTED);
    }

    #[test]
    fn markdown_and_tool_spans_keep_their_visual_hierarchy() {
        let mut heading = TranscriptText::default();
        heading.push_untrusted(
            "Heading",
            TranscriptStyle::tone(TranscriptTone::Label).bold(),
        );
        let mut inline = TranscriptText::plain_untrusted("use ");
        inline.push_untrusted(
            "cargo test",
            TranscriptStyle::tone(TranscriptTone::InlineCode),
        );
        let mut activity =
            TranscriptText::styled_untrusted("◆", TranscriptStyle::tone(TranscriptTone::Accent));
        activity.push_safe(" ", TranscriptStyle::default());
        activity.push_untrusted("shell", TranscriptStyle::tone(TranscriptTone::Label).bold());
        activity.push_safe("  ", TranscriptStyle::default());
        activity.push_untrusted("cargo test", TranscriptStyle::tone(TranscriptTone::Muted));
        let transcript = [
            TranscriptLine::styled(TranscriptKind::Assistant, heading),
            TranscriptLine::styled(TranscriptKind::Assistant, inline),
            TranscriptLine::styled(TranscriptKind::Activity, activity),
        ];
        let editor = TextArea::default();
        let geometry = workbench_geometry(12, &editor, 60);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &transcript,
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        let x = layout.transcript.x;
        let y = layout.transcript.y;
        assert_eq!(buffer[(x, y)].fg, WORKBENCH_GOLD);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x + 4, y + 1)].fg, WORKBENCH_GOLD);
        assert_eq!(buffer[(x + 4, y + 1)].bg, WORKBENCH_PANEL);
        assert_eq!(buffer[(x, y + 2)].fg, WORKBENCH_BLUE);
        assert_eq!(buffer[(x + 2, y + 2)].fg, WORKBENCH_GOLD);
        assert!(buffer[(x + 2, y + 2)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x + 9, y + 2)].fg, WORKBENCH_MUTED);
    }

    #[test]
    fn transcript_bottom_gutter_separates_output_from_the_composer() {
        let transcript = (0..16)
            .map(|row| TranscriptLine::new(TranscriptKind::Assistant, format!("row {row}")))
            .collect::<Vec<_>>();
        let editor = TextArea::default();
        let geometry = workbench_geometry(24, &editor, 80);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &transcript,
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        assert_eq!(layout.transcript.bottom() + 2, layout.composer.y);
        assert!(
            buffer_row(buffer, layout.transcript.bottom())
                .trim()
                .is_empty()
        );
        assert!(
            buffer_row(buffer, layout.transcript.bottom() + 1)
                .trim()
                .is_empty()
        );
        assert!(buffer_row(buffer, layout.transcript.bottom() - 1).contains("row 15"));
    }

    #[test]
    fn idle_frame_has_an_inset_rounded_composer() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let editor = TextArea::default();
        let geometry = workbench_geometry(24, &editor, 120);
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &[],
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(false),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        assert_eq!(layout.composer.x, 2);
        assert_eq!(layout.composer.right(), buffer.area.right() - 2);
        assert_eq!(layout.transcript.x, layout.composer.x + 1);
        assert_eq!(layout.status.x, layout.transcript.x);
        assert_eq!(layout.status.right(), layout.composer.right() - 1);
        assert!(buffer_row(buffer, 0).trim().is_empty());
        assert!(
            buffer_row(buffer, buffer.area.bottom() - 1)
                .trim()
                .is_empty()
        );
        assert_eq!(buffer[(layout.composer.x, layout.composer.y)].symbol(), "╭");
        assert_eq!(
            buffer[(layout.composer.right() - 1, layout.composer.y)].symbol(),
            "╮"
        );
        assert_eq!(
            buffer[(layout.composer.x, layout.composer.bottom() - 1)].symbol(),
            "╰"
        );
        assert_eq!(
            buffer[(layout.composer.right() - 1, layout.composer.bottom() - 1)].symbol(),
            "╯"
        );
        assert_eq!(
            buffer[(layout.composer.x, layout.composer.y + 1)].symbol(),
            "│"
        );
        assert!(buffer_row(buffer, layout.composer.y + 1).contains('❯'));
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor.y, layout.composer.y + 1);
        assert!(cursor.x > layout.composer.x + 1);
        assert!(cursor.x < layout.composer.right() - 1);
    }

    #[test]
    fn composer_placeholder_and_border_follow_the_runtime_phase() {
        for (phase, placeholder, border_color, modifier) in [
            (
                WorkbenchPhase::Ready,
                "Message · / commands · @ skills",
                WORKBENCH_BLUE,
                Modifier::DIM,
            ),
            (
                WorkbenchPhase::Thinking,
                "Steer · /stop",
                WORKBENCH_GOLD,
                Modifier::empty(),
            ),
            (
                WorkbenchPhase::ToolRunning,
                "Steer · /stop",
                WORKBENCH_BLUE,
                Modifier::BOLD,
            ),
            (
                WorkbenchPhase::Failed,
                "Message · / commands · @ skills",
                Color::Rgb(242, 112, 112),
                Modifier::DIM,
            ),
        ] {
            let mut editor = TextArea::default();
            style_editor(&mut editor, true, "");
            set_composer_phase(&mut editor, phase);
            let status = WorkbenchStatus {
                phase,
                ..WorkbenchStatus::default()
            };
            let geometry = workbench_geometry(12, &editor, 80);
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_workbench(
                        frame,
                        &[],
                        &editor,
                        CandidateView::empty(),
                        geometry,
                        Palette::new(true),
                        DockFooter::Status(&status),
                    );
                })
                .unwrap();

            let buffer = terminal.backend().buffer();
            let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
            assert!(
                buffer_row(buffer, layout.composer.y + 1).contains(placeholder),
                "phase {phase:?}"
            );
            let border = &buffer[(layout.composer.x, layout.composer.y)];
            assert_eq!(border.fg, border_color, "phase {phase:?}");
            assert_eq!(border.modifier, modifier, "phase {phase:?}");
        }
    }

    #[test]
    fn status_uses_quiet_telemetry_and_semantic_identity_colors() {
        let status = WorkbenchStatus {
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "high".to_string(),
            phase: WorkbenchPhase::Thinking,
            rounds: 3,
            tool_calls: 2,
            cache_hit_percent: Some(96.5),
            cost: Some("$0.001".to_string()),
            ..WorkbenchStatus::default()
        };
        let editor = TextArea::default();
        let geometry = workbench_geometry(12, &editor, 100);
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &[],
                    &editor,
                    CandidateView::empty(),
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&status),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
        let row = buffer_row(buffer, layout.status.y);
        let cell_x = |offset| UnicodeWidthStr::width(&row[..offset]) as u16;
        let telemetry_x = cell_x(row.find("turns").unwrap());
        let marker_x = cell_x(row.find('◌').unwrap());
        let model_x = cell_x(row.find("deepseek-v4-flash").unwrap());
        let effort_x = cell_x(row.rfind("high").unwrap());

        assert_eq!(buffer[(telemetry_x, layout.status.y)].fg, WORKBENCH_MUTED);
        assert!(
            buffer[(telemetry_x, layout.status.y)]
                .modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(buffer[(marker_x, layout.status.y)].fg, WORKBENCH_GOLD);
        assert_eq!(buffer[(model_x, layout.status.y)].fg, WORKBENCH_TEXT);
        assert_eq!(buffer[(effort_x, layout.status.y)].fg, WORKBENCH_GOLD);
        assert!(
            buffer[(effort_x, layout.status.y)]
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn candidate_window_tracks_the_selection_and_caps_at_five_rows() {
        let editor = TextArea::default();
        let items = [
            CandidateItem::new("one", None),
            CandidateItem::new("two", None),
            CandidateItem::new("three", None),
            CandidateItem::new("four", None),
            CandidateItem::new("five", None),
            CandidateItem::new("six", None),
            CandidateItem::new("seven", None),
        ];
        let candidates = CandidateView::new(&items, 6);
        let visible = candidates
            .visible(MAX_CANDIDATE_ROWS)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let geometry = workbench_geometry(24, &editor, 80);
        let layout = workbench_layout(Rect::new(0, 0, 80, 24), geometry, candidates);

        assert_eq!(visible, [2, 3, 4, 5, 6]);
        assert_eq!(layout.candidates.height, MAX_CANDIDATE_ROWS);
        assert_eq!(layout.candidates.bottom(), layout.composer.y);
    }

    #[test]
    fn candidate_rows_separate_values_descriptions_and_the_single_position() {
        let editor = TextArea::default();
        let items = [
            CandidateItem::new("/help", Some("show commands")),
            CandidateItem::new("/stop", Some("stop the active turn")),
        ];
        let candidates = CandidateView::new(&items, 1);
        let geometry = workbench_geometry(12, &editor, 60);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &[],
                    &editor,
                    candidates,
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, candidates);
        let first = buffer_row(buffer, layout.candidates.y);
        let selected = buffer_row(buffer, layout.candidates.y + 1);
        assert!(!first.contains("1/2"));
        assert!(selected.contains("2/2"));

        let first_value = first.find("/help").unwrap() as u16;
        let selected_marker = layout.candidates.x;
        let selected_value = selected.find("/stop").unwrap() as u16;
        let description = selected.find("stop the active").unwrap() as u16;
        assert_eq!(
            buffer[(first_value, layout.candidates.y)].fg,
            WORKBENCH_TEXT
        );
        assert_eq!(
            buffer[(selected_marker, layout.candidates.y + 1)].fg,
            WORKBENCH_GOLD
        );
        assert_eq!(
            buffer[(selected_value, layout.candidates.y + 1)].fg,
            WORKBENCH_GOLD
        );
        assert_eq!(
            buffer[(description, layout.candidates.y + 1)].fg,
            WORKBENCH_MUTED
        );
    }

    #[test]
    fn candidate_overlay_resets_transcript_background_and_modifiers() {
        let editor = TextArea::default();
        let transcript = (0..8)
            .map(|row| {
                let kind = if row % 2 == 0 {
                    TranscriptKind::User
                } else {
                    TranscriptKind::Reasoning
                };
                TranscriptLine::new(kind, format!("row {row}"))
            })
            .collect::<Vec<_>>();
        let items = [
            CandidateItem::new("/help", Some("commands")),
            CandidateItem::new("/stop", Some("stop")),
        ];
        let candidates = CandidateView::new(&items, 0);
        let geometry = workbench_geometry(12, &editor, 60);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &transcript,
                    &editor,
                    candidates,
                    geometry,
                    Palette::new(true),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, candidates);
        for y in layout.candidates.y..layout.candidates.bottom() {
            let cell = &buffer[(layout.candidates.x, y)];
            assert_eq!(cell.bg, WORKBENCH_BLACK);
            assert!(!cell.modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn short_screen_candidate_window_keeps_the_selection_visible() {
        let editor = TextArea::default();
        let items = [
            CandidateItem::new("one", None),
            CandidateItem::new("two", None),
            CandidateItem::new("three", None),
            CandidateItem::new("four", None),
            CandidateItem::new("five", None),
            CandidateItem::new("six", None),
            CandidateItem::new("seven", None),
        ];
        let candidates = CandidateView::new(&items, 6);
        let geometry = workbench_geometry(8, &editor, 40);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workbench(
                    frame,
                    &[],
                    &editor,
                    candidates,
                    geometry,
                    Palette::new(false),
                    DockFooter::Status(&WorkbenchStatus::default()),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let layout = workbench_layout(buffer.area, geometry, candidates);
        let menu = (layout.candidates.y..layout.candidates.bottom())
            .map(|row| buffer_row(buffer, row))
            .collect::<String>();
        assert_eq!(layout.candidates.height, 4);
        assert_eq!(layout.transcript.bottom(), layout.composer.y);
        assert!(menu.contains("seven"));
        assert!(menu.contains("7/7"));
    }

    #[test]
    fn extremely_narrow_frames_keep_all_rectangles_in_bounds() {
        for width in 1..=8 {
            for height in [1, 2, 3, 4, 8] {
                let editor = TextArea::from(["x"]);
                let geometry = workbench_geometry(height, &editor, width);
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| {
                        render_workbench(
                            frame,
                            &[],
                            &editor,
                            CandidateView::empty(),
                            geometry,
                            Palette::new(false),
                            DockFooter::Status(&WorkbenchStatus::default()),
                        );
                    })
                    .unwrap();

                let buffer = terminal.backend().buffer();
                let layout = workbench_layout(buffer.area, geometry, CandidateView::empty());
                assert!(layout.transcript.right() <= buffer.area.right());
                assert!(layout.composer.right() <= buffer.area.right());
                assert!(layout.status.right() <= buffer.area.right());
                assert!(layout.status.bottom() <= buffer.area.bottom());
                let screen = (0..height)
                    .map(|row| buffer_row(buffer, row))
                    .collect::<String>();
                assert!(screen.contains('x'), "{width}x{height}: {screen:?}");
                let cursor = terminal.backend_mut().get_cursor_position().unwrap();
                assert!(cursor.x < width && cursor.y < height);
            }
        }
    }

    fn buffer_row(buffer: &Buffer, row: u16) -> String {
        (buffer.area.x..buffer.area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn buffer_span(buffer: &Buffer, x: u16, row: u16, width: u16) -> String {
        (x..x.saturating_add(width))
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }
}
