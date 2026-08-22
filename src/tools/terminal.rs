//! terminal tool: persistent terminal sessions over a native PTY or tmux,
//! plus the `browser_terminal` tool (`action: "exec"` + `command`, a strict
//! wrapper around terminal exec — faithful to upstream
//! `tools/terminal/browser_terminal.zig`). Faithful to fx's
//! `tools/terminal/*` surface: actions `exec`, `start`/`create`, `read`,
//! `write`/`send`, `wait` (via `return_when`), `list`, `resize`, `close`.
//! Backed by `crate::terminal`. Sensitive — gated like run_command.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::terminal::{native_handle, Backend, TermStatus, TerminalStore};

use super::{arg, ToolContext};

const DEFAULT_READ_BYTES: usize = 64 * 1024;
const DEFAULT_SCROLLBACK: usize = 200;
const DEFAULT_WAIT_CEILING_MS: u64 = 60_000;
const MAX_COMMAND_BYTES: usize = 65_536;

pub async fn terminal(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let action = arg(args, "action").unwrap_or("list");
    let mut store = TerminalStore::open()?;
    match action {
        "create" | "start" => {
            let command = arg(args, "command");
            let name = arg(args, "name");
            let cwd = args
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| ctx.resolve(s))
                .unwrap_or_else(|| ctx.workspace.clone());
            let rows = args.get("rows").and_then(|v| v.as_u64()).map(|v| v as u32);
            let columns = args
                .get("columns")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let backend = parse_backend(args).unwrap_or(Backend::Native);
            let argv = shell_exec_argv(command);
            let opts = crate::terminal::TerminalCreateOptions {
                name,
                rows,
                columns,
                argv: &argv,
            };
            let record = store.create_backend(backend, &cwd, command, &opts)?;
            Ok(json!({
                "status": "created",
                "terminal_id": record.id,
                "name": record.name,
                "pid": record.pid,
                "backend": record.backend.as_str(),
                "command": record.command,
                "cwd": record.cwd,
                "rows": record.rows,
                "columns": record.columns,
                "note": "use `send` to type input and `read` to capture the pane; `stop` terminates the session",
            }))
        }
        "list" => {
            let rows: Vec<Value> = store
                .list()
                .iter()
                .map(|r| {
                    json!({
                        "terminal_id": r.id,
                        "name": r.name,
                        "pid": r.pid,
                        "status": status_str(r.status),
                        "backend": r.backend.as_str(),
                        "command": r.command,
                        "cwd": r.cwd,
                        "rows": r.rows,
                        "columns": r.columns,
                    })
                })
                .collect();
            Ok(json!({ "terminals": rows }))
        }
        "exec" => exec_in_terminal(ctx, args, &mut store, false).await,
        "send" | "write" => {
            let id = arg(args, "terminal_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `terminal_id`"))?;
            let input = arg(args, "input")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `input`"))?;
            let enter = args.get("enter").and_then(|v| v.as_bool()).unwrap_or(true);
            store.send(id, input, enter)?;
            Ok(json!({
                "status": "sent",
                "terminal_id": id,
                "enter": enter,
                "backend": record_backend(&store, id),
            }))
        }
        "read" | "get_output" => {
            let id = arg(args, "terminal_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `terminal_id`"))?;
            let scrollback = args
                .get("scrollback")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(DEFAULT_SCROLLBACK)
                .clamp(0, 5000);
            let max_bytes = args
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| (v as usize).clamp(1024, 1024 * 1024))
                .unwrap_or(DEFAULT_READ_BYTES);
            let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
            let clear_after = args
                .get("clear_after")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let text = store.read(id, scrollback, max_bytes, raw, clear_after)?;
            Ok(json!({
                "terminal_id": id,
                "output": text,
                "cleared": clear_after,
            }))
        }
        "resize" => {
            let id = arg(args, "terminal_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `terminal_id`"))?;
            let rows = args
                .get("rows")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing required argument `rows`"))?
                as u32;
            let columns = args
                .get("columns")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing required argument `columns`"))?
                as u32;
            let record = store.resize(id, rows, columns)?;
            Ok(json!({
                "status": "resized",
                "terminal_id": record.id,
                "rows": record.rows,
                "columns": record.columns,
            }))
        }
        "stop" | "destroy" | "close" => {
            let id = arg(args, "terminal_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `terminal_id`"))?;
            let record = store.stop(id)?;
            Ok(json!({
                "status": "stopped",
                "terminal_id": record.id,
                "pid": record.pid,
            }))
        }
        other => bail!(
            "unknown terminal action: {other} (expected create, list, exec, send, read, resize, stop)"
        ),
    }
}

