//! Terminal takeover controller — faithful port of the pure decision layer
//! of upstream `core/app/app_terminal_takeover_runtime.zig`.
//!
//! Upstream embeds one controller in the app runtime; it takes over the
//! alternate screen of a terminal session (lease acquire → interactive
//! forwarding of raw bytes → release back to the shell), understands a
//! `Ctrl-]` prefix (detach / help / literal prefix) while preserving
//! bracketed-paste data byte-for-byte, and decides how the surface returns
//! depending on which manager screen owns the alternate screen.
//!
//! This file ports the **pure state machine** (prefix parser, phase
//! transitions, bounded input retention, surface-return decisions) with the
//! same observable behavior as upstream. The app-runtime wiring (terminal
//! client admission, correlation tracking, alternate-screen enter/leave,
//! render requests) is deferred to the Phase 5 TUI; the decision layer is
//! fully testable without it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Ctrl-] — the takeover prefix introducer (upstream `control_prefix`).
pub const CONTROL_PREFIX: u8 = 0x1d;
/// Hard cap on forwarded input retained while acquisition is pending
/// (upstream `max_write_bytes` in `contracts.zig`).
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Screen polling interval (33 ms — matches upstream).
pub const SCREEN_POLL_INTERVAL_MS: i64 = 33;
/// Retry interval between release attempts (250 ms).
pub const RELEASE_RETRY_INTERVAL_MS: i64 = 250;

const PASTE_BEGIN: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixAction {
    None,
    Detach,
    Help,
}

/// Result of feeding one byte through the prefix parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixResult {
    pub action: PrefixAction,
    /// At most one forwarded byte (the typed byte, or the literal prefix).
    pub forwarded: Option<u8>,
}

impl PrefixResult {
    /// 0-or-1 forwarded bytes (API mirrors upstream `bytes()`).
    pub fn bytes(&self) -> [u8; 1] {
        match self.forwarded {
            Some(b) => [b],
            None => [0],
        }
    }

    pub fn has_forwarded(&self) -> bool {
        self.forwarded.is_some()
    }

    pub fn forwarded_len(&self) -> usize {
        usize::from(self.forwarded.is_some())
    }
}

fn forward(byte: u8) -> PrefixResult {
    PrefixResult {
        action: PrefixAction::None,
        forwarded: Some(byte),
    }
}

fn advance_delimiter(current: usize, byte: u8, delimiter: &[u8]) -> usize {
    if byte == delimiter[current] {
        current + 1
    } else if byte == delimiter[0] {
        1
    } else {
        0
    }
}

/// Streaming parser for the `Ctrl-]` prefix. Bracketed-paste data
/// (`\x1b[200~` … `\x1b[201~`) is forwarded verbatim, never interpreted.
#[derive(Debug, Clone, Default)]
pub struct PrefixParser {
    pub pending: bool,
    pub in_paste: bool,
    begin_match: usize,
    end_match: usize,
}

impl PrefixParser {
    pub fn feed(&mut self, byte: u8) -> PrefixResult {
        if self.in_paste {
            self.end_match = advance_delimiter(self.end_match, byte, PASTE_END);
            if self.end_match == PASTE_END.len() {
                self.in_paste = false;
                self.end_match = 0;
            }
            return forward(byte);
        }

        self.begin_match = advance_delimiter(self.begin_match, byte, PASTE_BEGIN);
        if self.begin_match == PASTE_BEGIN.len() {
            self.in_paste = true;
            self.begin_match = 0;
        }

        if self.pending {
            self.pending = false;
            return match byte {
                b'd' | b'D' => PrefixResult {
                    action: PrefixAction::Detach,
                    forwarded: None,
                },
                b'?' => PrefixResult {
                    action: PrefixAction::Help,
                    forwarded: None,
                },
                CONTROL_PREFIX => forward(CONTROL_PREFIX),
                other => forward(other),
            };
        }
        if byte == CONTROL_PREFIX {
            self.pending = true;
            return PrefixResult {
                action: PrefixAction::None,
                forwarded: None,
            };
        }
        forward(byte)
    }
}

/// Takeover phase (upstream `Phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Inactive,
    Acquiring,
    Active,
    Releasing,
}

/// Why the controller is returning (upstream `ReturnReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnReason {
    Detach,
    Exited,
    Lost,
    Failure,
}

/// Where the alternate screen sits when the controller releases it
/// (upstream `shell_runtime.AlternateScreenOwner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlternateScreenOwner {
    #[default]
    None,
    TerminalSession,
    SubagentManager,
}

