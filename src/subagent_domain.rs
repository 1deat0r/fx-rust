//! Subagent domain model — faithful port of the type + validation layer of
//! upstream `core/subagent/domain.zig` (the foundation the 21-file subagent
//! subsystem builds on).
//!
//! Ported here: every constant, enum, input/command/event/receipt type, the
//! `validateCommand` tree (create/inspect/message/relationship/configure/
//! lifecycle), `validateNotificationPolicy`, id/text validation, and the
//! pure `inspectWaitSatisfied` predicate. Rust owns memory, so upstream's
//! alloc/deinit/clone ceremony collapses to plain structs + `Clone`/serde.

use serde::{Deserialize, Serialize};

// ---- bounds (upstream domain.zig) ----
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_MODEL_BYTES: usize = 256;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_ROOT_USER_EVIDENCE_BYTES: usize = 8 * 1024;
pub const MAX_CANCELLATION_REASON_BYTES: usize = 512;
pub const MAX_OPERATION_ID_BYTES: usize = 128;
pub const MAX_ADMISSION_ITEMS: usize = 256;
pub const MAX_ADMISSION_ITEM_BYTES: usize = 4096;
pub const MAX_MILESTONES: usize = 32;
pub const MAX_STOP_CONDITIONS: usize = 8;
pub const DEFAULT_PAGE_LIMIT: usize = 50;
pub const MAX_PAGE_LIMIT: usize = 100;
pub const MAX_INSPECT_WAIT_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    OneOff,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Queued,
    Running,
    AwaitingApproval,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

