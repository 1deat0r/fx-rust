//! Input composer core — faithful port of the *testable core* of upstream
//! `core/input/*`: `text_boundaries.zig`, `editor_state.zig`,
//! `edit_history.zig`, `composer_undo.zig`, and `kill_ring.zig`.
//!
//! This is the pure editing engine (buffer + cursor + selection + undo/redo +
//! kill ring) the Phase 5 TUI composer and the terminal executor are built
//! on. UI/token coupling (images, skill entities, model pickers) is
//! deliberately omitted here; the pure byte + unicode model matches upstream.

use std::collections::VecDeque;

// ------------------------------------------------------------------ text

/// Is `codepoint` a word character? Upstream `isWordCharacter`: `_`, ASCII
/// alnum, or any code point >= 0xC0 (covers non-Latin scripts).
pub fn is_word_codepoint(cp: u32) -> bool {
    if cp == b'_' as u32 {
        return true;
    }
    if (b'a' as u32..=b'z' as u32).contains(&cp) {
        return true;
    }
    if (b'A' as u32..=b'Z' as u32).contains(&cp) {
        return true;
    }
    if (b'0' as u32..=b'9' as u32).contains(&cp) {
        return true;
    }
    cp >= 0xC0
}

/// UTF-8 scalar at byte offset (returns char byte length, min 1).
fn char_len_at(text: &[u8], offset: usize) -> usize {
    match text.get(offset) {
        Some(b) if b & 0x80 == 0 => 1,
        Some(b) if b & 0xE0 == 0xC0 => 2,
        Some(b) if b & 0xF0 == 0xE0 => 3,
        Some(b) if b & 0xF8 == 0xF0 => 4,
        _ => 1,
    }
}

fn char_at(text: &[u8], offset: usize) -> u32 {
    let s = std::str::from_utf8(&text[offset..]).ok();
    s.and_then(|s| s.chars().next())
        .map(|c| c as u32)
        .unwrap_or(0)
}

/// One display unit: (byte_len, cell_width). Zero-width combining marks
/// (modernized upstream `shared/display_width` ranges) have cell_width 0 and
/// are consumed with the preceding base character by `next_character_end`.
fn display_unit_at(text: &[u8], offset: usize) -> (usize, usize) {
    let Some(&first) = text.get(offset) else {
        return (0, 0);
    };
    let len = char_len_at(text, offset).max(1);
    let cp = char_at(text, offset);
    // Control characters carry width 0.
    if first.is_ascii_control() {
        return (len, 0);
    }
    // Common combining ranges (Mn/Me): U+0300–036F, U+0483–0489, U+1AB0–1AFF,
    // U+1DC0–1DFF, U+20D0–20FF, U+FE20–FE2F.
    let combining = matches!(cp, 0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF
        | 0xFE20..=0xFE2F);
    if combining {
        (len, 0)
    } else {
        // Any other display unit (incl. CJK) is non-zero width.
        (len, 1)
    }
}

fn is_control_boundary(text: &[u8], offset: usize) -> bool {
    matches!(text.get(offset), Some(b) if b.is_ascii_control())
}

/// Byte index just past the display unit at `start`, including any following
/// zero-width combining marks (upstream `nextCharacterEnd`).
pub fn next_character_end(text: &[u8], start: usize) -> usize {
    let mut end = start.min(text.len());
    if end == text.len() {
        return end;
    }
    let (byte_len, _) = display_unit_at(text, end);
    end += byte_len.max(1);
    if is_control_boundary(text, start) {
        return end.min(text.len());
    }
    while end < text.len() {
        let (byte_len, width) = display_unit_at(text, end);
        if width != 0 || is_control_boundary(text, end) {
            break;
        }
        end += byte_len.max(1);
    }
    end.min(text.len())
}

/// Byte index of the character start before `end` (upstream
/// `previousCharacterStart`).
pub fn previous_character_start(text: &[u8], end: usize) -> usize {
    let target = end.min(text.len());
    if target == 0 {
        return 0;
    }
    let mut current = 0;
    let mut previous = 0;
    while current < target {
        previous = current;
        let next = next_character_end(text, current);
        if next >= target {
            return previous;
        }
        current = next;
    }
    previous
}

