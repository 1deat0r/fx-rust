//! Terminal recovery decision model — a faithful port of upstream fx
//! `core/terminal/recovery.zig`.
//!
//! On open/resume, a durable terminal record is reconciled against evidence
//! gathered from the live environment (is the host present? is the recorded
//! process still there? is the durable screen checkpoint usable?). This module
//! turns that evidence into a single disposition decision. The model's
//! invariants, carried over from upstream:
//!
//! * never restart a session (no auto-revive),
//! * never fabricate a screen (missing/corrupt checkpoints surface as a
//!   `ScreenUnavailableReason`),
//! * isolate corrupt records instead of guessing,
//! * finalize durable termination evidence (`termination_present`) as exited
//!   even when the host is gone.

use serde::{Deserialize, Serialize};

/// Lifecycle of a terminal session (upstream `contracts.Lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    #[default]
    Running,
    Exited,
    Lost,
    Closed,
}

/// State of the durable record itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordEvidence {
    Valid,
    Missing,
    Partial,
    Corrupt,
    Unsupported,
}

/// Whether the terminal host (tmux server / native-pty host process) is
/// present, and if so whether it matches the record's host identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvidence {
    PresentSame,
    PresentForeign,
    Absent,
}

/// Whether the recorded process matches what the host currently reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEvidence {
    Matched,
    Missing,
    Mismatched,
    Unavailable,
}

/// Whether the durable screen checkpoint is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointEvidence {
    ValidContiguous,
    Missing,
    Corrupt,
    Unsupported,
    Disconnected,
    RetentionEvicted,
    ResizeUncheckpointed,
}

/// Why the terminal screen cannot be reconstructed for a resumed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenUnavailableReason {
    Missing,
    Corrupt,
    UnsupportedSchema,
    RetentionEvicted,
    RawGap,
    ResizeUncheckpointed,
}

/// What recovery should do with the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Record stays live; caller may attach and read the screen.
    RetainLive,
    /// Record is terminal (exited/lost/closed) and must be preserved as-is.
    RetainFinal,
    /// Termination evidence present — flip the record to `exited`.
    FinalizeExited,
    /// Host/process evidence is gone — mark the session lost.
    MarkLost,
    /// Durable record is corrupt/unsupported — isolate it (do not touch it).
    IsolateCorrupt,
    /// Session is genuinely unavailable right now (host same, process unknown).
    Unavailable,
}

/// Evidence inputs to the recovery decision.
#[derive(Debug, Clone, Copy)]
pub struct Input {
    pub record: RecordEvidence,
    pub lifecycle: Lifecycle,
    pub termination_present: bool,
    pub host: HostEvidence,
    pub process: ProcessEvidence,
    pub checkpoint: CheckpointEvidence,
}

/// The recovery decision for a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub disposition: Disposition,
    pub lifecycle: Lifecycle,
    pub screen_unavailable: Option<ScreenUnavailableReason>,
}