impl State {
    /// Whether a state is settled (terminal-ish) — used by wait predicates.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            State::Idle
                | State::Interrupted
                | State::Completed
                | State::Failed
                | State::Cancelled
                | State::Archived
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TerminalEvents {
    pub completed: bool,
    pub failed: bool,
    pub cancelled: bool,
}

impl Default for TerminalEvents {
    fn default() -> Self {
        Self {
            completed: true,
            failed: true,
            cancelled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCondition {
    Terminal,
    DurationElapsed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPolicyInput {
    pub terminal: TerminalEvents,
    pub milestones: Vec<String>,
    pub report_interval_ms: Option<u64>,
    pub report_duration_ms: Option<u64>,
    pub stop_conditions: Vec<StopCondition>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPolicy {
    pub terminal: TerminalEvents,
    pub milestones: Vec<String>,
    pub report_interval_ms: Option<u64>,
    pub report_duration_ms: Option<u64>,
    pub stop_conditions: Vec<StopCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    pub name: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: String,
    pub notifications: NotificationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectSection {
    Status,
    Messages,
    ToolActivity,
    Events,
    Configuration,
    Relationship,
}

pub const INSPECT_SECTION_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectWaitUntil {
    Settled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InspectWaitInput {
    pub until: Option<InspectWaitUntil>,
    pub after_generation: Option<u64>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectWait {
    pub until: InspectWaitUntil,
    pub after_generation: Option<u64>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipAction {
    Attach,
    Detach,
    Reparent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Cancel,
    Resume,
    Close,
    Reopen,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateInput {
    pub name: Option<String>,
    pub mode: Option<Mode>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub notifications: Option<NotificationPolicyInput>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InspectInput {
    pub id: Option<String>,
    pub sections: Vec<InspectSection>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub wait: Option<InspectWaitInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSendInput {
    pub id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMilestoneInput {
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageInput {
    pub send: Option<MessageSendInput>,
    pub milestone: Option<MessageMilestoneInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipInput {
    pub action: RelationshipAction,
    pub id: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigureInput {
    pub id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub notifications: Option<NotificationPolicyInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleInput {
    pub id: String,
    pub action: LifecycleAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandInput {
    pub create: Option<CreateInput>,
    pub inspect: Option<InspectInput>,
    pub message: Option<MessageInput>,
    pub relationship: Option<RelationshipInput>,
    pub configure: Option<ConfigureInput>,
    pub lifecycle: Option<LifecycleInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateCommand {
    pub configuration: Configuration,
    pub mode: Mode,
    pub prompt: Option<String>,
    pub permission_mode_explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectCommand {
    pub id: String,
    pub sections: Vec<InspectSection>,
    pub cursor: Option<String>,
    pub limit: usize,
    pub wait: Option<InspectWait>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageCommand {
    Send { id: String, content: String },
    Milestone { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipCommand {
    pub action: RelationshipAction,
    pub id: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigureCommand {
    pub id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub notifications: Option<NotificationPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleCommand {
    pub id: String,
    pub action: LifecycleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    Create(CreateCommand),
    Inspect(InspectCommand),
    Message(MessageCommand),
    Relationship(RelationshipCommand),
    Configure(ConfigureCommand),
    Lifecycle(LifecycleCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Pending,
    Running,
    AwaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// Validation errors (upstream `ValidationError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    InvalidBranchSelection,
    InvalidNestedBranchSelection,
    MissingName,
    MissingMode,
    MissingOneOffPrompt,
    MissingInspectId,
    InvalidId,
    InvalidName,
    InvalidModel,
    InvalidPrompt,
    InvalidMessage,
    InvalidOperationId,
    InvalidNotificationPolicy,
    DuplicateMilestone,
    DuplicateStopCondition,
    InvalidInspectSections,
    InvalidInspectWait,
    InvalidCursor,
    InvalidPageLimit,
    InvalidRelationship,
    EmptyConfiguration,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Session id validation (upstream delegates to
/// `session_layout.validateSessionId`).
pub fn validate_id(id: &str) -> Result<(), ValidationError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\');
    if !valid {
        return Err(ValidationError::InvalidId);
    }
    Ok(())
}

pub fn validate_operation_id(id: &str) -> Result<(), ValidationError> {
    if id.is_empty() || id.len() > MAX_OPERATION_ID_BYTES {
        return Err(ValidationError::InvalidOperationId);
    }
    if id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ValidationError::InvalidOperationId);
    }
    Ok(())
}

fn validate_bounded_text(
    text: &str,
    max_bytes: usize,
    invalid: ValidationError,
) -> Result<(), ValidationError> {
    if text.is_empty() || text.len() > max_bytes || text.contains('\u{0}') {
        return Err(invalid);
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<(), ValidationError> {
    validate_bounded_text(name, MAX_NAME_BYTES, ValidationError::InvalidName)
}

pub fn validate_model(model: &str) -> Result<(), ValidationError> {
    validate_bounded_text(model, MAX_MODEL_BYTES, ValidationError::InvalidModel)
}

/// Validates and owns a command (upstream `validateCommand`).
pub fn validate_command(input: &CommandInput) -> Result<Command, ValidationError> {
    let mut selected = 0usize;
    if input.create.is_some() {
        selected += 1;
    }
    if input.inspect.is_some() {
        selected += 1;
    }
    if input.message.is_some() {
        selected += 1;
    }
    if input.relationship.is_some() {
        selected += 1;
    }
    if input.configure.is_some() {
        selected += 1;
    }
    if input.lifecycle.is_some() {
        selected += 1;
    }
    if selected != 1 {
        return Err(ValidationError::InvalidBranchSelection);
    }
    if let Some(value) = &input.create {
        return validate_create(value).map(Command::Create);
    }
    if let Some(value) = &input.inspect {
        return validate_inspect(value).map(Command::Inspect);
    }
    if let Some(value) = &input.message {
        return validate_message(value).map(Command::Message);
    }
    if let Some(value) = &input.relationship {
        return validate_relationship(value).map(Command::Relationship);
    }
    if let Some(value) = &input.configure {
        return validate_configure(value).map(Command::Configure);
    }
    validate_lifecycle(input.lifecycle.as_ref().expect("exactly one branch"))
        .map(Command::Lifecycle)
}

fn validate_create(input: &CreateInput) -> Result<CreateCommand, ValidationError> {
    let name = input.name.as_deref().ok_or(ValidationError::MissingName)?;
    let mode = input.mode.ok_or(ValidationError::MissingMode)?;
    if mode == Mode::OneOff && input.prompt.is_none() {
        return Err(ValidationError::MissingOneOffPrompt);
    }
    validate_name(name)?;
    if let Some(model) = input.model.as_deref() {
        validate_model(model)?;
    }
    if let Some(prompt) = input.prompt.as_deref() {
        validate_bounded_text(prompt, MAX_PROMPT_BYTES, ValidationError::InvalidPrompt)?;
    }
    let notifications =
        validate_notification_policy(input.notifications.clone().unwrap_or_default())?;
    Ok(CreateCommand {
        configuration: Configuration {
            name: name.to_string(),
            model: input.model.clone(),
            effort: input.effort.clone(),
            permission_mode: input
                .permission_mode
                .clone()
                .unwrap_or_else(|| "yolo".to_string()),
            notifications,
        },
        mode,
        prompt: input.prompt.clone(),
        permission_mode_explicit: input.permission_mode.is_some(),
    })
}

fn validate_inspect(input: &InspectInput) -> Result<InspectCommand, ValidationError> {
    let id = input
        .id
        .as_deref()
        .ok_or(ValidationError::MissingInspectId)?;
    validate_id(id)?;
    let sections = &input.sections;
    if sections.is_empty() || sections.len() > INSPECT_SECTION_COUNT {
        return Err(ValidationError::InvalidInspectSections);
    }
    for (i, section) in sections.iter().enumerate() {
        if sections[..i].contains(section) {
            return Err(ValidationError::InvalidInspectSections);
        }
    }
    let limit = input.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        return Err(ValidationError::InvalidPageLimit);
    }
    if let Some(cursor) = input.cursor.as_deref() {
        parse_cursor(cursor)?;
    }
    let wait = match &input.wait {
        Some(requested) => {
            if input.cursor.is_some() || !sections.contains(&InspectSection::Status) {
                return Err(ValidationError::InvalidInspectWait);
            }
            let until = requested.until.ok_or(ValidationError::InvalidInspectWait)?;
            let timeout_ms = requested
                .timeout_ms
                .ok_or(ValidationError::InvalidInspectWait)?;
            if timeout_ms == 0 || timeout_ms > MAX_INSPECT_WAIT_MS {
                return Err(ValidationError::InvalidInspectWait);
            }
            Some(InspectWait {
                until,
                after_generation: requested.after_generation,
                timeout_ms,
            })
        }
        None => None,
    };
    Ok(InspectCommand {
        id: id.to_string(),
        sections: sections.clone(),
        cursor: input.cursor.clone(),
        limit,
        wait,
    })
}

/// Cursor parse (upstream `parseCursor`): a `seq:rev` revision pair, or a
/// bare sequence number; any other shape is an invalid cursor.
pub fn parse_cursor(cursor: &str) -> Result<(u64, u64), ValidationError> {
    let (seq, rev) = match cursor.split_once(':') {
        Some((s, r)) => (s, r),
        None => (cursor, "0"),
    };
    let seq: u64 = seq.parse().map_err(|_| ValidationError::InvalidCursor)?;
    let rev: u64 = rev.parse().map_err(|_| ValidationError::InvalidCursor)?;
    Ok((seq, rev))
}

/// Pure wait predicate over one authoritative inspection snapshot
/// (upstream `inspectWaitSatisfied`).
pub fn inspect_wait_satisfied(wait: &InspectWait, generation: u64, state: State) -> bool {
    if let Some(after) = wait.after_generation {
        if generation <= after {
            return false;
        }
    }
    match wait.until {
        InspectWaitUntil::Settled => state.is_settled(),
    }
}

fn validate_message(input: &MessageInput) -> Result<MessageCommand, ValidationError> {
    let selected = usize::from(input.send.is_some()) + usize::from(input.milestone.is_some());
    if selected != 1 {
        return Err(ValidationError::InvalidNestedBranchSelection);
    }
    if let Some(value) = &input.send {
        validate_id(&value.id)?;
        validate_bounded_text(
            &value.content,
            MAX_MESSAGE_BYTES,
            ValidationError::InvalidMessage,
        )?;
        return Ok(MessageCommand::Send {
            id: value.id.clone(),
            content: value.content.clone(),
        });
    }
    let milestone = input.milestone.as_ref().expect("exactly one nested branch");
    validate_name(&milestone.name)?;
    Ok(MessageCommand::Milestone {
        name: milestone.name.clone(),
    })
}

fn validate_relationship(
    input: &RelationshipInput,
) -> Result<RelationshipCommand, ValidationError> {
    validate_id(&input.id)?;
    if let Some(parent_id) = &input.parent_id {
        validate_id(parent_id)?;
    }
    match input.action {
        RelationshipAction::Attach => {}
        RelationshipAction::Detach => {
            if input.parent_id.is_some() {
                return Err(ValidationError::InvalidRelationship);
            }
        }
        RelationshipAction::Reparent => {
            if input.parent_id.is_none() {
                return Err(ValidationError::InvalidRelationship);
            }
        }
    }
    if let Some(parent_id) = &input.parent_id {
        if input.id == *parent_id {
            return Err(ValidationError::InvalidRelationship);
        }
    }
    Ok(RelationshipCommand {
        action: input.action,
        id: input.id.clone(),
        parent_id: input.parent_id.clone(),
    })
}

fn validate_configure(input: &ConfigureInput) -> Result<ConfigureCommand, ValidationError> {
    validate_id(&input.id)?;
    if input.name.is_none()
        && input.model.is_none()
        && input.effort.is_none()
        && input.permission_mode.is_none()
        && input.notifications.is_none()
    {
        return Err(ValidationError::EmptyConfiguration);
    }
    if let Some(name) = input.name.as_deref() {
        validate_name(name)?;
    }
    if let Some(model) = input.model.as_deref() {
        validate_model(model)?;
    }
    let notifications = match &input.notifications {
        Some(value) => Some(validate_notification_policy(value.clone())?),
        None => None,
    };
    Ok(ConfigureCommand {
        id: input.id.clone(),
        name: input.name.clone(),
        model: input.model.clone(),
        effort: input.effort.clone(),
        permission_mode: input.permission_mode.clone(),
        notifications,
    })
}

fn validate_lifecycle(input: &LifecycleInput) -> Result<LifecycleCommand, ValidationError> {
    validate_id(&input.id)?;
    Ok(LifecycleCommand {
        id: input.id.clone(),
        action: input.action,
    })
}

/// Validates and owns a notification policy (upstream
/// `validateNotificationPolicy`: bounded + deduplicated milestones,
/// duration stop conditions requiring a duration, implicit duration stop
/// appended when only a report duration is set).
pub fn validate_notification_policy(
    input: NotificationPolicyInput,
) -> Result<NotificationPolicy, ValidationError> {
    if input.milestones.len() > MAX_MILESTONES || input.stop_conditions.len() > MAX_STOP_CONDITIONS
    {
        return Err(ValidationError::InvalidNotificationPolicy);
    }
    if input.report_duration_ms.is_some() && input.report_interval_ms.is_none() {
        return Err(ValidationError::InvalidNotificationPolicy);
    }
    if input.report_interval_ms == Some(0) || input.report_duration_ms == Some(0) {
        return Err(ValidationError::InvalidNotificationPolicy);
    }
    for (i, name) in input.milestones.iter().enumerate() {
        validate_name(name)?;
        if input.milestones[..i].contains(name) {
            return Err(ValidationError::DuplicateMilestone);
        }
    }
    let mut has_duration_stop = false;
    for (i, condition) in input.stop_conditions.iter().enumerate() {
        if input.stop_conditions[..i].contains(condition) {
            return Err(ValidationError::DuplicateStopCondition);
        }
        if *condition == StopCondition::DurationElapsed && input.report_duration_ms.is_none() {
            return Err(ValidationError::InvalidNotificationPolicy);
        }
        has_duration_stop |= *condition == StopCondition::DurationElapsed;
    }
    let add_duration_stop = input.report_duration_ms.is_some() && !has_duration_stop;
    let stop_count = input.stop_conditions.len() + usize::from(add_duration_stop);
    if stop_count > MAX_STOP_CONDITIONS {
        return Err(ValidationError::InvalidNotificationPolicy);
    }
    let mut stop_conditions = input.stop_conditions.clone();
    if add_duration_stop {
        stop_conditions.push(StopCondition::DurationElapsed);
    }
    Ok(NotificationPolicy {
        terminal: input.terminal,
        milestones: input.milestones,
        report_interval_ms: input.report_interval_ms,
        report_duration_ms: input.report_duration_ms,
        stop_conditions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_input() -> CreateInput {
        CreateInput {
            name: Some("worker".into()),
            mode: Some(Mode::OneOff),
            prompt: Some("do the thing".into()),
            ..Default::default()
        }
    }

    #[test]
    fn command_requires_exactly_one_branch() {
        assert_eq!(
            validate_command(&CommandInput::default()),
            Err(ValidationError::InvalidBranchSelection)
        );
        let both = CommandInput {
            create: Some(create_input()),
            inspect: Some(InspectInput {
                id: Some("a".into()),
                sections: vec![InspectSection::Status],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            validate_command(&both),
            Err(ValidationError::InvalidBranchSelection)
        );
    }

    #[test]
    fn create_requires_name_mode_and_one_off_prompt() {
        let no_name = CreateInput {
            name: None,
            ..create_input()
        };
        assert_eq!(
            validate_command(&CommandInput {
                create: Some(no_name),
                ..Default::default()
            }),
            Err(ValidationError::MissingName)
        );
        let no_mode = CreateInput {
            mode: None,
            ..create_input()
        };
        assert_eq!(
            validate_command(&CommandInput {
                create: Some(no_mode),
                ..Default::default()
            }),
            Err(ValidationError::MissingMode)
        );
        let one_off_no_prompt = CreateInput {
            prompt: None,
            ..create_input()
        };
        assert_eq!(
            validate_command(&CommandInput {
                create: Some(one_off_no_prompt),
                ..Default::default()
            }),
            Err(ValidationError::MissingOneOffPrompt)
        );
        // Persistent mode does not require a prompt.
        let persistent = CreateInput {
            mode: Some(Mode::Persistent),
            prompt: None,
            ..create_input()
        };
        let cmd = validate_command(&CommandInput {
            create: Some(persistent),
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(cmd, Command::Create(_)));
    }

    #[test]
    fn inspect_validation_rules() {
        let base = InspectInput {
            id: Some("sub-1".into()),
            sections: vec![InspectSection::Status, InspectSection::Messages],
            ..Default::default()
        };
        let cmd = validate_command(&CommandInput {
            inspect: Some(base.clone()),
            ..Default::default()
        })
        .unwrap();
        let Command::Inspect(i) = cmd else {
            panic!("expected inspect")
        };
        assert_eq!(i.limit, DEFAULT_PAGE_LIMIT);

        // Duplicate sections are invalid.
        let dup = InspectInput {
            sections: vec![InspectSection::Status, InspectSection::Status],
            ..base.clone()
        };
        assert_eq!(
            validate_command(&CommandInput {
                inspect: Some(dup),
                ..Default::default()
            }),
            Err(ValidationError::InvalidInspectSections)
        );

        // Wait requires status section and no cursor.
        let wait = Some(InspectWaitInput {
            until: Some(InspectWaitUntil::Settled),
            timeout_ms: Some(1000),
            ..Default::default()
        });
        let ok_wait = InspectInput {
            wait: wait.clone(),
            ..base.clone()
        };
        assert!(validate_command(&CommandInput {
            inspect: Some(ok_wait),
            ..Default::default()
        })
        .is_ok());

        let bad_wait = InspectInput {
            cursor: Some("5:2".into()),
            sections: vec![InspectSection::Configuration],
            wait,
            ..base
        };
        assert_eq!(
            validate_command(&CommandInput {
                inspect: Some(bad_wait),
                ..Default::default()
            }),
            Err(ValidationError::InvalidInspectWait)
        );
    }

    #[test]
    fn inspect_wait_satisfied_uses_generation_and_settled_state() {
        let wait = InspectWait {
            until: InspectWaitUntil::Settled,
            after_generation: Some(10),
            timeout_ms: 1000,
        };
        assert!(!inspect_wait_satisfied(&wait, 10, State::Running));
        assert!(!inspect_wait_satisfied(&wait, 10, State::Completed));
        assert!(inspect_wait_satisfied(&wait, 11, State::Completed));
        assert!(!inspect_wait_satisfied(&wait, 11, State::Running));
        assert!(inspect_wait_satisfied(&wait, 11, State::Cancelled));
    }

    #[test]
    fn message_requires_send_or_milestone() {
        assert_eq!(
            validate_command(&CommandInput {
                message: Some(MessageInput::default()),
                ..Default::default()
            }),
            Err(ValidationError::InvalidNestedBranchSelection)
        );
        let send = MessageInput {
            send: Some(MessageSendInput {
                id: "sub-1".into(),
                content: "hello".into(),
            }),
            milestone: Some(MessageMilestoneInput { name: "m".into() }),
        };
        assert_eq!(
            validate_command(&CommandInput {
                message: Some(send),
                ..Default::default()
            }),
            Err(ValidationError::InvalidNestedBranchSelection)
        );
        let ok = MessageInput {
            send: Some(MessageSendInput {
                id: "sub-1".into(),
                content: "hello".into(),
            }),
            ..Default::default()
        };
        assert!(validate_command(&CommandInput {
            message: Some(ok),
            ..Default::default()
        })
        .is_ok());
    }

    #[test]
    fn relationship_action_rules_match_upstream() {
        // detach must not carry a parent.
        let bad = RelationshipInput {
            action: RelationshipAction::Detach,
            id: "sub-1".into(),
            parent_id: Some("p".into()),
        };
        assert_eq!(
            validate_command(&CommandInput {
                relationship: Some(bad),
                ..Default::default()
            }),
            Err(ValidationError::InvalidRelationship)
        );
        // reparent must carry a parent.
        let bad = RelationshipInput {
            action: RelationshipAction::Reparent,
            id: "sub-1".into(),
            parent_id: None,
        };
        assert_eq!(
            validate_command(&CommandInput {
                relationship: Some(bad),
                ..Default::default()
            }),
            Err(ValidationError::InvalidRelationship)
        );
        // self-parenting is invalid.
        let bad = RelationshipInput {
            action: RelationshipAction::Attach,
            id: "same".into(),
            parent_id: Some("same".into()),
        };
        assert_eq!(
            validate_command(&CommandInput {
                relationship: Some(bad),
                ..Default::default()
            }),
            Err(ValidationError::InvalidRelationship)
        );
    }

    #[test]
    fn configure_rejects_empty_changes() {
        let empty = ConfigureInput {
            id: "sub-1".into(),
            ..Default::default()
        };
        assert_eq!(
            validate_command(&CommandInput {
                configure: Some(empty),
                ..Default::default()
            }),
            Err(ValidationError::EmptyConfiguration)
        );
        let ok = ConfigureInput {
            id: "sub-1".into(),
            name: Some("renamed".into()),
            ..Default::default()
        };
        assert!(validate_command(&CommandInput {
            configure: Some(ok),
            ..Default::default()
        })
        .is_ok());
    }

    #[test]
    fn notification_policy_validation_rules() {
        // Duplicate milestones rejected.
        let dup = NotificationPolicyInput {
            milestones: vec!["a".into(), "a".into()],
            ..Default::default()
        };
        assert_eq!(
            validate_notification_policy(dup),
            Err(ValidationError::DuplicateMilestone)
        );
        // duration stop without report duration rejected.
        let missing = NotificationPolicyInput {
            stop_conditions: vec![StopCondition::DurationElapsed],
            ..Default::default()
        };
        assert_eq!(
            validate_notification_policy(missing),
            Err(ValidationError::InvalidNotificationPolicy)
        );
        // report duration without interval rejected.
        let no_interval = NotificationPolicyInput {
            report_duration_ms: Some(1000),
            ..Default::default()
        };
        assert_eq!(
            validate_notification_policy(no_interval),
            Err(ValidationError::InvalidNotificationPolicy)
        );
        // implicit duration stop appended when only report duration set.
        let implicit = NotificationPolicyInput {
            report_interval_ms: Some(100),
            report_duration_ms: Some(1000),
            ..Default::default()
        };
        let policy = validate_notification_policy(implicit).unwrap();
        assert_eq!(policy.stop_conditions, vec![StopCondition::DurationElapsed]);
    }

    #[test]
    fn ids_and_cursors_validate() {
        assert!(validate_id("sub-1").is_ok());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("..").is_err());
        assert!(validate_id("").is_err());
        assert!(validate_operation_id("op-1").is_ok());
        assert!(validate_operation_id("has space").is_err());
        assert_eq!(parse_cursor("5:2").unwrap(), (5, 2));
        assert_eq!(parse_cursor("7").unwrap(), (7, 0));
        assert!(parse_cursor("x").is_err());
    }
}