/// Byte index of the start of the word containing/before `cursor` (upstream
/// `previousWordStart`).
pub fn previous_word_start(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos > 0 {
        let previous = previous_character_start(text, pos);
        if is_word_codepoint(char_at(text, previous)) {
            break;
        }
        pos = previous;
    }
    while pos > 0 {
        let previous = previous_character_start(text, pos);
        if !is_word_codepoint(char_at(text, previous)) {
            break;
        }
        pos = previous;
    }
    pos
}

/// Byte index just past the word at/after `cursor` (upstream `nextWordEnd`).
pub fn next_word_end(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos < text.len() && !is_word_codepoint(char_at(text, pos)) {
        pos = next_character_end(text, pos);
    }
    while pos < text.len() && is_word_codepoint(char_at(text, pos)) {
        pos = next_character_end(text, pos);
    }
    pos
}

/// Byte index just past the run of word characters then whitespace (upstream
/// `nextWordDeleteEnd` — used by delete-word forward).
pub fn next_word_delete_end(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos < text.len() && is_word_codepoint(char_at(text, pos)) {
        pos = next_character_end(text, pos);
    }
    while pos < text.len() && !is_word_codepoint(char_at(text, pos)) {
        pos = next_character_end(text, pos);
    }
    pos
}

pub fn logical_line_start(text: &[u8], cursor: usize) -> usize {
    let mut start = cursor.min(text.len());
    while start > 0 && text[start - 1] != b'\n' {
        start -= 1;
    }
    start
}

pub fn logical_line_end(text: &[u8], cursor: usize) -> usize {
    let mut end = cursor.min(text.len());
    while end < text.len() && text[end] != b'\n' {
        end += 1;
    }
    end
}

fn line_is_blank(text: &[u8], start: usize, end: usize) -> bool {
    text[start..end]
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r'))
}

fn previous_line_start(text: &[u8], line_start: usize) -> Option<usize> {
    if line_start == 0 {
        return None;
    }
    Some(logical_line_start(text, line_start - 1))
}

/// Start of the paragraph before/containing `cursor` (upstream
/// `previousParagraphStart`).
pub fn previous_paragraph_start(text: &[u8], cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    let mut line_end = logical_line_end(text, line_start);

    while line_is_blank(text, line_start, line_end) {
        match previous_line_start(text, line_start) {
            Some(prev) => {
                line_start = prev;
                line_end = logical_line_end(text, line_start);
            }
            None => return 0,
        }
    }

    let mut paragraph_start = line_start;
    while let Some(candidate) = previous_line_start(text, paragraph_start) {
        if line_is_blank(text, candidate, logical_line_end(text, candidate)) {
            break;
        }
        paragraph_start = candidate;
    }
    if cursor > paragraph_start {
        return paragraph_start;
    }

    let mut previous = previous_line_start(text, paragraph_start).unwrap_or(0);
    if previous == 0 && paragraph_start == 0 {
        return 0;
    }
    while line_is_blank(text, previous, logical_line_end(text, previous)) {
        match previous_line_start(text, previous) {
            Some(prev) => previous = prev,
            None => return 0,
        }
    }
    let mut previous_start = previous;
    while let Some(candidate) = previous_line_start(text, previous_start) {
        if line_is_blank(text, candidate, logical_line_end(text, candidate)) {
            break;
        }
        previous_start = candidate;
    }
    previous_start
}

/// Start of the paragraph after `cursor` (upstream `nextParagraphStart`).
pub fn next_paragraph_start(text: &[u8], cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    let mut line_end = logical_line_end(text, line_start);

    while !line_is_blank(text, line_start, line_end) {
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
        line_end = logical_line_end(text, line_start);
    }
    while line_is_blank(text, line_start, line_end) {
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
        line_end = logical_line_end(text, line_start);
    }
    line_start
}

// ------------------------------------------------------------------ editor

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionEdge {
    Start,
    End,
}

/// Owns the composer text + layout-independent edit state (upstream
/// `editor_state.State`).
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    pub input: String,
    pub cursor: usize,
    pub selection_anchor: Option<usize>,
}

impl EditorState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text(&mut self, text: &str) {
        self.input = text.to_string();
        self.cursor = self.input.len();
        self.selection_anchor = None;
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub fn selected_text(&self) -> Option<&str> {
        let (start, end) = self.selection_range()?;
        Some(&self.input[start..end])
    }

