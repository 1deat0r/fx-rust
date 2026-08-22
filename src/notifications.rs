//! Notifications + sounds — port of upstream `core/notifications/*`
//! (`notification_contract.zig` + `sound.zig` + `builtins/hooks/notifications.zig`).
//!
//! The contract is a small state machine gating *cues* (sounds or desktop
//! notices) behind user preferences. Two kinds exist: `turn_end` (agent
//! finished a turn) and `attention_required` (the human must decide). The
//! player defaults are conservative: no embedded audio assets are bundled,
//! so the fallback is the terminal bell (upstream's non-macOS behavior) plus
//! a best-effort desktop notification for attention-required events.

use std::sync::atomic::{AtomicBool, Ordering};

/// Sound / notification preferences (upstream `Preferences`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Preferences {
    pub turn_end: bool,
    pub attention_required: bool,
    /// Max retains the ordinary sound gates and enables additional direct cues.
    pub max: bool,
}

impl Preferences {
    pub fn sound_on(&self) -> bool {
        self.turn_end
    }

    pub fn max_enabled(&self) -> bool {
        self.sound_on() && self.max
    }
}

/// A short sound/visual cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    Success,
    Error,
    Bloom,
    Press,
    Click,
    Release,
    Toggle,
}

impl Cue {
    pub fn as_str(self) -> &'static str {
        match self {
            Cue::Success => "success",
            Cue::Error => "error",
            Cue::Bloom => "bloom",
            Cue::Press => "press",
            Cue::Click => "click",
            Cue::Release => "release",
            Cue::Toggle => "toggle",
        }
    }
}

/// Notification kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TurnEnd,
    AttentionRequired,
}

/// One queued notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notification {
    pub kind: Kind,
    pub cue: Cue,
}

impl Notification {
    pub fn turn_end(cue: Cue) -> Self {
        Notification {
            kind: Kind::TurnEnd,
            cue,
        }
    }

    pub fn attention_required() -> Self {
        Notification {
            kind: Kind::AttentionRequired,
            cue: Cue::Bloom,
        }
    }
}

/// How the player surfaces a cue. The default is the terminal bell; desktop
/// integrations (notify-send / terminal-notifier) can be layered on top.
pub struct BellSink {
    /// Write the ASCII bell byte (or any cue side-effect). Boxed so tests
    /// can count emissions.
    pub emit_bell: Box<dyn Fn() + Send + Sync>,
}

pub fn terminal_bell() {
    print!("\x07");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

impl Default for BellSink {
    fn default() -> Self {
        BellSink {
            emit_bell: Box::new(terminal_bell),
        }
    }
}

impl std::fmt::Debug for BellSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BellSink").finish_non_exhaustive()
    }
}

/// Ready counts flushed in one presentation pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadyNotifications {
    pub turn_end_success: usize,
    pub turn_end_error: usize,
    pub attention_required: usize,
}

