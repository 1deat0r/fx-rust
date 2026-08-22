//! Transcript runtime — the conversation model behind the TUI.
//!
//! Every visible line of the conversation (user prompts, assistant text,
//! tool activity, system notices) is a `TranscriptLine`: a kind plus the
//! display text pre-wrapped to the current terminal width. The viewport logic
//! exposes a windowed slice of the total transcript and supports scroll
//! follow, page up/down, and line scroll, matching upstream's transcript
//! presentation.

use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;

use super::screen::{Cell, CellStyle, Screen};
use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The human's prompt.
    User,
    /// Streamed assistant text.
    Assistant,
    /// Tool calls / results.
    Tool,
    /// System notices (session id, timing, backgrounds).
    System,
    /// Errors.
    Error,
    /// A horizontal divider.
    Divider,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub kind: LineKind,
    /// Rendered text (may contain ANSI-free plain text only).
    pub text: String,
    /// Lines pre-wrapped to the current width.
    pub wrapped: Vec<String>,
    /// Number of terminal rows this line consumes (sum of wrapped widths).
    pub rows: usize,
    /// Continuous text content for copy/search.
    pub raw: String,
}

impl TranscriptLine {
    fn wrap(text: &str, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for ch in text.chars() {
            if ch == '\n' {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                continue;
            }
            let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if cur_w + w > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += w;
        }
        out.push(std::mem::take(&mut cur));
        out
    }

    pub fn new(kind: LineKind, text: impl Into<String>, width: usize) -> Self {
        let text = text.into();
        let raw = text.clone();
        let wrapped = Self::wrap(&text, width);
        let rows = wrapped.len();
        TranscriptLine {
            kind,
            text,
            wrapped,
            rows,
            raw,
        }
    }

    /// Re-wrap after a width change.
    pub fn rewrap(&mut self, width: usize) {
        self.wrapped = Self::wrap(&self.text, width);
        self.rows = self.wrapped.len();
    }
}

/// A conversation transcript with a scrollable viewport.
pub struct Transcript {
    pub lines: Vec<TranscriptLine>,
    width: usize,
    /// Total display rows.
    pub total_rows: usize,
    /// First visible logical line of the viewport.
    pub scroll_line: usize,
    /// Follow the bottom (auto-scroll) — disabled by an explicit scroll-up.
    pub follow: bool,
    pub active: Option<usize>,
}

