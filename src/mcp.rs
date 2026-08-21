//! MCP (Model Context Protocol) client, faithful to fx's `mcpServers` config
//! shape and the `mcp__<server>__<tool>` tool naming.
//!
//! Supports three transports, selected by the config `transport` key:
//!   - `stdio` (default): JSON-RPC over a child process, LSP-style
//!     `Content-Length` framing.
//!   - `http` / `streamable-http`: modern streamable HTTP transport with
//!     `Mcp-Protocol-Version` negotiation and `-32022` fallback.
//!   - `sse` / `http-sse`: legacy HTTP+SSE (`endpoint` event discovery).
//!
//! Every call opens a fresh connection, runs the protocol handshake
//! (initialize / notifications/initialized), issues the request, and tears
//! the connection down. This trades a little latency for robustness — a
//! misbehaving MCP server can never wedge the agent, and every agent turn
//! gets a clean connection. Remote tool arguments are validated against the
//! server's JSON schema before `tools/call` so invalid args become precise
//! model-visible errors instead of opaque server rejections.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{McpServerConfig, McpTransport};
use crate::mcp_transport::RemoteClient;

pub const PROTOCOL_VERSION: &str = "2025-03-26";
pub const CALL_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Catalog availability (upstream `model_catalog.zig` `Availability`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAvailability {
    Ready,
    Disabled,
    Failed,
    AuthRequired,
}

impl McpAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            McpAvailability::Ready => "ready",
            McpAvailability::Disabled => "disabled",
            McpAvailability::Failed => "failed",
            McpAvailability::AuthRequired => "authentication_required",
        }
    }
}

/// One configured server's discovery outcome (for `fxrs mcp` / `fxrs models`).
#[derive(Debug, Clone)]
pub struct McpServerState {
    pub name: String,
    pub transport: &'static str,
    pub enabled: bool,
    pub availability: McpAvailability,
    pub tool_count: usize,
    pub error: Option<String>,
}

pub fn prefixed_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

pub fn parse_prefixed(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server.to_string(), tool.to_string()))
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

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

