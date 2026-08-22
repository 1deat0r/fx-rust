//! Dedicated app screens — ports of upstream `src/ui/*_screen.zig` onto
//! the cell render engine: a full-page help screen, the full-transcript
//! view, the settings page, and the resume / models / skills pickers, plus
//! the structured approval modal.
//!
//! All screen rendering is stateless w.r.t. the app: callers pass the data
//! (or a reference needed) and the current scroll offset; the functions
//! paint into the `Screen` buffer and return the new maximum scroll.

use std::path::Path;

use super::screen::{Cell, CellStyle, Screen};
use super::theme::Theme;

// ------------------------------------------------------------------ picker

/// One selectable row in a picker screen.
#[derive(Debug, Clone)]
pub struct PickerItem {
    /// Primary line (id / name).
    pub key: String,
    /// Secondary detail on the line below (optional).
    pub detail: Option<String>,
    /// Right-aligned metadata on the primary line (optional).
    pub meta: Option<String>,
    /// Opaque value returned on selection (session id, model id, skill path…).
    pub value: String,
}

impl PickerItem {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        PickerItem {
            key: key.into(),
            detail: None,
            meta: None,
            value: value.into(),
        }
    }
}

/// A scrollable, selectable list. Used by the resume, models, and skills
/// screens (upstream's catalog/menu screens all share this layout: a
/// selectable list with a footer hint row).
#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    pub hint: String,
    pub items: Vec<PickerItem>,
    pub selected: usize,
    pub scroll: usize,
    /// Visible body rows (set each render so clamp uses the live viewport).
    pub view: usize,
}

impl Picker {
    pub fn new(title: impl Into<String>, hint: impl Into<String>) -> Self {
        Picker {
            title: title.into(),
            hint: hint.into(),
            items: Vec::new(),
            selected: 0,
            scroll: 0,
            view: 20,
        }
    }

    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        let prev = self
            .items
            .get(self.selected)
            .map(|it| it.value.clone())
            .unwrap_or_default();
        self.items = items;
        if let Some(pos) = self
            .items
            .iter()
            .position(|it| it.value == prev && !prev.is_empty())
        {
            self.selected = pos;
        } else {
            self.selected = 0;
        }
        self.clamp();
    }

    /// Number of visible rows in the picker body (rows 1..footer).
    pub fn visible_rows(&self, screen_rows: u16) -> usize {
        screen_rows.saturating_sub(2) as usize
    }

    pub fn clamp(&mut self) {
        let n = self.items.len();
        if n == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(n - 1);
        let view = self.view.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + view {
            self.scroll = self.selected.saturating_sub(view).saturating_add(1);
        }
    }

    pub fn move_up(&mut self) {
        if self.items.is_empty() || self.selected == 0 {
            return;
        }
        self.selected -= 1;
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() || self.selected + 1 >= self.items.len() {
            return;
        }
        self.selected += 1;
    }

    pub fn page_up(&mut self, rows: usize) {
        for _ in 0..rows {
            if self.selected == 0 {
                break;
            }
            self.selected -= 1;
        }
    }

    pub fn page_down(&mut self, rows: usize) {
        for _ in 0..rows {
            if self.selected + 1 >= self.items.len() {
                break;
            }
            self.selected += 1;
        }
    }

    pub fn selected_value(&self) -> Option<&str> {
        self.items.get(self.selected).map(|it| it.value.as_str())
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Paint the picker as a full-frame screen: title line, body, footer
    /// hint. Returns nothing (the app places the cursor).
    pub fn render(&self, screen: &mut Screen, theme: &Theme, footer_hint: &str) {
        let rows = screen.rows;
        let cols = screen.cols;

        // Header / title.
        screen.clear();
        screen.putstr_styled(0, 0, &self.title, CellStyle::bold(theme.selection));
        let count = format!("{} item(s)", self.items.len());
        let count_w = count.chars().count() as u16;
        if count_w + 2 < cols {
            screen.putstr_styled(0, cols - count_w - 1, &count, CellStyle::dim(theme.dim));
        }

        // Body.
        let view = self.visible_rows(rows);
        if self.items.is_empty() {
            screen.putstr_styled(1, 0, "(empty)", CellStyle::dim(theme.dim));
        } else {
            let scroll = self.scroll.min(self.items.len().saturating_sub(1));
            let end = (scroll + view).min(self.items.len());
            for (i, item) in self.items.iter().enumerate().take(end).skip(scroll) {
                let row = (1 + i - scroll) as u16;
                if row >= rows.saturating_sub(1) {
                    break;
                }
                let selected = i == self.selected;
                let bg = if selected { theme.selection } else { theme.bg };
                let fg = if selected { theme.bg } else { theme.fg };
                // Primary line, with right-aligned meta.
                screen.hline(
                    row,
                    0,
                    cols,
                    Cell {
                        ch: ' ',
                        fg,
                        bg,
                        bold: false,
                        dim: false,
                    },
                );
                let col = screen.putstr_styled(
                    row,
                    1,
                    &truncate(&item.key, (cols.saturating_sub(4)) as usize),
                    CellStyle {
                        fg,
                        bg,
                        bold: selected,
                        dim: false,
                    },
                );
                if let Some(meta) = &item.meta {
                    let meta =
                        truncate(meta, (cols.saturating_sub(col) as usize).saturating_sub(2));
                    let mw = meta.chars().count() as u16;
                    if col + mw + 2 < cols {
                        screen.putstr_styled(
                            row,
                            cols - mw - 1,
                            &meta,
                            CellStyle {
                                fg,
                                bg,
                                bold: false,
                                dim: true,
                            },
                        );
                    }
                }
                if let Some(detail) = &item.detail {
                    let drow = row + 1;
                    if drow < rows.saturating_sub(1) {
                        screen.hline(
                            drow,
                            0,
                            cols,
                            Cell {
                                ch: ' ',
                                fg,
                                bg,
                                bold: false,
                                dim: false,
                            },
                        );
                        screen.putstr_styled(
                            drow,
                            2,
                            &truncate(detail, (cols.saturating_sub(4)) as usize),
                            CellStyle {
                                fg,
                                bg,
                                bold: false,
                                dim: true,
                            },
                        );
                    }
                }
            }
        }

        // Footer hint.
        screen.hline(
            rows.saturating_sub(1),
            0,
            cols,
            Cell {
                ch: ' ',
                fg: theme.footer_fg,
                bg: theme.footer_bg,
                bold: false,
                dim: false,
            },
        );
        screen.putstr_styled(
            rows.saturating_sub(1),
            1,
            footer_hint,
            CellStyle {
                fg: theme.footer_fg,
                bg: theme.footer_bg,
                bold: false,
                dim: true,
            },
        );
    }
}

