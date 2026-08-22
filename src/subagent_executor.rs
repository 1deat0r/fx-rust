//! Subagent executor — runs pending work items through the nested agent
//! runtime and transitions the control record (upstream
//! `execution.zig`/manager work loop, 1:1 on the observable surface).
//!
//! For each invocation the executor:
//! 1. loads the child's control record,
//! 2. takes the oldest `Pending` work item, marks it `Running`
//!    (persisted before execution; a crash leaves the record reconcilable
//!    via `state_after_restart`),
//! 3. runs the nested agent on the item's prompt,
//! 4. appends a `work_transition` event with the terminal `QueueStatus`
//!    (`Completed` / `Failed`) via `append_revision`,
//! 5. moves the record to `State::Completed` / `State::Failed` and saves.
//!
//! The child runs under its own admission snapshot (already validated by the
//! caller through `capture_admission`); this module only consumes it.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::agent::{AgentRequest, FinishReason};
use crate::config::Config;
use crate::sessions::SessionStore;
use crate::subagent_control::{
    append_revision, now_ms, state_after_restart, SubagentRecord, SubagentStore, TransitionInput,
};
use crate::subagent_domain::{QueueStatus, QueuedMessage, State};

pub fn find_pending_item(record: &SubagentRecord) -> Option<&QueuedMessage> {
    record
        .queue
        .iter()
        .find(|w| w.status == QueueStatus::Pending)
}

fn transition_work(
    record: &mut SubagentRecord,
    work: &QueuedMessage,
    current: QueueStatus,
    ok: bool,
    now: i64,
) -> Result<()> {
    let previous = work.status;
    append_revision(
        record,
        &[TransitionInput {
            work_item_id: work.id.clone(),
            previous: Some(previous),
            current,
            reason: None,
        }],
        now,
    )
    .map_err(|e| anyhow::anyhow!("work transition failed: {e:?}"))?;
    record.state = if ok { State::Completed } else { State::Failed };
    if let Some(w) = record.queue.iter_mut().find(|w| w.id == work.id) {
        w.status = current;
    }
    record.updated_at_ms = now;
    Ok(())
}

