//! ACP (Agent Client Protocol) server — pragmatic faithful port of upstream
//! `src/acp/server.zig` + `sessions.zig` + `prompt.zig` + `prompt_test_controls`.
//!
//! Newline-delimited JSON-RPC over stdio:
//!   initialize, session/new, session/load, session/resume, session/list,
//!   session/close, session/remove, session/cancel, session/prompt,
//!   session/set_mode, session/set_config_option.
//!
//! `session/prompt` streams `session/update` notifications (agent_message_chunk,
//! tool_call, tool_call_update) through a [`StreamingHuman`] and returns
//! `{"stopReason": ...}`. Cancellation aborts the active prompt task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::acp::jsonrpc::{error as rpc_error, response, ErrorCode, Message};
use crate::acp::types::{self, StopReason, ToolCallKind, ToolCallStatus};
use crate::agent::{AgentOutput, AgentRequest, FinishReason};
use crate::sessions::SessionStore;

/// Human sink that emits ACP `session/update` notifications per activity.
struct StreamingHuman {
    session_id: String,
    cancel: Arc<AtomicBool>,
}

impl StreamingHuman {
    fn notify(&self, update: Value) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        // Streaming notifications always go to the process stdout: the ACP
        // server runs on stdio and prompts are the only writers mid-run.
        let msg = types::session_update(&self.session_id, update);
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(msg.to_string().as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

impl crate::ui::Human for StreamingHuman {
    fn step_started(&self, step: usize) {
        self.notify(json!({"sessionUpdate": "step_started", "step": step}));
    }
    fn text_delta(&self, text: &str) {
        self.notify(types::agent_message_chunk(text));
    }
    fn stream_done(&self) {
        // No-op: the final transcript is emitted by prompt completion.
    }
    fn trace_tool(&self, name: String) {
        let kind = tool_kind(&name);
        self.notify(types::tool_call(
            &name,
            &name,
            kind,
            ToolCallStatus::Pending,
        ));
    }
    fn tool_result(&self, name: &str, result: &str) {
        let kind = tool_kind(name);
        let ok = !result.contains("\"error\"");
        self.notify(types::tool_call_update(
            name,
            if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            },
            Some(if result.len() > 600 {
                format!("{}…", result.chars().take(600).collect::<String>())
            } else {
                result.to_string()
            }),
        ));
        let _ = kind;
    }
    fn approve(&self, _req: &crate::approval::ApprovalRequest) -> bool {
        // Non-interactive ACP runs approve nothing by default.
        false
    }
}

fn tool_kind(name: &str) -> ToolCallKind {
    match name {
        "read_file" | "list_files" | "glob_files" | "grep_files" | "file_info" | "view_image" => {
            ToolCallKind::Read
        }
        "write_file" | "edit_file" | "create_folder" | "delete_file" | "rename_file"
        | "copy_file" | "open_file" => ToolCallKind::Edit,
        "run_command" | "terminal" | "browser_terminal" | "background_process" => {
            ToolCallKind::Execute
        }
        "web_search" | "semantic_search" => ToolCallKind::Search,
        "web_fetch" => ToolCallKind::Fetch,
        _ => ToolCallKind::Other,
    }
}

fn stop_reason_for(reason: FinishReason) -> StopReason {
    match reason {
        FinishReason::Stop => StopReason::EndTurn,
        FinishReason::MaxSteps => StopReason::MaxModelTurns,
        FinishReason::Error => StopReason::MaxOutputTokens,
        FinishReason::UserExit => StopReason::Cancelled,
    }
}

async fn write_line<W: AsyncWrite + Send + Unpin>(out: &mut W, line: &str) {
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

pub struct AcpServer {
    pub app_version: String,
}

#[derive(Default)]
struct ActivePrompt {
    cancel: Arc<AtomicBool>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl AcpServer {
    pub fn new(app_version: &str) -> Self {
        Self {
            app_version: app_version.to_string(),
        }
    }

    /// Serve newline-delimited JSON-RPC from `input` to `output` (async IO:
    /// tokio streams are Send across the prompts we await). The caller owns
    /// the output stream (tests capture it).
    pub async fn serve_reader<R, W>(
        &mut self,
        input: R,
        mut output: W,
        workspace: &std::path::Path,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Send + Unpin,
        W: AsyncWrite + Send + Unpin,
    {
        let store = SessionStore::new()?;
        let mut active = ActivePrompt::default();
        let mut reader = BufReader::new(input);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| anyhow::anyhow!("read: {e}"))?;
            if n == 0 {
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg = match crate::acp::jsonrpc::parse_message(trimmed) {
                Ok(m) => m,
                Err((code, text)) => {
                    let body = format!("{}", rpc_error(&None, code, &text, None));
                    let _ = output.write_all(body.as_bytes()).await;
                    let _ = output.write_all(b"\n").await;
                    let _ = output.flush().await;
                    continue;
                }
            };
            self.dispatch(&msg, &store, &mut active, &mut output, workspace)
                .await?;
            let _ = output.flush().await;
        }
    }

    /// Serve from stdin to stdout (CLI entry).
    pub async fn serve_stdio(&mut self, workspace: &std::path::Path) -> Result<()> {
        self.serve_reader(
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            workspace,
        )
        .await
    }

    async fn dispatch<W: AsyncWrite + Send + Unpin>(
        &mut self,
        msg: &Message,
        store: &SessionStore,
        active: &mut ActivePrompt,
        out: &mut W,
        workspace: &std::path::Path,
    ) -> Result<()> {
        let method = msg.method.as_deref().unwrap_or("");
        match method {
            "initialize" => {
                let result = types::initialize_response(&self.app_version);
                write_line(out, &format!("{}", response(&msg.id, result))).await;
            }
            "session/new" => {
                let id = store
                    .create(workspace, false)
                    .context("creating session")?
                    .1;
                let cfg = crate::config::resolve(workspace)?;
                let result = json!({
                    "sessionId": id,
                    "configOptions": [
                        {"key":"model","options":[],"default": cfg.model},
                        {"key":"mode","options":["code","ask"],"default": cfg.mode}
                    ],
                    "modes": {"currentModeId": cfg.mode}
                });
                write_line(out, &format!("{}", response(&msg.id, result))).await;
            }
            "session/load" | "session/resume" => {
                let Some(sid) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                else {
                    write_line(
                        out,
                        &format!(
                            "{}",
                            rpc_error(
                                &msg.id,
                                ErrorCode::INVALID_PARAMS,
                                "Missing sessionId",
                                None
                            )
                        ),
                    )
                    .await;
                    return Ok(());
                };
                match store.load_by_id(sid) {
                    Ok(Some(sess)) => {
                        // Replay assistant/user history as session updates.
                        for m in sess.messages.iter().take(400) {
                            if let Some(text) = m.last_text() {
                                let chunk = if m.role_str() == "user" {
                                    types::user_message_chunk(&text)
                                } else {
                                    types::agent_message_chunk(&text)
                                };
                                let env = types::session_update(&sess.id, chunk);
                                write_line(out, &format!("{}", env)).await;
                            }
                        }
                        write_line(
                            out,
                            &format!("{}", response(&msg.id, json!({ "sessionId": sess.id }))),
                        )
                        .await;
                    }
                    _ => {
                        write_line(
                            out,
                            &format!(
                                "{}",
                                rpc_error(
                                    &msg.id,
                                    ErrorCode::INVALID_PARAMS,
                                    "Session not found",
                                    None
                                )
                            ),
                        )
                        .await;
                    }
                }
            }
            "session/list" => {
                let sessions: Vec<Value> = store
                    .list(Some(workspace))?
                    .into_iter()
                    .map(|s| {
                        json!({
                            "sessionId": s.id,
                            "workspace": s.workspace,
                            "updated_ms": s.updated_ms,
                            "tokens": s.tokens,
                            "tool_calls": s.tool_calls,
                        })
                    })
                    .collect();
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "sessions": sessions }))),
                )
                .await;
            }
            "session/close" => {
                let Some(sid) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                else {
                    write_line(
                        out,
                        &format!(
                            "{}",
                            rpc_error(
                                &msg.id,
                                ErrorCode::INVALID_PARAMS,
                                "Missing sessionId",
                                None
                            )
                        ),
                    )
                    .await;
                    return Ok(());
                };
                let _ = store.load_by_id(sid)?;
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "sessionId": sid }))),
                )
                .await;
            }
            "session/remove" => {
                let Some(sid) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                else {
                    write_line(
                        out,
                        &format!(
                            "{}",
                            rpc_error(
                                &msg.id,
                                ErrorCode::INVALID_PARAMS,
                                "Missing sessionId",
                                None
                            )
                        ),
                    )
                    .await;
                    return Ok(());
                };
                let deleted = store.delete(workspace, sid)?;
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "deleted": deleted }))),
                )
                .await;
            }
            "session/cancel" => {
                active.cancel.store(true, Ordering::Relaxed);
                if let Some(tx) = active.shutdown.take() {
                    let _ = tx.send(());
                }
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "cancelled": true }))),
                )
                .await;
            }
            "session/set_mode" => {
                let Some(mode_id) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("modeId"))
                    .and_then(|v| v.as_str())
                else {
                    write_line(
                        out,
                        &format!(
                            "{}",
                            rpc_error(&msg.id, ErrorCode::INVALID_PARAMS, "Missing modeId", None)
                        ),
                    )
                    .await;
                    return Ok(());
                };
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "currentModeId": mode_id }))),
                )
                .await;
            }
            "session/set_config_option" => {
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "ok": true }))),
                )
                .await;
            }
            "session/prompt" => {
                self.handle_prompt(msg, store, active, out, workspace)
                    .await?;
                return Ok(());
            }
            "shutdown" | "exit" => {
                write_line(
                    out,
                    &format!("{}", response(&msg.id, json!({ "ok": true }))),
                )
                .await;
                return Ok(());
            }
            _ => {
                write_line(
                    out,
                    &format!(
                        "{}",
                        rpc_error(
                            &msg.id,
                            ErrorCode::METHOD_NOT_FOUND,
                            "method not found",
                            Some(json!({ "method": method }))
                        )
                    ),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_prompt<W: AsyncWrite + Send + Unpin>(
        &self,
        msg: &Message,
        store: &SessionStore,
        active: &mut ActivePrompt,
        out: &mut W,
        workspace: &std::path::Path,
    ) -> Result<()> {
        let params = msg.params.clone().unwrap_or_else(|| json!({}));
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Fresh ACP session: allocate an id and persist an empty
                // session so the prompt can resume it (agent::run loads the
                // history from the store).
                match store.create(workspace, false) {
                    Ok((_messages, id, _grants)) => {
                        let empty: crate::sessions::Session = crate::sessions::Session {
                            schema_version: crate::sessions::SCHEMA_VERSION,
                            id: id.clone(),
                            workspace: workspace.display().to_string(),
                            created_ms: crate::util::now_ms(),
                            updated_ms: crate::util::now_ms(),
                            model: String::new(),
                            mode: crate::permissions::PermissionMode::Auto,
                            interactive: false,
                            messages: Vec::new(),
                            grants: Default::default(),
                            usage: Default::default(),
                        };
                        let _ = store.save(&empty);
                        id
                    }
                    Err(_) => "cmd-1".to_string(),
                }
            });
        let prompt_text = params
            .get("prompt_text")
            .or_else(|| params.get("promptText"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let command_text = params
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let prompt = match (prompt_text, command_text) {
            (Some(t), _) => t,
            (None, Some(c)) => c,
            (None, None) => {
                write_line(
                    out,
                    &format!(
                        "{}",
                        rpc_error(
                            &msg.id,
                            ErrorCode::INVALID_PARAMS,
                            "session/prompt requires prompt_text or command",
                            None
                        )
                    ),
                )
                .await;
                return Ok(());
            }
        };

        // Start the run with a cancellation flag the client can toggle.
        let cancel = Arc::new(AtomicBool::new(false));
        active.cancel = cancel.clone();
        let (tx, rx) = oneshot::channel::<()>();
        active.shutdown = Some(tx);

        let cfg = Arc::new(crate::config::resolve(workspace)?);
        let human = StreamingHuman {
            session_id: session_id.clone(),
            cancel: cancel.clone(),
        };
        let cfg2 = cfg.clone();
        let store2 = SessionStore::new()?;
        let req = AgentRequest {
            prompt: Some(prompt.clone()),
            system: None,
            interactive: false,
            resume: Some(session_id.clone()),
            messages: Vec::new(),
            images: Vec::new(),
        };

        let run_fut = crate::agent::run(req, cfg2, &human, &store2);
        let outcome = tokio::select! {
            biased;
            _ = rx => {
                // Cancelled: return cancelled stop reason.
                write_line(out, &format!("{}", response(&msg.id, types::prompt_response(StopReason::Cancelled)))).await;
                return Ok(());
            }
            res = run_fut => res,
        };

        let output: AgentOutput = match outcome {
            Ok(o) => o,
            Err(e) => {
                write_line(
                    out,
                    &format!(
                        "{}",
                        rpc_error(&msg.id, ErrorCode::INTERNAL_ERROR, &format!("{e:#}"), None)
                    ),
                )
                .await;
                return Ok(());
            }
        };
        let reason = stop_reason_for(output.finish_reason);
        let mut result = types::prompt_response(reason);
        result["sessionId"] = json!(output.session_id);
        if let Some(err) = &output.error {
            result["error"] = json!(err);
        }
        write_line(out, &format!("{}", response(&msg.id, result))).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_maps_common_tools() {
        assert_eq!(tool_kind("read_file"), ToolCallKind::Read);
        assert_eq!(tool_kind("run_command"), ToolCallKind::Execute);
        assert_eq!(tool_kind("web_search"), ToolCallKind::Search);
        assert_eq!(tool_kind("view_image"), ToolCallKind::Read);
        assert_eq!(tool_kind("something_else"), ToolCallKind::Other);
    }

    #[tokio::test]
    async fn server_handles_initialize_and_list() {
        let mut server = AcpServer::new("0.1.0");
        let dir = std::env::temp_dir().join(format!("fxrs-acp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let home_before = std::env::var("FX_HOME").ok();
        std::env::set_var("FX_HOME", dir.join("home"));

        let input_text = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/list\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{}}\n";
        let mut output: Vec<u8> = Vec::new();
        server
            .serve_reader(
                std::io::Cursor::new(input_text.as_bytes()),
                &mut output,
                &dir,
            )
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("\"protocolVersion\":1"), "{text}");
        assert!(text.contains("\"agentInfo\""));
        assert!(text.contains("\"sessions\":[]"), "{text}");
        // session/prompt without prompt_text/command args -> invalid params.
        assert!(text.contains("requires prompt_text or command"), "{text}");
        if let Some(h) = home_before {
            std::env::set_var("FX_HOME", h);
        } else {
            std::env::remove_var("FX_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
