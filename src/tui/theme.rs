//! Color theme for the TUI. The default palette mirrors fx's terminal look:
//! a light-on-dark transcript with muted tool lines, a bright prompt, and a
//! distinct footer. A light variant is available via `FX_TUI_THEME=light`;
//! `auto` (default) probes the terminal background with an OSC query and
//! falls back to dark when the query is unanswered.

use crossterm::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub prompt: Color,
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub tool_dim: Color,
    pub error: Color,
    pub ok: Color,
    pub footer_bg: Color,
    pub footer_fg: Color,
    pub selection: Color,
}

pub const DARK: Theme = Theme {
    bg: Color::Rgb {
        r: 15,
        g: 17,
        b: 26,
    },
    fg: Color::Rgb {
        r: 232,
        g: 230,
        b: 227,
    },
    dim: Color::Rgb {
        r: 110,
        g: 112,
        b: 120,
    },
    prompt: Color::Rgb {
        r: 86,
        g: 156,
        b: 255,
    },
    user: Color::Rgb {
        r: 161,
        g: 205,
        b: 255,
    },
    assistant: Color::Rgb {
        r: 232,
        g: 230,
        b: 227,
    },
    tool: Color::Rgb {
        r: 168,
        g: 185,
        b: 212,
    },
    tool_dim: Color::Rgb {
        r: 92,
        g: 104,
        b: 128,
    },
    error: Color::Rgb {
        r: 255,
        g: 92,
        b: 92,
    },
    ok: Color::Rgb {
        r: 106,
        g: 220,
        b: 133,
    },
    footer_bg: Color::Rgb {
        r: 32,
        g: 35,
        b: 48,
    },
    footer_fg: Color::Rgb {
        r: 142,
        g: 148,
        b: 162,
    },
    selection: Color::Rgb {
        r: 86,
        g: 156,
        b: 255,
    },
};

pub const LIGHT: Theme = Theme {
    bg: Color::Rgb {
        r: 250,
        g: 250,
        b: 252,
    },
    fg: Color::Rgb {
        r: 41,
        g: 42,
        b: 50,
    },
    dim: Color::Rgb {
        r: 140,
        g: 142,
        b: 152,
    },
    prompt: Color::Rgb {
        r: 24,
        g: 84,
        b: 200,
    },
    user: Color::Rgb {
        r: 30,
        g: 92,
        b: 180,
    },
    assistant: Color::Rgb {
        r: 41,
        g: 42,
        b: 50,
    },
    tool: Color::Rgb {
        r: 90,
        g: 104,
        b: 130,
    },
    tool_dim: Color::Rgb {
        r: 156,
        g: 164,
        b: 180,
    },
    error: Color::Rgb {
        r: 200,
        g: 30,
        b: 30,
    },
    ok: Color::Rgb {
        r: 30,
        g: 140,
        b: 70,
    },
    footer_bg: Color::Rgb {
        r: 236,
        g: 238,
        b: 244,
    },
    footer_fg: Color::Rgb {
        r: 96,
        g: 104,
        b: 122,
    },
    selection: Color::Rgb {
        r: 200,
        g: 218,
        b: 250,
    },
};

/// Resolve the theme mode from `FX_TUI_THEME` (or the config value the
/// caller passed in). The probe that auto-detects light terminals is gated
/// behind [`resolve_detected`] because reading the OSC reply can race the
/// TUI's key reader if run after raw mode is enabled.
pub fn resolve(name: Option<&str>) -> Theme {
    resolve_inner(name, false)
}

/// Resolve with background auto-detection (used by the TUI entry point before
/// raw mode is enabled, so the OSC query cannot race the key reader).
pub fn resolve_detected(name: Option<&str>) -> Theme {
    resolve_inner(name, true)
}

fn resolve_inner(name: Option<&str>, detect: bool) -> Theme {
    let mode = match name {
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => {
            if env_is("FX_TUI_THEME", "light") {
                ThemeMode::Light
            } else if env_is("FX_TUI_THEME", "dark") {
                ThemeMode::Dark
            } else if detect && probe_background_is_light().unwrap_or(false) {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            }
        }
    };
    match mode {
        ThemeMode::Dark => DARK,
        ThemeMode::Light => LIGHT,
    }
}