// ------------------------------------------------------------------ help

/// Keybinding rows for the full-page help screen.
pub const HELP_KEYS: &[(&str, &str)] = &[
    ("enter", "submit prompt"),
    ("ctrl-d", "leave fxrs / exit"),
    ("ctrl-c", "interrupt the running agent"),
    ("esc", "close help / dismiss a screen"),
    ("ctrl-t", "full-transcript view"),
    ("ctrl-s", "settings page"),
    ("ctrl-r", "resume session picker"),
    ("ctrl-m", "model picker"),
    ("ctrl-k", "skills catalog"),
    ("↑ ↓", "history in composer · scroll in transcript"),
    ("pgup / pgdn", "page the transcript / lists"),
    ("ctrl-a / ctrl-e", "line start / end"),
    ("ctrl-k / ctrl-u", "kill to end / start (composer)"),
    ("ctrl-w", "kill previous word"),
    ("alt-b / alt-f", "word left / right"),
    ("ctrl-y / alt-y", "yank / rotate kill-ring"),
    ("ctrl-z / ctrl-r", "undo / redo"),
    ("ctrl-l", "repaint"),
];

/// Slash commands shown on the help page.
pub const SLASH_HELP: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/exit", "leave fxrs"),
    ("/clear", "clear the transcript"),
    ("/version", "fxrs version"),
    ("/status", "model · permissions · workspace · max steps"),
    ("/model", "resolved model"),
    ("/permissions", "permission mode + rules"),
    ("/settings", "open the settings page"),
    ("/resume [id]", "resume a session (picker when no id)"),
    ("/sessions", "list sessions inline"),
    ("/session [id]", "load a session into the transcript"),
    ("/usage [period]", "usage totals (default 7d)"),
    ("/doctor", "run startup checks"),
    ("/history [n]", "recent prompts"),
    ("/workspace", "workspace + AGENTS.md info"),
    ("/skills", "open the skills catalog"),
    ("/background", "background process list + actions"),
    ("/terminal", "terminal sessions list + actions"),
    ("/credits", "gateway credits"),
    ("/trace", "toggle agent tracing"),
    ("/setup", "endpoint setup hints"),
    ("/compact", "compact the conversation"),
    ("/login", "authenticate (outside the TUI)"),
];

