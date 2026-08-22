//! `read_tool_result` — session-scoped reader over the durable tool-result
//! store (upstream `src/tools/session/read_tool_result.zig`).
//!
//! The model passes the exact `handle` advertised by a stored result
//! (`<tool_result_handle>`) plus an optional 1-based `start_byte` + bounded
//! `byte_count` (default [`crate::result_store::read_default_bytes`]) or a
//! literal `query` for a first-match context window.

use anyhow::Result;
use serde_json::{json, Value};

use crate::result_store;
use crate::tools::ToolContext;

pub fn schema() -> Value {
    json!({
        "name": "read_tool_result",
        "description": "Read a stored tool result byte range or literal query by its session handle. Use the exact handle copied from the tool result preview (e.g. result-web_search-abc123-def456.txt).",
        "input_schema": {
            "type": "object",
            "properties": {
                "handle": {
                    "type": "string",
                    "description": "Exact tool-result handle advertised by the stored preview"
                },
                "start_byte": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based byte offset to start reading"
                },
                "byte_count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": result_store::read_max_bytes,
                    "description": "Number of bytes to read (max 65536)"
                },
                "query": {
                    "type": "string",
                    "description": "Literal substring to locate in the stored result; returns a bounded context window"
                }
            },
            "required": ["handle"]
        }
    })
}

/// Normalize + validate the arguments, mirroring upstream `decode`/`validate`.
pub fn call(_ctx: &ToolContext, args: &Value) -> Result<Value> {
    let Some(handle) = args.get("handle").and_then(|v| v.as_str()) else {
        return Ok(json!({
            "error": "read_tool_result requires string field \"handle\""
        }));
    };
    let handle = handle.trim();
    if handle.is_empty() {
        return Ok(json!({
            "error": "read_tool_result field \"handle\" must not be empty"
        }));
    }
    let start_byte = match args.get("start_byte") {
        Some(v) if v.is_i64() && v.as_i64().unwrap() >= 1 => v.as_i64().unwrap() as usize,
        Some(_) => {
            return Ok(json!({
                "error": "read_tool_result field \"start_byte\" must be a positive integer"
            }));
        }
        None => 1,
    };
    let byte_count = match args.get("byte_count") {
        Some(v) if v.is_i64() && v.as_i64().unwrap() >= 1 => {
            (v.as_i64().unwrap() as usize).min(result_store::read_max_bytes)
        }
        Some(_) => {
            return Ok(json!({
                "error": "read_tool_result field \"byte_count\" must be a positive integer"
            }));
        }
        None => 0,
    };
    let query = args.get("query").map(|v| v.as_str().unwrap_or_default());

    let dir = result_store::result_dir();

    let output = match query {
        Some(q) if !q.is_empty() => result_store::search_by_query(&dir, handle, q),
        _ => result_store::read_by_range(&dir, handle, start_byte, byte_count),
    };

    match output {
        Ok(text) => Ok(json!({ "output": text })),
        Err(e) => {
            let msg = if e.to_string().contains("reading result") {
                format!(
                    "read_tool_result failed for handle {handle}: ResultHandleNotFound. No exact match exists in the active tool-result store; handles are session-scoped and must be copied exactly from the tool result preview."
                )
            } else {
                format!("read_tool_result failed for handle {handle}: {}", e)
            };
            Ok(json!({ "error": msg }))
        }
    }
}
