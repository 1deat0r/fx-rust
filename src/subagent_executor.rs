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

    let req = AgentRequest {
        prompt: Some(work.content.clone()),
        system: None,
        interactive: false,
        resume: None,
        messages: Vec::new(),
    };
    let human = crate::ui::QuietHuman;
    let outcome = crate::agent::run(req, config, &human, &session_store)
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