/// Render the full-page help screen. Returns the max scroll value.
pub fn render_help_page(screen: &mut Screen, theme: &Theme, scroll: usize) -> usize {
    screen.clear();
    screen.putstr_styled(0, 0, "fxrs — help", CellStyle::bold(theme.selection));

    // Build rows: (kind, key, desc) — kind 0 = keys, kind 1 = slash.
    let mut rows: Vec<(&str, &str, &str)> = Vec::new();
    for (k, d) in HELP_KEYS {
        rows.push(("key", k, d));
    }
    rows.push(("section", "Slash commands", ""));
    for (k, d) in SLASH_HELP {
        rows.push(("slash", k, d));
    }

    let view = screen.rows.saturating_sub(2) as usize;
    let max_scroll = rows.len().saturating_sub(view + 2);
    let scroll = scroll.min(max_scroll);
    let cols = screen.cols as usize;

    for (offset, (kind, key, desc)) in rows.iter().skip(scroll).enumerate() {
        let row = 1usize + offset;
        if row >= screen.rows as usize - 1 {
            break;
        }
        let r = row as u16;
        match *kind {
            "section" => {
                screen.putstr_styled(r, 0, &format!("— {} —", key), CellStyle::bold(theme.tool));
            }
            "slash" => {
                screen.putstr_styled(
                    r,
                    2,
                    key,
                    CellStyle {
                        fg: theme.user,
                        bg: theme.bg,
                        bold: false,
                        dim: false,
                    },
                );
            }
            _ => {
                screen.putstr_styled(
                    r,
                    2,
                    key,
                    CellStyle {
                        fg: theme.prompt,
                        bg: theme.bg,
                        bold: false,
                        dim: false,
                    },
                );
            }
        }
        let desc_x = 26usize;
        if desc_x < cols {
            screen.putstr_styled(
                r,
                desc_x as u16,
                &truncate(desc, cols.saturating_sub(desc_x).saturating_sub(2)),
                CellStyle {
                    fg: theme.fg,
                    bg: theme.bg,
                    bold: false,
                    dim: true,
                },
            );
        }
    }

    // Footer.
    let hint = "↑↓ page · q/esc close";
    screen.hline(
        screen.rows.saturating_sub(1),
        0,
        screen.cols,
        Cell {
            ch: ' ',
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: false,
        },
    );
    screen.putstr_styled(
        screen.rows.saturating_sub(1),
        1,
        hint,
        CellStyle {
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: true,
        },
    );
    max_scroll
}

// ------------------------------------------------------------- transcript

/// Paint the transcript full-frame (header + transcript + footer hint),
/// leveraging the transcript viewport for scrolling.
pub fn render_full_transcript(
    screen: &mut Screen,
    theme: &Theme,
    transcript: &mut super::transcript::Transcript,
    scroll: usize,
) -> usize {
    screen.clear();
    screen.putstr_styled(
        0,
        0,
        "fxrs — full transcript",
        CellStyle::bold(theme.selection),
    );
    let view_rows = screen.rows.saturating_sub(2);
    let max_scroll = transcript.clamp_scroll(view_rows as usize);
    let _ = scroll;
    transcript.render_into(screen, theme, 1, view_rows, theme.assistant);
    // Footer: scroll position + hints.
    let hint = "↑↓ scroll · pgup/pgdn page · ctrl-t toggle · q/esc close";
    screen.hline(
        screen.rows.saturating_sub(1),
        0,
        screen.cols,
        Cell {
            ch: ' ',
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: false,
        },
    );
    screen.putstr_styled(
        screen.rows.saturating_sub(1),
        1,
        hint,
        CellStyle {
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: true,
        },
    );
    max_scroll
}

// ------------------------------------------------------------- settings