/// Probe the terminal background color with an OSC 11 query and report
/// whether it is light. Best-effort: on timeout, closed stdin, or a non-TTY
/// context this returns `None` and callers fall back to dark.
pub fn probe_background_is_light() -> Option<bool> {
    use std::io::Write;
    use std::time::Duration;

    #[cfg(unix)]
    {
        use crossterm::tty::IsTty;
        let mut stdout = std::io::stdout();
        if !stdout.is_tty() {
            return None;
        }
        write!(stdout, "\x1b]11;?\x1b\\").ok()?;
        stdout.flush().ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_osc11_reply(&tx));
        let reply = rx.recv_timeout(Duration::from_millis(150)).ok()?;
        let text = String::from_utf8_lossy(&reply);
        let (r, g, b) = extract_rgb_reply(&text)?;
        // Perceived luminance (Rec. 709). Light background > ~180/255.
        let lum = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
        Some(lum > 180.0)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Read the OSC 11 reply from stdin into the channel (runs on a helper
/// thread so the probe can time out without blocking the caller).
#[cfg(unix)]
fn read_osc11_reply(tx: &std::sync::mpsc::Sender<Vec<u8>>) {
    use std::io::Read;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 128];
    if let Ok(n) = stdin.read(&mut buf) {
        let _ = tx.send(buf[..n].to_vec());
    }
}

fn env_is(name: &str, val: &str) -> bool {
    std::env::var(name)
        .map(|v| v.eq_ignore_ascii_case(val))
        .unwrap_or(false)
}

fn extract_rgb_reply(reply: &str) -> Option<(u8, u8, u8)> {
    // Parse `rgb:RRRR/GGGG/BBBB` — xterm pads to 4 hex digits, some terminals
    // use 2. Normalize by taking the rightmost two hex digits of each group.
    let idx = reply.find("rgb:")?;
    let tail = &reply[idx + 4..];
    let mut parts = tail.splitn(3, '/');
    let r = parts.next()?;
    let g = parts.next()?;
    let b = parts
        .next()?
        .split(|c: char| !c.is_ascii_hexdigit())
        .next()?;
    Some((hex_tail(r)?, hex_tail(g)?, hex_tail(b)?))
}

fn hex_tail(s: &str) -> Option<u8> {
    let trimmed: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let last2: String = trimmed
        .chars()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .iter()
        .rev()
        .collect();
    u8::from_str_radix(&last2, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_mode_selected_from_env() {
        std::env::set_var("FX_TUI_THEME", "dark");
        let t = resolve(None);
        assert_eq!(t.bg, DARK.bg);
        std::env::remove_var("FX_TUI_THEME");
    }

    #[test]
    fn light_mode_selected_from_env() {
        std::env::set_var("FX_TUI_THEME", "light");
        let t = resolve(None);
        assert_eq!(t.bg, LIGHT.bg);
        std::env::remove_var("FX_TUI_THEME");
    }

    #[test]
    fn unknown_env_falls_back_dark() {
        std::env::set_var("FX_TUI_THEME", "mystery");
        let t = resolve(None);
        assert_eq!(t.bg, DARK.bg);
        std::env::remove_var("FX_TUI_THEME");
    }

    #[test]
    fn rgb_reply_parses_xterm_form() {
        let reply = "\u{1b}]11;rgb:0f11/1a1c/1e20\u{1b}\\";
        let (r, g, b) = extract_rgb_reply(reply).unwrap();
        assert_eq!((r, g, b), (0x11, 0x1c, 0x20));
    }

    #[test]
    fn rgb_reply_parses_short_form() {
        let reply = "\u{1b}]11;rgb:ffff/ffff/ffff\u{1b}\\";
        let (r, g, b) = extract_rgb_reply(reply).unwrap();
        assert_eq!((r, g, b), (0xff, 0xff, 0xff));
    }
}