/// Reconcile durable-record evidence into a single disposition.
///
/// Mirrors upstream `recovery.zig reconcile()` decision table exactly:
///
/// | input                                          | disposition        |
/// |------------------------------------------------|--------------------|
/// | record not valid                               | isolate_corrupt    |
/// | lifecycle exited/lost/closed                   | retain_final       |
/// | termination present                            | finalize_exited    |
/// | host same + process matched                    | retain_live        |
/// | host same + process unavailable                | unavailable        |
/// | anything else (host absent/foreign, mismatch)  | mark_lost          |
pub fn reconcile(input: Input) -> Decision {
    if input.record != RecordEvidence::Valid {
        return Decision {
            disposition: Disposition::IsolateCorrupt,
            lifecycle: Lifecycle::Lost,
            screen_unavailable: record_screen_reason(input.record),
        };
    }

    let screen_unavailable = checkpoint_reason(input.checkpoint);
    if matches!(
        input.lifecycle,
        Lifecycle::Exited | Lifecycle::Lost | Lifecycle::Closed
    ) {
        return Decision {
            disposition: Disposition::RetainFinal,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    if input.termination_present {
        return Decision {
            disposition: Disposition::FinalizeExited,
            lifecycle: Lifecycle::Exited,
            screen_unavailable,
        };
    }
    if input.host == HostEvidence::PresentSame && input.process == ProcessEvidence::Matched {
        return Decision {
            disposition: Disposition::RetainLive,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    if input.host == HostEvidence::PresentSame && input.process == ProcessEvidence::Unavailable {
        return Decision {
            disposition: Disposition::Unavailable,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    Decision {
        disposition: Disposition::MarkLost,
        lifecycle: Lifecycle::Lost,
        screen_unavailable,
    }
}

fn record_screen_reason(record: RecordEvidence) -> Option<ScreenUnavailableReason> {
    match record {
        RecordEvidence::Valid | RecordEvidence::Missing => Some(ScreenUnavailableReason::Missing),
        RecordEvidence::Partial | RecordEvidence::Corrupt => Some(ScreenUnavailableReason::Corrupt),
        RecordEvidence::Unsupported => Some(ScreenUnavailableReason::UnsupportedSchema),
    }
}

fn checkpoint_reason(checkpoint: CheckpointEvidence) -> Option<ScreenUnavailableReason> {
    match checkpoint {
        CheckpointEvidence::ValidContiguous => None,
        CheckpointEvidence::Missing => Some(ScreenUnavailableReason::Missing),
        CheckpointEvidence::Corrupt => Some(ScreenUnavailableReason::Corrupt),
        CheckpointEvidence::Unsupported => Some(ScreenUnavailableReason::UnsupportedSchema),
        CheckpointEvidence::Disconnected => Some(ScreenUnavailableReason::RawGap),
        CheckpointEvidence::RetentionEvicted => Some(ScreenUnavailableReason::RetentionEvicted),
        CheckpointEvidence::ResizeUncheckpointed => {
            Some(ScreenUnavailableReason::ResizeUncheckpointed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_never_restarts_and_isolates_invalid_durable_records() {
        let evidences = [
            RecordEvidence::Missing,
            RecordEvidence::Partial,
            RecordEvidence::Corrupt,
            RecordEvidence::Unsupported,
        ];
        for record in evidences {
            let decision = reconcile(Input {
                record,
                lifecycle: Lifecycle::Running,
                termination_present: false,
                host: HostEvidence::PresentSame,
                process: ProcessEvidence::Matched,
                checkpoint: CheckpointEvidence::ValidContiguous,
            });
            assert_eq!(Disposition::IsolateCorrupt, decision.disposition);
        }
    }

    #[test]
    fn recovery_retains_only_a_matching_process_behind_the_same_host() {
        let retained = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::PresentSame,
            process: ProcessEvidence::Matched,
            checkpoint: CheckpointEvidence::ValidContiguous,
        });
        assert_eq!(Disposition::RetainLive, retained.disposition);

        let host_absent = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Matched,
            checkpoint: CheckpointEvidence::Missing,
        });
        assert_eq!(Disposition::MarkLost, host_absent.disposition);
        assert_eq!(Lifecycle::Lost, host_absent.lifecycle);

        let mismatched = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Starting,
            termination_present: false,
            host: HostEvidence::PresentSame,
            process: ProcessEvidence::Mismatched,
            checkpoint: CheckpointEvidence::Disconnected,
        });
        assert_eq!(Disposition::MarkLost, mismatched.disposition);
        assert_eq!(
            Some(ScreenUnavailableReason::RawGap),
            mismatched.screen_unavailable,
        );
    }

    #[test]
    fn recovery_finalizes_durable_termination_and_preserves_completed_records() {
        let exited = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: true,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Missing,
            checkpoint: CheckpointEvidence::RetentionEvicted,
        });
        assert_eq!(Disposition::FinalizeExited, exited.disposition);
        assert_eq!(Lifecycle::Exited, exited.lifecycle);
        assert_eq!(
            Some(ScreenUnavailableReason::RetentionEvicted),
            exited.screen_unavailable,
        );

        let closed = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Closed,
            termination_present: false,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Missing,
            checkpoint: CheckpointEvidence::Corrupt,
        });
        assert_eq!(Disposition::RetainFinal, closed.disposition);
    }

    #[test]
    fn host_same_with_unavailable_process_is_unavailable_not_lost() {
        let decision = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::PresentSame,
            process: ProcessEvidence::Unavailable,
            checkpoint: CheckpointEvidence::ValidContiguous,
        });
        assert_eq!(Disposition::Unavailable, decision.disposition);
        assert_eq!(Lifecycle::Running, decision.lifecycle);
    }

    #[test]
    fn foreign_host_is_never_attached() {
        let decision = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::PresentForeign,
            process: ProcessEvidence::Matched,
            checkpoint: CheckpointEvidence::ValidContiguous,
        });
        assert_eq!(Disposition::MarkLost, decision.disposition);
        // The checkpoint was valid — the screen is unavailable only because
        // the host is foreign (upstream returns the checkpoint reason).
        assert_eq!(None, decision.screen_unavailable);
        assert_eq!(Lifecycle::Lost, decision.lifecycle);
    }
}
