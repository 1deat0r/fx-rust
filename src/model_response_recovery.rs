//! Model-response recovery policy (faithful port of upstream
//! `core/agent/runtime/model_response_recovery.zig`).
//!
//! A **pure** decision model for what to do after a failed model (provider)
//! request: retry the request, regenerate a proven-unexecuted tool, continue
//! after a confirmed tool, reconcile an uncertain tool, pause, or stop. The
//! policy never sleeps, sends, mutates stream state, or persists a
//! checkpoint — the caller owns those effects, so the module is trivial to
//! test and safe to reuse across runtimes.

/// Why the provider request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    TransportInterrupted,
    ResponseInterrupted,
    ProviderUnavailable,
    RateLimited,
    SystemResumed,
    Authentication,
    RequestLimitReached,
    ContentFilter,
}

/// Whether the failed request may have reached the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    DefinitelyUnsent,
    PossiblySent,
}

/// How much assistant output arrived before the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEvidence {
    None,
    Partial,
}

/// How certain we are about a tool call named by the interrupted response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEvidence {
    None,
    ProvenUnexecuted,
    Confirmed,
    Uncertain,
}

pub const DEFAULT_MAX_PROVIDER_ATTEMPTS: usize = 10;
pub const MAX_RETRY_AFTER_SECONDS: u64 = 30;

/// Durable request budget. `consumed` persists across retries; `limit` is
/// the ceiling above which the policy pauses instead of spending again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptState {
    pub consumed: usize,
    pub limit: usize,
}

impl Default for AttemptState {
    fn default() -> Self {
        Self {
            consumed: 0,
            limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        }
    }
}

impl AttemptState {
    pub fn remaining(self) -> usize {
        self.limit.saturating_sub(self.consumed)
    }
}

/// Ephemeral backoff state. [`AttemptState`] remains the durable request
/// budget; `pacing` only shapes the inter-attempt delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPacingState {
    Idle,
    Implicit { cause: FailureCause, attempt: usize },
}

