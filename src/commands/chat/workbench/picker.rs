use std::io;

use crossterm::event;
use ratatui_textarea::{TextArea, WrapMode};

use super::{CandidateItem, CandidateView, WorkbenchTerminal, style_editor};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::commands::chat) struct SessionChoice {
    id: String,
    label: String,
    description: String,
    search: String,
}

impl SessionChoice {
    pub(in crate::commands::chat) fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        search: impl AsRef<str>,
    ) -> Self {
        let id = id.into();
        let label = label.into();
        let description = description.into();
        let search = format!("{id} {label} {description} {}", search.as_ref()).to_lowercase();
        Self {
            id,
            label,
            description,
            search,
        }
    }
}

#[derive(Debug)]
struct SessionPicker {
    choices: Vec<SessionChoice>,
    filtered: Vec<usize>,
    selected: usize,
}

impl SessionPicker {
    fn new(choices: Vec<SessionChoice>) -> Self {
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            filtered,
            selected: 0,
        }
    }

    fn set_query(&mut self, query: &str) {
        let selected_choice = self.filtered.get(self.selected).copied();
        let query = query.trim().to_lowercase();
        self.filtered = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| choice.search.contains(&query).then_some(index))
            .collect();
        self.selected = selected_choice
            .and_then(|selected| self.filtered.iter().position(|index| *index == selected))
            .unwrap_or(0);
    }

    fn next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
        }
    }

    fn previous(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.filtered.len() - 1);
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.choices.get(*index))
            .map(|choice| choice.id.clone())
    }
}

pub(in crate::commands::chat) fn select_session(
    choices: Vec<SessionChoice>,
    color: bool,
) -> io::Result<Option<String>> {
    let mut terminal = WorkbenchTerminal::new(color)?;
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    style_editor(&mut editor, color, "Search model, message, or workspace");
    let mut picker = SessionPicker::new(choices);

    let selected = loop {
        let candidates = picker
            .filtered
            .iter()
            .filter_map(|index| picker.choices.get(*index))
            .map(|choice| CandidateItem::new(&choice.label, Some(&choice.description)))
            .collect::<Vec<_>>();
        let title = format!("Resume · {}", candidates.len());
        terminal.draw_picker(
            &title,
            "↑↓ choose · Enter resume · Esc cancel",
            &mut editor,
            CandidateView::new(&candidates, picker.selected),
        )?;

        match event::read()? {
            event::Event::Resize(width, height) => terminal.resize(width.max(1), height.max(1))?,
            event::Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(event::KeyModifiers::CONTROL);
                match key.code {
                    event::KeyCode::Esc => break None,
                    event::KeyCode::Char('c') if ctrl => break None,
                    event::KeyCode::Enter => break picker.selected_id(),
                    event::KeyCode::Up | event::KeyCode::BackTab => picker.previous(),
                    event::KeyCode::Down | event::KeyCode::Tab => picker.next(),
                    event::KeyCode::Char('p') if ctrl => picker.previous(),
                    event::KeyCode::Char('n') if ctrl => picker.next(),
                    _ if editor.input(key) => {
                        picker.set_query(&editor.lines().join(" "));
                    }
                    _ => {}
                }
            }
            event::Event::Paste(text)
                if editor.insert_str(text.lines().collect::<Vec<_>>().join(" ")) =>
            {
                picker.set_query(&editor.lines().join(" "));
            }
            _ => {}
        }
    };
    terminal.finish()?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_search_text_and_keeps_a_matching_selection() {
        let mut picker = SessionPicker::new(vec![
            SessionChoice::new("one", "1m · deepseek", "修复 TUI", "D:/first"),
            SessionChoice::new("two", "2m · glm", "更新文档", "D:/second"),
            SessionChoice::new("three", "3m · codex", "run tests", "D:/third"),
        ]);
        picker.next();
        assert_eq!(picker.selected_id().as_deref(), Some("two"));

        picker.set_query("文档");
        assert_eq!(picker.selected_id().as_deref(), Some("two"));
        picker.set_query("THIRD");
        assert_eq!(picker.selected_id().as_deref(), Some("three"));
        picker.set_query("missing");
        assert_eq!(picker.selected_id(), None);
    }

    #[test]
    fn navigation_wraps() {
        let mut picker = SessionPicker::new(vec![
            SessionChoice::new("one", "first", "", ""),
            SessionChoice::new("two", "second", "", ""),
        ]);
        picker.previous();
        assert_eq!(picker.selected_id().as_deref(), Some("two"));
        picker.next();
        assert_eq!(picker.selected_id().as_deref(), Some("one"));
    }
}
