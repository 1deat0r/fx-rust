//! Minimal subagent: a tool that spawns a nested agent run in the same
//! workspace and returns the transcript tail. Upstream fx's subagent
//! system is ~26 files; this ports the smallest faithful subset: a single
//! tool the parent model can call to delegate a self-contained task.

use std::cell::Cell;
use std::sync::Arc;

use crate::agent::{AgentOutput, AgentRequest, FinishReason};
use crate::config::Config;
use crate::tools::err_json;
use crate::ui::QuietHuman;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

const MAX_DEPTH: u32 = 3;

pub async fn run_subagent(
    config: Arc<Config>,
    store: crate::sessions::SessionStore,
    prompt: String,
    model: Option<String>,
) -> Result<serde_json::Value, anyhow::Error> {
    run_subagent_named(config, store, prompt, model, None, None).await
}

/// `run_subagent` extended with domain-model validation (upstream
/// subagent domain: create requires a name, mode, and a one-off prompt).
pub async fn run_subagent_named(
    config: Arc<Config>,
    store: crate::sessions::SessionStore,
    prompt: String,
    model: Option<String>,
    name: Option<String>,
    permission_mode: Option<String>,
) -> Result<serde_json::Value, anyhow::Error> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Ok(err_json("subagent: `prompt` must not be empty".into()));
    }
    // Validate the create bundle through the ported subagent domain model.
    let create = crate::subagent_domain::CreateInput {
        name: name.clone(),
        mode: Some(crate::subagent_domain::Mode::OneOff),
        prompt: Some(prompt.clone()),
        model: model.clone(),
        permission_mode,
        ..Default::default()
    };
    let validated =
        crate::subagent_domain::validate_command(&crate::subagent_domain::CommandInput {
            create: Some(create),
            ..Default::default()
        });
    let (subagent_name, create_cmd) = match validated {
        Ok(crate::subagent_domain::Command::Create(cmd)) => (cmd.configuration.name.clone(), cmd),
        Err(e) => {
            return Ok(err_json(format!("subagent: {e}")));
        }
        Ok(_) => unreachable!("create validates to create"),
    };

    // Durable control record: created before the run, transitioned to
    // running, then completed/failed (upstream manager + work_events).
    let child_id = format!("sub-{}", crate::util::now_ms() as usize);
    let ctrl_store = match crate::subagent_control::SubagentStore::new() {
        Ok(s) => s,
        Err(e) => {
            return Ok(err_json(format!(
                "subagent: control store unavailable: {e:#}"
            )));
        }
    };
    let mut control = match ctrl_store.create(&child_id, &create_cmd) {
        Ok(r) => r,
        Err(e) => {
            return Ok(err_json(format!(
                "subagent: create control record failed: {e:#}"
            )));
        }
    };

    let depth = DEPTH.with(|d| d.get());
    if depth >= MAX_DEPTH {
        return Ok(err_json(format!(
            "subagent: nesting depth exceeded (max {MAX_DEPTH}); delegate less, or parent should finish its own work"
        )));
    }

    // Sub-agent config: same workspace, optional model override.
    let mut sub_config: Config = (*config).clone();
    if let Some(m) = model {
        sub_config.model = m;
    }
    let sub_config = Arc::new(sub_config);

    let req = AgentRequest {
        prompt: Some(prompt),
        system: None,
        interactive: false,
        resume: None,
        messages: Vec::new(),
    };

    DEPTH.with(|d| d.set(depth + 1));
    // Type-erase the nested future at the recursion boundary
    // (agent::run -> execute_tool -> subagent -> agent::run) so the type
    // checker never sees an infinitely-nested future. No Send required:
    // nesting is depth-capped and serialized within the parent loop.
    let fut: futures_util::future::LocalBoxFuture<
        '_,
        Result<crate::agent::AgentOutput, anyhow::Error>,
    > = Box::pin(async move {
        let human = QuietHuman;
        crate::agent::run(req, sub_config, &human, &store).await
    });
    // Mark the work item running before execution (quietly; the record is
    // best-effort bookkeeping and must never break the nested run).
    let base = match ctrl_store.load(&child_id) {
        Ok(Some(r)) => r,
        _ => control.clone(),
    };
    control = base;
    let t0 = crate::subagent_control::now_ms();
    if let Some(work) = control.queue.first_mut() {
        work.status = crate::subagent_domain::QueueStatus::Running;
        control.state = crate::subagent_domain::State::Running;
        control.updated_at_ms = t0;
        let _ = ctrl_store.save(&control);
    }

    let output = match fut.await {
        Ok(o) => o,
        Err(e) => {
            DEPTH.with(|d| d.set(depth));
            let _ = finish_control(&ctrl_store, &mut control, &child_id, t0, false);
            return Ok(err_json(format!("subagent: nested run failed: {e:#}")));
        }
    };
    DEPTH.with(|d| d.set(depth));
    let _ = finish_control(
        &ctrl_store,
        &mut control,
        &child_id,
        t0,
        output.error.is_none(),
    );

    Ok(summarize(&output, depth, &subagent_name))
}

/// Build the tool JSON result from a nested run.
fn summarize(output: &AgentOutput, depth: u32, name: &str) -> serde_json::Value {
    let mut result = serde_json::json!({
        "subagent": true,
        "name": name,
        "session_id": output.session_id,
        "finish_reason": finish_name(output.finish_reason),
        "steps": output.steps,
        "tool_calls": output.tool_calls,
        "error": output.error,
    });
    if let Some(text) = output.transcript.iter().rev().find_map(|m| m.last_text()) {
        result["text"] = serde_json::Value::String(text);
    }
    result["depth"] = serde_json::json!(depth);
    result
}

fn finish_name(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::MaxSteps => "max_steps",
        FinishReason::Error => "error",
        FinishReason::UserExit => "user_exit",
    }
}

/// Transition one subagent control record to its terminal state with a
/// work-transition event (upstream work_events.appendRevision). Best-effort:
/// failures must not surface to the parent model.
fn finish_control(
    store: &crate::subagent_control::SubagentStore,
    control: &mut crate::subagent_control::SubagentRecord,
    _child_id: &str,
    started_ms: i64,
    ok: bool,
) -> Result<(), anyhow::Error> {
    let now = crate::subagent_control::now_ms();
    let target = if ok {
        crate::subagent_domain::QueueStatus::Completed
    } else {
        crate::subagent_domain::QueueStatus::Failed
    };
    let transition = control
        .queue
        .first()
        .map(|work| crate::subagent_control::TransitionInput {
            work_item_id: work.id.clone(),
            previous: Some(work.status),
            current: target,
            reason: None,
        });
    if let Some(work) = control.queue.first_mut() {
        work.status = target;
    }
    if let Some(t) = transition {
        crate::subagent_control::append_revision(control, &[t], now)
            .map_err(|e| anyhow::anyhow!("append_revision failed: {e:?}"))?;
    }
    control.state = match target {
        crate::subagent_domain::QueueStatus::Completed => crate::subagent_domain::State::Completed,
        _ => crate::subagent_domain::State::Failed,
    };
    control.updated_at_ms = now;
    let _ = started_ms;
    store.save(control)
}
