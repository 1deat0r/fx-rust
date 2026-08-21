//! background_process tool: start/list/get_output/log/stop long-running
//! background commands, faithful to fx's `tools/shell/background_process.zig`
//! surface. Backed by `crate::background`.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::background::{process_table, BackgroundStore, BgStatus, SupervisedRecord};

use super::{arg, arg_i64, ToolContext};

const DEFAULT_TAIL_BYTES: usize = 16 * 1024;
const MAX_TAIL_BYTES: usize = 1024 * 1024;

pub async fn background_process(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let action = arg(args, "action").unwrap_or("list");
    let mut store = BackgroundStore::open()?;
    match action {
        "start" => {
            let command = arg(args, "command")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `command`"))?;
            let name = arg(args, "name");
            let cwd = args
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| ctx.resolve(s))
                .unwrap_or_else(|| ctx.workspace.clone());
            let session_id = (!ctx.session_id.is_empty()).then_some(ctx.session_id.as_str());
            let record = store.start_with_session(command, &cwd, name, session_id)?;
            Ok(json!({
                "status": "started",
                "process_id": record.id,
                "pid": record.pid,
                "command": record.command,
                "log_path": record.log_path,
                "session_id": record.session_id,
                "note": "use `get_output` to read output, `supervise` for liveness, and `stop`/`stop_tree` to terminate",
            }))
        }
        "list" => {
            let records = store.list().to_vec();
            let rows: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({
                        "process_id": r.id,
                        "name": r.name,
                        "pid": r.pid,
                        "status": status_str(r.status),
                        "exit_code": r.exit_code,
                        "command": r.command,
                        "log_path": r.log_path,
                        "session_id": r.session_id,
                    })
                })
                .collect();
            Ok(json!({ "processes": rows }))
        }
        "get_output" | "log" => {
            let id = arg(args, "process_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `process_id`"))?;
            let tail_rows = arg_i64(args, "tail").filter(|t| *t > 0).map(|t| t as usize);
            let max_bytes = arg_i64(args, "max_bytes")
                .map(|b| b.clamp(1024, MAX_TAIL_BYTES as i64) as usize)
                .unwrap_or(DEFAULT_TAIL_BYTES);
            let text = store.log_text(id, max_bytes, tail_rows)?;
            let record = store
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown background process id `{id}`"))?;
            Ok(json!({
                "process_id": id,
                "status": status_str(record.status),
                "exit_code": record.exit_code,
                "pid": record.pid,
                "output": text,
            }))
        }
        "supervise" => {
            let rows: Vec<Value> = store
                .supervise()
                .into_iter()
                .map(|s| supervised_json(&s))
                .collect();
            let counts = store
                .supervise()
                .iter()
                .fold((0usize, 0usize), |(run, total), s| {
                    (run + usize::from(s.alive), total + 1)
                });
            Ok(json!({
                "status": "ok",
                "running": counts.0,
                "total": counts.1,
                "processes": rows,
            }))
        }
        "tree" => {
            let id = arg(args, "process_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `process_id`"))?;
            let record = store
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown background process id `{id}`"))?;
            let table = process_table();
            Ok(json!({
                "process_id": id,
                "pid": record.pid,
                "status": status_str(record.status),
                "tree": crate::background::render_tree(record, &table),
            }))
        }
        "stop_tree" | "stop-tree" => {
            let id = arg(args, "process_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `process_id`"))?;
            let timeout_ms = arg_i64(args, "timeout_ms")
                .unwrap_or(5000)
                .clamp(500, 30_000) as u64;
            let record = store.stop_tree(id, timeout_ms)?;
            Ok(json!({
                "status": "stopped",
                "process_id": record.id,
                "pid": record.pid,
                "exit_code": record.exit_code,
                "note": "process tree terminated",
            }))
        }
        "stop" => {
            let id = arg(args, "process_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `process_id`"))?;
            let timeout_ms = arg_i64(args, "timeout_ms")
                .unwrap_or(5000)
                .clamp(500, 30_000) as u64;
            let record = store.stop(id, timeout_ms)?;
            Ok(json!({
                "status": "stopped",
                "process_id": record.id,
                "pid": record.pid,
                "exit_code": record.exit_code,
                "note": "process terminated",
            }))
        }
        other => bail!("unknown background_process action: {other} (expected start, list, get_output, log, supervise, tree, stop_tree, stop)"),
    }
}

fn supervised_json(s: &SupervisedRecord) -> Value {
    json!({
        "process_id": s.record.id,
        "name": s.record.name,
        "pid": s.record.pid,
        "status": if s.alive { "running" } else { status_str(s.record.status) },
        "exit_code": s.record.exit_code,
        "alive": s.alive,
        "children_alive": s.children_alive,
        "rss_kb": s.rss_kb,
        "etimes_secs": s.etimes_secs,
        "cpu_percent": s.cpu_percent,
        "command": s.record.command,
        "cwd": s.record.cwd,
        "log_path": s.record.log_path,
        "session_id": s.record.session_id,
        "started_at_ms": s.record.started_at_ms,
    })
}

fn status_str(s: BgStatus) -> &'static str {
    match s {
        BgStatus::Running => "running",
        BgStatus::Exited => "exited",
        BgStatus::Failed => "failed",
    }
}