/// The notification state machine (upstream `State`).
///
/// Semantics mirror upstream exactly:
/// - turn-end notifications are held while a paced presentation is pending;
/// - attention-required notifications are immediate;
/// - max-only cues are gated on `max`.
pub struct State {
    pub turn_end_enabled: AtomicBool,
    pub attention_required_enabled: AtomicBool,
    pub max_enabled: AtomicBool,
    pending_turn_end_success: std::sync::atomic::AtomicUsize,
    pending_turn_end_error: std::sync::atomic::AtomicUsize,
    pending_attention_required: std::sync::atomic::AtomicUsize,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        State {
            turn_end_enabled: AtomicBool::new(false),
            attention_required_enabled: AtomicBool::new(false),
            max_enabled: AtomicBool::new(false),
            pending_turn_end_success: std::sync::atomic::AtomicUsize::new(0),
            pending_turn_end_error: std::sync::atomic::AtomicUsize::new(0),
            pending_attention_required: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn set_preferences(&self, next: Preferences) {
        self.turn_end_enabled
            .store(next.turn_end, Ordering::Release);
        self.attention_required_enabled
            .store(next.attention_required, Ordering::Release);
        self.max_enabled.store(next.max, Ordering::Release);
    }

    pub fn preferences(&self) -> Preferences {
        Preferences {
            turn_end: self.turn_end_enabled.load(Ordering::Acquire),
            attention_required: self.attention_required_enabled.load(Ordering::Acquire),
            max: self.max_enabled.load(Ordering::Acquire),
        }
    }

    /// Max-only sound points require sound on AND max selected.
    pub fn max_enabled(&self) -> bool {
        self.preferences().max_enabled()
    }

    pub fn enabled(&self, kind: Kind) -> bool {
        match kind {
            Kind::TurnEnd => self.turn_end_enabled.load(Ordering::Acquire),
            Kind::AttentionRequired => self.attention_required_enabled.load(Ordering::Acquire),
        }
    }

    pub fn sound_enabled(&self) -> bool {
        self.turn_end_enabled.load(Ordering::Acquire)
    }

    /// Queue a notification. Returns `true` when the UI should present it
    /// immediately; turn-end notifications wait for the presentation to
    /// finish (mirrors upstream `queue` returning `!presentation_pending`).
    pub fn queue(&self, notification: Notification, presentation_pending: bool) -> bool {
        match notification.kind {
            Kind::TurnEnd => {
                if notification.cue == Cue::Error {
                    self.pending_turn_end_error.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.pending_turn_end_success
                        .fetch_add(1, Ordering::Relaxed);
                }
                !presentation_pending
            }
            Kind::AttentionRequired => {
                self.pending_attention_required
                    .fetch_add(1, Ordering::Relaxed);
                true
            }
        }
    }

    pub fn presentation_finished(&self) -> bool {
        self.pending_turn_end_success.load(Ordering::Relaxed) != 0
            || self.pending_turn_end_error.load(Ordering::Relaxed) != 0
    }

    pub fn take_ready(&self, presentation_pending: bool) -> ReadyNotifications {
        ReadyNotifications {
            turn_end_success: if presentation_pending {
                0
            } else {
                self.pending_turn_end_success.swap(0, Ordering::Relaxed)
            },
            turn_end_error: if presentation_pending {
                0
            } else {
                self.pending_turn_end_error.swap(0, Ordering::Relaxed)
            },
            attention_required: self.pending_attention_required.swap(0, Ordering::Relaxed),
        }
    }

    /// Flush any ready notifications through the player.
    pub fn flush(&self, player: &Player, presentation_pending: bool) -> usize {
        let ready = self.take_ready(presentation_pending);
        let count = ready.attention_required + ready.turn_end_success + ready.turn_end_error;
        if count == 0 {
            return 0;
        }
        for _ in 0..ready.attention_required {
            player.play_attention(Cue::Bloom);
        }
        for _ in 0..ready.turn_end_success {
            player.play(Cue::Success);
        }
        for _ in 0..ready.turn_end_error {
            player.play(Cue::Error);
        }
        count
    }
}

/// A minimal sound player (upstream `sound.zig` `Player`). No embedded audio
/// assets are bundled; the terminal bell is the fallback (upstream behavior
/// off macOS), and attention-required also tries a desktop notification.
pub struct Player {
    sink: BellSink,
    /// Try a desktop notification on attention-required when Some (fn pair
    /// so tests can stub it out).
    desktop_notify: Option<fn(&str)>,
}

impl Default for Player {
    fn default() -> Self {
        Player {
            sink: BellSink::default(),
            desktop_notify: Some(desktop_notify_best_effort),
        }
    }
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player").finish_non_exhaustive()
    }
}

impl Player {
    pub fn new(sink: BellSink) -> Self {
        Player {
            sink,
            desktop_notify: None,
        }
    }

    pub fn with_desktop(mut self, notify: fn(&str)) -> Self {
        self.desktop_notify = Some(notify);
        self
    }

    pub fn play(&self, cue: Cue) {
        let _ = cue;
        (self.sink.emit_bell)();
    }

