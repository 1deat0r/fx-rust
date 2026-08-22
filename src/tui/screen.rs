//! Render engine — a full-screen cell buffer with incremental diff painting.
//!
//! The app draws into a `Screen` (a grid of `Cell`s). `flush` compares the
//! current buffer against the previous frame and emits only the changed run
//! of cells, grouped into styled segments, using ANSI cursor addressing. This
//! keeps repaints cheap on slow terminals and avoids flicker.

use std::io::{self, Write};

use crossterm::cursor;
use crossterm::style::{Attribute, Color, PrintStyledContent, Stylize};
use crossterm::terminal;
use crossterm::QueueableCommand;

use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
}

impl Cell {
    pub fn blank() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Reset,
            bg: Color::Reset,
            bold: false,
            dim: false,
        }
    }

    pub fn styled(ch: char, fg: Color) -> Self {
        Cell {
            ch,
            fg,
            ..Cell::blank()
        }
    }
}

const CLEAR: Cell = Cell {
    ch: ' ',
    fg: Color::Reset,
    bg: Color::Reset,
    bold: false,
    dim: false,
};

/// A row-major screen buffer. Coordinates are 0-based; the terminal is 1-based.
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    cells: Vec<Cell>,
    dirty: Vec<Cell>,
    theme: Theme,
}

impl Screen {
    pub fn new(cols: u16, rows: u16, theme: Theme) -> Self {
        let n = (cols as usize) * (rows as usize);
        Screen {
            cols,
            rows,
            cells: vec![CLEAR; n],
            dirty: vec![CLEAR; n],
            theme,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let n = (self.cols as usize) * (self.rows as usize);
        self.cells = vec![CLEAR; n];
        self.dirty = vec![CLEAR; n];
    }

    fn idx(&self, row: u16, col: u16) -> Option<usize> {
        if row < self.rows && col < self.cols {
            Some(row as usize * self.cols as usize + col as usize)
        } else {
            None
        }
    }

    /// Put a single styled character at (row, col). Out-of-bounds writes are
    /// ignored (the layout code may speculate past the edge).
    pub fn put(&mut self, row: u16, col: u16, cell: Cell) {
        if let Some(i) = self.idx(row, col) {
            self.dirty[i] = cell;
        }
    }

    /// Put a plain character using a color.
    pub fn putc(&mut self, row: u16, col: u16, ch: char, fg: Color) {
        self.put(row, col, Cell::styled(ch, fg));
    }

    /// Write a string starting at (row, col), wrapping nothing: any content
    /// past the right edge is truncated. Returns the next column.
    pub fn putstr(&mut self, row: u16, col: u16, text: &str, fg: Color) -> u16 {
        let mut c = col;
        for ch in text.chars() {
            if c >= self.cols {
                break;
            }
            if ch == '\n' {
                continue;
            }
            self.putc(row, c, ch, fg);
            c += 1;
        }
        c
    }

    /// Like `putstr` but with full cell styling.
    pub fn putstr_styled<I>(&mut self, row: u16, col: u16, text: &str, style: I) -> u16
    where
        I: Into<CellStyle>,
    {
        let s = style.into();
        let mut c = col;
        for ch in text.chars() {
            if c >= self.cols {
                break;
            }
            self.put(
                row,
                c,
                Cell {
                    ch,
                    fg: s.fg,
                    bg: s.bg,
                    bold: s.bold,
                    dim: s.dim,
                },
            );
            c += 1;
        }
        c
    }

    /// Fill a horizontal span with a styled cell.
    pub fn hline(&mut self, row: u16, col: u16, len: u16, cell: Cell) {
        for i in 0..len {
            let c = col.saturating_add(i);
            if c >= self.cols {
                break;
            }
            self.put(row, c, cell);
        }
    }

    pub fn clear_rect(&mut self, row: u16, col: u16, rows: u16, cols: u16) {
        let row_end = (row + rows).min(self.rows);
        let col_end = (col + cols).min(self.cols);
        for r in row..row_end {
            for c in col..col_end {
                self.put(r, c, CLEAR);
            }
        }
    }

    pub fn clear(&mut self) {
        for i in 0..self.dirty.len() {
            self.dirty[i] = CLEAR;
        }
    }

    /// Flush dirty cells to the terminal using minimal cursor movement and
    /// styled run-length segments. Left/right edges follow the terminal, not
    /// the buffer, so we move before each edited run.
    pub fn flush<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let cols = self.cols as usize;
        let mut changed = false;
        for r in 0..self.rows as usize {
            // Find changed runs in this row.
            let mut c = 0;
            while c < cols {
                if self.dirty[r * cols + c] == self.cells[r * cols + c] {
                    c += 1;
                    continue;
                }
                let start = c;
                // Trim trailing blanks that would just erase (if prior cell
                // is blank and we're at the end of the row, skip).
                while c < cols && self.dirty[r * cols + c] != self.cells[r * cols + c] {
                    c += 1;
                }
                let end = c;
                // Paint the run as styled segments.
                out.queue(cursor::MoveTo(start as u16, r as u16))?;
                let mut seg_fg = Color::Reset;
                let mut seg_bg = Color::Reset;
                let mut seg_bold = false;
                let mut seg_dim = false;
                let mut seg = String::new();
                for i in start..end {
                    let cell = self.dirty[r * cols + i];
                    if cell.fg != seg_fg
                        || cell.bg != seg_bg
                        || cell.bold != seg_bold
                        || cell.dim != seg_dim
                    {
                        if !seg.is_empty() {
                            paint_segment(out, &seg, seg_fg, seg_bg, seg_bold, seg_dim)?;
                            seg.clear();
                        }
                        seg_fg = cell.fg;
                        seg_bg = cell.bg;
                        seg_bold = cell.bold;
                        seg_dim = cell.dim;
                    }
                    seg.push(cell.ch);
                }
                if !seg.is_empty() {
                    paint_segment(out, &seg, seg_fg, seg_bg, seg_bold, seg_dim)?;
                }
                changed = true;
            }
        }

        if changed {
            out.queue(cursor::MoveTo(0, 0))?;
            out.flush()?;
        }

        // Commit the frame.
        std::mem::swap(&mut self.cells, &mut self.dirty);
        for cell in self.dirty.iter_mut() {
            *cell = CLEAR;
        }
        Ok(())
    }