    pub fn begin_selection(&mut self, offset: usize) -> bool {
        let offset = offset.min(self.input.len());
        let changed = self.selection_anchor != Some(offset) || self.cursor != offset;
        self.selection_anchor = Some(offset);
        self.cursor = offset;
        changed
    }

    pub fn extend_selection(&mut self, offset: usize) -> bool {
        if self.selection_anchor.is_none() {
            return false;
        }
        let offset = offset.min(self.input.len());
        if self.cursor == offset {
            return false;
        }
        self.cursor = offset;
        true
    }

    pub fn clear_selection(&mut self) -> bool {
        if self.selection_anchor.is_none() {
            return false;
        }
        self.selection_anchor = None;
        true
    }

    /// Collapse the selection to one edge; returns the dropped byte count
    /// (upstream `collapseSelection`).
    pub fn collapse_selection(&mut self, edge: SelectionEdge) -> Option<usize> {
        let (start, end) = self.selection_range()?;
        self.selection_anchor = None;
        self.cursor = match edge {
            SelectionEdge::Start => start,
            SelectionEdge::End => end,
        };
        Some(end - start)
    }

    /// Replace any selection with `text`; cursor lands after the insertion.
    pub fn insert_replacing_selection(&mut self, text: &str) -> usize {
        if let Some((start, end)) = self.selection_range() {
            self.input.replace_range(start..end, text);
            self.selection_anchor = None;
            self.cursor = start + text.len();
            return start;
        }
        let at = self.cursor.min(self.input.len());
        self.input.insert_str(at, text);
        self.cursor = at + text.len();
        at
    }

    /// Backspace one character (or the selection); returns bytes removed.
    pub fn backspace(&mut self) -> usize {
        if let Some((start, end)) = self.selection_range() {
            self.input.replace_range(start..end, "");
            self.selection_anchor = None;
            self.cursor = start;
            return end - start;
        }
        if self.cursor == 0 {
            return 0;
        }
        let start = previous_character_start(self.input.as_bytes(), self.cursor);
        self.input.replace_range(start..self.cursor, "");
        let removed = self.cursor - start;
        self.cursor = start;
        removed
    }

    /// Delete forward one character (or the selection); returns bytes removed.
    pub fn delete_forward(&mut self) -> usize {
        if let Some((start, end)) = self.selection_range() {
            self.input.replace_range(start..end, "");
            self.selection_anchor = None;
            self.cursor = start;
            return end - start;
        }
        if self.cursor >= self.input.len() {
            return 0;
        }
        let end = next_character_end(self.input.as_bytes(), self.cursor);
        self.input.replace_range(self.cursor..end, "");
        end - self.cursor
    }

    /// Cut the active selection; returns the removed text (or None).
    pub fn cut_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let removed: String = self.input[start..end].to_string();
        self.input.replace_range(start..end, "");
        self.selection_anchor = None;
        self.cursor = start;
        Some(removed)
    }

    pub fn text_len(&self) -> usize {
        self.input.len()
    }
}

// ------------------------------------------------------------------ history

/// One undo/redo edit entry (upstream `edit_history.Entry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditEntry {
    pub start: usize,
    pub inserted: String,
    pub removed: String,
    pub cursor_before: usize,
    pub cursor_after: usize,
}

/// Undo/redo stacks (upstream `edit_history.State`).
#[derive(Debug, Clone, Default)]
pub struct EditHistory {
    pub undo: Vec<EditEntry>,
    pub redo: Vec<EditEntry>,
    /// When true, a new edit coalesces with the previous entry.
    pub coalescing: bool,
    pub last_ts_ms: i64,
}

