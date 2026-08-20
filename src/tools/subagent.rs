//! Minimal subagent: a tool that spawns a nested agent run in the same
//! workspace and returns the transcript tail. Upstream fx's subagent
//! system is ~26 files; this ports the smallest faithful subset: a single
//! tool the parent model can call to delegate a self-contained task.

use std::cell::Cell;
use std::sync::Arc;

use crate::agent::{AgentOutput, AgentRequest, FinishReason};
use crate::tools::err_json;
use crate::config::Config;
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
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Ok(err_json("subagent: `prompt` must not be empty".into()));
    }

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
    let output = match fut.await {
        Ok(o) => o,
        Err(e) => {
            DEPTH.with(|d| d.set(depth));
            return Ok(err_json(format!("subagent: nested run failed: {e:#}")));
        }
    };
    DEPTH.with(|d| d.set(depth));

    Ok(summarize(&output, depth))
}

/// Build the tool JSON result from a nested run.
fn summarize(output: &AgentOutput, depth: u32) -> serde_json::Value {
    let mut result = serde_json::json!({
        "subagent": true,
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