/// What the surface should do after a terminal takeover returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceReturnAction {
    HandoffToManager,
    EnterManager,
    RenderManager,
    LeaveToInline,
    RecoverInline,
    None,
}

pub fn surface_return_action(
    manager_active: bool,
    owner: AlternateScreenOwner,
    inline_recovery_pending: bool,
) -> SurfaceReturnAction {
    if manager_active {
        return match owner {
            AlternateScreenOwner::TerminalSession => SurfaceReturnAction::HandoffToManager,
            AlternateScreenOwner::None => SurfaceReturnAction::EnterManager,
            AlternateScreenOwner::SubagentManager => SurfaceReturnAction::RenderManager,
        };
    }
    match owner {
        AlternateScreenOwner::TerminalSession => SurfaceReturnAction::LeaveToInline,
        AlternateScreenOwner::None => {
            if inline_recovery_pending {
                SurfaceReturnAction::RecoverInline
            } else {
                SurfaceReturnAction::None
            }
        }
        AlternateScreenOwner::SubagentManager => SurfaceReturnAction::None,
    }
}

/// Minimal alternate-screen state the controller consults.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalState {
    pub alternate_screen_owner: AlternateScreenOwner,
}

/// Terminal lifecycle (upstream `contracts.Lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Starting,
    Running,
    Exited,
    Lost,
    Closed,
}

pub fn terminal_ended(lifecycle: Lifecycle) -> bool {
    matches!(
        lifecycle,
        Lifecycle::Exited | Lifecycle::Lost | Lifecycle::Closed
    )
}

pub fn lifecycle_reason(lifecycle: Lifecycle) -> ReturnReason {
    if matches!(lifecycle, Lifecycle::Exited | Lifecycle::Closed) {
        ReturnReason::Exited
    } else {
        ReturnReason::Lost
    }
}

/// Terminal-write completion failure kinds (subset of upstream
/// `client.Completion` failure codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionFailure {
    Disconnected,
    Unavailable,
    SessionNotFound,
    SessionLost,
    AuthorityDenied,
    LeaseConflict,
    Other,
}

pub fn completion_reason(failure: CompletionFailure) -> ReturnReason {
    match failure {
        CompletionFailure::SessionNotFound | CompletionFailure::SessionLost => ReturnReason::Lost,
        CompletionFailure::Disconnected | CompletionFailure::Unavailable => ReturnReason::Lost,
        _ => ReturnReason::Failure,
    }
}

pub fn release_ownership_gone(failure: CompletionFailure) -> bool {
    matches!(
        failure,
        CompletionFailure::SessionNotFound
            | CompletionFailure::SessionLost
            | CompletionFailure::AuthorityDenied
            | CompletionFailure::LeaseConflict
    )
}