/// Run the next pending work item of `child_id` and transition the record.
/// Returns a JSON summary of the execution.
pub async fn run_work_item(
    config: Arc<Config>,
    session_store: SessionStore,
    child_id: &str,
) -> Result<Value> {
    let ctrl = SubagentStore::new()?;
    let mut record = ctrl
        .load(child_id)?
        .ok_or_else(|| anyhow::anyhow!("no subagent `{child_id}`"))?;

    // Restart reconciliation: live work is never resumed implicitly.
    let reconciled = state_after_restart(record.state);
    if reconciled != record.state {
        record.state = reconciled;
    }

    let Some(work) = find_pending_item(&record).cloned() else {
        return Ok(json!({
            "ok": false,
            "child_id": child_id,
            "status": "no_pending_work",
            "state": format!("{:?}", record.state).to_lowercase(),
        }));
    };

    // Mark running before execution so a crash is reconcilable.
    let t0 = now_ms();
    if let Some(w) = record.queue.iter_mut().find(|w| w.id == work.id) {
        w.status = QueueStatus::Running;
    }
    record.state = State::Running;
    record.updated_at_ms = t0;
    ctrl.save(&record)?;

    // Child authority: the subagent runs under its own configuration —
    // optional model override, its permission mode (upstream default yolo),
    // and its admission tool filter.
    let mut child_config: Config = (*config).clone();
    if let Some(model) = record.configuration.model.clone() {
        child_config.model = model;
    }
    child_config.permission_mode =
        crate::permissions::PermissionMode::parse(&record.configuration.permission_mode)
            .unwrap_or(crate::permissions::PermissionMode::Yolo);
    if let Some(effort) = record.configuration.effort.clone() {
        if matches!(effort.as_str(), "low" | "medium" | "high") {
            child_config.reasoning_effort = Some(effort);
        } else if effort.as_str() == "none" || effort.is_empty() {
            child_config.reasoning_effort = None;
        } else {
            child_config.reasoning_effort = Some(effort);
        }
    }
    // Resume admission: when the child is run again the manager re-derives
    // authority from the live configuration (model may have changed, mode may
    // have changed); the parent's tool restriction persists across resume.
    let parent_id = record
        .parent_id
        .clone()
        .unwrap_or_else(|| "parent".to_string());
    if let Ok(resumed) = crate::subagent_authority::resume_admission(
        &record,
        &parent_id,
        &crate::operation_id::operation_id("subagent-resume"),
    ) {
        record.admission = Some(resumed.clone());
        if !resumed.tool_names.is_empty() {
            child_config.tool_filter = Some(resumed.tool_names);
        }
    } else if let Some(admission) = record.admission.clone() {
        if !admission.tool_names.is_empty() {
            child_config.tool_filter = Some(admission.tool_names);
        }
    }
    let child_config = Arc::new(child_config);

    let req = AgentRequest {
        prompt: Some(work.content.clone()),
        system: None,
        interactive: false,
        resume: None,
        messages: Vec::new(),
    };
    let human = crate::ui::QuietHuman;
    let outcome = crate::agent::run(req, child_config, &human, &session_store)
        .await
        .context("nested agent run")?;

    let success = outcome.error.is_none();
    let now = now_ms();
    let mut record = ctrl
        .load(child_id)?
        .ok_or_else(|| anyhow::anyhow!("control record disappeared: `{child_id}`"))?;
    if let Err(e) = transition_work(
        &mut record,
        &work,
        if success {
            QueueStatus::Completed
        } else {
            QueueStatus::Failed
        },
        success,
        now,
    ) {
        return Ok(json!({
            "ok": false,
            "child_id": child_id,
            "status": "transition_failed",
            "error": format!("{e:#}"),
        }));
    }
    ctrl.save(&record)?;

    // Communication: the manager delivers the run outcome to the parent
    // ledger (upstream deliver_result). Summary is the last assistant text
    // (or the error), truncated for envelope sanity.
    let summary = if let Some(err) = &outcome.error {
        format!("error: {err}")
    } else {
        outcome
            .transcript
            .iter()
            .rev()
            .find_map(|m| m.last_text())
            .unwrap_or_default()
    };
    deliver_result_to_parent(child_id, &work.id, success, summary);

    let mut result = json!({
        "ok": success,
        "child_id": child_id,
        "session_id": outcome.session_id,
        "finish_reason": finish_name(outcome.finish_reason),
        "steps": outcome.steps,
        "tool_calls": outcome.tool_calls,
        "state": format!("{:?}", record.state).to_lowercase(),
    });
    if let Some(last) = outcome.transcript.iter().rev().find_map(|m| m.last_text()) {
        result["text"] = Value::String(last);
    }
    if let Some(err) = &outcome.error {
        result["error"] = Value::String(err.clone());
    }
    Ok(result)
}

fn finish_name(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::MaxSteps => "max_steps",
        FinishReason::Error => "error",
        FinishReason::UserExit => "user_exit",
    }
}

/// Deliver the child's run outcome to its parent through the communication
/// ledger (upstream manager's deliver_result): a `Message` envelope with the
/// textual summary on success, or a `Milestone` envelope named `failed` when
/// the work item errored. Attaches the work id so the parent can correlate.
/// Failures write the communication envelope best-effort — the control
/// transition is the durable record.
pub fn deliver_result_to_parent(child_id: &str, work_id: &str, success: bool, summary: String) {
    use crate::subagent_communication::{deliver, CommunicationStore};
    use crate::subagent_control::DeliveryPayload;

    // Authoritative parent id comes from the control record.
    let parent_id = match SubagentStore::new().and_then(|store| store.load(child_id)) {
        Ok(Some(record)) => record
            .parent_id
            .clone()
            .unwrap_or_else(|| "parent".to_string()),
        _ => "parent".to_string(),
    };
    let Ok(store) = CommunicationStore::new() else {
        return;
    };
    let Ok(mut ledger) = store.load(child_id) else {
        return;
    };
    let payload = if success {
        DeliveryPayload::Message(summary)
    } else {
        DeliveryPayload::Milestone("failed".to_string())
    };
    let mut delivery = deliver(
        &mut ledger,
        child_id,
        &parent_id,
        payload,
        crate::subagent_control::now_ms(),
    );
    delivery.work_id = Some(work_id.to_string());
    delivery.operation_id = Some(crate::operation_id::operation_id("subagent-delivery"));
    if let Some(last) = ledger.deliveries.last_mut() {
        if last.sequence == delivery.sequence {
            *last = delivery;
        }
    }
    let _ = store.save(&ledger);
}
