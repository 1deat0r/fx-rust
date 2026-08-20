//! Agent loop: system prompt + context, model streaming, tool execution,
//! permission gating, auto-review, and step accounting — mirrors fx's
//! behavioral contract (Unix-shell agent, layered config, four-gate
//! permission runtime, workspace-scoped sessions).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;

use crate::config::{Config, FirstCallToolChoice};
use crate::permissions::{Decision, PermissionMode, Permissions};
use crate::providers::{
    self, ContentBlock, Message, ProviderConfig, StreamEvent, ToolUse,
};
use crate::sessions::{Session, SessionStore};
use crate::tools::{self, ToolContext};
use crate::ui::Human;

// --------------------------------------------------------------------- request
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: Option<String>,
    pub system: Option<String>,
    /// Interactive (REPL) vs one-shot (ask). Interactive keeps the session open.
    pub interactive: bool,
    /// A session id to continue (resume). When set, history is loaded from the store.
    pub resume: Option<String>,
    /// Explicit message files to append after the prompt (fx --message style).
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    MaxSteps,
    Error,
    UserExit,
}

pub struct AgentOutput {
    pub session_id: String,
    pub finish_reason: FinishReason,
    pub steps: usize,
    pub tool_calls: usize,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub transcript: Vec<Message>,
    pub error: Option<String>,
}