    /// Place the cursor at a logical position for the next frame.
    pub fn place_cursor<W: Write>(&mut self, out: &mut W, row: u16, col: u16) -> io::Result<()> {
        out.queue(cursor::MoveTo(col, row))?;
        out.queue(crossterm::style::SetForegroundColor(self.theme.fg))?;
        out.queue(crossterm::style::ResetColor)?;
        out.flush()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
}

impl CellStyle {
    pub fn plain(fg: Color) -> Self {
        CellStyle {
            fg,
            bg: Color::Reset,
            bold: false,
            dim: false,
        }
    }
    pub fn bold(fg: Color) -> Self {
        CellStyle {
            fg,
            bg: Color::Reset,
            bold: true,
            dim: false,
        }
    }
    pub fn dim(fg: Color) -> Self {
        CellStyle {
            fg,
            bg: Color::Reset,
            bold: false,
            dim: true,
        }
    }
}

impl From<CellStyle> for Cell {
    fn from(s: CellStyle) -> Self {
        Cell {
            ch: ' ',
            fg: s.fg,
            bg: s.bg,
            bold: s.bold,
            dim: s.dim,
        }
    }
}

impl From<Color> for CellStyle {
    fn from(fg: Color) -> Self {
        CellStyle::plain(fg)
    }
}

fn paint_segment<W: Write>(
    out: &mut W,
    text: &str,
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
) -> io::Result<()> {
    let mut styled = text.stylize();
    styled = styled.with(fg);
    if bg != Color::Reset {
        styled = styled.on(bg);
    }
    if bold {
        styled = styled.attribute(Attribute::Bold);
    }
    if dim {
        styled = styled.attribute(Attribute::Dim);
    }
    out.queue(PrintStyledContent(styled))?;
    Ok(())
}

/// Query the terminal size, falling back to 80x24.
pub fn terminal_size() -> (u16, u16) {
    terminal::size().unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::no_effect)]
    fn put_and_read_roundtrip() {
        let mut s = Screen::new(10, 3, super::super::theme::DARK);
        s.putc(1, 2, 'x', Color::Red);
        assert_eq!(s.dirty[10 + 2].ch, 'x');
        s.flush(&mut io::sink()).unwrap();
        // After flush, dirty is cleared and cells holds the frame.
        assert_eq!(s.cells[10 + 2].ch, 'x');
        assert_eq!(s.dirty[10 + 2].ch, ' ');
    }

    #[test]
    #[allow(clippy::no_effect)]
    fn out_of_bounds_ignored() {
        let mut s = Screen::new(10, 3, super::super::theme::DARK);
        s.putc(99, 99, 'x', Color::Red); // no panic
        s.putstr(0, 8, "toolong", Color::White); // truncates at col 10
        assert_eq!(s.dirty[8].ch, 't');
        assert_eq!(s.dirty[9].ch, 'o');
    }

    #[test]
    #[allow(clippy::no_effect)]
    fn hline_truncates_at_edge() {
        let mut s = Screen::new(10, 3, super::super::theme::DARK);
        s.hline(0, 8, 5, Cell::styled('x', Color::White)); // cols 8,9 filled, rest ignored
        assert_eq!(s.dirty[8].ch, 'x');
        assert_eq!(s.dirty[9].ch, 'x');
    }

    #[test]
    fn second_flush_is_empty_diff() {
        let mut s = Screen::new(10, 3, super::super::theme::DARK);
        s.putc(0, 0, 'a', Color::White);
        s.flush(&mut io::sink()).unwrap();
        // Nothing changed between frames: dirty stays all blank.
        let mut changed = false;
        for cell in &s.dirty {
            if cell.ch != ' ' {
                changed = true;
            }
        }
        assert!(!changed);
    }
}