impl Transcript {
    pub fn new(width: usize) -> Self {
        Transcript {
            lines: Vec::new(),
            width,
            total_rows: 0,
            scroll_line: 0,
            follow: true,
            active: None,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn set_width(&mut self, width: usize) {
        if width == self.width {
            return;
        }
        self.width = width;
        self.total_rows = 0;
        for line in &mut self.lines {
            line.rewrap(width);
            self.total_rows += line.rows;
        }
    }

    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.push_line(TranscriptLine::new(kind, text, self.width));
    }

    pub fn push_line(&mut self, line: TranscriptLine) {
        let rows = line.rows;
        self.lines.push(line);
        self.total_rows += rows;
        if self.follow {
            self.scroll_line = usize::MAX; // clamped at render
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.total_rows = 0;
        self.scroll_line = 0;
        self.follow = true;
    }

    /// Replace the transcript with a snapshot (used for /history, /sessions).
    pub fn replace(&mut self, lines: Vec<TranscriptLine>) {
        self.lines = lines;
        self.total_rows = self.lines.iter().map(|l| l.rows).sum();
        self.scroll_line = usize::MAX;
        self.follow = true;
    }

    pub fn can_scroll_up(&self, view_rows: usize) -> bool {
        self.total_rows > view_rows
    }

    /// Clamp the scroll top so the viewport shows the bottom `view_rows` rows
    /// when following. Returns the clamped top line.
    pub fn clamp_scroll(&mut self, view_rows: usize) -> usize {
        if self.follow {
            self.scroll_line = usize::MAX;
        }
        let top = self.scroll_line.min(self.lines.len());
        // Find the smallest start <= top whose suffix covers the viewport,
        // preferring to always show the newest content. Walk backwards from
        // `top` accumulating rows, then advance forward while the first line
        // can be dropped without exposing blank rows at the bottom.
        let mut acc = 0usize;
        let mut start = top;
        while start > 0 {
            let prev = start - 1;
            if acc + self.lines[prev].rows > view_rows && acc >= view_rows {
                break;
            }
            start = prev;
            acc += self.lines[prev].rows;
        }
        // Drop leading lines that we only kept to pad the view; the newest
        // content must always be fully visible.
        while start + 1 < self.lines.len() && acc > view_rows {
            acc -= self.lines[start].rows;
            start += 1;
        }
        if self.follow {
            self.scroll_line = start;
        }
        start
    }

    pub fn scroll_by(&mut self, delta: isize, view_rows: usize) {
        self.follow = false;
        let top = self.clamp_scroll(view_rows);
        if delta < 0 {
            // Scroll up: move `top` towards the beginning by delta rows.
            let mut target = top as isize;
            let mut remaining = -delta;
            while remaining > 0 && target > 0 {
                target -= 1;
                remaining -= self.lines[target as usize].rows as isize;
            }
            self.scroll_line = target.max(0) as usize;
        } else {
            // Scroll down: move `top` towards the end by delta rows.
            let mut target = top as isize;
            let mut remaining = delta;
            while remaining > 0 && (target as usize) < self.lines.len() {
                remaining -= self.lines[target as usize].rows as isize;
                target += 1;
            }
            let n = self.lines.len() as isize;
            if target > n {
                target = n;
            }
            self.scroll_line = target as usize;
            // Touch bottom => follow again.
            if self.is_at_bottom(view_rows) {
                self.follow = true;
            }
        }
    }

    pub fn page_up(&mut self, view_rows: usize) {
        self.scroll_by(-(view_rows as isize), view_rows);
    }

    pub fn page_down(&mut self, view_rows: usize) {
        self.scroll_by(view_rows as isize, view_rows);
    }

    pub fn to_bottom(&mut self, view_rows: usize) {
        self.follow = true;
        let _ = self.clamp_scroll(view_rows);
    }

    fn is_at_bottom(&self, view_rows: usize) -> bool {
        // A heuristic: no lines follow within view_rows of the end.
        let mut rows_after = 0usize;
        let mut i = self.scroll_line.min(self.lines.len());
        while i < self.lines.len() && rows_after < view_rows {
            rows_after += self.lines[i].rows;
            i += 1;
        }
        rows_after <= view_rows
    }

    /// Render the visible window into the screen.
    /// `area_top` is the screen row where the transcript starts; returns the
    /// screen row just past the last drawn row (for the composer).
    pub fn render_into(
        &mut self,
        screen: &mut Screen,
        theme: &Theme,
        area_top: u16,
        view_rows: u16,
        active_color: Color,
    ) -> u16 {
        if view_rows == 0 {
            return area_top;
        }
        let top_line = self.clamp_scroll(view_rows as usize);
        let mut row = area_top;
        let mut remaining = view_rows as usize;
        let mut li = top_line;

        while remaining > 0 && li < self.lines.len() {
            let line = &self.lines[li];
            for wl in &line.wrapped {
                if remaining == 0 {
                    break;
                }
                let style = style_for(line.kind, theme, li == self.active.unwrap_or(usize::MAX), active_color);
                screen.putstr_styled(row, 0, wl, style);
                // Clear the rest of the row (in case a previous longer line
                // left residue after the diff — flush diff handles it, but
                // explicit blanking keeps the buffer honest).
                screen.hline(row, wl.chars().count() as u16, screen.cols - (wl.chars().count() as u16).min(screen.cols), Cell::styled(' ', theme.bg));
                row += 1;
                remaining -= 1;
            }
            li += 1;
        }
        // Blank any leftover transcript rows.
        while remaining > 0 {
            screen.hline(row, 0, screen.cols, Cell::styled(' ', theme.bg));
            row += 1;
            remaining -= 1;
        }
        row
    }

    /// Find a rendered row index for a logical line (for cursor placement).
    pub fn line_screen_row(&mut self, logical: usize, area_top: u16, view_rows: u16) -> Option<u16> {
        let top_line = self.clamp_scroll(view_rows as usize);
        if logical < top_line {
            return None;
        }
        let mut row = area_top;
        for li in top_line..=logical.min(self.lines.len().saturating_sub(1)) {
            let r = self.lines[li].rows as u16;
            if li == logical {
                return Some(row);
            }
            row += r;
        }
        None
    }
}

fn style_for(kind: LineKind, theme: &Theme, active: bool, active_color: Color) -> CellStyle {
    use LineKind::*;
    let fg = match kind {
        User => theme.user,
        Assistant => {
            if active {
                active_color
            } else {
                theme.assistant
            }
        }
        Tool => theme.tool,
        System => theme.dim,
        Error => theme.error,
        Divider => theme.dim,
    };
    match kind {
        User => CellStyle::bold(fg),
        Assistant => CellStyle::plain(fg),
        Tool => CellStyle::dim(fg),
        System => CellStyle::dim(fg),
        Error => CellStyle::bold(fg),
        Divider => CellStyle::dim(fg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_long_lines_to_width() {
        let l = TranscriptLine::new(LineKind::Assistant, "abcdefghij", 5);
        assert_eq!(l.wrapped, vec!["abcde", "fghij"]);
    }

    #[test]
    fn wrap_respects_newlines() {
        let l = TranscriptLine::new(LineKind::User, "ab\ncd", 10);
        assert_eq!(l.wrapped, vec!["ab", "cd"]);
    }

    #[test]
    fn total_rows_counts_wrapped() {
        let mut t = Transcript::new(10);
        t.push(LineKind::User, "hello world this is a long line");
        t.push(LineKind::Assistant, "ok");
        // "hello world this is a long line" is 32 chars => 4 rows at width 10
        // (because wrap breaks at width: 10,10,10,2)
        assert_eq!(t.total_rows(), 5);
    }

    #[test]
    fn scroll_follow_clamps_to_bottom() {
        let mut t = Transcript::new(20);
        for i in 0..20 {
            t.push(LineKind::System, format!("line {i}"));
        }
        let top = t.clamp_scroll(10);
        assert_eq!(top, 10);
        assert_eq!(t.scroll_line, 10);
    }

    #[test]
    fn scroll_up_then_down_restores_follow() {
        let mut t = Transcript::new(20);
        for i in 0..20 {
            t.push(LineKind::System, format!("line {i}"));
        }
        t.scroll_by(-3, 10);
        assert!(!t.follow);
        assert!(t.scroll_line < 20);
        t.scroll_by(100, 10);
        assert!(t.follow);
    }

    #[test]
    fn rewrap_on_width_change() {
        let mut t = Transcript::new(10);
        t.push(LineKind::Assistant, "abcdefghijklmnop");
        assert_eq!(t.total_rows(), 2);
        t.set_width(4);
        assert_eq!(t.total_rows(), 4);
    }
}