    /// Attention-required: bell plus a desktop notification.
    pub fn play_attention(&self, cue: Cue) {
        let _ = cue;
        (self.sink.emit_bell)();
        if let Some(notify) = self.desktop_notify {
            notify("fxrs needs your attention — approve or deny a pending permission request.");
        }
    }
}

/// Best-effort desktop notification: `notify-send` on Linux, fallback silent.
pub fn desktop_notify_best_effort(body: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["fxrs", body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!("display notification \"{body}\" with title \"fxrs\""),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = body;
    }
}

/// Load persisted preferences (defaults when no store exists yet).
pub fn load_preferences() -> Preferences {
    let store = crate::config::fx_home().join("sound.json");
    std::fs::read_to_string(store)
        .ok()
        .and_then(|s| serde_json::from_str::<Preferences>(&s).ok())
        .unwrap_or_else(default_preferences)
}

/// Construct the default player (terminal bell + best-effort desktop notify).
pub fn make_player() -> Player {
    Player::default()
}

/// Default offline preferences (mirrors upstream: sound defaults on only
/// where a real audio player exists — macOS; everywhere else the fallback is
/// the terminal bell, so notifications stay opt-in).
pub fn default_preferences() -> Preferences {
    #[cfg(target_os = "macos")]
    {
        Preferences {
            turn_end: true,
            attention_required: true,
            max: false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Preferences::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_end_delivery_waits_for_paced_presentation_while_attention_is_immediate() {
        let state = State::new();
        state.set_preferences(Preferences {
            turn_end: true,
            attention_required: true,
            max: false,
        });
        assert!(state.enabled(Kind::TurnEnd));
        assert!(state.enabled(Kind::AttentionRequired));

        // Non-interactive scopes above are filtered by the caller; the state
        // machine itself only gates on kind.
        assert!(!state.queue(Notification::turn_end(Cue::Error), true));
        assert!(state.queue(Notification::attention_required(), true));

        let mut ready = state.take_ready(true);
        assert_eq!(ready.turn_end_error, 0);
        assert_eq!(ready.attention_required, 1);
        assert!(state.presentation_finished());

        ready = state.take_ready(false);
        assert_eq!(ready.turn_end_error, 1);
        assert_eq!(ready.turn_end_success, 0);
        assert_eq!(ready.attention_required, 0);
        assert!(!state.presentation_finished());
    }

    #[test]
    fn max_gate_requires_both_sound_on_and_max_selected() {
        let state = State::new();
        assert!(!state.max_enabled());

        state.set_preferences(Preferences {
            turn_end: true,
            attention_required: true,
            max: false,
        });
        assert!(!state.max_enabled());
        assert!(!state.preferences().max);

        state.set_preferences(Preferences {
            turn_end: true,
            attention_required: true,
            max: true,
        });
        assert!(state.max_enabled());
        assert!(state.preferences().max);

        // Sound off suppresses max even if the flag lingers.
        state.set_preferences(Preferences {
            turn_end: false,
            attention_required: false,
            max: true,
        });
        assert!(!state.max_enabled());
    }

    #[test]
    fn flush_delivers_success_and_error_counts() {
        let bells = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let bells2 = bells.clone();
        let state = State::new();
        state.set_preferences(Preferences {
            turn_end: true,
            attention_required: true,
            max: false,
        });
        let player = Player::new(BellSink {
            emit_bell: Box::new(move || {
                bells2.fetch_add(1, Ordering::Relaxed);
            }),
        });
        // 2 success + 1 error + 1 attention.
        state.queue(Notification::turn_end(Cue::Success), false);
        state.queue(Notification::turn_end(Cue::Success), false);
        state.queue(Notification::turn_end(Cue::Error), false);
        state.queue(Notification::attention_required(), false);
        let total = state.flush(&player, false);
        assert_eq!(total, 4);
        assert_eq!(bells.load(Ordering::Relaxed), 4);
    }
}