// --------------------------------------------------------------------- runner
pub async fn run(
    req: AgentRequest,
    config: Arc<Config>,
    human: &dyn Human,
    store: &SessionStore,
) -> Result<AgentOutput> {
    let provider = providers::resolve_provider(&config)?;

    // Session: resume vs fresh.
    let (mut transcript, session_id, mut grants_map) = match &req.resume {
        Some(id) => {
            let sess = store.load_or_error(&config.workspace, id)?;
            (sess.messages.clone(), sess.id.clone(), sess.grants.clone())
        }
        None => store.create(&config.workspace, req.interactive)?,
    };

    let mut permissions = Permissions::new(config.permission_mode, config.permission_rules.clone());
    for (tool, pattern) in grants_map.iter() {
        permissions.grants.allow(tool, pattern);
    }

    // System prompt: static guidance + workspace context + AGENTS.md.
    let mut system = String::new();
    system.push_str(include_str!("agent/system_prompt.txt"));
    system.push_str("\n\n## Workspace\n");
    system.push_str(&format!("- workspace: `{}`\n", config.workspace.display()));
    system.push_str(&format!("- today's date: {}\n", crate::util::today()));
    if let Some(extra) = &req.system {
        system.push_str(&format!("\n{extra}\n"));
    }
    let project_instructions = if config.context {
        crate::config::load_project_instructions(&config.workspace)
    } else {
        Vec::new()
    };
    if !project_instructions.is_empty() {
        system.push_str("\n## Project instructions (AGENTS.md)\n");
        system.push_str(&project_instructions.join("\n\n"));
    }
    if let Some(t) = &req.prompt {
        system.push_str(&format!("\n## User's current request\n{t}\n"));
    }

    // Seed the transcript with the user's first message.
    if transcript.is_empty() {
        if let Some(p) = req.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            transcript.push(Message::user(p));
        }
    }
    for extra in req.messages.iter().filter(|m| !m.trim().is_empty()) {
        transcript.push(Message::user(extra));
    }

    let mut steps: usize = 0;
    let mut tool_calls: usize = 0;
    let mut total_tokens: u64 = 0;
    let cost_usd: f64 = 0.0;
    let mut finish: FinishReason = FinishReason::Stop;
    let mut error: Option<String> = None;

    let ctx = ToolContext {
        workspace: config.workspace.clone(),
        max_result_bytes: config.max_tool_result_bytes,
        interactive: req.interactive,
        config: config.clone(),
    };

    let max_steps = config.max_agent_steps;

    loop {
        if max_steps > 0 && steps >= max_steps {
            finish = FinishReason::MaxSteps;
            break;
        }
        steps += 1;
        human.step_started(steps);

        let tools_schema = if steps == 1 && config.first_call_tool_choice == FirstCallToolChoice::None {
            None
        } else {
            Some(tools::schemas())
        };
        let mut stream = providers::stream(
            &provider,
            &transcript,
            tools_schema.as_deref(),
            &system,
            Some(provider_max_tokens(&provider)),
        );

        // Accumulate assistant response.
        let mut text = String::new();
        let mut calls: BTreeMap<usize, ToolUse> = BTreeMap::new();
        let mut order: Vec<usize> = Vec::new();
        use futures_util::StreamExt;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::TextDelta(s)) => {
                    text.push_str(&s);
                    human.text_delta(&s);
                }
                Ok(StreamEvent::ToolCallStart { index, id, name }) => {
                    if !calls.contains_key(&index) {
                        order.push(index);
                    }
                    human.trace_tool(name.clone());
                    calls.insert(index, ToolUse { id, name, arguments: String::new() });
                }
                Ok(StreamEvent::ToolCallArgDelta { index, delta }) => {
                    if let Some(c) = calls.get_mut(&index) {
                        c.arguments.push_str(&delta);
                    }
                }
                Ok(StreamEvent::ToolCallDone { index, id, name, input }) => {
                    let args = serde_json::to_string(&input).unwrap_or_default();
                    if let Some(c) = calls.get_mut(&index) {
                        c.id = id;
                        c.name = name;
                        c.arguments = args;
                    }
                }
                Ok(StreamEvent::Finish) => {}
                Ok(StreamEvent::Usage { input_tokens, output_tokens }) => {
                    total_tokens = total_tokens.saturating_add(
                        input_tokens.unwrap_or(0) + output_tokens.unwrap_or(0),
                    );
                }
                Err(e) => {
                    finish = FinishReason::Error;
                    error = Some(format!("stream error: {e:#}"));
                    break;
                }
            }
        }
        human.stream_done();
        if error.is_some() {
            break;
        }

        let mut call_ids = calls.keys().cloned().collect::<Vec<_>>();

        // Fallback: models/endpoints without structured tool-calling often emit
        // text `<invoke name=...>` blocks instead. Parse and execute those too.
        if call_ids.is_empty() {
            let parsed = tools::parse_text_tool_calls(&text);
            if !parsed.is_empty() {
                for (i, (tname, targs)) in parsed.into_iter().enumerate() {
                    let idx = i as usize;
                    order.push(idx);
                    calls.insert(
                        idx,
                        ToolUse {
                            id: format!("txt_{idx}"),
                            name: tname,
                            arguments: serde_json::to_string(&targs).unwrap_or_default(),
                        },
                    );
                }
                call_ids = calls.keys().cloned().collect::<Vec<_>>();
            }
        }

        if text.trim().is_empty() && call_ids.is_empty() {
            // Model said nothing — stop to avoid a tight loop.
            finish = FinishReason::Stop;
            break;
        }

        // Commit assistant turn to transcript.
        let mut assistant_blocks = Vec::new();
        if !text.trim().is_empty() {
            assistant_blocks.push(ContentBlock::Text(text.clone()));
        }
        for idx in &order {
            if let Some(c) = calls.get(idx) {
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    input: serde_json::from_str(&c.arguments).unwrap_or(serde_json::Value::Null),
                });
            }
        }
        if !assistant_blocks.is_empty() {
            transcript.push(Message {
                role: "assistant".into(),
                content: assistant_blocks,
            });
        }

        if call_ids.is_empty() {
            finish = FinishReason::Stop;
            break;
        }

        // Execute each tool call sequentially, gated by permissions.
        for idx in &order {
            let call = calls.get(idx).cloned().unwrap();
            tool_calls += 1;

            let target = tools::target_for(&call.name, &call.arguments).unwrap_or_default();
            let decision = permissions.decide(&crate::permissions::PermissionRequest {
                tool_name: &call.name,
                target: &target,
                input_text: call.arguments.clone(),
                workspace: &config.workspace,
            });

            let result = match decision {
                Decision::Allow => execute_tool(&ctx, &call).await,
                Decision::Deny(reason) => {
                    Ok(tools::err_json(format!("{name}: blocked by permission rules: {reason}", name = call.name)))
                }
                Decision::Unresolved => match config.permission_mode {
                    PermissionMode::Yolo => execute_tool(&ctx, &call).await,
                    PermissionMode::Ask if req.interactive => {
                        let allowed = human.approve(call.name.clone(), target.clone(), call.arguments.clone());
                        if allowed {
                            let pattern = grant_pattern(&target);
                            grants_map.insert(call.name.clone(), pattern.clone());
                            permissions.grants.allow(&call.name, &pattern);
                            execute_tool(&ctx, &call).await
                        } else {
                            Ok(tools::err_json(format!("{}: denied by user", call.name)))
                        }
                    }
                    PermissionMode::Ask => Ok(tools::err_json(format!(
                        "{}: blocked: permission needed but interactive approval unavailable",
                        call.name
                    ))),
                    PermissionMode::Auto => {
                        auto_review(&provider, &system, &transcript, &call, config.clone()).await
                    }
                },
            };

            let result_text = result.unwrap_or_else(tools::err_result);
            human.tool_result(&call.name, &result_text.to_string());
            transcript.push(Message::tool(call.id.clone(), result_text.to_string()));
        }

        // Persist session (grants + transcript) after each model round.
        let sess = Session {
            id: session_id.clone(),
            workspace: config.workspace.display().to_string(),
            created_ms: 0,
            updated_ms: crate::util::now_ms(),
            model: provider.model.clone(),
            mode: config.permission_mode,
            interactive: req.interactive,
            messages: transcript.clone(),
            grants: grants_map.clone(),
        };
        if let Err(e) = store.save(&sess) {
            eprintln!("[fxrs] session save failed: {e:#}");
        }
    }

    let sess = Session {
        id: session_id.clone(),
        workspace: config.workspace.display().to_string(),
        created_ms: 0,
        updated_ms: crate::util::now_ms(),
        model: provider.model.clone(),
        mode: config.permission_mode,
        interactive: req.interactive,
        messages: transcript.clone(),
        grants: grants_map.clone(),
    };
    if let Err(e) = store.save(&sess) {
        eprintln!("[fxrs] session save failed: {e:#}");
    }

    Ok(AgentOutput {
        session_id,
        finish_reason: finish,
        steps,
        tool_calls,
        total_tokens,
        cost_usd,
        transcript,
        error,
    })
}

