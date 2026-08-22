//! Key event decoding for the TUI. Bridges crossterm `Event::Key` into a
//! small, editor-friendly `Key` enum that the composer and app state handle.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Backspace,
    Delete,
    Enter,
    Tab,
    BackTab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Unknown,
}

impl Key {
    /// True when the key should be handled by the composer as text input.
    pub fn is_text(self) -> Option<char> {
        match self {
            Key::Char(c) => Some(c),
            _ => None,
        }
    }
}

pub fn from_crossterm(ev: KeyEvent) -> Key {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    match ev.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-<letter> is convention lowercase.
                Key::Ctrl(c.to_ascii_lowercase())
            } else if alt {
                Key::Alt(c)
            } else {
                Key::Char(c)
            }
        }
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => {
            if shift {
                Key::BackTab
            } else {
                Key::Tab
            }
        }
        KeyCode::Esc => Key::Escape,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        _ => Key::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn plain_char() {
        assert_eq!(
            from_crossterm(KeyEvent::from(KeyCode::Char('x'))),
            Key::Char('x')
        );
    }

    #[test]
    fn ctrl_char_lowercases() {
        let ev = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::CONTROL);
        assert_eq!(from_crossterm(ev), Key::Ctrl('a'));
    }

    #[test]
    fn shift_letter_stays_uppercase() {
        let ev = KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT);
        assert_eq!(from_crossterm(ev), Key::Char('H'));
    }

    #[test]
    fn ctrl_h_is_backspace_compat() {
        // Ctrl-H is often used as backspace; the app may map it, but the
        // decoder must keep the raw form.
        let ev = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(from_crossterm(ev), Key::Ctrl('h'));
    }

    #[test]
    fn other_modifiers_route_to_alt() {
        let ev = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(from_crossterm(ev), Key::Alt('b'));
    }
}