impl EditHistory {
    pub fn record(&mut self, entry: EditEntry, coalesce: bool) {
        if coalesce && self.coalescing {
            if let Some(last) = self.undo.last_mut() {
                // Merge adjacent inserts by extending the cursor span when the
                // new insert appends exactly at the previous cursor.
                let coalesces = last.cursor_after == entry.start
                    && !last.inserted.is_empty()
                    && last.removed.is_empty()
                    && entry.removed.is_empty();
                if coalesces {
                    last.inserted.push_str(&entry.inserted);
                    last.cursor_after = entry.cursor_after;
                    return;
                }
            }
        }
        self.undo.push(entry);
        self.redo.clear();
        self.coalescing = coalesce;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn peek_undo(&self) -> Option<&EditEntry> {
        self.undo.last()
    }

    /// Apply the top undo entry onto `state`; returns false when the recorded
    /// span no longer matches (invalid transition → history reset upstream).
    pub fn undo(&mut self, state: &mut EditorState) -> bool {
        let Some(entry) = self.undo.pop() else {
            return false;
        };
        let end = entry
            .start
            .checked_add(entry.inserted.len())
            .filter(|e| *e <= state.input.len())
            .unwrap_or(usize::MAX);
        if end == usize::MAX || &state.input[entry.start..end] != entry.inserted.as_str() {
            self.undo.clear();
            self.redo.clear();
            return false;
        }
        state.input.replace_range(entry.start..end, &entry.removed);
        state.cursor = entry.cursor_before;
        state.selection_anchor = None;
        self.redo.push(entry);
        true
    }

    pub fn redo(&mut self, state: &mut EditorState) -> bool {
        let Some(entry) = self.redo.pop() else {
            return false;
        };
        let end = entry
            .start
            .checked_add(entry.removed.len())
            .filter(|e| *e <= state.input.len())
            .unwrap_or(usize::MAX);
        if end == usize::MAX || &state.input[entry.start..end] != entry.removed.as_str() {
            self.undo.clear();
            self.redo.clear();
            return false;
        }
        state.input.replace_range(entry.start..end, &entry.inserted);
        state.cursor = entry.cursor_after;
        state.selection_anchor = None;
        self.undo.push(entry);
        true
    }

    pub fn reset(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.coalescing = false;
    }
}

// ------------------------------------------------------------------ kill ring

/// Bounded kill-ring of yanked/cut slices (upstream `kill_ring.State`).
#[derive(Debug, Clone, Default)]
pub struct KillRing {
    pub entries: VecDeque<String>,
    pub capacity: usize,
    pub rotation: usize,
}

impl KillRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            rotation: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Kill (push) a slice to the ring; the newest entry is at the front.
    pub fn kill(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Coalesce a contiguous kill with the previous entry (upstream
        // consecutive-kill coalescing: continue the previous killed run).
        if let Some(front) = self.entries.front_mut() {
            if self.rotation == 1 {
                *front = format!("{}{}", front, text);
                return;
            }
        }
        self.entries.push_front(text.to_string());
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
        self.rotation = 1;
    }

    /// Yank the front (most recent) entry (upstream yank). Repeated yanks
    /// while the same kill is active rotate the ring one step.
    pub fn yank(&mut self) -> Option<String> {
        let text = self.entries.front().cloned()?;
        Some(text)
    }

    /// End the current kill sequence: the next `kill` starts a fresh entry
    /// instead of coalescing with the previous one (upstream tracks whether
    /// the previous operation was a kill).
    pub fn finish_kill(&mut self) {
        self.rotation = 0;
    }

    /// Rotate the ring so the previously-yanked entry moves to the back
    /// (upstream rotate). Returns the new front.
    pub fn rotate(&mut self) -> Option<String> {
        if self.entries.len() <= 1 {
            return self.entries.front().cloned();
        }
        let front = self.entries.pop_front()?;
        self.entries.push_back(front);
        self.entries.front().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundaries_work_forward_and_back() {
        let t = b"hello world";
        assert_eq!(previous_word_start(t, 11), 6);
        assert_eq!(previous_word_start(t, 5), 0);
        assert_eq!(next_word_end(t, 0), 5);
        assert_eq!(next_word_end(t, 6), 11);
        // Delete-word-forward consumes the word plus trailing whitespace.
        assert_eq!(next_word_delete_end(t, 0), 6);
    }

    #[test]
    fn unicode_offsets_respect_codepoints() {
        // bytes: h(0) é(1-2) l(3) l(4) o(5) ' '(6) 世(7-9) 界(10-12)
        let t = "héllo 世界".as_bytes();
        assert_eq!(t.len(), 13);
        // next char from the space lands on the start of 世.
        assert_eq!(next_character_end(t, 6), 7);
        // previous char start before 界's end (10) is the start of 界 (10).
        assert_eq!(previous_character_start(t, 10), 7);
        // 世 and 界 are word characters (>= 0xC0).
        assert!(is_word_codepoint(char_at(t, 7)));
        assert!(is_word_codepoint(char_at(t, 10)));
    }

    #[test]
    fn paragraph_boundaries_skip_blank_lines() {
        let t = "alpha\n\nbeta\ngamma";
        assert_eq!(previous_paragraph_start(t.as_bytes(), 14), 7);
        assert_eq!(next_paragraph_start(t.as_bytes(), 0), 7);
    }

    #[test]
    fn logical_lines() {
        let t = "ab\ncd";
        assert_eq!(logical_line_start(t.as_bytes(), 4), 3);
        assert_eq!(logical_line_end(t.as_bytes(), 1), 2);
    }

    #[test]
    fn editor_selection_and_edits() {
        let mut e = EditorState::new();
        e.set_text("hello world");
        assert_eq!(e.cursor, 11);
        e.begin_selection(6);
        e.extend_selection(11);
        assert_eq!(e.selected_text(), Some("world"));
        assert_eq!(e.cut_selection(), Some("world".to_string()));
        assert_eq!(e.input, "hello ");
        assert_eq!(e.cursor, 6);
    }

    #[test]
    fn editor_backspace_and_delete_forward() {
        // bytes: h(0) é(1-2) l(3) l(4) o(5)
        let mut e = EditorState::new();
        e.set_text("héllo");
        e.cursor = e.text_len();
        assert_eq!(e.backspace(), 1); // removes 'o'
        assert_eq!(e.input, "héll");

        let mut d = EditorState::new();
        d.set_text("héllo");
        d.cursor = 1; // after 'h', before 'é'
        assert_eq!(d.delete_forward(), 2); // removes the 2-byte é
        assert_eq!(d.input, "hllo");
    }

    #[test]
    fn history_undo_redo_roundtrip() {
        let mut e = EditorState::new();
        let mut h = EditHistory::default();
        // Insert 'abc' at 0 with a coalescing record.
        e.input.push_str("abc");
        e.cursor = 3;
        h.record(
            EditEntry {
                start: 0,
                inserted: "abc".into(),
                removed: String::new(),
                cursor_before: 0,
                cursor_after: 3,
            },
            false,
        );
        // Insert ' x' at 3 separately (no coalesce on the second record).
        e.input.push_str(" x");
        e.cursor = 5;
        h.record(
            EditEntry {
                start: 3,
                inserted: " x".into(),
                removed: String::new(),
                cursor_before: 3,
                cursor_after: 5,
            },
            false,
        );

        assert!(h.can_undo());
        assert!(h.undo(&mut e));
        assert_eq!(e.input, "abc");
        assert_eq!(e.cursor, 3);
        assert!(h.undo(&mut e));
        assert_eq!(e.input, "");
        assert!(h.redo(&mut e));
        assert_eq!(e.input, "abc");
        assert!(h.redo(&mut e));
        assert_eq!(e.input, "abc x");
    }

    #[test]
    fn history_invalid_transition_resets() {
        let mut e = EditorState::new();
        e.set_text("original");
        let mut h = EditHistory::default();
        h.record(
            EditEntry {
                start: 0,
                inserted: "XXXX".into(),
                removed: String::new(),
                cursor_before: 0,
                cursor_after: 4,
            },
            false,
        );
        // The input no longer matches the recorded insertion.
        assert!(!h.undo(&mut e));
        assert!(!h.can_undo());
    }

    #[test]
    fn kill_ring_bounded_and_rotates() {
        let mut k = KillRing::new(3);
        k.kill("one");
        k.finish_kill();
        k.kill("two");
        k.finish_kill();
        k.kill("three");
        k.finish_kill();
        k.kill("four");
        k.finish_kill();
        assert_eq!(k.len(), 3);
        assert_eq!(k.yank().as_deref(), Some("four"));
        let rotated = k.rotate();
        assert_eq!(rotated.as_deref(), Some("three"));
    }

    #[test]
    fn kill_ring_coalesces_consecutive_kills() {
        let mut k = KillRing::new(8);
        k.kill("he");
        k.kill("llo");
        assert_eq!(k.len(), 1);
        assert_eq!(k.yank().as_deref(), Some("hello"));
        // A non-kill operation between kills breaks the coalesce run.
        k.finish_kill();
        k.kill(" world");
        assert_eq!(k.len(), 2);
    }
}
