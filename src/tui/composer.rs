//! Input composer TUI surface — renders the editable input line and maps
//! editor keys onto the `input_composer` core (EditorState / EditHistory /
//! KillRing). Supports multi-line input wrap, Emacs-style word motions and
//! kills, undo/redo, and a per-session prompt history.

use unicode_width::UnicodeWidthChar;

use crate::input_composer::{EditorState, KillRing};

use super::keys::Key;
use super::screen::{CellStyle, Screen};
use super::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    None,
    Submit(String),
    Cancel,
    Interrupt,
}

/// A single prompt-history entry.
#[derive(Debug, Clone)]
struct HistoryEntry {
    text: String,
}

pub struct Composer {
    pub state: EditorState,
    pub kill_ring: KillRing,
    pub undo_redo: crate::input_composer::EditHistory,
    /// Submitted prompt history (most recent last).
    history: Vec<HistoryEntry>,
    history_pos: Option<usize>,
    /// Kill coalescing flag between operations.
    last_was_kill: bool,
    pub prompt_prefix: String,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        Composer {
            state: EditorState::new(),
            kill_ring: KillRing::new(16),
            undo_redo: crate::input_composer::EditHistory::default(),
            history: Vec::new(),
            history_pos: None,
            last_was_kill: false,
            prompt_prefix: "ƒ> ".to_string(),
        }
    }

    pub fn set_prompt_prefix(&mut self, prefix: &str) {
        self.prompt_prefix = prefix.to_string();
    }

    fn apply_text(&mut self, text: &str) {
        self.state.set_text(text);
        self.undo_redo.reset();
    }

    pub fn set_text(&mut self, text: &str) {
        self.apply_text(text);
        self.history_pos = None;
    }

    pub fn text(&self) -> &str {
        &self.state.input
    }

    pub fn cursor(&self) -> usize {
        self.state.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.state.input.trim().is_empty()
    }

    pub fn submit(&mut self) {
        let text = self.state.input.trim().to_string();
        if !text.is_empty() {
            self.history.push(HistoryEntry { text });
        }
        self.set_text("");
    }

    pub fn handle_key(&mut self, key: Key) -> ComposerAction {
        use crate::input_composer as ic;
        match key {
            Key::Enter => {
                if !self.is_empty() {
                    let text = self.state.input.trim().to_string();
                    self.history.push(HistoryEntry { text: text.clone() });
                    self.set_text("");
                    self.last_was_kill = false;
                    return ComposerAction::Submit(text);
                }
                ComposerAction::None
            }
            Key::Escape => ComposerAction::Cancel,
            Key::Ctrl('c') => ComposerAction::Interrupt,
            Key::Char(c) => {
                let idx = self.state.insert_replacing_selection(&c.to_string());
                self.record_insert(idx, &c.to_string(), 0);
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Backspace => {
                let before = self.state.input.clone();
                let removed = self.state.backspace();
                self.record_delete(&before, removed);
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Delete => {
                let before = self.state.input.clone();
                let removed = self.state.delete_forward();
                self.record_delete(&before, removed);
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Left => {
                let _ = self.state.clear_selection();
                let at = self.state.cursor;
                self.state.cursor = ic::previous_character_start(self.state.input.as_bytes(), at);
                ComposerAction::None
            }
            Key::Right => {
                let _ = self.state.clear_selection();
                let at = self.state.cursor;
                self.state.cursor = ic::next_character_end(self.state.input.as_bytes(), at);
                ComposerAction::None
            }
            Key::Home | Key::Ctrl('a') => {
                let _ = self.state.clear_selection();
                self.state.cursor = 0;
                ComposerAction::None
            }
            Key::End | Key::Ctrl('e') => {
                let _ = self.state.clear_selection();
                self.state.cursor = self.state.input.len();
                ComposerAction::None
            }
            Key::Ctrl('b') => {
                let _ = self.state.clear_selection();
                self.state.cursor =
                    ic::previous_character_start(self.state.input.as_bytes(), self.state.cursor);
                ComposerAction::None
            }
            Key::Ctrl('f') => {
                let _ = self.state.clear_selection();
                self.state.cursor =
                    ic::next_character_end(self.state.input.as_bytes(), self.state.cursor);
                ComposerAction::None
            }
            Key::Alt('b') => {
                let _ = self.state.clear_selection();
                self.state.cursor =
                    ic::previous_word_start(self.state.input.as_bytes(), self.state.cursor);
                ComposerAction::None
            }
            Key::Alt('f') => {
                let _ = self.state.clear_selection();
                self.state.cursor =
                    ic::next_word_end(self.state.input.as_bytes(), self.state.cursor);
                ComposerAction::None
            }
            Key::Ctrl('u') => {
                // Kill to line start.
                if self.state.cursor > 0 {
                    let removed: String = self.state.input[..self.state.cursor].to_string();
                    let rest: String = self.state.input[self.state.cursor..].to_string();
                    self.state.input = rest;
                    self.state.cursor = 0;
                    self.state.selection_anchor = None;
                    self.kill(removed.as_str());
                }
                ComposerAction::None
            }
            Key::Ctrl('k') => {
                // Kill to line end.
                if self.state.cursor < self.state.input.len() {
                    let removed: String = self.state.input[self.state.cursor..].to_string();
                    self.state.input.truncate(self.state.cursor);
                    self.kill(removed.as_str());
                }
                ComposerAction::None
            }
            Key::Ctrl('w') => {
                // Kill previous word.
                let start = ic::previous_word_start(self.state.input.as_bytes(), self.state.cursor);
                if start < self.state.cursor {
                    let removed: String = self.state.input[start..self.state.cursor].to_string();
                    self.state.input.replace_range(start..self.state.cursor, "");
                    self.state.cursor = start;
                    self.state.selection_anchor = None;
                    self.kill(removed.as_str());
                }
                ComposerAction::None
            }
            Key::Ctrl('y') => {
                if let Some(text) = self.kill_ring.yank() {
                    let idx = self.state.insert_replacing_selection(&text);
                    self.record_insert(idx, &text, 0);
                }
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Alt('y') => {
                if let Some(text) = self.kill_ring.rotate() {
                    // Replace the previous yank region with the rotated entry.
                    // For simplicity (and parity with upstream behavior), when
                    // a rotation is requested right after a yank, the last
                    // inserted run is replaced.
                    self.replace_last_yank(&text);
                }
                ComposerAction::None
            }
            Key::Ctrl('z' | '/') => {
                self.undo_redo.undo(&mut self.state);
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Ctrl('r') | Key::Ctrl('x') => {
                // Ctrl-R redo (also Ctrl-X for vi muscle memory).
                self.undo_redo.redo(&mut self.state);
                self.last_was_kill = false;
                ComposerAction::None
            }
            Key::Up => {
                self.history_back();
                ComposerAction::None
            }
            Key::Down => {
                self.history_forward();
                ComposerAction::None
            }
            _ => ComposerAction::None,
        }
    }

    fn record_insert(&mut self, start: usize, inserted: &str, removed: usize) {
        let cursor_before = if removed > 0 { start + removed } else { start };
        let entry = crate::input_composer::EditEntry {
            start,
            inserted: inserted.to_string(),
            removed: String::new(),
            cursor_before,
            cursor_after: start + inserted.len(),
        };
        self.undo_redo.record(entry, true);
    }

    fn record_delete(&mut self, before: &str, removed: usize) {
        if removed == 0 {
            return;
        }
        let _after = &self.state.input;
        let removed_text = &before[self.state.cursor..self.state.cursor + removed];
        let entry = crate::input_composer::EditEntry {
            start: self.state.cursor,
            inserted: String::new(),
            removed: removed_text.to_string(),
            cursor_before: self.state.cursor + removed,
            cursor_after: self.state.cursor,
        };
        self.undo_redo.record(entry, false);
    }

    fn kill(&mut self, text: &str) {
        self.kill_ring.kill(text);
        self.last_was_kill = true;
    }

    /// Replace the previous yank region (the last inserted run) with `text`.
    fn replace_last_yank(&mut self, text: &str) {
        // Drop the last inserted entry from undo history and swap its span.
        if let Some(last) = self.undo_redo.undo.last_mut() {
            if !last.inserted.is_empty() && last.removed.is_empty() {
                let start = last.start;
                let end = start + last.inserted.len();
                if end <= self.state.input.len() {
                    self.state.input.replace_range(start..end, text);
                    self.state.cursor = start + text.len();
                    self.state.selection_anchor = None;
                    last.inserted = text.to_string();
                }
            }
        }
    }

    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(p) => p - 1,
        };
        let text = self.history[pos].text.clone();
        self.history_pos = Some(pos);
        self.apply_text(&text);
    }

    fn history_forward(&mut self) {
        match self.history_pos {
            None => {}
            Some(p) if p + 1 < self.history.len() => {
                let text = self.history[p + 1].text.clone();
                self.history_pos = Some(p + 1);
                self.apply_text(&text);
            }
            Some(_) => {
                self.history_pos = None;
                self.apply_text("");
            }
        }
    }

    pub fn reset_history_navigation(&mut self) {
        self.history_pos = None;
    }

    /// Render the composer row (single line). Returns the screen column of
    /// the cursor (for `place_cursor` after flush).
    #[allow(unused_assignments, unused_mut)]
    pub fn render(&self, screen: &mut Screen, theme: &Theme, row: u16, multiline: bool) -> u16 {
        let prompt = &self.prompt_prefix;
        let prompt_w = display_width(prompt);
        let mut col;
        // Prompt.
        col = screen.putstr_styled(row, 0, prompt, CellStyle::bold(theme.prompt));
        let text_col = col;
        let text = self.state.input.as_str();
        if multiline {
            // Wrap the input across rows: compute the wrap width then render
            // line by line starting at `row`. Only the first line is offset by
            // the prompt width.
            let width = (screen.cols as usize).saturating_sub(prompt_w);
            let mut cur = String::new();
            let mut cur_w = 0usize;
            #[allow(unused_assignments)]
            let mut r = row;
            let mut is_first = true;
            for ch in text.chars() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                if cur_w + w > width && !cur.is_empty() {
                    let base = if is_first { text_col } else { 0 };
                    screen.putstr_styled(r, base, &cur, CellStyle::plain(theme.assistant));
                    r += 1;
                    is_first = false;
                    cur.clear();
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += w;
            }
            let base = if is_first { text_col } else { 0 };
            screen.putstr_styled(r, base, &cur, CellStyle::plain(theme.assistant));
            // Cursor position: find the row/col of the cursor.
            let (_crow, ccol) = cursor_position(text, self.state.cursor, width, text_col.into());
            ccol as u16
        } else {
            // Single line: clip to screen width.
            let width = (screen.cols as usize).saturating_sub(prompt_w);
            let visible = text.chars().take(width).collect::<String>();
            screen.putstr_styled(row, text_col, &visible, CellStyle::plain(theme.assistant));
            // Cursor col = prompt_w + display width before cursor (clipped).
            let before = text
                .chars()
                .take(self.state.cursor.min(text.len()))
                .collect::<String>();
            let w = display_width(&before);
            text_col + (w.min(width) as u16)
        }
    }

    /// Render ONLY the cursor (after flush). Returns the terminal cursor
    /// position. For single-line: (row, prompt_w + cursor_width).
    pub fn cursor_position_single(&self, row: u16) -> (u16, u16) {
        let prompt_w = display_width(&self.prompt_prefix) as u16;
        let before = self
            .state
            .input
            .chars()
            .take(self.state.cursor.min(self.state.input.len()))
            .collect::<String>();
        (row, prompt_w + display_width(&before) as u16)
    }
}

fn cursor_position(text: &str, cursor: usize, width: usize, text_col: usize) -> (usize, usize) {
    // Returns (row_offset, col).
    let before = text.chars().take(cursor).collect::<String>();
    let mut cur_w = 0usize;
    let mut row = 0usize;
    let mut col = text_col;
    for ch in before.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if cur_w + w > width && cur_w > 0 {
            row += 1;
            cur_w = 0;
            col = 0;
        }
        col += w;
        cur_w += w;
    }
    (row, col)
}

pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0).max(1))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_trims_and_clears() {
        let mut c = Composer::new();
        c.set_text("  hello  ");
        let action = c.handle_key(Key::Enter);
        assert_eq!(action, ComposerAction::Submit("hello".to_string()));
        assert!(c.is_empty());
    }

    #[test]
    fn empty_enter_does_nothing() {
        let mut c = Composer::new();
        let action = c.handle_key(Key::Enter);
        assert_eq!(action, ComposerAction::None);
    }

    #[test]
    fn typing_inserts_and_moves_cursor() {
        let mut c = Composer::new();
        c.handle_key(Key::Char('a'));
        c.handle_key(Key::Char('b'));
        assert_eq!(c.text(), "ab");
        assert_eq!(c.cursor(), 2);
    }

    #[test]
    fn backspace_removes() {
        let mut c = Composer::new();
        c.set_text("hello");
        c.handle_key(Key::Left);
        c.handle_key(Key::Left);
        c.handle_key(Key::Backspace);
        assert_eq!(c.text(), "helo");
    }

    #[test]
    fn kill_yank_roundtrip() {
        let mut c = Composer::new();
        c.set_text("foo bar baz");
        c.handle_key(Key::Ctrl('a'));
        c.handle_key(Key::Ctrl('k'));
        assert_eq!(c.text(), "");
        c.handle_key(Key::Ctrl('y'));
        assert_eq!(c.text(), "foo bar baz");
    }

    #[test]
    fn undo_restores_coalesced_insert() {
        let mut c = Composer::new();
        c.handle_key(Key::Char('a'));
        c.handle_key(Key::Char('b'));
        assert_eq!(c.text(), "ab");
        // Consecutive typing coalesces into one edit entry: one undo removes
        // the whole run (upstream insert coalescing).
        c.handle_key(Key::Ctrl('z'));
        assert_eq!(c.text(), "");
        c.handle_key(Key::Ctrl('r'));
        assert_eq!(c.text(), "ab");
    }

    #[test]
    fn undo_steps_back_after_separate_edits() {
        let mut c = Composer::new();
        c.set_text("start");
        c.handle_key(Key::Char('!'));
        c.handle_key(Key::Ctrl('z'));
        assert_eq!(c.text(), "start");
    }

    #[test]
    fn history_navigation() {
        let mut c = Composer::new();
        c.handle_key(Key::Char('o'));
        c.handle_key(Key::Char('n'));
        c.handle_key(Key::Enter);
        c.handle_key(Key::Char('t'));
        c.handle_key(Key::Char('w'));
        c.handle_key(Key::Enter);
        c.handle_key(Key::Up);
        assert_eq!(c.text(), "tw");
        c.handle_key(Key::Up);
        assert_eq!(c.text(), "on");
        c.handle_key(Key::Down);
        assert_eq!(c.text(), "tw");
        c.handle_key(Key::Down);
        assert_eq!(c.text(), "");
    }

    #[test]
    fn home_end_and_word_motions() {
        let mut c = Composer::new();
        c.set_text("hello brave world");
        c.handle_key(Key::Ctrl('a'));
        assert_eq!(c.cursor(), 0);
        c.handle_key(Key::Alt('f'));
        assert_eq!(c.cursor(), 5);
        c.handle_key(Key::Alt('f'));
        assert_eq!(c.cursor(), 11);
        c.handle_key(Key::End);
        assert_eq!(c.cursor(), c.text().len());
    }
}
