//! Reusable TUI widgets: footer, single-line status bar, and the help
//! overlay screen.

use super::screen::{Cell, CellStyle, Screen};
use super::theme::Theme;

/// Render the footer bar: workspace, model, permission mode, counts, and the
/// standard key hints on the right side.
#[allow(unused_assignments)]
pub fn footer(screen: &mut Screen, theme: &Theme, row: u16, info: &FooterInfo) {
    let bg = theme.footer_bg;
    screen.hline(
        row,
        0,
        screen.cols,
        Cell {
            ch: ' ',
            fg: theme.footer_fg,
            bg,
            bold: false,
            dim: false,
        },
    );
    let mut col = 1u16;
    if let Some(ws) = &info.workspace {
        let label = truncate(ws, 24);
        col = screen.putstr_styled(
            row,
            col,
            &label,
            CellStyle {
                fg: theme.footer_fg,
                bg,
                bold: false,
                dim: true,
            },
        );
        col += 1;
    }
    if let Some(model) = &info.model {
        let label = truncate(model, 26);
        col = screen.putstr_styled(
            row,
            col,
            &label,
            CellStyle {
                fg: theme.footer_fg,
                bg,
                bold: false,
                dim: false,
            },
        );
        col += 1;
    }
    if let Some(mode) = &info.permission_mode {
        let label = truncate(mode, 10);
        col = screen.putstr_styled(
            row,
            col,
            &label,
            CellStyle {
                fg: theme.ok,
                bg,
                bold: false,
                dim: false,
            },
        );
        col += 1;
    }
    if info.running {
        col = screen.putstr_styled(
            row,
            col,
            "●",
            CellStyle {
                fg: theme.ok,
                bg,
                bold: false,
                dim: false,
            },
        );
        col += 1;
    }
    if info.unread > 0 {
        let label = format!("{}↑", info.unread);
        col = screen.putstr_styled(
            row,
            col,
            &label,
            CellStyle {
                fg: theme.selection,
                bg,
                bold: false,
                dim: false,
            },
        );
        col += 1;
    }

    // Right-aligned hints: [esc help] [↑↓ scroll] [ctrl-d exit]
    let hint = info.hints.as_str();
    let hint_w = hint.chars().count() as u16;
    if hint_w + 2 < screen.cols {
        let start = screen.cols - hint_w - 2;
        screen.putstr_styled(
            row,
            start,
            hint,
            CellStyle {
                fg: theme.footer_fg,
                bg,
                bold: false,
                dim: true,
            },
        );
    }
}

#[derive(Debug, Default)]
pub struct FooterInfo {
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub running: bool,
    pub unread: usize,
    pub hints: String,
}

/// Render the help overlay as a centered panel over the current frame.
pub fn help_overlay(screen: &mut Screen, theme: &Theme) {
    let w = screen.cols as usize;
    let h = screen.rows as usize;
    let panel_w = 66usize.min(w.saturating_sub(4));
    let panel_h = 22usize.min(h.saturating_sub(2));
    let left = ((w - panel_w) / 2) as u16;
    let top = ((h - panel_h) / 2) as u16;

    let bg = theme.footer_bg;
    let fg = theme.footer_fg;
    for r in top..top + panel_h as u16 {
        screen.clear_rect(r, left, 1, panel_w as u16);
        screen.hline(
            r,
            left,
            panel_w as u16,
            Cell {
                ch: ' ',
                fg,
                bg,
                bold: false,
                dim: false,
            },
        );
    }
    // Border.
    screen.hline(
        top,
        left,
        panel_w as u16,
        Cell {
            ch: '─',
            fg,
            bg,
            bold: true,
            dim: false,
        },
    );
    screen.hline(
        top + panel_h as u16 - 1,
        left,
        panel_w as u16,
        Cell {
            ch: '─',
            fg,
            bg,
            bold: true,
            dim: false,
        },
    );
    for r in (top + 1)..(top + panel_h as u16 - 1) {
        screen.putc(r, left, '│', fg);
        screen.putc(r, left + panel_w as u16 - 1, '│', fg);
    }

    let title = "fxrs — help";
    screen.putstr_styled(
        top + 1,
        left + 2,
        title,
        CellStyle {
            fg: theme.selection,
            bg,
            bold: true,
            dim: false,
        },
    );

    let rows: &[(&str, &str)] = &[
        ("enter", "submit prompt"),
        ("ctrl-d / exit", "leave fxrs"),
        ("ctrl-c", "interrupt the agent"),
        ("esc", "close help / dismiss"),
        ("↑ ↓", "history (composer), scroll (transcript)"),
        ("pgup / pgdn", "page the transcript"),
        ("ctrl-a / ctrl-e", "line start / end"),
        ("ctrl-k / ctrl-u", "kill to end / start"),
        ("ctrl-w", "kill previous word"),
        ("alt-b / alt-f", "word left / right"),
        ("ctrl-y / alt-y", "yank / rotate kill-ring"),
        ("ctrl-z / ctrl-r", "undo / redo"),
        ("ctrl-l", "repaint"),
        ("/help, /exit, ...", "slash commands run inline"),
    ];
    for (offset, (key, desc)) in rows.iter().enumerate() {
        let r = top + 3 + offset as u16;
        if r + 1 >= top + panel_h as u16 - 1 {
            break;
        }
        screen.putstr_styled(
            r,
            left + 3,
            key,
            CellStyle {
                fg: theme.user,
                bg,
                bold: false,
                dim: false,
            },
        );
        screen.putstr_styled(
            r,
            left + 22,
            desc,
            CellStyle {
                fg: theme.footer_fg,
                bg,
                bold: false,
                dim: false,
            },
        );
    }
}

/// A one-line status message at the top (e.g. "resuming session 123…").
pub fn status_line(screen: &mut Screen, theme: &Theme, row: u16, text: &str) {
    screen.clear_rect(row, 0, 1, screen.cols);
    screen.putstr_styled(row, 0, text, CellStyle::dim(theme.prompt));
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{}…", t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_noop() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_long_ellipsizes() {
        let t = truncate("abcdefghijklmnop", 6);
        assert!(t.starts_with("abcdef"));
        assert!(t.ends_with('…'));
    }
}