fn grant_pattern(target: &str) -> String {
    target.to_string()
}

fn provider_max_tokens(p: &ProviderConfig) -> u32 {
    match p.provider {
        providers::ProviderKind::Gateway | providers::ProviderKind::OpenAi => 4096,
        providers::ProviderKind::Anthropic => 8192,
    }
}

async fn execute_tool(ctx: &ToolContext, call: &ToolUse) -> Result<serde_json::Value> {
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    tools::execute(ctx, &call.name, &args).await
}

/// Auto-review: unresolved sensitive call → same-provider reviewer request.
async fn auto_review(
    provider: &ProviderConfig,
    system: &str,
    transcript: &[Message],
    call: &ToolUse,
    config: Arc<Config>,
) -> Result<serde_json::Value> {
    let review_prompt = format!(
        "You are the security reviewer for a coding agent. The assistant wants to run this tool call:\n\n\
         tool: {}\narguments: {}\n\n\
         Reply with exactly one JSON object:\n{{\"allow\": true|false, \"reason\": \"...\"}}\n\
         allow=true only if the call is clearly safe within the workspace {}.\n\
         Be conservative: filesystem writes stay under the workspace, commands are non-destructive.",
        call.name,
        call.arguments,
        config.workspace.display()
    );
    let mut msgs = transcript.to_vec();
    msgs.push(Message::user(&review_prompt));

    match providers::chat(provider, &msgs, None, &format!("{system}\n\n(reviewer mode)"), Some(512)).await {
        Ok((reply, _, _)) => {
            let decision = serde_json::from_str::<serde_json::Value>(&reply)
                .ok()
                .and_then(|v| v.get("allow").and_then(|a| a.as_bool()))
                .unwrap_or(false);
            if decision {
                Ok(serde_json::json!({
                    "result": "approved by auto-review",
                    "call": call.name,
                    "auto_reviewed": true,
                }))
            } else {
                Ok(tools::err_json(format!("{}: blocked by auto-review", call.name)))
            }
        }
        Err(e) => Ok(tools::err_json(format!(
            "{}: auto-review failed ({e:#}); call blocked in non-interactive mode",
            call.name
        ))),
    }
}

#[allow(dead_code)]
fn _price(_: ()) -> f64 {
    0.0
}
