use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};
use unicode_segmentation::UnicodeSegmentation;

use crate::runtime::is_skill_reference_char;

use super::super::message::CHAT_COMMANDS;

const HISTORY_CAPACITY: usize = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct TriggerCandidate {
    pub(in crate::commands::chat) value: String,
    pub(in crate::commands::chat) description: String,
}

impl TriggerCandidate {
    pub(in crate::commands::chat) fn new(
        value: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct CompletionItem {
    pub(in crate::commands::chat) value: String,
    pub(in crate::commands::chat) description: String,
    pub(in crate::commands::chat) replacement: Range<usize>,
    pub(in crate::commands::chat) append_whitespace: bool,
    submit_when_exact: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct CompletionState {
    items: Vec<CompletionItem>,
    selected: usize,
}

impl CompletionState {
    pub(in crate::commands::chat) fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    pub(in crate::commands::chat) const fn selected(&self) -> usize {
        self.selected
    }

    pub(in crate::commands::chat) fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    fn next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    fn previous(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) enum ComposerAction {
    None,
    Redraw,
    Submit(String),
    Interrupt,
    Eof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ComposerSnapshot {
    text: String,
    cursor: (usize, usize),
    selection_anchor: Option<(usize, usize)>,
}

impl ComposerSnapshot {
    fn capture(textarea: &TextArea<'_>) -> Self {
        let DataCursor(row, col) = textarea.cursor();
        let cursor = (row, col);
        let selection_anchor = textarea
            .selection_range()
            .map(|(start, end)| if cursor == start { end } else { start });
        Self {
            text: joined_text(textarea),
            cursor,
            selection_anchor,
        }
    }
}

pub(in crate::commands::chat) struct Composer {
    textarea: TextArea<'static>,
    reference_candidates: Vec<TriggerCandidate>,
    completion: Option<CompletionState>,
    history: VecDeque<String>,
    history_cursor: Option<usize>,
    history_prefix: Option<String>,
    draft: Option<ComposerSnapshot>,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Composer {
    pub(in crate::commands::chat) fn new(reference_candidates: Vec<TriggerCandidate>) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        let mut composer = Self {
            textarea,
            reference_candidates: Vec::new(),
            completion: None,
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            history_cursor: None,
            history_prefix: None,
            draft: None,
        };
        composer.set_reference_candidates(reference_candidates);
        composer
    }

    #[cfg(test)]
    fn textarea(&self) -> &TextArea<'static> {
        &self.textarea
    }

    pub(in crate::commands::chat) fn textarea_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.textarea
    }

    pub(in crate::commands::chat) fn text(&self) -> String {
        joined_text(&self.textarea)
    }

    pub(in crate::commands::chat) fn completion(&self) -> Option<&CompletionState> {
        self.completion.as_ref()
    }

    pub(in crate::commands::chat) fn set_reference_candidates(
        &mut self,
        mut reference_candidates: Vec<TriggerCandidate>,
    ) {
        reference_candidates.sort_by(|left, right| left.value.cmp(&right.value));
        reference_candidates.dedup_by(|left, right| left.value == right.value);
        self.reference_candidates = reference_candidates;
        if self.completion.is_some() {
            self.refresh_completion();
        }
    }

    #[cfg(test)]
    pub(in crate::commands::chat) fn history_len(&self) -> usize {
        self.history.len()
    }

    pub(in crate::commands::chat) fn clear(&mut self) {
        self.replace_textarea(ComposerSnapshot {
            text: String::new(),
            cursor: (0, 0),
            selection_anchor: None,
        });
        self.completion = None;
        self.reset_history_navigation();
    }

    pub(in crate::commands::chat) fn handle_event(&mut self, event: Event) -> ComposerAction {
        match event {
            Event::Paste(text) => {
                let text = normalize_paste(&text);
                if self.textarea.insert_str(text.as_ref()) {
                    self.after_edit(false, true)
                } else {
                    ComposerAction::None
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            _ => ComposerAction::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ComposerAction {
        if let Some(action) = self.handle_completion_key(key) {
            return action;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match (key.code, ctrl, alt, shift) {
            (KeyCode::Enter, _, true, _) | (KeyCode::Enter, false, false, true) => {
                self.textarea.insert_newline();
                self.after_edit(false, true)
            }
            (KeyCode::Char('j'), true, false, _) => {
                self.textarea.insert_newline();
                self.after_edit(false, true)
            }
            (KeyCode::Enter, _, false, false) => self.submit(),
            (KeyCode::Char('c'), true, false, _) => ComposerAction::Interrupt,
            (KeyCode::Char('d'), true, false, _) if self.textarea.is_empty() => ComposerAction::Eof,
            (KeyCode::Char('d'), true, false, _) | (KeyCode::Delete, false, false, _) => {
                let changed = self.delete_next_grapheme();
                self.after_edit(false, changed)
            }
            (KeyCode::Backspace, false, false, _) | (KeyCode::Char('h'), true, false, _) => {
                let changed = self.delete_previous_grapheme();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('u'), true, false, _) => {
                let changed = self.cut_to_buffer_start();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('z'), true, false, _) => {
                let changed = self.textarea.undo();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('g'), true, false, _) => {
                let changed = self.textarea.redo();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('k'), true, false, _) => {
                let changed = self.textarea.delete_line_by_end();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('w'), true, false, _) | (KeyCode::Backspace, false, true, _) => {
                let changed = self.textarea.delete_word();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('y'), true, false, _) => {
                let changed = self.textarea.paste();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('x'), true, false, _) => {
                let changed = self.textarea.cut();
                self.after_edit(false, changed)
            }
            (KeyCode::Char('r'), true, false, _) => self.move_up_or_history(),
            (KeyCode::Char('a'), true, false, true) => {
                self.textarea.select_all();
                ComposerAction::Redraw
            }
            (KeyCode::Char('a'), true, false, false) => self.move_cursor(CursorMove::Head, false),
            (KeyCode::Char('e'), true, false, _) => self.move_cursor(CursorMove::End, shift),
            (KeyCode::Char('b'), true, false, _) => self.move_cursor(CursorMove::Back, shift),
            (KeyCode::Char('f'), true, false, _) => self.move_cursor(CursorMove::Forward, shift),
            (KeyCode::Char('b'), false, true, _) | (KeyCode::Left, true, false, _) => {
                self.move_cursor(CursorMove::WordBack, shift)
            }
            (KeyCode::Char('f'), false, true, _) | (KeyCode::Right, true, false, _) => {
                self.move_cursor(CursorMove::WordForward, shift)
            }
            (KeyCode::Home, true, false, _) => self.move_cursor(CursorMove::Top, shift),
            (KeyCode::End, true, false, _) => self.move_cursor(CursorMove::Bottom, shift),
            (KeyCode::Home, false, false, _) => self.move_cursor(CursorMove::Head, shift),
            (KeyCode::End, false, false, _) => self.move_cursor(CursorMove::End, shift),
            (KeyCode::Left, false, false, _) => self.move_cursor(CursorMove::Back, shift),
            (KeyCode::Right, false, false, _) => self.move_cursor(CursorMove::Forward, shift),
            (KeyCode::Up, false, false, _) | (KeyCode::Char('p'), true, false, _) if !shift => {
                self.move_up_or_history()
            }
            (KeyCode::Down, false, false, _) | (KeyCode::Char('n'), true, false, _) if !shift => {
                self.move_down_or_history()
            }
            (KeyCode::Up, false, false, _) => self.move_cursor(CursorMove::Up, shift),
            (KeyCode::Down, false, false, _) => self.move_cursor(CursorMove::Down, shift),
            (KeyCode::Esc, false, false, _) => {
                self.textarea.cancel_selection();
                ComposerAction::Redraw
            }
            (KeyCode::Tab | KeyCode::BackTab, _, _, _) => ComposerAction::None,
            (KeyCode::Char(character), false, false, _)
            | (KeyCode::Char(character), true, true, _) => {
                self.textarea.insert_char(character);
                self.after_edit(matches!(character, '/' | '@'), true)
            }
            (KeyCode::Char(character), false, true, _) if !character.is_ascii() => {
                self.textarea.insert_char(character);
                self.after_edit(matches!(character, '/' | '@'), true)
            }
            _ => ComposerAction::None,
        }
    }

    fn handle_completion_key(&mut self, key: KeyEvent) -> Option<ComposerAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if self.completion.is_none() {
            return match key.code {
                KeyCode::Tab if !ctrl && !alt => {
                    self.open_completion(false);
                    Some(ComposerAction::Redraw)
                }
                KeyCode::BackTab if !ctrl && !alt => {
                    self.open_completion(true);
                    Some(ComposerAction::Redraw)
                }
                _ => None,
            };
        }

        match key.code {
            KeyCode::Esc if !ctrl && !alt => {
                self.completion = None;
                Some(ComposerAction::Redraw)
            }
            KeyCode::Tab if !ctrl && !alt => {
                self.completion.as_mut().expect("open completion").next();
                Some(ComposerAction::Redraw)
            }
            KeyCode::BackTab if !ctrl && !alt => {
                self.completion
                    .as_mut()
                    .expect("open completion")
                    .previous();
                Some(ComposerAction::Redraw)
            }
            KeyCode::Up if !ctrl && !alt && !shift => {
                self.completion
                    .as_mut()
                    .expect("open completion")
                    .previous();
                Some(ComposerAction::Redraw)
            }
            KeyCode::Down if !ctrl && !alt && !shift => {
                self.completion.as_mut().expect("open completion").next();
                Some(ComposerAction::Redraw)
            }
            KeyCode::Enter if !alt && !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let text = self.text();
                let submit = self.completion.as_ref().is_some_and(|completion| {
                    completion.selected_item().is_some_and(|item| {
                        item.submit_when_exact
                            && text
                                .get(item.replacement.clone())
                                .is_some_and(|value| value == item.value.as_str())
                    })
                });
                self.accept_completion();
                Some(if submit {
                    self.submit()
                } else {
                    ComposerAction::Redraw
                })
            }
            KeyCode::Backspace if !ctrl && !alt => {
                self.completion = None;
                None
            }
            KeyCode::Char('u') if ctrl && !alt => {
                self.completion = None;
                None
            }
            _ => None,
        }
    }

    fn move_cursor(&mut self, movement: CursorMove, selecting: bool) -> ComposerAction {
        if selecting {
            if !self.textarea.is_selecting() {
                self.textarea.start_selection();
            }
        } else {
            self.textarea.cancel_selection();
        }
        match movement {
            CursorMove::Back => {
                if let Some(target) = previous_grapheme_cursor(&self.textarea) {
                    move_to(&mut self.textarea, target);
                }
            }
            CursorMove::Forward => {
                if let Some(target) = next_grapheme_cursor(&self.textarea) {
                    move_to(&mut self.textarea, target);
                }
            }
            _ => {
                let snap_forward = matches!(movement, CursorMove::WordForward);
                self.textarea.move_cursor(movement);
                snap_cursor_to_grapheme(&mut self.textarea, snap_forward);
            }
        }
        if self.completion.is_some() {
            self.refresh_completion();
        }
        ComposerAction::Redraw
    }

    fn move_up_or_history(&mut self) -> ComposerAction {
        if self.try_move_vertical(CursorMove::Up) {
            return ComposerAction::Redraw;
        }
        if self.history.is_empty() {
            return ComposerAction::Redraw;
        }

        let next = match self.history_cursor {
            Some(current) => self.previous_history_match(current),
            None => {
                let snapshot = ComposerSnapshot::capture(&self.textarea);
                self.history_prefix = is_at_buffer_end(&self.textarea)
                    .then(|| snapshot.text.clone())
                    .filter(|prefix| !prefix.is_empty());
                self.draft = Some(snapshot);
                self.last_history_match()
            }
        };
        if let Some(index) = next {
            self.history_cursor = Some(index);
            self.load_history_entry(index);
        }
        ComposerAction::Redraw
    }

    fn move_down_or_history(&mut self) -> ComposerAction {
        if self.try_move_vertical(CursorMove::Down) {
            return ComposerAction::Redraw;
        }
        let Some(current) = self.history_cursor else {
            return ComposerAction::Redraw;
        };

        if let Some(index) = self.next_history_match(current) {
            self.history_cursor = Some(index);
            self.load_history_entry(index);
        } else if let Some(draft) = self.draft.take() {
            self.replace_textarea(draft);
            self.history_cursor = None;
            self.history_prefix = None;
        }
        ComposerAction::Redraw
    }

    fn try_move_vertical(&mut self, movement: CursorMove) -> bool {
        let before = self.textarea.cursor();
        self.textarea.cancel_selection();
        self.textarea.move_cursor(movement);
        snap_cursor_to_grapheme(&mut self.textarea, false);
        let changed = self.textarea.cursor() != before;
        if changed && self.completion.is_some() {
            self.refresh_completion();
        }
        changed
    }

    fn submit(&mut self) -> ComposerAction {
        if self.completion.is_some() {
            self.accept_completion();
        }
        let text = self.text();
        if !text.trim().is_empty() && self.history.back() != Some(&text) {
            if self.history.len() == HISTORY_CAPACITY {
                self.history.pop_front();
            }
            self.history.push_back(text.clone());
        }
        self.clear();
        ComposerAction::Submit(text)
    }

    fn after_edit(&mut self, open_completion: bool, changed: bool) -> ComposerAction {
        if !changed {
            return ComposerAction::Redraw;
        }
        self.reset_history_navigation();
        if open_completion {
            self.open_completion(false);
        } else if self.completion.is_some() {
            self.refresh_completion();
        }
        ComposerAction::Redraw
    }

    fn cut_to_buffer_start(&mut self) -> bool {
        if self.textarea.cursor() == (0, 0) {
            self.textarea.cancel_selection();
            return false;
        }
        self.textarea.cancel_selection();
        self.textarea.start_selection();
        move_to(&mut self.textarea, (0, 0));
        self.textarea.cut()
    }

    fn delete_previous_grapheme(&mut self) -> bool {
        if self.textarea.selection_range().is_some() {
            return self.textarea.insert_str("");
        }
        let Some(target) = previous_grapheme_cursor(&self.textarea) else {
            return false;
        };
        self.textarea.start_selection();
        move_to(&mut self.textarea, target);
        self.textarea.insert_str("")
    }

    fn delete_next_grapheme(&mut self) -> bool {
        if self.textarea.selection_range().is_some() {
            return self.textarea.insert_str("");
        }
        let Some(target) = next_grapheme_cursor(&self.textarea) else {
            return false;
        };
        self.textarea.start_selection();
        move_to(&mut self.textarea, target);
        self.textarea.insert_str("")
    }

    fn open_completion(&mut self, select_last: bool) {
        let text = self.text();
        let cursor = cursor_byte_offset(&self.textarea);
        let items = completion_items(&text, cursor, &self.reference_candidates);
        self.completion = (!items.is_empty()).then(|| CompletionState {
            selected: if select_last { items.len() - 1 } else { 0 },
            items,
        });
    }

    fn refresh_completion(&mut self) {
        let selected_value = self
            .completion
            .as_ref()
            .and_then(CompletionState::selected_item)
            .map(|item| item.value.clone());
        let text = self.text();
        let cursor = cursor_byte_offset(&self.textarea);
        let items = completion_items(&text, cursor, &self.reference_candidates);
        self.completion = (!items.is_empty()).then(|| {
            let selected = selected_value
                .as_deref()
                .and_then(|value| items.iter().position(|item| item.value == value))
                .unwrap_or(0);
            CompletionState { items, selected }
        });
    }

    fn accept_completion(&mut self) -> bool {
        let Some(item) = self
            .completion
            .take()
            .and_then(|state| state.selected_item().cloned())
        else {
            return false;
        };
        let text = self.text();
        let Some(start) = byte_offset_to_cursor(&text, item.replacement.start) else {
            return false;
        };
        let Some(end) = byte_offset_to_cursor(&text, item.replacement.end) else {
            return false;
        };

        self.textarea.cancel_selection();
        move_to(&mut self.textarea, start);
        self.textarea.start_selection();
        move_to(&mut self.textarea, end);
        let mut value = item.value;
        if item.append_whitespace {
            value.push(' ');
        }
        let changed = self.textarea.insert_str(value);
        if changed {
            self.reset_history_navigation();
        }
        changed
    }

    fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_prefix = None;
        self.draft = None;
    }

    fn history_matches(&self, value: &str) -> bool {
        self.history_prefix
            .as_deref()
            .is_none_or(|prefix| value.starts_with(prefix))
    }

    fn last_history_match(&self) -> Option<usize> {
        self.history
            .iter()
            .rposition(|entry| self.history_matches(entry))
    }

    fn previous_history_match(&self, current: usize) -> Option<usize> {
        self.history
            .iter()
            .take(current)
            .rposition(|entry| self.history_matches(entry))
    }

    fn next_history_match(&self, current: usize) -> Option<usize> {
        self.history
            .iter()
            .enumerate()
            .skip(current + 1)
            .find_map(|(index, entry)| self.history_matches(entry).then_some(index))
    }

    fn load_history_entry(&mut self, index: usize) {
        if let Some(text) = self.history.get(index).cloned() {
            let cursor = text_end_cursor(&text);
            self.replace_textarea(ComposerSnapshot {
                text,
                cursor,
                selection_anchor: None,
            });
            self.completion = None;
        }
    }

    fn replace_textarea(&mut self, snapshot: ComposerSnapshot) {
        let style = self.textarea.style();
        let cursor_style = self.textarea.cursor_style();
        let cursor_line_style = self.textarea.cursor_line_style();
        let selection_style = self.textarea.selection_style();
        let block = self.textarea.block().cloned();
        let tab_length = self.textarea.tab_length();
        let hard_tab_indent = self.textarea.hard_tab_indent();
        let line_number_style = self.textarea.line_number_style();
        let alignment = self.textarea.alignment();
        let wrap_mode = self.textarea.wrap_mode();
        let placeholder_text = self.textarea.placeholder_text().to_owned();
        let placeholder_style = self.textarea.placeholder_style();

        let lines = snapshot.text.split('\n').map(str::to_owned).collect();
        let mut textarea = TextArea::new(lines);
        textarea.set_style(style);
        textarea.set_cursor_style(cursor_style);
        textarea.set_cursor_line_style(cursor_line_style);
        textarea.set_selection_style(selection_style);
        if let Some(block) = block {
            textarea.set_block(block);
        }
        textarea.set_tab_length(tab_length);
        textarea.set_hard_tab_indent(hard_tab_indent);
        if let Some(style) = line_number_style {
            textarea.set_line_number_style(style);
        }
        textarea.set_alignment(alignment);
        textarea.set_wrap_mode(wrap_mode);
        textarea.set_placeholder_text(placeholder_text);
        if let Some(style) = placeholder_style {
            textarea.set_placeholder_style(style);
        }
        self.textarea = textarea;

        if let Some(anchor) = snapshot.selection_anchor {
            move_to(&mut self.textarea, anchor);
            self.textarea.start_selection();
        }
        move_to(&mut self.textarea, snapshot.cursor);
    }
}

fn completion_items(
    line: &str,
    pos: usize,
    reference_candidates: &[TriggerCandidate],
) -> Vec<CompletionItem> {
    if pos > line.len() || !line.is_char_boundary(pos) {
        return Vec::new();
    }
    let prefix = &line[..pos];
    let leading = line.len() - line.trim_start().len();
    let command_end = line[leading..]
        .find(char::is_whitespace)
        .map_or(line.len(), |offset| leading + offset);
    if (leading..=command_end).contains(&pos) {
        let command_prefix = &line[leading..pos];
        let remainder_is_whitespace = line[command_end..].chars().all(char::is_whitespace);
        if command_prefix.starts_with('/')
            && !command_prefix.starts_with("//")
            && !command_prefix.chars().any(char::is_whitespace)
            && remainder_is_whitespace
        {
            let builtins = CHAT_COMMANDS
                .iter()
                .filter(|(command, _, _)| command.starts_with(command_prefix))
                .map(|(command, description, _)| CompletionItem {
                    value: (*command).to_owned(),
                    description: (*description).to_owned(),
                    replacement: leading..command_end,
                    append_whitespace: false,
                    submit_when_exact: true,
                });
            let plugins = reference_candidates
                .iter()
                .filter(|candidate| {
                    candidate.value.starts_with('/') && candidate.value.starts_with(command_prefix)
                })
                .map(|candidate| CompletionItem {
                    value: candidate.value.clone(),
                    description: candidate.description.clone(),
                    replacement: leading..command_end,
                    append_whitespace: true,
                    submit_when_exact: false,
                });
            return builtins.chain(plugins).collect();
        }
    }

    let token_start = prefix
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let reference_prefix = &prefix[token_start..];
    let token_end = line[pos..]
        .find(char::is_whitespace)
        .map_or(line.len(), |offset| pos + offset);
    let reference_suffix = &line[pos..token_end];
    if !reference_prefix.starts_with('@')
        || reference_prefix.starts_with("@@")
        || !reference_suffix.chars().all(is_skill_reference_char)
    {
        return Vec::new();
    }
    reference_candidates
        .iter()
        .filter(|candidate| candidate.value.starts_with(reference_prefix))
        .map(|candidate| CompletionItem {
            value: candidate.value.clone(),
            description: candidate.description.clone(),
            replacement: token_start..token_end,
            append_whitespace: false,
            submit_when_exact: false,
        })
        .collect()
}

fn joined_text(textarea: &TextArea<'_>) -> String {
    textarea.lines().join("\n")
}

fn cursor_byte_offset(textarea: &TextArea<'_>) -> usize {
    let DataCursor(row, col) = textarea.cursor();
    let prior = textarea
        .lines()
        .iter()
        .take(row)
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let line = textarea.lines().get(row).map_or("", String::as_str);
    prior
        + line
            .char_indices()
            .nth(col)
            .map_or(line.len(), |(offset, _)| offset)
}

fn byte_offset_to_cursor(text: &str, offset: usize) -> Option<(usize, usize)> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let prefix = &text[..offset];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    Some((row, prefix[line_start..].chars().count()))
}

fn text_end_cursor(text: &str) -> (usize, usize) {
    let row = text.bytes().filter(|byte| *byte == b'\n').count();
    let line = text.rsplit_once('\n').map_or(text, |(_, line)| line);
    (row, line.chars().count())
}

fn move_to(textarea: &mut TextArea<'_>, target: (usize, usize)) {
    if let (Ok(row), Ok(col)) = (u16::try_from(target.0), u16::try_from(target.1)) {
        textarea.move_cursor(CursorMove::Jump(row, col));
        return;
    }

    textarea.move_cursor(CursorMove::Top);
    for _ in 0..target.0 {
        textarea.move_cursor(CursorMove::Down);
    }
    textarea.move_cursor(CursorMove::Head);
    for _ in 0..target.1 {
        textarea.move_cursor(CursorMove::Forward);
    }
}

fn previous_grapheme_cursor(textarea: &TextArea<'_>) -> Option<(usize, usize)> {
    let DataCursor(row, col) = textarea.cursor();
    if col == 0 {
        let previous_row = row.checked_sub(1)?;
        let previous_col = textarea.lines().get(previous_row)?.chars().count();
        return Some((previous_row, previous_col));
    }
    let line = textarea.lines().get(row)?;
    grapheme_char_boundaries(line)
        .into_iter()
        .rev()
        .find(|boundary| *boundary < col)
        .map(|boundary| (row, boundary))
}

fn next_grapheme_cursor(textarea: &TextArea<'_>) -> Option<(usize, usize)> {
    let DataCursor(row, col) = textarea.cursor();
    let line = textarea.lines().get(row)?;
    if let Some(boundary) = grapheme_char_boundaries(line)
        .into_iter()
        .find(|boundary| *boundary > col)
    {
        return Some((row, boundary));
    }
    (row + 1 < textarea.lines().len()).then_some((row + 1, 0))
}

fn snap_cursor_to_grapheme(textarea: &mut TextArea<'_>, forward: bool) {
    let DataCursor(row, col) = textarea.cursor();
    let Some(line) = textarea.lines().get(row) else {
        return;
    };
    let boundaries = grapheme_char_boundaries(line);
    if boundaries.contains(&col) {
        return;
    }
    let target = if forward {
        boundaries.into_iter().find(|boundary| *boundary > col)
    } else {
        boundaries
            .into_iter()
            .rev()
            .find(|boundary| *boundary < col)
    };
    if let Some(col) = target {
        move_to(textarea, (row, col));
    }
}

fn grapheme_char_boundaries(line: &str) -> Vec<usize> {
    let mut boundaries = Vec::with_capacity(line.graphemes(true).count() + 1);
    boundaries.push(0);
    let mut chars = 0;
    for grapheme in line.graphemes(true) {
        chars += grapheme.chars().count();
        boundaries.push(chars);
    }
    boundaries
}

fn is_at_buffer_end(textarea: &TextArea<'_>) -> bool {
    let DataCursor(row, col) = textarea.cursor();
    row + 1 == textarea.lines().len()
        && textarea
            .lines()
            .get(row)
            .is_some_and(|line| col == line.chars().count())
}

fn normalize_paste(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn type_text(composer: &mut Composer, text: &str) {
        for character in text.chars() {
            assert_eq!(
                composer.handle_event(key(KeyCode::Char(character), KeyModifiers::NONE)),
                ComposerAction::Redraw
            );
        }
    }

    fn submit(composer: &mut Composer, text: &str) {
        type_text(composer, text);
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit(text.to_owned())
        );
    }

    fn render_at_width(composer: &Composer, width: u16) {
        let area = Rect::new(0, 0, width, 4);
        let mut buffer = Buffer::empty(area);
        composer.textarea().render(area, &mut buffer);
    }

    #[test]
    fn cjk_emoji_and_normalized_multiline_paste_are_preserved() {
        let mut composer = Composer::default();
        assert_eq!(composer.textarea().wrap_mode(), WrapMode::WordOrGlyph);
        assert_eq!(
            composer.handle_event(Event::Paste("你好🙂\r\n第二行🚀\r末行".to_owned())),
            ComposerAction::Redraw
        );
        assert_eq!(composer.text(), "你好🙂\n第二行🚀\n末行");
        assert_eq!(composer.textarea().cursor(), (2, 2));
    }

    #[test]
    fn movement_selection_and_deletion_keep_extended_graphemes_whole() {
        let mut composer = Composer::default();
        composer.handle_event(Event::Paste("e\u{301}👍🏽🇨🇳👨‍👩‍👧‍👦".to_owned()));

        for expected in [6, 4, 2, 0] {
            composer.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
            assert_eq!(composer.textarea().cursor(), (0, expected));
        }
        composer.handle_event(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(composer.textarea().cursor(), (0, 2));

        composer.handle_event(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(composer.text(), "e\u{301}🇨🇳👨‍👩‍👧‍👦");
        composer.handle_event(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(composer.text(), "🇨🇳👨‍👩‍👧‍👦");
        assert_eq!(composer.textarea().cursor(), (0, 0));

        composer.handle_event(key(KeyCode::Right, KeyModifiers::SHIFT));
        type_text(&mut composer, "X");
        assert_eq!(composer.text(), "X👨‍👩‍👧‍👦");
    }

    #[test]
    fn altgr_printable_characters_are_inserted() {
        let mut composer = Composer::default();
        composer.handle_event(key(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(composer.text(), "@");

        composer.handle_event(key(KeyCode::Char('é'), KeyModifiers::ALT));
        assert_eq!(composer.text(), "@é");
    }

    #[test]
    fn alt_enter_ctrl_j_and_shift_enter_insert_newlines_while_enter_submits() {
        let mut composer = Composer::default();
        type_text(&mut composer, "one");
        composer.handle_event(key(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut composer, "two");
        composer.handle_event(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_text(&mut composer, "three");
        composer.handle_event(key(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut composer, "four");

        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit("one\ntwo\nthree\nfour".to_owned())
        );
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn slash_completion_keeps_prefix_order_and_full_token_span() {
        let mut composer = Composer::default();
        type_text(&mut composer, "\u{2003}/cost");
        composer.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        composer.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        let completion = composer.completion().expect("slash completion");
        assert_eq!(
            completion
                .items()
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            ["/cost", "/compact"]
        );
        assert_eq!(
            completion.items()[0].replacement,
            "\u{2003}".len().."\u{2003}/cost".len()
        );
    }

    #[test]
    fn tab_navigation_and_enter_accept_then_submit_once() {
        let mut composer = Composer::default();
        type_text(&mut composer, "/co");
        assert_eq!(composer.completion().expect("menu").selected(), 0);
        composer.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(composer.completion().expect("menu").selected(), 1);
        composer.handle_event(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(composer.completion().expect("menu").selected(), 0);

        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Redraw
        );
        assert_eq!(composer.text(), "/cost");
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit("/cost".to_owned())
        );
        assert_eq!(composer.history_len(), 1);
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn stop_submission_leaves_composer_ready_for_the_next_message() {
        let mut composer = Composer::default();
        type_text(&mut composer, "/stop");
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit("/stop".to_owned())
        );
        assert_eq!(composer.text(), "");

        submit(&mut composer, "continue with the next request");
        assert_eq!(composer.text(), "");
        assert_eq!(composer.history_len(), 2);
    }

    #[test]
    fn plugin_slash_completion_appends_whitespace_before_submit() {
        let mut composer = Composer::new(vec![TriggerCandidate::new(
            "/agent",
            "prepared specialists",
        )]);
        type_text(&mut composer, "/ag");
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Redraw
        );
        assert_eq!(composer.text(), "/agent ");
        type_text(&mut composer, "reviewer");
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit("/agent reviewer".to_owned())
        );
    }

    #[test]
    fn at_completion_accepts_without_submitting_the_prompt() {
        let mut composer = Composer::new(vec![TriggerCandidate::new(
            "@skill:planning",
            "planning context",
        )]);
        type_text(&mut composer, "@skill:pl");
        assert_eq!(
            composer.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Redraw
        );
        assert_eq!(composer.text(), "@skill:planning");
        type_text(&mut composer, " continue");
        assert_eq!(composer.text(), "@skill:planning continue");
    }

    #[test]
    fn at_completion_uses_utf8_byte_spans_and_does_not_consume_punctuation() {
        let references = [TriggerCandidate::new(
            "@skill:context-plan",
            "project Skill · planning",
        )];
        let line = "比较 @skill:context-plan later";
        let cursor = "比较 @skill:con".len();
        let items = completion_items(line, cursor, &references);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].replacement,
            "比较 ".len().."比较 @skill:context-plan".len()
        );
        assert!(completion_items("use @con,", "use @con".len(), &references).is_empty());
        assert!(completion_items("use (@con", "use (@con".len(), &references).is_empty());
        assert!(completion_items("compare @@skill:c", 17, &references).is_empty());
    }

    #[test]
    fn backspace_ctrl_u_and_escape_close_the_menu_before_editing() {
        let mut composer = Composer::default();
        type_text(&mut composer, "/co");
        assert!(composer.completion().is_some());
        composer.handle_event(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(composer.text(), "/c");
        assert!(composer.completion().is_none());

        composer.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(composer.completion().is_some());
        composer.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(composer.text(), "/c");
        assert!(composer.completion().is_none());

        composer.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
        composer.handle_event(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "");
        assert!(composer.completion().is_none());
    }

    #[test]
    fn ctrl_u_cuts_from_the_entire_multiline_buffer_start() {
        let mut composer = Composer::default();
        composer.handle_event(Event::Paste("first\nsecond\nthird".to_owned()));
        composer.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        composer.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        composer.handle_event(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "rd");
        assert_eq!(composer.textarea().cursor(), (0, 0));
    }

    #[test]
    fn ctrl_z_and_ctrl_g_map_to_undo_and_redo() {
        let mut composer = Composer::default();
        composer.handle_event(Event::Paste("你好🙂".to_owned()));
        composer.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "");
        composer.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "你好🙂");
    }

    #[test]
    fn ctrl_x_cuts_the_current_selection() {
        let mut composer = Composer::default();
        type_text(&mut composer, "abc");
        composer.handle_event(key(KeyCode::Left, KeyModifiers::SHIFT));
        composer.handle_event(key(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "ab");
    }

    #[test]
    fn selection_and_common_emacs_movement_replace_selected_text() {
        let mut composer = Composer::default();
        type_text(&mut composer, "abc def");
        composer.handle_event(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(composer.textarea().cursor(), (0, 0));
        composer.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(composer.textarea().cursor(), (0, 7));
        composer.handle_event(key(KeyCode::Left, KeyModifiers::SHIFT));
        composer.handle_event(key(KeyCode::Left, KeyModifiers::SHIFT));
        assert!(composer.textarea().selection_range().is_some());
        type_text(&mut composer, "X");
        assert_eq!(composer.text(), "abc dX");

        composer.handle_event(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.textarea().cursor(), (0, 4));
        composer.handle_event(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.textarea().cursor(), (0, 6));
    }

    #[test]
    fn history_restores_multiline_draft_and_suppresses_adjacent_duplicates() {
        let mut composer = Composer::default();
        submit(&mut composer, "first");
        submit(&mut composer, "second");
        submit(&mut composer, "second");
        assert_eq!(composer.history_len(), 2);

        composer.handle_event(Event::Paste("draft\nline".to_owned()));
        composer.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.text(), "draft\nline");
        composer.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.text(), "second");
        composer.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.text(), "draft\nline");
    }

    #[test]
    fn history_uses_prefix_matching_only_at_the_buffer_end() {
        let mut composer = Composer::default();
        submit(&mut composer, "cost report");
        submit(&mut composer, "compact now");
        submit(&mut composer, "other");

        type_text(&mut composer, "co");
        composer.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.text(), "compact now");
        composer.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.text(), "cost report");
        composer.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.text(), "compact now");
        composer.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.text(), "co");
    }

