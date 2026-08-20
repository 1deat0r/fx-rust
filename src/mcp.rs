//! Minimal MCP (Model Context Protocol) stdio client, faithful to fx's
//! `mcpServers` config shape and the `mcp__<server>__<tool>` tool naming.
//!
//! Supports stdio servers: JSON-RPC 2.0 messages framed with
//! `Content-Length` headers (LSP-style). Each call opens a fresh server
//! process, runs the protocol handshake (initialize / notifications/initialized),
//! issues the request, and tears the process down. This trades a little
//! latency for robustness — a misbehaving MCP server can never wedge the
//! agent, and every agent turn gets a clean process.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Value, json};

use crate::config::McpServerConfig;

pub const PROTOCOL_VERSION: &str = "2025-03-26";
pub const CALL_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn prefixed_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

pub fn parse_prefixed(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server.to_string(), tool.to_string()))
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(cfg: &McpServerConfig) -> Result<Session> {
    let command = cfg.command.clone().context("mcp server has no command")?;
    let mut cmd = Command::new(&command);
    for a in &cfg.args {
        cmd.arg(a);
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = cmd.spawn().with_context(|| format!("spawn MCP server {}", cfg.name))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("MCP server stdin not available"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("MCP server stdout not available"))?;
    Ok(Session { child, stdin, reader: BufReader::new(stdout) })
}

fn write_msg(stdin: &mut ChildStdin, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

fn read_msg(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("MCP server closed stdout");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("MCP frame missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

fn request(session: &mut Session, id: u64, method: &str, params: Value) -> Result<Value> {
    write_msg(
        &mut session.stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )?;
    for _ in 0..500 {
        let msg = read_msg(&mut session.reader)?;
        if let Some(mid) = msg.get("id").and_then(|v| v.as_u64()) {
            if mid == id {
                if let Some(err) = msg.get("error") {
                    bail!("MCP {method} error: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // A response to some other (stale) request — skip.
        } else {
            // Server notification (logging etc.) — skip.
            eprintln!(
                "[fxrs] mcp notification: {}",
                msg.get("method").and_then(|m| m.as_str()).unwrap_or("?")
            );
        }
    }
    bail!("MCP {method}: no response from server")
}

fn handshake(session: &mut Session, server: &str) -> Result<()> {
    let resp = request(
        session,
        1,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "fxrs", "version": crate::version::VERSION },
        }),
    )?;
    if let Some(v) = resp.get("protocolVersion").and_then(|v| v.as_str()) {
        if !v.starts_with("202") && v != PROTOCOL_VERSION {
            eprintln!("[fxrs] MCP server {server} speaks protocol {v}; may be incompatible");
        }
    } else {
        bail!("MCP initialize returned no protocolVersion");
    }
    write_msg(&mut session.stdin, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
    Ok(())
}

/// List tools exposed by one server (already configured). Returns empty on
/// server failure (logged) so a broken server never aborts the agent.
pub fn list_tools(cfg: &McpServerConfig) -> Vec<McpTool> {
    let name = cfg.name.clone();
    let cfg = cfg.clone();
    let out = with_timeout(CALL_TIMEOUT_SECS, move || {
        let mut session = spawn(&cfg)?;
        handshake(&mut session, &cfg.name)?;
        let result = request(&mut session, 2, "tools/list", json!({}))?;
        let mut tools = Vec::new();
        if let Some(arr) = result.get("tools").and_then(|t| t.as_array()) {
            for t in arr {
                tools.push(McpTool {
                    server: cfg.name.clone(),
                    name: t.get("name").and_then(|n| n.as_str()).unwrap_or("unnamed").to_string(),
                    description: t.get("description").and_then(|d| d.as_str()).unwrap_or_default().to_string(),
                    input_schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
        }
        Ok::<_, anyhow::Error>(tools)
    });
    match out {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("[fxrs] MCP server {name} unavailable: {e:#}");
            Vec::new()
        }
    }
}

/// Call `tool` on `server` with `arguments`. Returns a JSON result:
/// {content: <concatenated text>, is_error: bool}.
pub fn call(server_cfg: &McpServerConfig, tool: &str, arguments: Value) -> Result<Value> {
    let server_cfg = server_cfg.clone();
    let tool = tool.to_string();
    with_timeout(CALL_TIMEOUT_SECS, move || {
        let mut session = spawn(&server_cfg)?;
        handshake(&mut session, &server_cfg.name)?;
        let result = request(
            &mut session,
            3,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )?;
        let mut text = String::new();
        let mut is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
            for item in arr {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                    text.push('\n');
                } else if item.get("type").and_then(|v| v.as_str()) == Some("image") {
                    text.push_str("[image result omitted]\n");
                }
            }
        }
        if text.trim().is_empty() {
            if let Some(sc) = result.get("structuredContent") {
                text = serde_json::to_string_pretty(sc).unwrap_or_default();
            } else {
                text = serde_json::to_string(&result).unwrap_or_default();
            }
        }
        if let Some(e) = result.get("error").and_then(|e| e.as_str()) {
            text = format!("{e}\n{text}");
            is_error = true;
        }
        Ok(json!({ "content": text.trim_end().to_string(), "is_error": is_error }))
    })
}

/// Run `f` on a worker thread, aborting after `secs` seconds.
fn with_timeout<T: Send + 'static>(secs: u64, f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("MCP call timed out after {secs}s")),
    }
}

/// All configured servers' tools, flattened (per-server failures logged).
pub fn list_all_tools(servers: &[McpServerConfig]) -> Vec<McpTool> {
    let mut out = Vec::new();
    for cfg in servers {
        out.extend(list_tools(cfg));
    }
    out
}

/// Execute a `mcp__<server>__<tool>` call given the full config list.
pub fn execute_mcp(name: &str, args: &Value, servers: &[McpServerConfig]) -> Value {
    let Some((server, tool)) = parse_prefixed(name) else {
        return json!({ "error": format!("invalid MCP tool name: {name}") });
    };
    let Some(cfg) = servers.iter().find(|c| c.name == server) else {
        return json!({ "error": format!("MCP server not configured: {server}") });
    };
    let args_val = args.get("arguments").cloned().unwrap_or_else(|| args.clone());
    match call(cfg, &tool, args_val) {
        Ok(v) => v,
        Err(e) => json!({ "error": format!("MCP {server}/{tool}: {e:#}") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_names_roundtrip() {
        assert_eq!(prefixed_name("fetch", "http_fetch"), "mcp__fetch__http_fetch");
        let (s, t) = parse_prefixed("mcp__fetch__http_fetch").unwrap();
        assert_eq!((s.as_str(), t.as_str()), ("fetch", "http_fetch"));
        assert!(parse_prefixed("run_command").is_none());
        assert!(parse_prefixed("mcp__only_server").is_none());
    }
}