/// Parse the optional `backend` argument ("native" | "tmux").
fn parse_backend(args: &Value) -> Option<Backend> {
    args.get("backend")
        .and_then(|v| v.as_str())
        .and_then(Backend::parse)
}

fn record_backend(store: &TerminalStore, id: &str) -> &'static str {
    store
        .get(id)
        .map(|r| r.backend.as_str())
        .unwrap_or("unknown")
}

/// For native sessions, `command` is the program; we pass shell `-c` args
/// so the recorded command stays the raw user string when it looks like a
/// shell command line.
/// Native `create` passes no extra argv for interactive shells.
fn shell_exec_argv(_command: Option<&str>) -> Vec<String> {
    Vec::new()
}

/// Shared exec implementation for the `terminal` action and the
/// `browser_terminal` tool.
///
/// Runs a command in a real (native) terminal session and waits until the
/// shell exits — or until `wait_ceiling_ms`, or until first output when
/// `return_when` is `"started"`. Mirrors upstream terminal `exec` /
/// `return_when` semantics over `bash -c`.
async fn exec_in_terminal(
    ctx: &ToolContext,
    args: &Value,
    store: &mut TerminalStore,
    browser_strict: bool,
) -> Result<Value> {
    let command = arg(args, "command")
        .ok_or_else(|| anyhow::anyhow!("missing required argument `command`"))?;
    if command.is_empty() {
        bail!("terminal exec: command must not be empty");
    }
    if command.len() > MAX_COMMAND_BYTES {
        bail!("terminal exec: command exceeds {MAX_COMMAND_BYTES} bytes");
    }
    let cwd = if browser_strict {
        ctx.workspace.clone()
    } else {
        args.get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| ctx.resolve(s))
            .unwrap_or_else(|| ctx.workspace.clone())
    };
    let wait_ceiling_ms = args
        .get("wait_ceiling_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_WAIT_CEILING_MS)
        .clamp(1000, 300_000);
    let return_when = args
        .get("return_when")
        .and_then(|v| v.as_str())
        .unwrap_or("exit");

    // Native backend default (upstream `backend orelse .native`): spawn
    // `bash -c <command>` under a real PTY.
    let exec_argv = vec!["-c".to_string(), command.to_string()];
    let opts = crate::terminal::TerminalCreateOptions {
        rows: Some(40),
        columns: Some(120),
        argv: &exec_argv,
        ..crate::terminal::TerminalCreateOptions::default()
    };
    let record = store.create_backend(Backend::Native, &cwd, Some("bash"), &opts)?;
    let id = record.id.clone();
    let handle = native_handle(&id)
        .ok_or_else(|| anyhow::anyhow!("exec session `{id}` has no live native handle"))?;

    let started = std::time::Instant::now();
    let mut exit_code: Option<u32> = None;
    loop {
        if let Some(code) = handle.try_exit() {
            exit_code = Some(code);
            break;
        }
        if return_when == "started" && handle.has_output() {
            break;
        }
        if started.elapsed() >= Duration::from_millis(wait_ceiling_ms) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let output = handle.read_text(500, 256 * 1024, false, false);
    let timed_out =
        exit_code.is_none() && started.elapsed() >= Duration::from_millis(wait_ceiling_ms);
    let still_running = exit_code.is_none() && !handle.has_exited() && !timed_out;

    if return_when == "started" && still_running {
        // Returning while the command is still running (upstream
        // `return_when: started`). Leave the session alive for follow-up
        // reads; the caller owns `stop`.
        return Ok(json!({
            "status": "started",
            "terminal_id": id,
            "command": command,
            "output": output,
            "note": "command still running — use terminal read <id> to follow, stop <id> to terminate",
        }));
    }

    // Exited or ceiling reached: reap and close the session.
    let _ = store.stop(&id);
    Ok(json!({
        "status": if timed_out { "timed_out" } else { "ran" },
        "terminal_id": id,
        "command": command,
        "exit_code": exit_code.unwrap_or(1),
        "output": output,
    }))
}

/// `browser_terminal` tool — strictly `{action: "exec", command}` (matching
/// upstream `tools/terminal/browser_terminal.zig`): the only accepted fields
/// are `action` and `command`, `action` must be `exec`, and `command` is
/// bounded at 65536 bytes.
pub async fn browser_terminal(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let obj = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("browser terminal arguments must be an object"))?;
    if obj.len() != 2 {
        bail!(
            "browser terminal accepts only the \"action\" and \"command\" fields (got {})",
            obj.len()
        );
    }
    let action = arg(args, "action").unwrap_or("");
    if action != "exec" {
        bail!("browser terminal action must be \"exec\"");
    }
    let command = arg(args, "command")
        .ok_or_else(|| anyhow::anyhow!("browser terminal requires string field \"command\""))?;
    if command.len() > MAX_COMMAND_BYTES {
        bail!("browser terminal field \"command\" exceeds {MAX_COMMAND_BYTES} bytes");
    }
    let mut store = TerminalStore::open()?;
    exec_in_terminal(ctx, args, &mut store, true).await
}

