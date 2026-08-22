//! Agent loop: system prompt + context, model streaming, tool execution,
//! permission gating, auto-review, and step accounting — mirrors fx's
//! behavioral contract (Unix-shell agent, layered config, four-gate
//! permission runtime, workspace-scoped sessions).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::config::{Config, FirstCallToolChoice};
use crate::permissions::{Decision, PermissionMode, Permissions};
use crate::providers::{self, ContentBlock, Message, ProviderConfig, StreamEvent, ToolUse};
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

    let mut permissions = Permissions::new(
        config.effective_permission_mode(),
        config.permission_rules.clone(),
    );
    for (tool, pattern) in grants_map.iter() {
        permissions.grants.allow(tool, pattern);
    }
    // Path sandbox: workspace plus configured additional directories.
    let sandbox = crate::permissions::Sandbox {
        mode: config.sandbox,
        workspace: config.workspace.clone(),
        additional: config.additional_directories.clone(),
    };

    // System prompt: static guidance + workspace context + AGENTS.md.
    // Base system prompt (static): exec memory is appended per-turn below,
    // NOT accumulated across turns.
    let mut system_base = String::from(include_str!("agent/system_prompt.txt"));
    system_base.push_str("\n\n## Workspace\n");
    system_base.push_str(&format!("- workspace: `{}`\n", config.workspace.display()));
    system_base.push_str(&format!("- today's date: {}\n", crate::util::today()));
    if let Some(extra) = &req.system {
        system_base.push_str(&format!("\n{extra}\n"));
    }
    let project_instructions = if config.context {
        crate::config::load_project_instructions(&config.workspace)
    } else {
        Vec::new()
    };
    if !project_instructions.is_empty() {
        system_base.push_str("\n## Project instructions (AGENTS.md)\n");
        system_base.push_str(&project_instructions.join("\n\n"));
    }
    if let Some(t) = &req.prompt {
        system_base.push_str(&format!("\n## User's current request\n{t}\n"));
    }

    // MCP discovery happens once per run: server availability (for the
    // model catalog) plus the flattened tool list published to the model.
    let mcp_discovery = crate::mcp::discover(&config.mcp_servers);
    if !mcp_discovery.states.is_empty() {
        system_base.push_str(
            "

## MCP servers
",
        );
        system_base.push_str(&crate::model_catalog::render_prompt_section(&mcp_discovery));
    }

    // Skills: advertise the discovered `<available_skills>` catalog so the
    // model can load a skill when a task matches its description
    // (core/skills/skill_runtime built prompt section).
    let skill_catalog = crate::skills::discover(&config.workspace);
    if !skill_catalog.skills.is_empty() {
        let (section, _notice) = crate::skills::build_prompt_section(
            &skill_catalog.skills,
            crate::skills::SKILL_DESCRIPTION_BYTES_DEFAULT,
            crate::skills::SKILL_CATALOG_BYTES_DEFAULT,
        );
        system_base.push_str(&section);
        // If the user prompt explicitly references a discovered skill, load
        // its content into the prompt (core/skills/skill_invocation
        // buildExplicitPromptSection): sigil (/name, $name) or natural
        // language "use/run ... skill" references, deduped by path.
        if let Some(prompt) = req.prompt.as_deref() {
            let matched = crate::skills::invocation::match_explicit_skill_indices(
                prompt,
                &skill_catalog.skills,
            );
            if !matched.is_empty() {
                let bindings: Vec<(String, String)> = Vec::new();
                let (section, _notice) = crate::skills::invocation::build_explicit_prompt_section(
                    &skill_catalog,
                    prompt,
                    &bindings,
                    32 * 1024,
                );
                if !section.is_empty() {
                    system_base.push('\n');
                    system_base.push_str(&section);
                }
            }
        }
    }

    // Prompt history: append user prompts to ~/.fx/history.jsonl.
    if let Some(p) = req.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        let _ =
            crate::history::HistoryStore::new().record(&config.workspace.display().to_string(), p);
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
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let cost_usd: f64 = 0.0;
    let mut finish: FinishReason = FinishReason::Stop;
    let mut error: Option<String> = None;

    let ctx = ToolContext {
        workspace: config.workspace.clone(),
        max_result_bytes: config.max_tool_result_bytes,
        interactive: req.interactive,
        config: config.clone(),
        store: store.clone(),
        session_id: session_id.clone(),
    };

    let max_steps = config.max_agent_steps;

    // Publish connected MCP server tools to the model (once per agent run).
    let mcp_tools = mcp_discovery.tools;

    // Execution memory: a bounded record of executed tool calls, replayed
    // into the system prompt each round (fx execution_memory).
    let mut exec_memory = crate::exec_memory::ExecMemory::default();
    // Replay tape: per-session JSONL of every executed tool call.
    let tape = crate::tape::TapeStore::for_session(&config.workspace, &session_id);

    loop {
        if max_steps > 0 && steps >= max_steps {
            finish = FinishReason::MaxSteps;
            break;
        }
        // Context accounting: warn at the soft ceiling, stop at the hard one.
        let ctx_limits = config.context_limits;
        let estimated_ctx = crate::context::estimate_messages(&transcript);
        if estimated_ctx > ctx_limits.max_tokens {
            finish = FinishReason::Error;
            error = Some(format!(
                "context limit exceeded (~{}k est. tokens > {}k max); start a new session or resume with a shorter history",
                estimated_ctx / 1000,
                ctx_limits.max_tokens / 1000
            ));
            eprintln!(
                "[fxrs] \x1b[33mcontext limit exceeded\x1b[0m: ~{}k est. tokens > limit {}k",
                estimated_ctx / 1000,
                ctx_limits.max_tokens / 1000,
            );
            break;
        }
        if estimated_ctx > ctx_limits.warn_at_tokens {
            eprintln!(
                "[fxrs] \x1b[33mwarning\x1b[0m: context ~{}k est. tokens (warn at {}k, hard stop {}k)",
                estimated_ctx / 1000,
                ctx_limits.warn_at_tokens / 1000,
                ctx_limits.max_tokens / 1000,
            );
        }
        steps += 1;
        human.step_started(steps);

        let tools_schema =
            if steps == 1 && config.first_call_tool_choice == FirstCallToolChoice::None {
                None
            } else if mcp_tools.is_empty() {
                Some(tools::schemas_filtered(config.tool_filter.as_deref()))
            } else {
                Some(filter_mcp_schemas(
                    tools::schemas_with_mcp(&mcp_tools),
                    config.tool_filter.as_deref(),
                ))
            };
        // Execution-memory block is rebuilt fresh each turn (append once).
        let mut turn_system = system_base.clone();
        if !exec_memory.is_empty() {
            turn_system.push('\n');
            turn_system.push_str(&exec_memory.snapshot());
        }
        // Model-response recovery (upstream model_response_recovery.zig):
        // provider failures are classified and the decision policy picks
        // retry (with implicit/explicit backoff) or a terminal pause/stop,
        // bounded by a per-run attempt budget. The request is re-sent
        // verbatim on recoverable strategies; accumulators reset each attempt
        // because a resend replays the response from its beginning.
        let mut text = String::new();
        let mut display_text = String::new();
        let mut calls: BTreeMap<usize, ToolUse> = BTreeMap::new();
        let mut order: Vec<usize> = Vec::new();
        // DSML markup (DeepSeek text-blob tool calls) is hidden from the
        // terminal and from the committed transcript, but kept raw in `text`
        // for parse_text_tool_calls.
        let mut markup_mask = tools::ToolMarkupMask::new();
        let mut reasoning_started = false;
        use futures_util::StreamExt;
        let mut provider_attempts = crate::model_response_recovery::AttemptState::default();
        let mut pacing = crate::model_response_recovery::RetryPacingState::Idle;
        'provider_attempts: loop {
            let mut stream = providers::stream(
                &provider,
                &transcript,
                tools_schema.as_deref(),
                &turn_system,
                Some(provider_max_tokens(&provider)),
            );
            text.clear();
            display_text.clear();
            calls.clear();
            order.clear();
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(StreamEvent::TextDelta(s)) => {
                        text.push_str(&s);
                        let shown = markup_mask.filter(&s);
                        display_text.push_str(&shown);
                        if !shown.is_empty() {
                            human.text_delta(&shown);
                        }
                    }
                    Ok(StreamEvent::ReasoningDelta(s)) => {
                        if !reasoning_started {
                            human.reasoning_started();
                            reasoning_started = true;
                        }
                        human.reasoning_delta(&s);
                    }
                    Ok(StreamEvent::ToolCallStart { index, id, name }) => {
                        if !calls.contains_key(&index) {
                            order.push(index);
                        }
                        human.trace_tool(name.clone());
                        calls.insert(
                            index,
                            ToolUse {
                                id,
                                name,
                                arguments: String::new(),
                            },
                        );
                    }
                    Ok(StreamEvent::ToolCallArgDelta { index, delta }) => {
                        if let Some(c) = calls.get_mut(&index) {
                            c.arguments.push_str(&delta);
                        }
                    }
                    Ok(StreamEvent::ToolCallDone {
                        index,
                        id,
                        name,
                        input,
                    }) => {
                        let args = serde_json::to_string(&input).unwrap_or_default();
                        if let Some(c) = calls.get_mut(&index) {
                            c.id = id;
                            c.name = name;
                            c.arguments = args;
                        }
                    }
                    Ok(StreamEvent::Finish) => {}
                    Ok(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                    }) => {
                        let i = input_tokens.unwrap_or(0);
                        let o = output_tokens.unwrap_or(0);
                        total_input_tokens = total_input_tokens.saturating_add(i);
                        total_output_tokens = total_output_tokens.saturating_add(o);
                        total_tokens = total_tokens.saturating_add(i + o);
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let Some(cause) = crate::model_response_recovery::classify_failure(&msg)
                        else {
                            finish = FinishReason::Error;
                            error = Some(format!("stream error: {msg}"));
                            break 'provider_attempts;
                        };
                        use crate::model_response_recovery::{
                            decide, Delivery, OutputEvidence, ToolEvidence,
                        };
                        let evidence = crate::model_response_recovery::Evidence {
                            cause,
                            delivery: if text.is_empty() && calls.is_empty() {
                                Delivery::DefinitelyUnsent
                            } else {
                                Delivery::PossiblySent
                            },
                            attempts: provider_attempts,
                            output: if text.is_empty() {
                                OutputEvidence::None
                            } else {
                                OutputEvidence::Partial
                            },
                            tool: if calls.is_empty() {
                                ToolEvidence::None
                            } else {
                                ToolEvidence::Uncertain
                            },
                            pacing,
                            retry_after_seconds: parse_retry_after(&msg),
                            cancelled: false,
                        };
                        let decision = decide(evidence);
                        if decision.reserve_provider_attempt {
                            provider_attempts.consumed += 1;
                        }
                        pacing = decision.next_pacing;
                        match decision.strategy {
                            crate::model_response_recovery::Strategy::RetryRequest
                            | crate::model_response_recovery::Strategy::RegenerateTool
                            | crate::model_response_recovery::Strategy::ContinueResponse
                            | crate::model_response_recovery::Strategy::ContinueAfterConfirmedTool => {
                                markup_mask = tools::ToolMarkupMask::new();
                                reasoning_started = false;
                                if decision.delay_ns > 0 {
                                    eprintln!(
                                        "[fxrs] \x1b[33mprovider {cause:?}; retrying in {:.1}s\x1b[0m",
                                        decision.delay_ns as f64 / 1_000_000_000.0
                                    );
                                    tokio::time::sleep(std::time::Duration::from_nanos(
                                        decision.delay_ns,
                                    ))
                                    .await;
                                } else {
                                    eprintln!(
                                        "[fxrs] \x1b[33mprovider {cause:?}; retrying\x1b[0m"
                                    );
                                }
                                continue 'provider_attempts;
                            }
                            _ => {
                                finish = FinishReason::Error;
                                error = Some(format!(
                                    "stream error: {msg} (recovery decision: {:?}{})",
                                    decision.strategy,
                                    if decision
                                        .required_action
                                        != crate::model_response_recovery::RequiredAction::None
                                    {
                                        format!(
                                            ", action: {:?}",
                                            decision.required_action
                                        )
                                    } else {
                                        String::new()
                                    }
                                ));
                                break 'provider_attempts;
                            }
                        }
                    }
                }
            }
            break;
        }
        let residue = markup_mask.finish();
        if !residue.is_empty() {
            display_text.push_str(&residue);
            human.text_delta(&residue);
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
                    let idx = i;
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

        // Commit assistant turn to transcript. The *visible* text is stored
        // (DSML markup was masked out); the raw `text` was only needed for
        // parse_text_tool_calls above.
        let mut assistant_blocks = Vec::new();
        if !display_text.trim().is_empty() {
            assistant_blocks.push(ContentBlock::Text(display_text.clone()));
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
            // Lifecycle hooks at stop / end of turn.
            let visible_text = if display_text.trim().is_empty() {
                text.clone()
            } else {
                display_text.clone()
            };
            crate::hooks::run(
                crate::hooks::HookKind::Stop,
                crate::hooks::stop_input(&config.workspace, Some(&session_id), &visible_text),
                &config.workspace,
                30,
            );
            crate::hooks::run(
                crate::hooks::HookKind::PostTurnEnd,
                crate::hooks::post_turn_end_input(&config.workspace, Some(&session_id), steps),
                &config.workspace,
                30,
            );
            break;
        }

        // Execute each tool call sequentially, gated by permissions.
        for idx in &order {
            let call = calls.get(idx).cloned().unwrap();
            tool_calls += 1;

            // Subagent authority tool filter: names outside the admission
            // snapshot are denied pre-execution.
            if let Some(filter) = config.tool_filter.as_deref() {
                if !filter.iter().any(|t| t == &call.name) && tools_allow_check(&call.name) {
                    let denied = crate::subagent_domain::blocked_tool_json_public(
                        &call.name,
                        "Tool blocked by subagent admission authority.",
                    );
                    transcript.push(Message::tool(call.id.clone(), denied.to_string()));
                    continue;
                }
            }

            // Active-mode tool policy (upstream mode_registry.toolAllowed):
            // a read-only mode blocks non-read-only tools pre-execution.
            let active_mode = config.active_mode();
            if active_mode.tool_policy == crate::modes::ToolPolicy::ReadOnly
                && !crate::modes::READ_ONLY_TOOL_NAMES.contains(&call.name.as_str())
                && tools_allow_check(&call.name)
            {
                let denied = crate::modes::blocked_tool_json(
                    &call.name,
                    active_mode
                        .tool_policy_denial_message
                        .unwrap_or("Tool blocked by the active mode policy."),
                );
                transcript.push(Message::tool(call.id.clone(), denied.to_string()));
                continue;
            }

            let target = tools::target_for(&call.name, &call.arguments).unwrap_or_default();
            let decision = permissions.decide(&crate::permissions::PermissionRequest {
                tool_name: &call.name,
                target: &target,
                input_text: call.arguments.clone(),
                workspace: &config.workspace,
            });

            // Lifecycle hooks: PreToolUse may block or rewrite.
            let hook_args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
            let mut rewritten_args: Option<Value> = None;
            let mut hook_blocked: Option<String> = None;
            for outcome in crate::hooks::run(
                crate::hooks::HookKind::PreToolUse,
                crate::hooks::pre_tool_use_input(
                    &call.name,
                    &call.id,
                    steps,
                    &hook_args,
                    &config.workspace,
                    Some(&session_id),
                ),
                &config.workspace,
                30,
            ) {
                match outcome {
                    crate::hooks::HookOutcome::Block { reason } => {
                        hook_blocked = Some(reason);
                        break;
                    }
                    crate::hooks::HookOutcome::Rewrite { args } => {
                        rewritten_args = Some(args);
                    }
                    crate::hooks::HookOutcome::Allow => {}
                }
            }
            let result = if let Some(reason) = hook_blocked {
                Ok(tools::err_json(format!(
                    "{}: blocked by PreToolUse hook: {reason}",
                    call.name
                )))
            } else {
                let effective_arguments = rewritten_args
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_else(|| call.arguments.clone());
                let effective_call = if rewritten_args.is_some() {
                    ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: effective_arguments,
                    }
                } else {
                    call.clone()
                };
                // Tool preparation (fx tool_preparation): normalize paths +
                // enforce required fields before anything executes.
                let prepared_args: Value =
                    serde_json::from_str(&effective_call.arguments).unwrap_or(Value::Null);
                let prepared = crate::tool_prep::prepare(&call.name, &prepared_args, &ctx);
                let r = if let Some(prep_err) = prepared.error {
                    Ok(tools::err_json(format!("{}: {prep_err}", call.name)))
                } else {
                    match decision {
                        Decision::Allow => {
                            execute_tool_prepared(&ctx, &effective_call, &prepared).await
                        }
                        Decision::Deny(reason) => Ok(tools::err_json(format!(
                            "{name}: blocked by permission rules: {reason}",
                            name = call.name
                        ))),
                        Decision::Unresolved => match config.effective_permission_mode() {
                            PermissionMode::Yolo => {
                                execute_tool_prepared(&ctx, &effective_call, &prepared).await
                            }
                            PermissionMode::Ask if req.interactive => {
                                let approval_req = crate::approval::ApprovalRequest {
                                    tool_name: call.name.clone(),
                                    target: target.clone(),
                                    input_text: call.arguments.clone(),
                                    workspace: config.workspace.clone(),
                                };
                                crate::hooks::run(
                                    crate::hooks::HookKind::AttentionRequired,
                                    crate::hooks::attention_required_input(
                                        &config.workspace,
                                        Some(&session_id),
                                        "permission approval needed",
                                    ),
                                    &config.workspace,
                                    30,
                                );
                                let allowed = human.approve(&approval_req);
                                if allowed {
                                    let pattern = grant_pattern(&target);
                                    grants_map.insert(call.name.clone(), pattern.clone());
                                    permissions.grants.allow(&call.name, &pattern);
                                    execute_tool_prepared(&ctx, &effective_call, &prepared).await
                                } else {
                                    Ok(tools::err_json(format!("{}: denied by user", call.name)))
                                }
                            }
                            PermissionMode::Ask => Ok(tools::err_json(format!(
                            "{}: blocked: permission needed but interactive approval unavailable",
                            call.name
                        ))),
                            PermissionMode::Auto => {
                                use crate::permissions::{auto_classify, AutoDecision};
                                match auto_classify(
                                    &crate::permissions::PermissionRequest {
                                        tool_name: &call.name,
                                        target: &target,
                                        input_text: call.arguments.clone(),
                                        workspace: &config.workspace,
                                    },
                                    &sandbox,
                                ) {
                                    AutoDecision::Allow => {
                                        execute_tool_prepared(&ctx, &effective_call, &prepared)
                                            .await
                                    }
                                    AutoDecision::Deny(reason) => Ok(tools::err_json(format!(
                                        "{}: not auto-approved: {}",
                                        call.name, reason
                                    ))),
                                    AutoDecision::Undetermined => {
                                        auto_review(
                                            &provider,
                                            &system_base,
                                            &transcript,
                                            &call,
                                            config.clone(),
                                        )
                                        .await
                                    }
                                }
                            }
                        },
                    }
                };
                r
            };
            let result_text = result.unwrap_or_else(tools::err_result);
            human.tool_result(&call.name, &result_text.to_string());
            // Large tool results are persisted to the durable result store
            // and the model receives a bounded preview + read_tool_result
            // handle instead (upstream result_store.prepare).
            let result_str = result_text.to_string();
            let prepared = crate::result_store::prepare(
                Some(&crate::result_store::result_dir()),
                &call.id,
                &call.name,
                result_str.as_bytes(),
                config.max_tool_result_bytes,
            );
            let result_text = serde_json::Value::String(prepared.model_output.clone());
            let result_str = prepared.model_output;
            let result_ok = !result_str.contains("\"error\"");
            exec_memory.record(
                &call.name,
                &prepared_args_debug(&call),
                &result_str,
                result_ok,
            );
            tape.record(
                &crate::tape::TapeEntry {
                    ts_ms: crate::util::now_ms(),
                    tool: call.name.clone(),
                    target,
                    ok: result_ok,
                    preview: result_str.chars().take(400).collect(),
                },
                &session_id,
            );
            if call.name == "view_image" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
                if let Some((media, data)) = crate::tools::vision::attachment_for(&args) {
                    transcript.push(Message::tool_with_image(
                        call.id.clone(),
                        result_text.to_string(),
                        media,
                        data,
                    ));
                } else {
                    transcript.push(Message::tool(call.id.clone(), result_text.to_string()));
                }
            } else {
                transcript.push(Message::tool(call.id.clone(), result_text.to_string()));
            }
        }

        // Persist session (grants + transcript) after each model round.
        let sess = Session {
            schema_version: crate::sessions::SCHEMA_VERSION,
            id: session_id.clone(),
            workspace: config.workspace.display().to_string(),
            created_ms: 0,
            updated_ms: crate::util::now_ms(),
            model: provider.model.clone(),
            mode: config.effective_permission_mode(),
            interactive: req.interactive,
            messages: transcript.clone(),
            grants: grants_map.clone(),
            usage: crate::sessions::SessionUsage {
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                total_tokens,
                cost_usd,
                steps,
                tool_calls,
            },
        };
        if let Err(e) = store.save(&sess) {
            eprintln!("[fxrs] session save failed: {e:#}");
        }
        usage_recovery_checkpoint(&session_id, &sess);
    }

    let sess = Session {
        schema_version: crate::sessions::SCHEMA_VERSION,
        id: session_id.clone(),
        workspace: config.workspace.display().to_string(),
        created_ms: 0,
        updated_ms: crate::util::now_ms(),
        model: provider.model.clone(),
        mode: config.permission_mode,
        interactive: req.interactive,
        messages: transcript.clone(),
        grants: grants_map.clone(),
        usage: crate::sessions::SessionUsage {
            input_tokens: total_input_tokens,
            output_tokens: total_output_tokens,
            total_tokens,
            cost_usd,
            steps,
            tool_calls,
        },
    };
    if let Err(e) = store.save(&sess) {
        eprintln!("[fxrs] session save failed: {e:#}");
    }
    usage_recovery_checkpoint(&session_id, &sess);

    // Usage sidecar: one record per turn (~/.fx/usage.jsonl).
    {
        let usage = crate::usage::UsageStore::new();
        let rec = crate::usage::UsageRecord {
            ts_ms: crate::usage::now_ms(),
            workspace: config.workspace.display().to_string(),
            session_id: session_id.clone(),
            model: provider.model.clone(),
            input_tokens: total_input_tokens,
            output_tokens: total_output_tokens,
            total_tokens,
            cost_usd,
            steps,
            tool_calls,
            interactive: req.interactive,
        };
        if let Err(e) = usage.record(&rec) {
            eprintln!("[fxrs] usage record failed: {e:#}");
        }
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

fn prepared_args_debug(call: &ToolUse) -> serde_json::Value {
    serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null)
}