/// Build the settings page lines (text rows) from the resolved config.
/// Mirrors `fxrs settings` / the upstream settings screen.
pub fn settings_lines(cfg: &crate::config::Config) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("model: {}", cfg.model));
    lines.push(format!("permission mode: {}", cfg.permission_mode));
    lines.push(format!("workspace: {}", cfg.workspace.display()));
    lines.push(format!("max agent steps: {}", cfg.max_agent_steps));
    lines.push(String::new());
    lines.push("settings catalog".to_string());
    lines.push("--------".to_string());
    for s in crate::settings_catalog::catalog() {
        let val = resolved_setting_value(cfg, s);
        lines.push(format!("{} = {}", s.name, val));
    }
    lines
}

fn resolved_setting_value(
    cfg: &crate::config::Config,
    key: &crate::settings_catalog::SettingKey,
) -> String {
    match key.name {
        "model" => cfg.model.clone(),
        "permission_mode" => format!("{}", cfg.permission_mode),
        "max_agent_steps" => format!("{}", cfg.max_agent_steps),
        "max_tool_result_bytes" => format!("{}", cfg.max_tool_result_bytes),
        "first_call_tool_choice" => format!("{:?}", cfg.first_call_tool_choice),
        "sandbox_mode" => format!("{:?}", cfg.sandbox),
        "context" => format!("{}", cfg.context),
        "additional_directories" => cfg
            .additional_directories
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(","),
        k => {
            let env_note = key.env.map(|e| format!(" (env {e})")).unwrap_or_default();
            format!(
                "{} (default: {}){env_note}",
                cfg_unknown(cfg, k),
                key.default
            )
        }
    }
}

/// Read a custom/unknown key from the settings map if present.
fn cfg_unknown(cfg: &crate::config::Config, key: &str) -> String {
    std::env::var(key.to_ascii_uppercase().replace('-', "_")).unwrap_or_else(|_| match key {
        "workspace" => cfg.workspace.display().to_string(),
        _ => "unset".to_string(),
    })
}

/// Render the settings page. Returns max scroll.
pub fn render_settings_page(
    screen: &mut Screen,
    theme: &Theme,
    lines: &[String],
    scroll: usize,
) -> usize {
    screen.clear();
    screen.putstr_styled(0, 0, "fxrs — settings", CellStyle::bold(theme.selection));
    let view = screen.rows.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(view);
    let scroll = scroll.min(max_scroll);
    let cols = screen.cols as usize;
    for (i, line) in lines.iter().enumerate().skip(scroll) {
        let row = (1 + i - scroll) as u16;
        if row >= screen.rows.saturating_sub(1) {
            break;
        }
        let is_header = i == 0 || line.starts_with("settings catalog") || line.starts_with("---");
        let style = if is_header {
            CellStyle::bold(theme.selection)
        } else if let Some(eq) = line.find('=') {
            let (k, v) = line.split_at(eq);
            screen.putstr_styled(
                row,
                1,
                k.trim(),
                CellStyle {
                    fg: theme.user,
                    bg: theme.bg,
                    bold: false,
                    dim: false,
                },
            );
            let val = v.trim();
            screen.putstr_styled(
                row,
                (1 + k.trim().len() + 1) as u16,
                "=",
                CellStyle::dim(theme.dim),
            );
            screen.putstr_styled(
                row,
                (1 + k.trim().len() + 3) as u16,
                &truncate(&val[1..], cols.saturating_sub(8)),
                CellStyle {
                    fg: theme.fg,
                    bg: theme.bg,
                    bold: false,
                    dim: false,
                },
            );
            continue;
        } else {
            CellStyle {
                fg: theme.fg,
                bg: theme.bg,
                bold: false,
                dim: true,
            }
        };
        screen.putstr_styled(row, 1, &truncate(line, cols.saturating_sub(3)), style);
    }
    let hint = "↑↓ scroll · q/esc close";
    screen.hline(
        screen.rows.saturating_sub(1),
        0,
        screen.cols,
        Cell {
            ch: ' ',
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: false,
        },
    );
    screen.putstr_styled(
        screen.rows.saturating_sub(1),
        1,
        hint,
        CellStyle {
            fg: theme.footer_fg,
            bg: theme.footer_bg,
            bold: false,
            dim: true,
        },
    );
    max_scroll
}

// ------------------------------------------------------------- approval