fn status_str(s: TermStatus) -> &'static str {
    match s {
        TermStatus::Running => "running",
        TermStatus::Exited => "exited",
        TermStatus::Lost => "lost",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(ws: &str) -> ToolContext {
        ToolContext {
            workspace: ws.into(),
            max_result_bytes: 65536,
            interactive: false,
            session_id: String::new(),
            config: std::sync::Arc::new(crate::config::Config {
                mode: "ask".into(),
                workspace: ws.into(),
                model: "m".into(),
                permission_mode: crate::permissions::PermissionMode::Auto,
                max_agent_steps: 0,
                max_tool_result_bytes: 65536,
                first_call_tool_choice: crate::config::FirstCallToolChoice::Auto,
                context: true,
                sandbox: crate::config::SandboxMode::None,
                permission_rules: Default::default(),
                settings_path: None,
                additional_directories: vec![],
                mcp_servers: vec![],
                context_limits: crate::context::ContextLimits::default(),
                input_appearance: "auto".into(),
                presentation_mode: "default".into(),
                update_channel: "stable".into(),
                tool_filter: None,
                reasoning_effort: None,
            }),
            store: crate::sessions::SessionStore::new().unwrap(),
        }
    }

    #[test]
    fn browser_terminal_validates_strictly() {
        // Upstream browser_terminal.zig contract — only action+command.
        let bad_action = json!({"action": "start", "command": "ls"});
        let ctx = ctx("/workspace");
        let out = block_on_test(browser_terminal(&ctx, &bad_action));
        assert!(out.is_err(), "non-exec action must fail");

        let extra = json!({"action": "exec", "command": "ls", "cwd": "/tmp"});
        let out = block_on_test(browser_terminal(&ctx, &extra));
        assert!(out.is_err(), "extra fields must fail");

        let missing = json!({"action": "exec"});
        let out = block_on_test(browser_terminal(&ctx, &missing));
        assert!(out.is_err(), "missing command must fail");
    }

    fn block_on_test<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn backend_parse_helper() {
        assert_eq!(
            parse_backend(&json!({"backend": "tmux"})),
            Some(Backend::Tmux)
        );
        assert_eq!(
            parse_backend(&json!({"backend": "native"})),
            Some(Backend::Native)
        );
        assert_eq!(parse_backend(&json!({})), None);
    }
}
