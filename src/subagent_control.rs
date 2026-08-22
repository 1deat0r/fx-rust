//! Subagent control store + manager state machine — faithful port of
//! `work_events.zig` transitions, the `control_store` record shape (the
//! dual of `subagent`'s domain model), the `domain.nextLifecycleState`
//! transition rules, and the communication `Delivery` envelope types.
//!
//! A subagent's durable state is one [`SubagentRecord`] (upstream
//! `control_store.Record`): identity, generation, parent relationship,
//! configuration, lifecycle state, a work queue of [`QueuedMessage`]s, an
//! append-only [`Event`] log with sequence/revision accounting, and
//! operation receipts. The manager mutates records only through validated
//! commands and pure transitions; this module ships the whole decision
//! layer (no agent-runtime coupling), so it is testable standalone.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::subagent_domain::{
    Command, Configuration, Event, InspectSection, LifecycleAction, LifecycleCommand, Mode,
    NotificationPolicy, QueuedMessage, RelationshipAction, State,
};

// ---- constants (upstream work_events.zig + communication.zig bounds) ----
pub const MAX_DELIVERIES: usize = 256;
pub const MAX_CONSUMERS: usize = 16;
pub const MAX_APPROVALS: usize = 64;
pub const MAX_ACTIVE_WORK_NOTIFICATIONS: usize = 8;
pub const MAX_ACTIVE_WORK_NOTIFICATION_BYTES: usize = 48 * 1024;