impl RetryPacingState {
    fn after_failure(self, cause: FailureCause, retry_after_seconds: Option<u64>) -> Self {
        if retry_after_seconds.is_some() {
            return Self::Idle;
        }
        match self {
            Self::Idle => Self::Implicit { cause, attempt: 1 },
            Self::Implicit {
                cause: previous,
                attempt,
            } if previous == cause => Self::Implicit {
                cause,
                attempt: attempt.saturating_add(1),
            },
            _ => Self::Implicit { cause, attempt: 1 },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    RetryRequest,
    ContinueResponse,
    RegenerateTool,
    ContinueAfterConfirmedTool,
    ReconcileTool,
    Pause,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredAction {
    None,
    ContinueLater,
    InspectUncertainTool,
    ChangeRequest,
}

#[derive(Debug, Clone, Copy)]
pub struct Evidence {
    pub cause: FailureCause,
    pub delivery: Delivery,
    pub attempts: AttemptState,
    pub output: OutputEvidence,
    pub tool: ToolEvidence,
    pub pacing: RetryPacingState,
    pub retry_after_seconds: Option<u64>,
    pub cancelled: bool,
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            cause: FailureCause::TransportInterrupted,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState::default(),
            output: OutputEvidence::None,
            tool: ToolEvidence::None,
            pacing: RetryPacingState::Idle,
            retry_after_seconds: None,
            cancelled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub strategy: Strategy,
    pub delay_ns: u64,
    pub next_pacing: RetryPacingState,
    pub reserve_provider_attempt: bool,
    pub required_action: RequiredAction,
}

impl Default for Decision {
    fn default() -> Self {
        Self {
            strategy: Strategy::Stop,
            delay_ns: 0,
            next_pacing: RetryPacingState::Idle,
            reserve_provider_attempt: false,
            required_action: RequiredAction::None,
        }
    }
}

/// Delay before the next provider request after `attempt` consecutive
/// implicit backoffs: 250 ms, 1 s, then exponential growth capped at 30 s.
pub fn retry_delay_ns(attempt: usize) -> u64 {
    const NS_PER_MS: u64 = 1_000_000;
    const NS_PER_S: u64 = 1_000_000_000;
    if attempt == 0 {
        return 0;
    }
    if attempt == 1 {
        return 250 * NS_PER_MS;
    }
    let mut seconds: u64 = 1;
    let mut current: usize = 2;
    while current < attempt && seconds < MAX_RETRY_AFTER_SECONDS {
        seconds = seconds.saturating_mul(2).min(MAX_RETRY_AFTER_SECONDS);
        current += 1;
    }
    seconds * NS_PER_S
}

/// Pure model-response policy. It describes the next effect but never sleeps,
/// sends, mutates stream state, or persists a checkpoint.
pub fn decide(evidence: Evidence) -> Decision {
    if evidence.cancelled {
        return Decision {
            strategy: Strategy::Stop,
            ..Decision::default()
        };
    }

    match evidence.cause {
        FailureCause::ContentFilter => {
            return Decision {
                strategy: Strategy::Stop,
                required_action: RequiredAction::ChangeRequest,
                ..Decision::default()
            };
        }
        FailureCause::RequestLimitReached => {
            return Decision {
                strategy: Strategy::Pause,
                required_action: RequiredAction::ContinueLater,
                ..Decision::default()
            };
        }
        _ => {}
    }

    if evidence.attempts.remaining() == 0 {
        return Decision {
            strategy: Strategy::Pause,
            required_action: if evidence.tool == ToolEvidence::Uncertain {
                RequiredAction::InspectUncertainTool
            } else {
                RequiredAction::ContinueLater
            },
            ..Decision::default()
        };
    }

    let strategy = if evidence.delivery == Delivery::DefinitelyUnsent {
        Strategy::RetryRequest
    } else {
        match evidence.tool {
            ToolEvidence::ProvenUnexecuted => Strategy::RegenerateTool,
            ToolEvidence::Confirmed => Strategy::ContinueAfterConfirmedTool,
            ToolEvidence::Uncertain => Strategy::ReconcileTool,
            ToolEvidence::None => {
                if evidence.output == OutputEvidence::Partial {
                    Strategy::ContinueResponse
                } else {
                    Strategy::RetryRequest
                }
            }
        }
    };
    let next_pacing = evidence
        .pacing
        .after_failure(evidence.cause, evidence.retry_after_seconds);
    let delay_ns = match evidence.retry_after_seconds {
        Some(seconds) => seconds.min(MAX_RETRY_AFTER_SECONDS) * 1_000_000_000,
        None => match next_pacing {
            RetryPacingState::Idle => 0, // unreachable: after_failure with no Retry-After yields Implicit
            RetryPacingState::Implicit { attempt, .. } => retry_delay_ns(attempt),
        },
    };
    Decision {
        strategy,
        delay_ns,
        next_pacing,
        reserve_provider_attempt: true,
        required_action: RequiredAction::None,
    }
}

/// Fast is an optimization, not a recovery requirement. A replay-safe
/// provider outage may fall back to the canonical route without changing the
/// semantic request budget.
pub fn should_disable_fast_route(fast_mode: bool, cause: FailureCause, replay_safe: bool) -> bool {
    fast_mode && cause == FailureCause::ProviderUnavailable && replay_safe
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationProbeOutcome {
    MayExtend,
    CannotExtend,
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuationProbe {
    pub outcome: ContinuationProbeOutcome,
    pub comparisons: usize,
}

/// Determines whether more incoming bytes could extend the overlap while
/// performing no more than `comparison_budget` byte comparisons.
pub fn probe_continuation_extension(
    existing: &[u8],
    incoming: &[u8],
    comparison_budget: usize,
) -> ContinuationProbe {
    if incoming.len() >= existing.len() {
        return ContinuationProbe {
            outcome: ContinuationProbeOutcome::CannotExtend,
            comparisons: 0,
        };
    }
    let candidate_count = existing.len() - incoming.len();
    let mut comparisons = 0usize;
    for start in 0..candidate_count {
        let mut matched = true;
        for (offset, byte) in incoming.iter().enumerate() {
            if comparisons == comparison_budget {
                return ContinuationProbe {
                    outcome: ContinuationProbeOutcome::BudgetExhausted,
                    comparisons,
                };
            }
            comparisons += 1;
            if existing[start + offset] != *byte {
                matched = false;
                break;
            }
        }
        if matched {
            return ContinuationProbe {
                outcome: ContinuationProbeOutcome::MayExtend,
                comparisons,
            };
        }
    }
    ContinuationProbe {
        outcome: ContinuationProbeOutcome::CannotExtend,
        comparisons,
    }
}

/// Returns only the longest byte-exact suffix/prefix overlap in linear time.
/// This deliberately avoids fuzzy or semantic matching, which could delete
/// legitimate output.
///
/// KMP: `incoming` is the pattern; `existing` is the text. The final matched
/// length is the longest prefix of `incoming` that is a suffix of `existing`.
pub fn exact_continuation_overlap(existing: &[u8], incoming: &[u8]) -> usize {
    if existing.is_empty() || incoming.is_empty() {
        return 0;
    }
    // Build the KMP prefix table for `incoming`.
    let mut prefix = vec![0usize; incoming.len()];
    let mut matched = 0usize;
    for (incoming_idx, &byte) in incoming.iter().enumerate().skip(1) {
        while matched > 0 && byte != incoming[matched] {
            matched = prefix[matched - 1];
        }
        if byte == incoming[matched] {
            matched += 1;
        }
        prefix[incoming_idx] = matched;
    }
    // Scan `existing`. The while condition mirrors the Zig original: a full
    // match (matched == incoming.len) always falls back via the prefix table.
    matched = 0;
    for &byte in existing {
        while matched > 0 && (matched == incoming.len() || byte != incoming[matched]) {
            matched = prefix[matched - 1];
        }
        // After the loop `matched` is always < `incoming.len()`: a full match
        // (matched == len) collapses through the prefix table first, exactly
        // as in the Zig original.
        if byte == incoming[matched] {
            matched += 1;
        }
    }
    matched
}

/// Best-effort classification of a stream/transport error string into a
/// recovery [`FailureCause`]. Returns `None` for errors that should not be
/// retried (client/request bugs), mirroring upstream's non-transient
/// admission errors bypassing the decision policy.
pub fn classify_failure(message: &str) -> Option<FailureCause> {
    let m = message.to_ascii_lowercase();
    let contains = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    if contains(&[
        "content filter",
        "safety",
        "refused to respond",
        "content_policy",
    ]) {
        return Some(FailureCause::ContentFilter);
    }
    if contains(&["request limit", "usage limit", "quota exceeded"]) {
        return Some(FailureCause::RequestLimitReached);
    }
    if contains(&["429", "rate limit", "too many requests"]) {
        return Some(FailureCause::RateLimited);
    }
    if contains(&[
        "401",
        "403",
        "unauthorized",
        "invalid api key",
        "authentication",
    ]) {
        return Some(FailureCause::Authentication);
    }
    if contains(&[
        "502",
        "503",
        "504",
        "service unavailable",
        "bad gateway",
        "overloaded",
    ]) {
        return Some(FailureCause::ProviderUnavailable);
    }
    if contains(&[
        "connection",
        "connect",
        "timed out",
        "timeout",
        "eof",
        "unexpected end",
        "reset",
        "broken pipe",
        "tls",
        "certificate",
        "dns",
        "transport",
    ]) {
        return Some(FailureCause::TransportInterrupted);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS_PER_MS: u64 = 1_000_000;
    const NS_PER_S: u64 = 1_000_000_000;

    fn base() -> Evidence {
        Evidence {
            cause: FailureCause::TransportInterrupted,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 1,
                limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
            },
            ..Evidence::default()
        }
    }

    #[test]
    fn policy_is_deterministic_and_bounded() {
        let first = decide(base());
        assert_eq!(first, decide(base()));
        assert_eq!(first.strategy, Strategy::RetryRequest);
        assert!(first.reserve_provider_attempt);
        assert_eq!(first.delay_ns, 250 * NS_PER_MS);

        let partial = Evidence {
            output: OutputEvidence::Partial,
            ..base()
        };
        assert_eq!(decide(partial).strategy, Strategy::ContinueResponse);

        let unexecuted = Evidence {
            tool: ToolEvidence::ProvenUnexecuted,
            ..partial
        };
        assert_eq!(decide(unexecuted).strategy, Strategy::RegenerateTool);

        let uncertain = Evidence {
            tool: ToolEvidence::Uncertain,
            ..partial
        };
        assert_eq!(decide(uncertain).strategy, Strategy::ReconcileTool);

        let definitely_unsent = Evidence {
            delivery: Delivery::DefinitelyUnsent,
            ..uncertain
        };
        assert_eq!(decide(definitely_unsent).strategy, Strategy::RetryRequest);

        let exhausted = Evidence {
            attempts: AttemptState {
                consumed: DEFAULT_MAX_PROVIDER_ATTEMPTS,
                ..Default::default()
            },
            ..base()
        };
        let paused = decide(exhausted);
        assert_eq!(paused.strategy, Strategy::Pause);
        assert!(!paused.reserve_provider_attempt);

        let request_limit = Evidence {
            cause: FailureCause::RequestLimitReached,
            ..base()
        };
        let rl_pause = decide(request_limit);
        assert_eq!(rl_pause.strategy, Strategy::Pause);
        assert!(!rl_pause.reserve_provider_attempt);
    }

    #[test]
    fn retry_after_and_cancellation_override_automatic_recovery() {
        let base = Evidence {
            cause: FailureCause::RateLimited,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 3,
                ..Default::default()
            },
            ..Evidence::default()
        };

        let bounded = Evidence {
            retry_after_seconds: Some(30),
            ..base
        };
        let wait = decide(bounded);
        assert_eq!(wait.strategy, Strategy::RetryRequest);
        assert_eq!(wait.delay_ns, 30 * NS_PER_S);

        let over_cap = Evidence {
            retry_after_seconds: Some(31),
            ..base
        };
        let capped = decide(over_cap);
        assert_eq!(capped.strategy, Strategy::RetryRequest);
        assert_eq!(capped.delay_ns, MAX_RETRY_AFTER_SECONDS * NS_PER_S);
        assert!(capped.reserve_provider_attempt);

        let overflow_retry_after = Evidence {
            retry_after_seconds: Some(u64::MAX),
            ..base
        };
        let overflow_capped = decide(overflow_retry_after);
        assert_eq!(overflow_capped.strategy, Strategy::RetryRequest);
        assert_eq!(overflow_capped.delay_ns, MAX_RETRY_AFTER_SECONDS * NS_PER_S);
        assert!(overflow_capped.reserve_provider_attempt);

        let cancelled = Evidence {
            cancelled: true,
            ..base
        };
        assert_eq!(decide(cancelled).strategy, Strategy::Stop);
    }

    #[test]
    fn retry_schedule_uses_the_approved_cap() {
        let expected = [
            250 * NS_PER_MS,
            NS_PER_S,
            2 * NS_PER_S,
            4 * NS_PER_S,
            8 * NS_PER_S,
            16 * NS_PER_S,
            30 * NS_PER_S,
            30 * NS_PER_S,
            30 * NS_PER_S,
        ];
        let mut total = 0u64;
        for (i, delay) in expected.iter().enumerate() {
            assert_eq!(*delay, retry_delay_ns(i + 1));
            total += delay;
        }
        assert_eq!(total, 121_250 * NS_PER_MS);
    }

    #[test]
    fn implicit_retry_pacing_is_independent_from_the_shared_attempt_budget() {
        let first_network = decide(Evidence {
            cause: FailureCause::TransportInterrupted,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 6,
                ..Default::default()
            },
            ..Evidence::default()
        });
        assert_eq!(first_network.strategy, Strategy::RetryRequest);
        assert_eq!(first_network.delay_ns, 250 * NS_PER_MS);

        let second_network = decide(Evidence {
            cause: FailureCause::TransportInterrupted,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 7,
                ..Default::default()
            },
            pacing: first_network.next_pacing,
            ..Evidence::default()
        });
        assert_eq!(second_network.delay_ns, NS_PER_S);

        let provider_failure = decide(Evidence {
            cause: FailureCause::ProviderUnavailable,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 8,
                ..Default::default()
            },
            pacing: second_network.next_pacing,
            ..Evidence::default()
        });
        assert_eq!(provider_failure.delay_ns, 250 * NS_PER_MS);

        let explicitly_timed = decide(Evidence {
            cause: FailureCause::ProviderUnavailable,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 9,
                ..Default::default()
            },
            pacing: provider_failure.next_pacing,
            retry_after_seconds: Some(0),
            ..Evidence::default()
        });
        assert_eq!(explicitly_timed.delay_ns, 0);
        assert_eq!(explicitly_timed.next_pacing, RetryPacingState::Idle);
    }