    #[test]
    fn ctrl_r_recalls_the_previous_history_entry() {
        let mut composer = Composer::default();
        submit(&mut composer, "first");
        submit(&mut composer, "second");
        composer.handle_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(composer.text(), "second");
    }

    #[test]
    fn wrapped_visual_rows_move_before_history_navigation() {
        let mut composer = Composer::default();
        submit(&mut composer, "older");
        type_text(&mut composer, "abcdefghij");
        render_at_width(&composer, 5);

        composer.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.text(), "abcdefghij");
        assert_eq!(composer.textarea().cursor(), (0, 4));
        composer.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.textarea().cursor(), (0, 9));
        assert_eq!(composer.text(), "abcdefghij");
    }

    #[test]
    fn ctrl_c_ctrl_d_and_release_events_are_integration_actions() {
        let mut composer = Composer::default();
        type_text(&mut composer, "/co");
        assert_eq!(
            composer.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ComposerAction::Interrupt
        );
        assert_eq!(composer.text(), "/co");
        assert!(composer.completion().is_some());
        composer.clear();
        assert_eq!(
            composer.handle_event(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            ComposerAction::Eof
        );

        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            composer.handle_event(Event::Key(release)),
            ComposerAction::None
        );
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn slash_and_at_escape_forms_never_open_completion() {
        let references = [TriggerCandidate::new("@skill:plan", "planning")];
        assert!(completion_items("//co", 4, &references).is_empty());
        assert!(completion_items("write /co", 9, &references).is_empty());
        assert!(completion_items("@@skill:p", 9, &references).is_empty());
        assert!(completion_items("/co\nordinary text", 3, &references).is_empty());
    }
}