/// Render the structured approval modal over the present frame.
pub fn render_approval_modal(
    screen: &mut Screen,
    theme: &Theme,
    req: &crate::approval::ApprovalRequest,
) {
    let w = screen.cols as usize;
    let h = screen.rows as usize;
    let panel_w = (76usize).min(w.saturating_sub(4));
    let panel_h = (14usize).min(h.saturating_sub(2));
    let left = ((w - panel_w) / 2) as u16;
    let top = ((h - panel_h) / 2) as u16;
    let bg = theme.footer_bg;
    let fg = theme.footer_fg;
    for r in top..top + panel_h as u16 {
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
    screen.putstr_styled(
        top + 1,
        left + 2,
        &format!("ƒ permission needed: {}", req.tool_name),
        CellStyle::bold(theme.error),
    );
    let target = req.display_target();
    screen.putstr_styled(top + 3, left + 2, "target", CellStyle::dim(theme.tool_dim));
    screen.putstr_styled(
        top + 3,
        left + 10,
        &truncate(&target, panel_w.saturating_sub(14)),
        CellStyle {
            fg: theme.fg,
            bg,
            bold: false,
            dim: false,
        },
    );
    let preview = req.preview();
    screen.putstr_styled(top + 5, left + 2, "command", CellStyle::dim(theme.tool_dim));
    let preview_line: String = preview.chars().take(panel_w.saturating_sub(14)).collect();
    screen.putstr_styled(
        top + 5,
        left + 11,
        &preview_line,
        CellStyle {
            fg: theme.user,
            bg,
            bold: false,
            dim: false,
        },
    );
    if preview.lines().count() > 1 {
        screen.putstr_styled(
            top + 6,
            left + 11,
            &format!("… ({} more line(s))", preview.lines().count() - 1),
            CellStyle::dim(theme.dim),
        );
    }
    screen.putstr_styled(
        top + panel_h as u16 - 2,
        left + 2,
        "(y)es  (n)o  (a)lways allow  esc=no",
        CellStyle::bold(theme.selection),
    );
}

// -------------------------------------------------------------- helpers

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n.max(1) {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.max(1)).collect();
        format!("{}…", t)
    }
}

pub fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| workspace.display().to_string())
}

pub fn short_id(id: &str) -> String {
    if id.chars().count() > 10 {
        id.chars().take(10).collect()
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_selects_and_scrolls() {
        let mut p = Picker::new("t", "h");
        let items: Vec<PickerItem> = (0..10)
            .map(|i| PickerItem::new(format!("item {i}"), format!("v{i}")))
            .collect();
        p.set_items(items);
        assert_eq!(p.selected_value(), Some("v0"));
        p.move_down();
        assert_eq!(p.selected_value(), Some("v1"));
        for _ in 0..20 {
            p.move_down();
        }
        assert_eq!(p.selected_value(), Some("v9"));
        p.move_up();
        assert_eq!(p.selected_value(), Some("v8"));
    }

    #[test]
    fn picker_empty_does_not_panic() {
        let mut p = Picker::new("t", "h");
        p.move_down();
        p.move_up();
        p.page_down(3);
        assert_eq!(p.selected_value(), None);
    }

    #[test]
    fn picker_preserves_selection_on_refresh() {
        let mut p = Picker::new("t", "h");
        p.set_items(vec![
            PickerItem::new("a", "id-a"),
            PickerItem::new("b", "id-b"),
            PickerItem::new("c", "id-c"),
        ]);
        p.move_down();
        p.move_down();
        assert_eq!(p.selected_value(), Some("id-c"));
        p.set_items(vec![
            PickerItem::new("a", "id-a"),
            PickerItem::new("b", "id-b"),
            PickerItem::new("c", "id-c"),
            PickerItem::new("d", "id-d"),
        ]);
        // Selection is preserved by matching value.
        assert_eq!(p.selected_value(), Some("id-c"));
    }

    #[test]
    fn help_page_layout_rows_do_not_overflow() {
        let theme = super::super::theme::DARK;
        let mut screen = Screen::new(100, 10, theme);
        let max = render_help_page(&mut screen, &theme, 0);
        // With 10 rows: header + footer, body is 8 rows; max_scroll should be >0
        // and painting at the max must not panic.
        render_help_page(&mut screen, &theme, max);
        // Re-paint at a huge scroll clamps.
        render_help_page(&mut screen, &theme, usize::MAX);
    }

    #[test]
    fn truncate_short_and_long() {
        assert_eq!(truncate("abc", 10), "abc");
        assert!(truncate("abcdefghij", 4).starts_with("abcd"));
        assert!(truncate("abcdefghij", 4).ends_with('…'));
    }
}