async fn execute_tool(ctx: &ToolContext, call: &ToolUse) -> Result<serde_json::Value> {
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    tools::execute(ctx, &call.name, &args).await
}

/// Execute a tool call using tool-prep-normalized arguments (absolute paths,
/// defaults applied) when they differ from the raw arguments.
async fn execute_tool_prepared(
    ctx: &ToolContext,
    call: &ToolUse,
    prepared: &crate::tool_prep::Prepared,
) -> Result<serde_json::Value> {
    let raw: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    if prepared.args == raw {
        return execute_tool(ctx, call).await;
    }
    let final_call = ToolUse {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: serde_json::to_string(&prepared.args).unwrap_or_else(|_| call.arguments.clone()),
    };
    execute_tool(ctx, &final_call).await
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

    match providers::chat(
        provider,
        &msgs,
        None,
        &format!("{system}\n\n(reviewer mode)"),
        Some(512),
    )
    .await
    {
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
                Ok(tools::err_json(format!(
                    "{}: blocked by auto-review",
                    call.name
                )))
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

/// Extract a `Retry-After`-style delay from a provider error message.
/// Upstream reads the HTTP header; we only have the transport error text, so
/// accept `retry after: <n>` / `retry_after=<n>` / `retry-after: <n>`.
fn parse_retry_after(msg: &str) -> Option<u64> {
    let m = msg.to_ascii_lowercase();
    let needle = [
        "retry after:",
        "retry-after:",
        "retry_after=",
        "retry after ",
    ]
    .iter()
    .find(|n| m.contains(**n))?;
    let idx = m.find(needle)? + needle.len();
    let rest: String = m[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    rest.parse::<u64>()
        .ok()
        .filter(|&v| v <= crate::model_response_recovery::MAX_RETRY_AFTER_SECONDS)
}

/// Usage-recovery checkpoint (upstream session_store.zig): after writing a
/// session checkpoint that carries usage the durable ledger may not yet
/// cover, record a `usage_recovery` marker; once the ledger covers the
/// session's claimed totals, clear it. A crash in the window between the
/// checkpoint and the ledger append leaves the marker so the next run's
/// `fxrs usage` / `fxrs doctor` can surface the unresolved state.
fn usage_recovery_checkpoint(session_id: &str, sess: &crate::sessions::Session) {
    let ledger = crate::usage::UsageStore::new();
    let ledger_tokens: u64 = ledger
        .read_all()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .map(|r| r.total_tokens)
        .sum();
    if crate::usage_recovery::needs_recovery(sess, ledger_tokens) {
        let _ = crate::usage_recovery::mark_pending(session_id, sess.updated_ms as i64);
    } else {
        let _ = crate::usage_recovery::clear_pending(session_id);
    }
}

/// Whether `name` is a tool the local toolset publishes (used by the
/// active-mode read-only policy; MCP/dynamic names are always allowed,
/// like upstream ToolSet.registry).
fn tools_allow_check(name: &str) -> bool {
    crate::tools::is_builtin_tool_name(name)
}

/// Apply a subagent tool filter to MCP-published schemas.
fn filter_mcp_schemas(
    schemas: Vec<serde_json::Value>,
    filter: Option<&[String]>,
) -> Vec<serde_json::Value> {
    let Some(filter) = filter else {
        return schemas;
    };
    schemas
        .into_iter()
        .filter(|s| {
            let name = s
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            name.is_empty() || filter.contains(&name.to_string())
        })
        .collect()
}