    #[test]
    fn system_resume_strategy_uses_independent_retry_pacing() {
        let retrying = decide(Evidence {
            cause: FailureCause::SystemResumed,
            delivery: Delivery::DefinitelyUnsent,
            attempts: AttemptState {
                consumed: 4,
                ..Default::default()
            },
            ..Evidence::default()
        });
        assert_eq!(retrying.strategy, Strategy::RetryRequest);
        assert_eq!(retrying.delay_ns, 250 * NS_PER_MS);
        assert!(retrying.reserve_provider_attempt);

        let continuing = decide(Evidence {
            cause: FailureCause::SystemResumed,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 4,
                ..Default::default()
            },
            output: OutputEvidence::Partial,
            ..Evidence::default()
        });
        assert_eq!(continuing.strategy, Strategy::ContinueResponse);
        assert!(continuing.reserve_provider_attempt);

        let exhausted = decide(Evidence {
            cause: FailureCause::SystemResumed,
            delivery: Delivery::PossiblySent,
            attempts: AttemptState {
                consumed: 10,
                ..Default::default()
            },
            ..Evidence::default()
        });
        assert_eq!(exhausted.strategy, Strategy::Pause);
    }

    #[test]
    fn fast_fallback_is_limited_to_replay_safe_provider_outages() {
        assert!(should_disable_fast_route(
            true,
            FailureCause::ProviderUnavailable,
            true
        ));
        assert!(!should_disable_fast_route(
            false,
            FailureCause::ProviderUnavailable,
            true
        ));
        assert!(!should_disable_fast_route(
            true,
            FailureCause::ProviderUnavailable,
            false
        ));
        assert!(!should_disable_fast_route(
            true,
            FailureCause::RateLimited,
            true
        ));
        assert!(!should_disable_fast_route(
            true,
            FailureCause::TransportInterrupted,
            true
        ));
    }

    #[test]
    fn continuation_overlap_removes_exact_repetition_only() {
        let cases: &[(&[u8], &[u8], usize)] = &[
            (b"", b"next", 0),
            (b"hello", b" world", 0),
            (b"hello world", b"world again", 5),
            (b"abcabc", b"abcX", 3),
            (b"same", b"same", 4),
            ("café".as_bytes(), "é noir".as_bytes(), 2),
            (b"almost", b"Almost", 0),
        ];
        for (existing, incoming, expected) in cases {
            assert_eq!(*expected, exact_continuation_overlap(existing, incoming));
        }
    }

    #[test]
    fn continuation_overlap_remains_exact_for_a_long_repetitive_no_match_boundary() {
        let boundary_len = 32 * 1024;
        let mut existing = vec![b'a'; boundary_len];
        let incoming = vec![b'a'; boundary_len];
        existing[boundary_len - 1] = b'b';
        assert_eq!(0, exact_continuation_overlap(&existing, &incoming));
    }

    #[test]
    fn continuation_extension_probe_never_exceeds_its_comparison_budget() {
        let boundary_len = 4096;
        let mut existing = vec![b'a'; boundary_len];
        let incoming = vec![b'a'; boundary_len - 1];
        existing[boundary_len - 1] = b'b';

        let possible = probe_continuation_extension(&existing, &incoming, incoming.len());
        assert_eq!(possible.outcome, ContinuationProbeOutcome::MayExtend);
        assert_eq!(possible.comparisons, incoming.len());

        let exhausted = probe_continuation_extension(&existing, &incoming, 1);
        assert_eq!(exhausted.outcome, ContinuationProbeOutcome::BudgetExhausted);
        assert_eq!(exhausted.comparisons, 1);
    }

    #[test]
    fn reporter_classifies_failure_causes() {
        assert_eq!(
            classify_failure("429 Too Many Requests"),
            Some(FailureCause::RateLimited)
        );
        assert_eq!(
            classify_failure("connection reset by peer"),
            Some(FailureCause::TransportInterrupted)
        );
        assert_eq!(
            classify_failure("503 service unavailable"),
            Some(FailureCause::ProviderUnavailable)
        );
        assert_eq!(
            classify_failure("401 unauthorized"),
            Some(FailureCause::Authentication)
        );
        assert_eq!(classify_failure("invalid request body"), None);
    }
}