/// The controller's durable decision layer. Engine admission (lease acquire /
/// write / screen / release correlation flows) is intentionally absent;
/// `handle_byte` and the transitions below are the same pure pipeline upstream
/// runs before touching the engine.
#[derive(Debug, Clone)]
pub struct Controller {
    pub phase: Phase,
    pub lease_acquired: bool,
    pub return_reason: ReturnReason,
    pub discard_input: bool,
    /// Bounded retained input (typed while acquisition is pending).
    pub input: Vec<u8>,
    pub prefix: PrefixParser,
    pub help_visible: bool,
    pub inflight_write_bytes: usize,
    pub release_attempts: u8,
    pub surface_return_attempts: u8,
    pub inline_recovery_pending: bool,
    pub next_release_ms: i64,
    pub last_dimensions: Option<(u32, u32)>,
    pub next_screen_ms: i64,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            phase: Phase::Inactive,
            lease_acquired: false,
            return_reason: ReturnReason::Detach,
            discard_input: false,
            input: Vec::new(),
            prefix: PrefixParser::default(),
            help_visible: false,
            inflight_write_bytes: 0,
            release_attempts: 0,
            surface_return_attempts: 0,
            inline_recovery_pending: false,
            next_release_ms: 0,
            last_dimensions: None,
            next_screen_ms: 0,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Controller {
    /// Upstream `blocksFxSurface`: the controller owns the surface only while
    /// the alternate screen belongs to a terminal session.
    pub fn blocks_fx_surface(&self, terminal: &TerminalState) -> bool {
        terminal.alternate_screen_owner == AlternateScreenOwner::TerminalSession
    }

    /// Retain forwarded input while acquisition is pending (bounded).
    pub fn retain_input(&mut self, bytes: &[u8]) -> Result<(), TakeoverError> {
        if self.input.len() > MAX_WRITE_BYTES || bytes.len() > MAX_WRITE_BYTES - self.input.len() {
            return Err(TakeoverError::InputFull);
        }
        self.input.extend_from_slice(bytes);
        Ok(())
    }

    /// Upstream `beginReturn`: transition to releasing with a reason, drop
    /// input when discarding is requested. Returns `false` when the
    /// transition is a no-op (already inactive/releasing).
    pub fn begin_return(&mut self, reason: ReturnReason, discard_input: bool) -> bool {
        if matches!(self.phase, Phase::Inactive | Phase::Releasing) {
            return false;
        }
        self.phase = Phase::Releasing;
        self.return_reason = reason;
        self.discard_input = discard_input;
        if discard_input && !self.input.is_empty() {
            self.input.clear();
        }
        true
    }

    /// Schedule a release retry (upstream `scheduleReleaseRetry`-adjacent
    /// admission pulse): bumps the attempt counter and defers the next
    /// admission by [`RELEASE_RETRY_INTERVAL_MS`].
    pub fn defer_release(&mut self) {
        self.release_attempts = self.release_attempts.saturating_add(1);
        self.next_release_ms = now_ms() + RELEASE_RETRY_INTERVAL_MS;
    }

    /// Upstream `handleByte`: process one raw byte from the human.
    ///
    /// Returns `true` when the takeover consumed the byte (blocks the FX
    /// surface), `false` when it is inactive or not the surface owner.
    /// `terminal` is the current alternate-screen owner state.
    pub fn handle_byte(&mut self, terminal: &TerminalState, byte: u8) -> bool {
        if self.phase == Phase::Inactive {
            return false;
        }
        if self.phase == Phase::Releasing {
            return self.blocks_fx_surface(terminal);
        }
        if self.phase != Phase::Acquiring && !self.blocks_fx_surface(terminal) {
            return false;
        }
        if self.help_visible {
            self.help_visible = false;
            self.next_screen_ms = 0;
        }
        let parsed = self.prefix.feed(byte);
        match parsed.action {
            PrefixAction::None => {}
            PrefixAction::Detach => {
                self.begin_return(ReturnReason::Detach, false);
            }
            PrefixAction::Help => {
                self.help_visible = true;
                self.next_screen_ms = 0;
            }
        }
        if parsed.forwarded.is_some() && matches!(self.phase, Phase::Acquiring | Phase::Active) {
            let _ = self.retain_input(&parsed.bytes());
        }
        true
    }

    /// Feed a slice of bytes; returns the number of bytes the controller
    /// consumed (took over). Used by tests and the future TUI input seam.
    pub fn handle_bytes(&mut self, terminal: &TerminalState, bytes: &[u8]) -> usize {
        bytes
            .iter()
            .filter(|&&b| self.handle_byte(terminal, b))
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverError {
    InputFull,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_fragments_locally_without_leaking_consumed_bytes() {
        let mut parser = PrefixParser::default();
        assert_eq!(parser.feed(CONTROL_PREFIX).forwarded_len(), 0);
        assert_eq!(parser.feed(b'd').action, PrefixAction::Detach);

        assert_eq!(parser.feed(CONTROL_PREFIX).forwarded_len(), 0);
        assert_eq!(parser.feed(b'?').action, PrefixAction::Help);

        assert_eq!(parser.feed(CONTROL_PREFIX).forwarded_len(), 0);
        let literal = parser.feed(CONTROL_PREFIX);
        assert_eq!(literal.bytes(), [CONTROL_PREFIX]);

        assert_eq!(parser.feed(CONTROL_PREFIX).forwarded_len(), 0);
        let unknown = parser.feed(b'x');
        assert_eq!(unknown.bytes(), [b'x']);
    }

    #[test]
    fn prefix_is_raw_data_inside_fragmented_bracketed_paste() {
        let mut parser = PrefixParser::default();
        for &b in "\x1b[200~".as_bytes() {
            parser.feed(b);
        }
        // The paste has begun; the Ctrl-] byte is raw data, not a prefix.
        assert!(parser.in_paste);
        let inside = parser.feed(CONTROL_PREFIX);
        assert!(parser.in_paste);
        assert_eq!(inside.bytes(), [CONTROL_PREFIX]);
        assert_eq!(inside.action, PrefixAction::None);
        for &b in "\x1b[201~".as_bytes() {
            parser.feed(b);
        }
        assert!(!parser.in_paste);
    }

    #[test]
    fn controller_retains_bounded_input_while_acquisition_is_pending() {
        let mut controller = Controller {
            phase: Phase::Acquiring,
            ..Controller::default()
        };
        assert!(controller.retain_input(b"before-acquire").is_ok());
        assert_eq!(controller.input, b"before-acquire");

        controller.input.resize(MAX_WRITE_BYTES, 0);
        assert_eq!(controller.retain_input(b"x"), Err(TakeoverError::InputFull));
    }

    #[test]
    fn controller_retain_input_failure_leaves_intact_buffer() {
        let mut controller = Controller {
            phase: Phase::Acquiring,
            ..Controller::default()
        };
        // Allocation failure equivalent: a full input buffer rejects with the
        // same error and the buffer is unchanged.
        controller.input.resize(MAX_WRITE_BYTES, 0);
        assert_eq!(
            controller.retain_input(b"pending"),
            Err(TakeoverError::InputFull)
        );
        assert_eq!(controller.input.len(), MAX_WRITE_BYTES);
    }

    #[test]
    fn alternate_screen_ownership_is_the_takeover_visibility_oracle() {
        let controller = Controller {
            phase: Phase::Releasing,
            lease_acquired: true,
            ..Controller::default()
        };
        let mut terminal = TerminalState {
            alternate_screen_owner: AlternateScreenOwner::TerminalSession,
        };
        assert!(controller.blocks_fx_surface(&terminal));
        terminal.alternate_screen_owner = AlternateScreenOwner::SubagentManager;
        assert!(!controller.blocks_fx_surface(&terminal));
        assert_eq!(controller.phase, Phase::Releasing);
        assert!(controller.lease_acquired);
    }

    #[test]
    fn surface_return_decisions_are_independent_of_lease_cleanup_order() {
        assert_eq!(
            surface_return_action(true, AlternateScreenOwner::TerminalSession, false),
            SurfaceReturnAction::HandoffToManager
        );
        assert_eq!(
            surface_return_action(true, AlternateScreenOwner::SubagentManager, false),
            SurfaceReturnAction::RenderManager
        );
        assert_eq!(
            surface_return_action(false, AlternateScreenOwner::TerminalSession, false),
            SurfaceReturnAction::LeaveToInline
        );
        assert_eq!(
            surface_return_action(false, AlternateScreenOwner::None, true),
            SurfaceReturnAction::RecoverInline
        );
        assert_eq!(
            surface_return_action(false, AlternateScreenOwner::None, false),
            SurfaceReturnAction::None
        );
    }

    #[test]
    fn handle_byte_forwards_typed_bytes_and_consumes_only_when_owning() {
        let terminal = TerminalState {
            alternate_screen_owner: AlternateScreenOwner::TerminalSession,
        };
        // Inactive: nothing consumed.
        let mut c = Controller::default();
        assert!(!c.handle_byte(&terminal, b'a'));

        // Active + owning: bytes forwarded, Ctrl-] help works, 'd' detaches.
        let mut c = Controller {
            phase: Phase::Active,
            ..Controller::default()
        };
        assert!(c.handle_byte(&terminal, b'a'));
        assert_eq!(c.input, b"a");
        assert!(c.handle_byte(&terminal, CONTROL_PREFIX));
        assert!(c.handle_byte(&terminal, b'?'));
        assert!(c.help_visible);
        // Any subsequent byte clears help.
        assert!(c.handle_byte(&terminal, b'b'));
        assert!(!c.help_visible);
        assert!(c.handle_byte(&terminal, CONTROL_PREFIX));
        assert!(c.handle_byte(&terminal, b'd'));
        assert_eq!(c.phase, Phase::Releasing);
        assert_eq!(c.return_reason, ReturnReason::Detach);
        // Releasing: still consumes while the surface is owned.
        assert!(c.handle_byte(&terminal, b'x'));
    }

    #[test]
    fn terminal_lifecycle_maps_to_return_reasons() {
        assert!(terminal_ended(Lifecycle::Exited));
        assert!(terminal_ended(Lifecycle::Lost));
        assert!(terminal_ended(Lifecycle::Closed));
        assert!(!terminal_ended(Lifecycle::Running));
        assert_eq!(lifecycle_reason(Lifecycle::Exited), ReturnReason::Exited);
        assert_eq!(lifecycle_reason(Lifecycle::Closed), ReturnReason::Exited);
        assert_eq!(lifecycle_reason(Lifecycle::Lost), ReturnReason::Lost);

        assert_eq!(
            completion_reason(CompletionFailure::SessionLost),
            ReturnReason::Lost
        );
        assert_eq!(
            completion_reason(CompletionFailure::AuthorityDenied),
            ReturnReason::Failure
        );
        assert!(release_ownership_gone(CompletionFailure::LeaseConflict));
        assert!(!release_ownership_gone(CompletionFailure::Unavailable));
    }
}