fn spawn_stdio(cfg: &McpServerConfig) -> Result<Session> {
    let command = cfg
        .stdio_command()
        .context("mcp server has no command")?
        .to_string();
    let mut cmd = Command::new(&command);
    for a in &cfg.args {
        cmd.arg(a);
    }
    for (k, v) in cfg.resolved_env() {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn MCP server {}", cfg.name))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("MCP server stdin not available"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("MCP server stdout not available"))?;
    Ok(Session {
        child,
        stdin,
        reader: BufReader::new(stdout),
    })
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

fn stdio_request(session: &mut Session, id: u64, method: &str, params: Value) -> Result<Value> {
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

fn stdio_handshake(session: &mut Session, cfg: &McpServerConfig) -> Result<()> {
    let resp = stdio_request(
        session,
        1,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "fxrs", "version": crate::version::VERSION },
        }),
    )?;
    let _ = cfg;
    if let Some(v) = resp.get("protocolVersion").and_then(|v| v.as_str()) {
        if !v.starts_with("202") && v != PROTOCOL_VERSION {
            eprintln!("[fxrs] MCP server speaks protocol {v}; may be incompatible");
        }
    } else {
        bail!("MCP initialize returned no protocolVersion");
    }
    write_msg(
        &mut session.stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// remote transport
// ---------------------------------------------------------------------------

fn remote_initialize(cfg: &McpServerConfig) -> Result<RemoteClient> {
    let mut client = RemoteClient::open(cfg)?;
    // request() negotiates protocol versions on -32022 and returns the
    // initialize result.
    let result = client.request(
        1,
        "initialize",
        json!({ "capabilities": {}, "clientInfo": { "name": "fxrs", "version": crate::version::VERSION } }),
    )?;
    if result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .is_none()
    {
        bail!("MCP initialize returned no protocolVersion");
    }
    Ok(client)
}

/// Probe one server's tools via its configured transport (fallible).
fn list_tools_result(cfg: &McpServerConfig) -> Result<Vec<McpTool>> {
    let cfg = cfg.clone();
    with_timeout(timeout_for(&cfg), move || match cfg.transport_kind() {
        McpTransport::Stdio => {
            let mut session = spawn_stdio(&cfg)?;
            stdio_handshake(&mut session, &cfg)?;
            let result = stdio_request(&mut session, 2, "tools/list", json!({}))?;
            parse_tools(&result, &cfg.name)
        }
        _ => {
            let mut client = remote_initialize(&cfg)?;
            let result = client.request(2, "tools/list", json!({}))?;
            parse_tools(&result, &cfg.name)
        }
    })
}

/// List tools from one server via its configured transport. Returns empty on
/// server failure (logged) so a broken server never aborts the agent.
pub fn list_tools(cfg: &McpServerConfig) -> Vec<McpTool> {
    match list_tools_result(cfg) {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("[fxrs] MCP server {} unavailable: {e:#}", cfg.name);
            Vec::new()
        }
    }
}

fn parse_tools(result: &Value, server: &str) -> Result<Vec<McpTool>> {
    let mut tools = Vec::new();
    if let Some(arr) = result.get("tools").and_then(|t| t.as_array()) {
        for t in arr {
            tools.push(McpTool {
                server: server.to_string(),
                name: t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unnamed")
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            });
        }
    }
    Ok(tools)
}

/// Call `tool` on `server` with `arguments`. Returns a JSON result:
/// {content: <concatenated text>, is_error: bool}.
pub fn call(server_cfg: &McpServerConfig, tool: &str, arguments: Value) -> Result<Value> {
    let server_cfg = server_cfg.clone();
    let tool = tool.to_string();
    with_timeout(timeout_for(&server_cfg), move || {
        match server_cfg.transport_kind() {
            McpTransport::Stdio => {
                let mut session = spawn_stdio(&server_cfg)?;
                stdio_handshake(&mut session, &server_cfg)?;
                // No schema validation on the stdio fast path (fresh process
                // per call); the server reports invalid args directly.
                let result = stdio_request(
                    &mut session,
                    3,
                    "tools/call",
                    json!({ "name": tool, "arguments": arguments }),
                )?;
                tool_result(&result)
            }
            _ => {
                let mut client = remote_initialize(&server_cfg)?;
                let listing = client.request(2, "tools/list", json!({}))?;
                let tools = parse_tools(&listing, &server_cfg.name)?;
                let schema = tools
                    .iter()
                    .find(|t| t.name == tool)
                    .map(|t| t.input_schema.clone());
                if let Some(schema) = schema {
                    if let Err(errors) = crate::mcp_schema::validate(&schema, &arguments) {
                        let joined = errors
                            .iter()
                            .take(5)
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        bail!("invalid arguments for {tool}: {joined}");
                    }
                }
                let result = client.request(
                    3,
                    "tools/call",
                    json!({ "name": tool, "arguments": arguments }),
                )?;
                tool_result(&result)
            }
        }
    })
}

fn tool_result(result: &Value) -> Result<Value> {
    let mut text = String::new();
    let mut is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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
            text = serde_json::to_string(result).unwrap_or_default();
        }
    }
    if let Some(e) = result.get("error").and_then(|e| e.as_str()) {
        text = format!("{e}\n{text}");
        is_error = true;
    }
    Ok(json!({ "content": text.trim_end().to_string(), "is_error": is_error }))
}

fn timeout_for(cfg: &McpServerConfig) -> u64 {
    cfg.operation_timeout_ms.unwrap_or(CALL_TIMEOUT_SECS * 1000) / 1000
}

/// Run `f` on a worker thread, aborting after `secs` seconds.
fn with_timeout<T: Send + 'static>(
    secs: u64,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
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
        if !cfg.is_enabled() {
            continue;
        }
        out.extend(list_tools(cfg));
    }
    out
}

/// Probe every configured server and classify its availability (for the
/// model catalog, `fxrs mcp`, `fxrs models`).
/// Result of a single discovery pass over all configured servers.
pub struct McpDiscovery {
    pub states: Vec<McpServerState>,
    pub tools: Vec<McpTool>,
}