/// Delivery payload kinds (upstream `DeliveryKind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Message,
    Milestone,
    Terminal,
    Interval,
    Approval,
    ToolActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivityPhase {
    Started,
    Succeeded,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolActivity {
    pub tool_name: String,
    pub phase: ToolActivityPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryPayload {
    Message(String),
    Milestone(String),
    Terminal(State),
    Interval { state: State, coalesced_ticks: u32 },
    Approval(String),
    ToolActivity(ToolActivity),
}

/// One immutable communication envelope (upstream `Delivery`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub sequence: u64,
    pub revision: u64,
    pub id: String,
    pub source_id: String,
    pub target_id: String,
    pub work_id: Option<String>,
    pub operation_id: Option<String>,
    pub timestamp_ms: i64,
    pub payload: DeliveryPayload,
}

/// Durable subagent control record (upstream `control_store.Record`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentRecord {
    pub child_id: String,
    pub generation: u64,
    pub parent_id: Option<String>,
    pub mode: Mode,
    pub configuration: Configuration,
    pub state: State,
    pub archived_from: Option<State>,
    pub queue: Vec<QueuedMessage>,
    pub events: Vec<Event>,
    pub operations: Vec<crate::subagent_domain::OperationReceipt>,
    pub next_event_sequence: u64,
    pub notification_cursor: u64,
    pub events_evicted_through: u64,
    pub queue_evicted: bool,
    pub legacy_replay_closed: bool,
    pub model_replay_floor: u64,
    pub human_replay_floor: u64,
    pub model_epoch_high: u64,
    pub human_epoch_high: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Admission snapshot captured when the child was created (authority for
    /// the child's tool filter). Optional for CLI/legacy records.
    pub admission: Option<crate::subagent_authority::AdmissionSnapshot>,
}

impl Default for SubagentRecord {
    fn default() -> Self {
        Self {
            child_id: String::new(),
            generation: 0,
            parent_id: None,
            mode: Mode::OneOff,
            configuration: Configuration {
                name: String::new(),
                model: None,
                effort: None,
                permission_mode: "yolo".into(),
                notifications: NotificationPolicy::default(),
            },
            state: State::Idle,
            archived_from: None,
            queue: Vec::new(),
            events: Vec::new(),
            operations: Vec::new(),
            next_event_sequence: 1,
            notification_cursor: 0,
            events_evicted_through: 0,
            queue_evicted: false,
            legacy_replay_closed: false,
            model_replay_floor: 0,
            human_replay_floor: 0,
            model_epoch_high: 0,
            human_epoch_high: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            admission: None,
        }
    }
}

/// Lifecycle transition rules (upstream `domain.nextLifecycleState`).
pub fn next_lifecycle_state(
    mode: Mode,
    current: State,
    action: LifecycleAction,
    has_pending_messages: bool,
    archived_from: Option<State>,
) -> Result<State, TransitionError> {
    Ok(match action {
        LifecycleAction::Cancel => match current {
            State::Queued | State::Running | State::AwaitingApproval | State::Interrupted => {
                if mode == Mode::Persistent {
                    State::Idle
                } else {
                    State::Cancelled
                }
            }
            State::Idle | State::Completed | State::Failed | State::Cancelled | State::Archived => {
                return Err(TransitionError::InvalidLifecycleTransition);
            }
        },
        LifecycleAction::Resume => match current {
            State::Interrupted => {
                if has_pending_messages {
                    State::Queued
                } else {
                    State::Idle
                }
            }
            State::Queued if has_pending_messages => State::Queued,
            State::Queued => {
                return Err(TransitionError::InvalidLifecycleTransition);
            }
            _ => return Err(TransitionError::InvalidLifecycleTransition),
        },
        LifecycleAction::Close => {
            if current == State::Archived {
                return Err(TransitionError::InvalidLifecycleTransition);
            }
            State::Archived
        }
        LifecycleAction::Reopen => {
            if current != State::Archived {
                return Err(TransitionError::InvalidLifecycleTransition);
            }
            if has_pending_messages {
                State::Queued
            } else {
                match archived_from.unwrap_or(State::Idle) {
                    State::Completed | State::Failed | State::Cancelled => {
                        archived_from.unwrap_or(State::Idle)
                    }
                    _ => State::Idle,
                }
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    InvalidLifecycleTransition,
    GenerationExhausted,
    StaleWork,
}

/// Pure restart reconciliation: live work is never resumed implicitly.
pub fn state_after_restart(state: State) -> State {
    match state {
        State::Queued | State::Running | State::AwaitingApproval => State::Interrupted,
        other => other,
    }
}

/// Work transition input (upstream `work_events.TransitionInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionInput {
    pub work_item_id: String,
    pub previous: Option<crate::subagent_domain::QueueStatus>,
    pub current: crate::subagent_domain::QueueStatus,
    pub reason: Option<String>,
}

fn find_work<'a>(queue: &'a mut [QueuedMessage], work_id: &str) -> Option<&'a mut QueuedMessage> {
    queue.iter_mut().find(|w| w.id == work_id)
}

/// Append one autonomous execution revision: bumps the generation, records
/// one `work_transition` event per transition, advances the event sequence.
pub fn append_revision(
    record: &mut SubagentRecord,
    transitions: &[TransitionInput],
    timestamp_ms: i64,
) -> Result<(), TransitionError> {
    if transitions.is_empty() {
        return Ok(());
    }
    let revision = record
        .generation
        .checked_add(1)
        .ok_or(TransitionError::GenerationExhausted)?;
    for transition in transitions {
        append_at_revision(record, revision, transition, timestamp_ms)?;
    }
    record.generation = revision;
    record.updated_at_ms = timestamp_ms;
    Ok(())
}

fn append_at_revision(
    record: &mut SubagentRecord,
    revision: u64,
    transition: &TransitionInput,
    timestamp_ms: i64,
) -> Result<(), TransitionError> {
    let sequence = record.next_event_sequence;
    record.events.push(Event {
        sequence,
        revision,
        id: transition.work_item_id.clone(),
        timestamp_ms,
        kind: crate::subagent_domain::EventKind::WorkTransition {
            work_item_id: transition.work_item_id.clone(),
            previous: transition.previous,
            current: transition.current,
            reason: transition.reason.clone(),
        },
    });
    record.next_event_sequence = sequence
        .checked_add(1)
        .ok_or(TransitionError::GenerationExhausted)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalTransition {
    Changed,
    AlreadyInState,
    CancellationWon,
    StaleWork,
}

/// Purely appends the awaiting-approval transition (upstream
/// `work_events.awaitApproval`).
pub fn await_approval(
    record: &mut SubagentRecord,
    work_id: &str,
    timestamp_ms: i64,
) -> Result<ApprovalTransition, TransitionError> {
    let Some(work) = find_work(&mut record.queue, work_id) else {
        return Ok(ApprovalTransition::StaleWork);
    };
    match work.status {
        crate::subagent_domain::QueueStatus::AwaitingApproval => {
            return Ok(ApprovalTransition::AlreadyInState);
        }
        crate::subagent_domain::QueueStatus::Cancelled => {
            return Ok(ApprovalTransition::CancellationWon);
        }
        crate::subagent_domain::QueueStatus::Running => {}
        _ => return Ok(ApprovalTransition::StaleWork),
    }
    work.status = crate::subagent_domain::QueueStatus::AwaitingApproval;
    record.state = State::AwaitingApproval;
    append_revision(
        record,
        &[TransitionInput {
            work_item_id: work_id.to_string(),
            previous: Some(crate::subagent_domain::QueueStatus::Running),
            current: crate::subagent_domain::QueueStatus::AwaitingApproval,
            reason: None,
        }],
        timestamp_ms,
    )?;
    Ok(ApprovalTransition::Changed)
}

/// Pure idempotent resume transition (upstream `work_events.resumeApproval`).
pub fn resume_approval(
    record: &mut SubagentRecord,
    work_id: &str,
    timestamp_ms: i64,
) -> Result<ApprovalTransition, TransitionError> {
    let Some(work) = find_work(&mut record.queue, work_id) else {
        return Ok(ApprovalTransition::StaleWork);
    };
    match work.status {
        crate::subagent_domain::QueueStatus::Running => {
            return Ok(ApprovalTransition::AlreadyInState);
        }
        crate::subagent_domain::QueueStatus::Cancelled => {
            return Ok(ApprovalTransition::CancellationWon);
        }
        crate::subagent_domain::QueueStatus::AwaitingApproval => {}
        _ => return Ok(ApprovalTransition::StaleWork),
    }
    work.status = crate::subagent_domain::QueueStatus::Running;
    record.state = State::Running;
    append_revision(
        record,
        &[TransitionInput {
            work_item_id: work_id.to_string(),
            previous: Some(crate::subagent_domain::QueueStatus::AwaitingApproval),
            current: crate::subagent_domain::QueueStatus::Running,
            reason: None,
        }],
        timestamp_ms,
    )?;
    Ok(ApprovalTransition::Changed)
}

// ---- manager command application ----

/// Outcome of applying a lifecycle command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleOutcome {
    pub previous: State,
    pub current: State,
}

/// Outcome of a message command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageOutcome {
    Queued {
        message_id: String,
        sequence: u64,
    },
    Milestone {
        operation_id: String,
        event_sequence: u64,
    },
}

/// Apply a validated [`Command`] to a record (manager-level pure effect
/// without the agent runtime). Returns a description of what happened.
pub fn apply_command(
    record: &mut SubagentRecord,
    command: &Command,
    timestamp_ms: i64,
) -> Result<CommandOutcome, anyhow::Error> {
    match command {
        Command::Create(cmd) => apply_create(record, cmd, timestamp_ms),
        Command::Configure(cmd) => {
            apply_configure(record, cmd);
            Ok(CommandOutcome::Configured)
        }
        Command::Relationship(cmd) => {
            apply_relationship(record, cmd)?;
            Ok(CommandOutcome::RelationshipChanged {
                parent_id: record.parent_id.clone(),
            })
        }
        Command::Lifecycle(cmd) => {
            let outcome = apply_lifecycle(record, cmd)?;
            Ok(CommandOutcome::LifecycleChanged(outcome))
        }
        Command::Message(msg) => Ok(CommandOutcome::Message(apply_message(
            record,
            msg,
            timestamp_ms,
        )?)),
        Command::Inspect(_inspect) => {
            // Inspection is read-only; handled by `inspect_record`.
            Ok(CommandOutcome::Inspected)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandOutcome {
    Created,
    Configured,
    RelationshipChanged { parent_id: Option<String> },
    LifecycleChanged(LifecycleOutcome),
    Message(MessageOutcome),
    Inspected,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn apply_create(
    record: &mut SubagentRecord,
    cmd: &crate::subagent_domain::CreateCommand,
    timestamp_ms: i64,
) -> Result<CommandOutcome, anyhow::Error> {
    if record.created_at_ms != 0 || !record.events.is_empty() {
        anyhow::bail!("subagent `{}` already exists", record.child_id);
    }
    record.configuration = cmd.configuration.clone();
    record.mode = cmd.mode;
    if let Some(prompt) = &cmd.prompt {
        record.queue.push(QueuedMessage {
            id: format!("w-{}", prompt.len()),
            source_id: record.child_id.clone(),
            content: prompt.clone(),
            root_user_intent_context: String::new(),
            root_user_messages: Vec::new(),
            root_user_evidence_complete: false,
            status: crate::subagent_domain::QueueStatus::Pending,
            cancellation_reason: None,
            created_at_ms: timestamp_ms,
        });
    }
    record.state = State::Queued;
    record.created_at_ms = timestamp_ms;
    record.updated_at_ms = timestamp_ms;
    record.events.push(Event {
        sequence: record.next_event_sequence,
        revision: 0,
        id: record.child_id.clone(),
        timestamp_ms,
        kind: crate::subagent_domain::EventKind::Created,
    });
    record.next_event_sequence += 1;
    Ok(CommandOutcome::Created)
}

fn apply_configure(record: &mut SubagentRecord, cmd: &crate::subagent_domain::ConfigureCommand) {
    if let Some(name) = &cmd.name {
        record.configuration.name = name.clone();
    }
    if let Some(model) = &cmd.model {
        record.configuration.model = Some(model.clone());
    }
    if let Some(effort) = &cmd.effort {
        record.configuration.effort = Some(effort.clone());
    }
    if let Some(permission_mode) = &cmd.permission_mode {
        record.configuration.permission_mode = permission_mode.clone();
    }
    if let Some(notifications) = &cmd.notifications {
        record.configuration.notifications = notifications.clone();
    }
    record.updated_at_ms = now_ms();
}

fn apply_relationship(
    record: &mut SubagentRecord,
    cmd: &crate::subagent_domain::RelationshipCommand,
) -> Result<(), anyhow::Error> {
    match cmd.action {
        RelationshipAction::Attach => {
            record.parent_id = cmd.parent_id.clone();
        }
        RelationshipAction::Detach => {
            record.parent_id = None;
        }
        RelationshipAction::Reparent => {
            record.parent_id = cmd.parent_id.clone();
        }
    }
    record.updated_at_ms = now_ms();
    Ok(())
}

fn apply_lifecycle(
    record: &mut SubagentRecord,
    cmd: &LifecycleCommand,
) -> Result<LifecycleOutcome, anyhow::Error> {
    let previous = record.state;
    let has_pending = record
        .queue
        .iter()
        .any(|w| w.status == crate::subagent_domain::QueueStatus::Pending);
    let next = next_lifecycle_state(
        record.mode,
        previous,
        cmd.action,
        has_pending,
        record.archived_from,
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "invalid lifecycle transition {:?} from {:?}",
            cmd.action,
            previous
        )
    })?;
    if next == State::Archived && previous != State::Archived {
        record.archived_from = Some(previous);
    }
    record.state = next;
    record.updated_at_ms = now_ms();
    let timestamp = now_ms();
    record.events.push(Event {
        sequence: record.next_event_sequence,
        revision: record.generation,
        id: cmd.id.clone(),
        timestamp_ms: timestamp,
        kind: crate::subagent_domain::EventKind::LifecycleChanged {
            previous,
            current: next,
        },
    });
    record.next_event_sequence += 1;
    Ok(LifecycleOutcome {
        previous,
        current: next,
    })
}

fn apply_message(
    record: &mut SubagentRecord,
    msg: &crate::subagent_domain::MessageCommand,
    _timestamp_ms: i64,
) -> Result<MessageOutcome, anyhow::Error> {
    let timestamp = now_ms();
    match msg {
        crate::subagent_domain::MessageCommand::Send { id, content } => {
            let sequence = record.next_event_sequence;
            let message_id = format!("m-{sequence}");
            record.queue.push(QueuedMessage {
                id: message_id.clone(),
                source_id: id.clone(),
                content: content.clone(),
                root_user_intent_context: String::new(),
                root_user_messages: Vec::new(),
                root_user_evidence_complete: false,
                status: crate::subagent_domain::QueueStatus::Pending,
                cancellation_reason: None,
                created_at_ms: timestamp,
            });
            record.events.push(Event {
                sequence,
                revision: record.generation,
                id: message_id.clone(),
                timestamp_ms: timestamp,
                kind: crate::subagent_domain::EventKind::MessageQueued {
                    message_id: message_id.clone(),
                },
            });
            record.next_event_sequence = sequence + 1;
            Ok(MessageOutcome::Queued {
                message_id,
                sequence,
            })
        }
        crate::subagent_domain::MessageCommand::Milestone { name } => {
            let sequence = record.next_event_sequence;
            let operation_id = format!("op-{sequence}");
            record.events.push(Event {
                sequence,
                revision: record.generation,
                id: operation_id.clone(),
                timestamp_ms: timestamp,
                kind: crate::subagent_domain::EventKind::MilestoneEmitted {
                    operation_id: operation_id.clone(),
                    source_child_id: record.child_id.clone(),
                    target_parent_id: record.parent_id.clone().unwrap_or_default(),
                    work_item_id: record.child_id.clone(),
                    name: name.clone(),
                },
            });
            record.next_event_sequence = sequence + 1;
            Ok(MessageOutcome::Milestone {
                operation_id,
                event_sequence: sequence,
            })
        }
    }
}

/// Inspection snapshot for the read-only inspect section (upstream
/// `domain.InspectSection` semantics).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub child_id: String,
    pub state: State,
    pub generation: u64,
    pub parent_id: Option<String>,
    pub mode: Mode,
    pub configuration: Configuration,
    pub queue: Vec<QueuedMessage>,
    pub events: Vec<Event>,
    pub updated_at_ms: i64,
}

pub fn inspect_record(record: &SubagentRecord, _sections: &[InspectSection]) -> Inspection {
    Inspection {
        child_id: record.child_id.clone(),
        state: record.state,
        generation: record.generation,
        parent_id: record.parent_id.clone(),
        mode: record.mode,
        configuration: record.configuration.clone(),
        queue: record.queue.clone(),
        events: {
            // Respect event eviction: only events after the eviction point.
            record
                .events
                .iter()
                .filter(|e| e.sequence > record.events_evicted_through)
                .cloned()
                .collect()
        },
        updated_at_ms: record.updated_at_ms,
    }
}

// ---- store ----

pub fn store_root() -> PathBuf {
    crate::config::fx_home().join("subagents")
}

impl SubagentRecord {
    pub fn id(&self) -> &str {
        &self.child_id
    }
}

pub struct SubagentStore {
    root: PathBuf,
}

impl SubagentStore {
    pub fn new() -> Result<Self> {
        let root = store_root();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self, child_id: &str) -> PathBuf {
        self.root.join(format!("{child_id}.json"))
    }

    pub fn load(&self, child_id: &str) -> Result<Option<SubagentRecord>> {
        let path = self.path(child_id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let record: SubagentRecord =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        if record.child_id != child_id {
            anyhow::bail!(
                "control record identity mismatch: file {child_id}, record {}",
                record.child_id
            );
        }
        Ok(Some(record))
    }

    pub fn load_or_create(&self, child_id: &str) -> Result<SubagentRecord> {
        Ok(self.load(child_id)?.unwrap_or_else(|| SubagentRecord {
            child_id: child_id.to_string(),
            ..Default::default()
        }))
    }

    pub fn save(&self, record: &SubagentRecord) -> Result<()> {
        let path = self.path(record.child_id.as_str());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(record)?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn delete(&self, child_id: &str) -> Result<()> {
        let path = self.path(child_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    pub fn list(&self) -> Vec<SubagentRecord> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let id = name.trim_end_matches(".json").to_string();
            if let Ok(Some(record)) = self.load(&id) {
                out.push(record);
            }
        }
        out.sort_by_key(|a| a.created_at_ms);
        out
    }

    /// Create a new subagent record via the validated create command.
    pub fn create(
        &self,
        child_id: &str,
        cmd: &crate::subagent_domain::CreateCommand,
    ) -> Result<SubagentRecord> {
        if self.load(child_id)?.is_some() {
            anyhow::bail!("subagent `{child_id}` already exists");
        }
        let mut record = SubagentRecord {
            child_id: child_id.to_string(),
            ..Default::default()
        };
        apply_command(
            &mut record,
            &crate::subagent_domain::Command::Create(cmd.clone()),
            now_ms(),
        )
        .context("creating subagent record")?;
        self.save(&record)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_domain::{CreateCommand, CreateInput, MessageCommand};
    use crate::test_env::with;

    fn create_cmd(name: &str, prompt: &str) -> CreateCommand {
        let input = crate::subagent_domain::CommandInput {
            create: Some(CreateInput {
                name: Some(name.into()),
                mode: Some(Mode::OneOff),
                prompt: Some(prompt.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        match crate::subagent_domain::validate_command(&input) {
            Ok(Command::Create(cmd)) => cmd,
            _ => panic!("valid create"),
        }
    }

    #[test]
    fn lifecycle_transitions_follow_upstream_rules() {
        // cancel: one-off running -> cancelled.
        assert_eq!(
            next_lifecycle_state(
                Mode::OneOff,
                State::Running,
                LifecycleAction::Cancel,
                false,
                None
            ),
            Ok(State::Cancelled)
        );
        // cancel: persistent running -> idle.
        assert_eq!(
            next_lifecycle_state(
                Mode::Persistent,
                State::Running,
                LifecycleAction::Cancel,
                false,
                None
            ),
            Ok(State::Idle)
        );
        // resume: interrupted with pending -> queued.
        assert_eq!(
            next_lifecycle_state(
                Mode::Persistent,
                State::Interrupted,
                LifecycleAction::Resume,
                true,
                None
            ),
            Ok(State::Queued)
        );
        // resume: interrupted without pending -> idle.
        assert_eq!(
            next_lifecycle_state(
                Mode::Persistent,
                State::Interrupted,
                LifecycleAction::Resume,
                false,
                None
            ),
            Ok(State::Idle)
        );
        // resume from idle is invalid.
        assert!(matches!(
            next_lifecycle_state(
                Mode::Persistent,
                State::Idle,
                LifecycleAction::Resume,
                false,
                None
            ),
            Err(TransitionError::InvalidLifecycleTransition)
        ));
        // close archives; reopen restores terminal from archived_from.
        assert_eq!(
            next_lifecycle_state(
                Mode::OneOff,
                State::Completed,
                LifecycleAction::Close,
                false,
                None
            ),
            Ok(State::Archived)
        );
        assert_eq!(
            next_lifecycle_state(
                Mode::OneOff,
                State::Archived,
                LifecycleAction::Reopen,
                false,
                Some(State::Completed)
            ),
            Ok(State::Completed)
        );
        // reopen requires archived.
        assert!(matches!(
            next_lifecycle_state(
                Mode::OneOff,
                State::Idle,
                LifecycleAction::Reopen,
                false,
                None
            ),
            Err(TransitionError::InvalidLifecycleTransition)
        ));
    }

    #[test]
    fn restart_reconciles_live_work_to_interrupted() {
        assert_eq!(state_after_restart(State::Running), State::Interrupted);
        assert_eq!(state_after_restart(State::Queued), State::Interrupted);
        assert_eq!(state_after_restart(State::Completed), State::Completed);
    }

    #[test]
    fn approval_transitions_are_idempotent_and_cancellation_wins() {
        let mut record = SubagentRecord {
            child_id: "sub-1".into(),
            state: State::Running,
            queue: vec![QueuedMessage {
                id: "w-1".into(),
                source_id: "sub-1".into(),
                content: "x".into(),
                status: crate::subagent_domain::QueueStatus::Running,
                created_at_ms: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let t = 100i64;
        assert_eq!(
            await_approval(&mut record, "w-1", t).unwrap(),
            ApprovalTransition::Changed
        );
        assert_eq!(record.state, State::AwaitingApproval);
        assert_eq!(
            record.queue[0].status,
            crate::subagent_domain::QueueStatus::AwaitingApproval
        );
        assert_eq!(record.generation, 1);
        assert_eq!(record.next_event_sequence, 2);
        // Idempotent second attempt.
        assert_eq!(
            await_approval(&mut record, "w-1", t + 1).unwrap(),
            ApprovalTransition::AlreadyInState
        );
        assert_eq!(record.generation, 1, "no new event when already in state");

        assert_eq!(
            resume_approval(&mut record, "w-1", t + 2).unwrap(),
            ApprovalTransition::Changed
        );
        assert_eq!(record.state, State::Running);
    }

    #[test]
    fn cancel_wins_over_late_approval() {
        let mut record = SubagentRecord {
            child_id: "sub-1".into(),
            queue: vec![QueuedMessage {
                id: "w-1".into(),
                source_id: "sub-1".into(),
                content: "x".into(),
                status: crate::subagent_domain::QueueStatus::Cancelled,
                created_at_ms: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            await_approval(&mut record, "w-1", 10).unwrap(),
            ApprovalTransition::CancellationWon
        );
    }

    #[test]
    fn create_then_message_then_lifecycle_roundtrip() {
        with(|| {
            let home = std::env::temp_dir().join(format!("fxrs-sub-{}", std::process::id()));
            std::env::set_var("FX_HOME", &home);
            let store = SubagentStore::new().unwrap();
            let cmd = create_cmd("worker", "do work");
            let record = store.create("sub-abc", &cmd).unwrap();
            assert_eq!(record.state, State::Queued);
            assert_eq!(record.parent_id, None);
            assert_eq!(record.queue.len(), 1);

            // message send.
            let mut record = record;
            let outcome = apply_command(
                &mut record,
                &Command::Message(MessageCommand::Send {
                    id: "parent-1".into(),
                    content: "please also do X".into(),
                }),
                200,
            )
            .unwrap();
            assert!(matches!(
                outcome,
                CommandOutcome::Message(MessageOutcome::Queued { .. })
            ));
            assert_eq!(record.queue.len(), 2);

            // lifecycle cancel -> cancelled (one-off).
            apply_command(
                &mut record,
                &Command::Lifecycle(LifecycleCommand {
                    id: "sub-abc".into(),
                    action: LifecycleAction::Cancel,
                }),
                300,
            )
            .unwrap();
            assert_eq!(record.state, State::Cancelled);

            store.save(&record).unwrap();
            let loaded = store.load("sub-abc").unwrap().unwrap();
            assert_eq!(loaded.state, State::Cancelled);
            assert_eq!(loaded.queue.len(), 2);
            assert_eq!(loaded.generation, 0);

            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn store_list_and_delete() {
        with(|| {
            let home = std::env::temp_dir().join(format!("fxrs-sub2-{}", std::process::id()));
            std::env::set_var("FX_HOME", &home);
            let store = SubagentStore::new().unwrap();
            let cmd = create_cmd("a", "p");
            store.create("sub-a", &cmd).unwrap();
            store.create("sub-b", &cmd).unwrap();
            assert_eq!(store.list().len(), 2);
            store.delete("sub-a").unwrap();
            assert_eq!(store.list().len(), 1);
            let _ = std::fs::remove_dir_all(&home);
        });
    }
}
