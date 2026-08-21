//! terminal tool: persistent terminal sessions over tmux. Faithful to fx's
//! `tools/terminal/*` surface: create/list/send/read/resize/stop. Backed by
//! `crate::terminal` (tmux). Sensitive — gated like run_command.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::terminal::{TermStatus, TerminalStore};

use super::{arg, ToolContext};

const DEFAULT_READ_BYTES: usize = 64 * 1024;
const DEFAULT_SCROLLBACK: usize = 200;

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
            let record = store.create(&cwd, command, name, rows, columns)?;
            Ok(json!({
                "status": "created",
                "terminal_id": record.id,
                "name": record.name,
                "pid": record.pid,
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
                        "command": r.command,
                        "cwd": r.cwd,
                        "rows": r.rows,
                        "columns": r.columns,
                    })
                })
                .collect();
            Ok(json!({ "terminals": rows }))
        }
        "send" => {
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
        "stop" | "destroy" => {
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
            "unknown terminal action: {other} (expected create, list, send, read, resize, stop)"
        ),
    }
}

fn status_str(s: TermStatus) -> &'static str {
    match s {
        TermStatus::Running => "running",
        TermStatus::Exited => "exited",
    }
}