/// Probe every configured server once, classifying availability and
/// collecting tools (for the model catalog, `fxrs mcp`, `fxrs models`).
pub fn discover(servers: &[McpServerConfig]) -> McpDiscovery {
    let mut states = Vec::new();
    let mut tools = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    for cfg in servers {
        if !seen_names.insert(cfg.name.clone()) {
            // Last config wins by name (workspace layer beats profile layer);
            // tools from shadowed entries are dropped this pass.
            continue;
        }
        if !cfg.is_enabled() {
            states.push(McpServerState {
                name: cfg.name.clone(),
                transport: cfg.transport_kind().as_str(),
                enabled: false,
                availability: McpAvailability::Disabled,
                tool_count: 0,
                error: None,
            });
            continue;
        }
        let transport = cfg.transport_kind().as_str();
        let config_error = match cfg.transport_kind() {
            McpTransport::Stdio => {
                if cfg.stdio_command().is_none() {
                    Some("no `command` configured for stdio server".to_string())
                } else {
                    None
                }
            }
            _ => {
                if cfg.remote_url().is_none() {
                    Some(format!("no `url` configured for {transport} server"))
                } else {
                    match crate::mcp_transport::validate_endpoint(
                        cfg.remote_url().unwrap_or_default(),
                    ) {
                        Ok(()) => None,
                        Err(e) => Some(e.to_string()),
                    }
                }
            }
        };
        if let Some(err) = config_error {
            states.push(McpServerState {
                name: cfg.name.clone(),
                transport,
                enabled: true,
                availability: McpAvailability::Failed,
                tool_count: 0,
                error: Some(err),
            });
            continue;
        }
        let auth_required = cfg.transport_kind() != McpTransport::Stdio
            && cfg.bearer_token_env.is_some()
            && cfg.bearer_token().is_none();
        if auth_required {
            states.push(McpServerState {
                name: cfg.name.clone(),
                transport,
                enabled: true,
                availability: McpAvailability::AuthRequired,
                tool_count: 0,
                error: Some(format!(
                    "bearer_token_env `{}` is unset",
                    cfg.bearer_token_env.as_deref().unwrap_or("")
                )),
            });
            continue;
        }
        match list_tools_result(cfg) {
            Ok(found) => {
                tools.extend(found.iter().cloned());
                states.push(McpServerState {
                    name: cfg.name.clone(),
                    transport,
                    enabled: true,
                    availability: McpAvailability::Ready,
                    tool_count: found.len(),
                    error: None,
                });
            }
            Err(e) => {
                states.push(McpServerState {
                    name: cfg.name.clone(),
                    transport,
                    enabled: true,
                    availability: McpAvailability::Failed,
                    tool_count: 0,
                    error: Some(format!("{e:#}")),
                });
            }
        }
    }
    McpDiscovery { states, tools }
}

/// Execute a `mcp__<server>__<tool>` call given the full config list.
pub fn execute_mcp(name: &str, args: &Value, servers: &[McpServerConfig]) -> Value {
    let Some((server, tool)) = parse_prefixed(name) else {
        return json!({ "error": format!("invalid MCP tool name: {name}") });
    };
    let Some(cfg) = servers.iter().find(|c| c.name == server) else {
        return json!({ "error": format!("MCP server not configured: {server}") });
    };
    if !cfg.is_enabled() {
        return json!({ "error": format!("MCP server `{server}` is disabled") });
    }
    let args_val = args
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| args.clone());
    match call(cfg, &tool, args_val) {
        Ok(v) => v,
        Err(e) => {
            let text = format!("MCP {server}/{tool}: {e:#}");
            json!({ "content": text, "is_error": true })
        }
    }
}

/// Summarize auth state for a remote server (used by diagnostics).
pub fn auth_hint(cfg: &McpServerConfig) -> Option<String> {
    match cfg.transport_kind() {
        McpTransport::Stdio => None,
        _ => {
            if cfg.bearer_token().is_some() {
                None
            } else if cfg.bearer_token_env.is_some() {
                Some(format!(
                    "set {} to authenticate",
                    cfg.bearer_token_env.as_deref().unwrap_or("")
                ))
            } else if cfg.auth.is_some() {
                Some("OAuth auth configured (not yet negotiated)".to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_names_roundtrip() {
        assert_eq!(
            prefixed_name("fetch", "http_fetch"),
            "mcp__fetch__http_fetch"
        );
        let (s, t) = parse_prefixed("mcp__fetch__http_fetch").unwrap();
        assert_eq!((s.as_str(), t.as_str()), ("fetch", "http_fetch"));
        assert!(parse_prefixed("run_command").is_none());
        assert!(parse_prefixed("mcp__only_server").is_none());
    }

    #[test]
    fn tool_result_formats() {
        let r = json!({
            "content": [{ "type": "text", "text": "hello" }],
            "isError": false
        });
        assert_eq!(tool_result(&r).unwrap()["content"], "hello");
        let r2 = json!({ "structuredContent": { "a": 1 } });
        assert!(tool_result(&r2).unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("\"a\""));
    }
}
