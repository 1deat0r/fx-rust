//! Remote MCP wire transports: modern streamable HTTP and legacy HTTP+SSE.
//!
//! Faithful to upstream fx's `src/core/mcp/streamable_http.zig`,
//! `legacy_streamable_http.zig`, and `legacy_http_sse.zig`: endpoint
//! validation, protocol-version negotiation with -32022 fallback, session
//! id header round-trips, and SSE framing. Clients are per-call (fresh
//! connection per request) mirroring the stdio per-call design in `mcp.rs`,
//! so a broken or hung remote server can never wedge the agent.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::McpServerConfig;

// Protocol versions, mirroring upstream `protocol_negotiation.zig`.
pub const LEGACY_PROTOCOL_VERSION_2024: &str = "2024-11-05";
pub const LEGACY_PROTOCOL_VERSION_2025_06: &str = "2025-06-18";
pub const LEGACY_PROTOCOL_VERSION_2025_11: &str = "2025-11-25";
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// JSON-RPC error code a server uses to report an unsupported protocol version.
pub const UNSUPPORTED_PROTOCOL_VERSION_CODE: i64 = -32022;

/// Streamable HTTP supported versions, in preference order (upstream
/// `legacy_streamable_http.zig` `supported_versions`).
pub const HTTP_SUPPORTED_VERSIONS: [&str; 3] = [
    LEGACY_PROTOCOL_VERSION_2025_11,
    LEGACY_PROTOCOL_VERSION_2025_06,
    "2025-03-26",
];

/// Legacy HTTP+SSE versions ... plus the oldest legacy protocol for raw
/// `sse` endpoints.
pub const SSE_SUPPORTED_VERSIONS: [&str; 4] = [
    LEGACY_PROTOCOL_VERSION_2025_11,
    LEGACY_PROTOCOL_VERSION_2025_06,
    "2025-03-26",
    LEGACY_PROTOCOL_VERSION_2024,
];

pub const MAX_SSE_EVENTS: usize = 1024;
pub const MAX_SSE_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TOOL_SCHEMA_DEPTH: usize = 64;

/// HTTP auth rejection classification (upstream `AuthRejection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRejection {
    None,
    Unauthorized,
    InsufficientScope,
}

// ---------------------------------------------------------------------------
// Endpoint validation
// ---------------------------------------------------------------------------

/// Validate an MCP endpoint: https is always allowed; plain http is allowed
/// only for loopback hosts, matching upstream `streamable_http.validateEndpoint`.
pub fn validate_endpoint(url: &str) -> Result<()> {
    let url = url.trim();
    let (scheme, rest) = if let Some(r) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("HTTPS://"))
    {
        ("https", r)
    } else if let Some(r) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
    {
        ("http", r)
    } else {
        bail!("MCP endpoint `{url}` must be an http(s) URL");
    };
    if let Some(pos) = rest.find(['/', '?', '#']) {
        let head = &rest[..pos];
        if !head.is_empty()
            && (rest[pos..].starts_with('/') || rest[pos..].starts_with('?'))
            && rest[pos..].starts_with('#')
        {
            bail!("MCP endpoint `{url}` must not contain a fragment");
        }
    }
    if rest.contains('#') {
        bail!("MCP endpoint `{url}` must not contain a fragment");
    }
    // userinfo: everything before the first '@' before any '/' is userinfo.
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    if authority.contains('@') {
        bail!("MCP endpoint `{url}` must not contain userinfo");
    }
    if scheme == "http" {
        let host = host_only(authority);
        let loopback = host == "localhost"
            || host == "127.0.0.1"
            || host == "[::1]"
            || host
                .parse::<std::net::Ipv4Addr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !loopback {
            bail!(
                "MCP endpoint `{url}` uses insecure http with a non-loopback host; \
                 switch to https or allowlist the host"
            );
        }
    }
    Ok(())
}

fn host_only(authority: &str) -> &str {
    // IPv6 literal is bracketed: [::1]:8080
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &authority[..=end + 1];
        }
    }
    // Last colon separates port.
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    }
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub kind: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry_ms: Option<u64>,
}

/// Incremental SSE parser (upstream `legacy_sse.zig` `Parser`).
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    kind: Vec<u8>,
    id: Vec<u8>,
    retry_ms: Option<u64>,
    saw_data: bool,
    saw_kind: bool,
    saw_id: bool,
    total_bytes: usize,
    pub event_count: usize,
    max_total_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    pending_cr: bool,
}

impl SseParser {
    pub fn new(max_total_bytes: usize, max_event_bytes: usize, max_events: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            kind: Vec::new(),
            id: Vec::new(),
            retry_ms: None,
            saw_data: false,
            saw_kind: false,
            saw_id: false,
            total_bytes: 0,
            event_count: 0,
            max_total_bytes,
            max_event_bytes,
            max_events,
            pending_cr: false,
        }
    }

    /// Feed bytes; completed events are appended to `out`.
    pub fn feed(&mut self, buf: &[u8], out: &mut Vec<SseEvent>) -> Result<()> {
        for &b in buf {
            self.total_bytes += 1;
            if self.total_bytes > self.max_total_bytes {
                bail!(
                    "SSE stream exceeded {self} byte limit",
                    self = self.max_total_bytes
                );
            }
            if self.pending_cr {
                self.pending_cr = false;
                // The preceding '\r' already terminates the line; a following
                // '\n' is folded into it. Either way, dispatch the line now.
                let was_blank = self.dispatch_line()?;
                if b == b'\n' {
                    if was_blank {
                        self.dispatch_event(out);
                    }
                    continue;
                }
                if was_blank {
                    self.dispatch_event(out);
                }
                if b == b'\r' {
                    self.pending_cr = true;
                    continue;
                }
            }
            match b {
                b'\r' => {
                    self.pending_cr = true;
                }
                b'\n' => {
                    if self.dispatch_line()? && self.line.is_empty() {
                        self.dispatch_event(out);
                    }
                }
                _ => self.line.push(b),
            }
        }
        Ok(())
    }

    /// Flush any trailing unterminated line and pending event at EOF.
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) -> Result<()> {
        if self.pending_cr {
            self.pending_cr = false;
            self.dispatch_line()?;
        }
        if !self.line.is_empty() {
            self.dispatch_line()?;
        }
        self.dispatch_event(out);
        Ok(())
    }

    /// Process one complete line. Returns true when the line was blank (the
    /// SSE event boundary), so the caller can dispatch a completed event.
    fn dispatch_line(&mut self) -> Result<bool> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            return Ok(true);
        }
        // Comments (lines starting with ':') are ignored.
        if line.first() == Some(&b':') {
            return Ok(false);
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(pos) => (&line[..pos], &line[pos + 1..]),
            None => (&line[..], &[][..]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"data" => {
                self.saw_data = true;
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
            b"event" => {
                self.saw_kind = true;
                self.kind.clear();
                self.kind.extend_from_slice(value);
            }
            b"id" => {
                self.saw_id = true;
                self.id.clear();
                // NUL bytes are dropped per the SSE spec.
                self.id.extend(value.iter().copied().filter(|&b| b != 0));
            }
            b"retry" => {
                self.retry_ms = String::from_utf8_lossy(value).trim().parse::<u64>().ok();
            }
            _ => {}
        }
        Ok(false)
    }

    fn dispatch_event(&mut self, out: &mut Vec<SseEvent>) {
        if !(self.saw_data || self.saw_kind || self.saw_id) {
            return;
        }
        if self.event_count >= self.max_events {
            return;
        }
        if self.data.len() > self.max_event_bytes {
            return;
        }
        self.event_count += 1;
        let data = String::from_utf8_lossy(&self.data).to_string();
        // Trim the single trailing newline appended by field concatenation.
        let data = data.strip_suffix('\n').unwrap_or(&data).to_string();
        let kind = if self.saw_kind {
            Some(String::from_utf8_lossy(&self.kind).to_string())
        } else {
            None
        };
        let id = if self.saw_id {
            Some(String::from_utf8_lossy(&self.id).to_string())
        } else {
            None
        };
        out.push(SseEvent {
            kind,
            data,
            id,
            retry_ms: self.retry_ms,
        });
        self.data.clear();
        self.kind.clear();
        self.id.clear();
        self.retry_ms = None;
        self.saw_data = false;
        self.saw_kind = false;
        self.saw_id = false;
    }
}

/// Parse a complete byte buffer as one or more SSE events.
pub fn parse_sse(buf: &[u8]) -> Result<Vec<SseEvent>> {
    let mut parser = SseParser::new(MAX_SSE_TOTAL_BYTES, MAX_EVENT_BYTES, MAX_SSE_EVENTS);
    let mut out = Vec::new();
    parser.feed(buf, &mut out)?;
    parser.finish(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Common request plumbing
// ---------------------------------------------------------------------------

fn split_header_pairs(cfg: &McpServerConfig) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in &cfg.headers {
        out.push((k.clone(), v.clone()));
    }
    for (k, v) in cfg.resolved_header_env() {
        if !out.iter().any(|(ek, _)| ek.eq_ignore_ascii_case(&k)) {
            out.push((k, v));
        }
    }
    out
}

fn apply_request(
    req: ureq::Request,
    headers: &[(String, String)],
    bearer: Option<&str>,
    session_id: Option<&str>,
    protocol_version: &str,
    timeout: Duration,
) -> ureq::Request {
    let mut req = req
        .timeout(timeout)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .set("Mcp-Protocol-Version", protocol_version);
    if let Some(sid) = session_id {
        req = req.set("Mcp-Session-Id", sid);
    }
    if let Some(token) = bearer {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    for (k, v) in headers {
        req = req.set(k, v);
    }
    req
}

fn read_body(reader: Box<dyn Read + Send + Sync + 'static>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(MAX_RESPONSE_BYTES as u64)
        .read_to_end(&mut buf)
        .context("read MCP response body")?;
    Ok(buf)
}

/// Inspect a possibly JSON-RPC response body for a -32022 version error.
fn json_version_error(v: &Value) -> bool {
    v.get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_i64())
        == Some(UNSUPPORTED_PROTOCOL_VERSION_CODE)
}

// ---------------------------------------------------------------------------
// Streamable HTTP transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpPostOutcome {
    pub value: Value,
    pub version_error: bool,
    pub auth_rejection: AuthRejection,
    pub session_id: Option<String>,
    pub server_protocol_version: Option<String>,
}

/// One JSON-RPC round trip over the streamable HTTP transport.
/// `session_id` is the client-side session id to echo (carried by the caller
/// who owns the connection state).
pub fn http_post(
    url: &str,
    body: &Value,
    protocol_version: &str,
    session_id: Option<&str>,
    static_headers: &[(String, String)],
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<HttpPostOutcome> {
    let req = apply_request(
        ureq::post(url),
        static_headers,
        bearer,
        session_id,
        protocol_version,
        timeout,
    );
    let payload = serde_json::to_vec(body)?;
    let payload_str = String::from_utf8_lossy(&payload).into_owned();
    let resp = match req.send_string(&payload_str) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, resp)) => {
            let status = code;
            if status == 401 {
                return Ok(HttpPostOutcome {
                    value: Value::Null,
                    version_error: false,
                    auth_rejection: AuthRejection::Unauthorized,
                    session_id: None,
                    server_protocol_version: None,
                });
            }
            if status == 403 {
                return Ok(HttpPostOutcome {
                    value: Value::Null,
                    version_error: false,
                    auth_rejection: AuthRejection::InsufficientScope,
                    session_id: None,
                    server_protocol_version: None,
                });
            }
            // Some servers return 400 with a JSON-RPC error body (e.g. the
            // modern-version retry signal). Read it and classify.
            let mut buf = Vec::new();
            resp.into_reader()
                .take(MAX_RESPONSE_BYTES as u64)
                .read_to_end(&mut buf)
                .context("read MCP error response body")?;
            if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                let version_error = json_version_error(&v);
                return Ok(HttpPostOutcome {
                    value: v,
                    version_error,
                    auth_rejection: AuthRejection::None,
                    session_id: None,
                    server_protocol_version: None,
                });
            }
            bail!(
                "MCP HTTP {status}: {}",
                String::from_utf8_lossy(&buf)
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
        Err(e) => return Err(e).context("MCP HTTP request failed"),
    };

    let server_protocol_version = resp.header("Mcp-Protocol-Version").map(str::to_string);
    let session_id_header = resp.header("Mcp-Session-Id").map(str::to_string);
    let content_type = resp
        .header("Content-Type")
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = read_body(resp.into_reader())?;

    let (value, sse_session) = if content_type.contains("text/event-stream") {
        parse_event_stream_json(&body).context("parse MCP SSE response")?
    } else {
        let v: Value = serde_json::from_slice(&body).context("parse MCP JSON response")?;
        (v, None)
    };
    let version_error = json_version_error(&value);
    Ok(HttpPostOutcome {
        value,
        version_error,
        auth_rejection: AuthRejection::None,
        session_id: session_id_header.or(sse_session),
        server_protocol_version,
    })
}

/// Extract the first JSON-RPC value from an SSE response stream, plus any
/// `mcp-session-id` event observed along the way.
fn parse_event_stream_json(buf: &[u8]) -> Result<(Value, Option<String>)> {
    let events = parse_sse(buf)?;
    let mut session_id = None;
    let mut first_json: Option<Value> = None;
    for ev in &events {
        if ev.kind.as_deref() == Some("mcp-session-id") {
            session_id = Some(ev.data.clone());
            continue;
        }
        if first_json.is_none() {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                if v.get("jsonrpc").is_some()
                    || v.get("result").is_some()
                    || v.get("error").is_some()
                {
                    first_json = Some(v);
                }
            }
        }
    }
    let value =
        first_json.ok_or_else(|| anyhow!("MCP SSE stream contained no JSON-RPC message"))?;
    Ok((value, session_id))
}

// ---------------------------------------------------------------------------
// Legacy HTTP+SSE transport
// ---------------------------------------------------------------------------

/// A live legacy SSE connection: the long-lived GET stream plus the POST
/// endpoint discovered from the `endpoint` event.
pub struct LegacySseConn {
    reader: Box<dyn Read + Send + Sync + 'static>,
    post_url: String,
    session_id: Option<String>,
    last_event_id: Option<String>,
    bearer: Option<String>,
    static_headers: Vec<(String, String)>,
    timeout: Duration,
}

impl LegacySseConn {
    /// Open the GET stream and discover the POST endpoint.
    pub fn open(
        url: &str,
        static_headers: &[(String, String)],
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<LegacySseConn> {
        let mut req = ureq::get(url)
            .timeout(timeout)
            .set("Accept", "text/event-stream");
        if let Some(token) = bearer {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in static_headers {
            req = req.set(k, v);
        }
        let resp = req.call().context("MCP SSE GET failed")?;
        let mut reader = resp.into_reader();
        let mut parser = SseParser::new(MAX_SSE_TOTAL_BYTES, MAX_EVENT_BYTES, MAX_SSE_EVENTS);
        let mut events = Vec::new();
        let mut buf = [0u8; 8192];
        let mut post_url: Option<String> = None;
        let mut session_id: Option<String> = None;
        // Read until we discover the endpoint event (bounded).
        for _ in 0..4096 {
            let n = reader.read(&mut buf).context("read MCP SSE stream")?;
            if n == 0 {
                break;
            }
            parser.feed(&buf[..n], &mut events)?;
            for ev in &events {
                if ev.kind.as_deref() == Some("endpoint") {
                    post_url = Some(ev.data.clone());
                } else if ev.kind.as_deref() == Some("mcp-session-id") {
                    session_id = Some(ev.data.clone());
                }
            }
            if post_url.is_some() {
                break;
            }
        }
        let post_url =
            post_url.ok_or_else(|| anyhow!("MCP SSE stream never sent an `endpoint` event"))?;
        Ok(LegacySseConn {
            reader,
            post_url,
            session_id,
            last_event_id: None,
            bearer: bearer.map(str::to_string),
            static_headers: static_headers.to_vec(),
            timeout,
        })
    }

    /// Send one request and await the matching response on the stream.
    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut req = ureq::post(&self.post_url)
            .timeout(self.timeout)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            req = req.set("Mcp-Session-Id", sid);
        }
        if let Some(eid) = &self.last_event_id {
            req = req.set("Last-Event-ID", eid);
        }
        if let Some(token) = &self.bearer {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in &self.static_headers {
            req = req.set(k, v);
        }
        let payload = serde_json::to_string(&body)?;
        let resp = req.send_string(&payload).context("MCP SSE POST failed")?;
        // The response body comes through the long-lived GET stream; a server
        // may also return an immediate JSON body (drain it to keep the stream healthy).
        let mut drain = Vec::new();
        resp.into_reader()
            .take(1024 * 1024)
            .read_to_end(&mut drain)
            .ok();
        if let Ok(v) = serde_json::from_slice::<Value>(&drain) {
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return Ok(v);
            }
        }
        // Otherwise read the SSE stream for a `message` event with our id.
        let mut parser = SseParser::new(MAX_SSE_TOTAL_BYTES, MAX_EVENT_BYTES, MAX_SSE_EVENTS);
        let mut events = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = self.reader.read(&mut buf).context("read MCP SSE stream")?;
            if n == 0 {
                bail!("MCP SSE stream closed before response to {method}");
            }
            parser.feed(&buf[..n], &mut events)?;
            for ev in &events {
                if ev.kind.as_deref() == Some("endpoint") {
                    self.post_url = ev.data.clone();
                } else if ev.kind.as_deref() == Some("mcp-session-id") {
                    self.session_id = Some(ev.data.clone());
                }
                if let Some(eid) = &ev.id {
                    self.last_event_id = Some(eid.clone());
                }
                if ev.kind.as_deref() == Some("message")
                    || (ev.kind.is_none() && ev.data.starts_with('{'))
                {
                    if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                        if v.get("id").and_then(|i| i.as_u64()) == Some(id)
                            || (v.get("id").is_none() && v.get("method").is_none())
                        {
                            // Match by id when present; accept id-less message-complete.
                            if v.get("id").is_none() {
                                continue;
                            }
                            return Ok(v);
                        }
                    }
                }
            }
            events.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// High-level per-call remote client
// ---------------------------------------------------------------------------

pub enum RemoteClient {
    /// Modern streamable HTTP (per-request session id, version negotiated).
    Http {
        url: String,
        static_headers: Vec<(String, String)>,
        bearer: Option<String>,
        protocol_version: String,
        timeout: Duration,
    },
    /// Legacy HTTP+SSE (long-lived GET stream + POST endpoint).
    Sse(LegacySseConn),
}

impl RemoteClient {
    /// Create a remote client for a server config, validating the endpoint.
    pub fn open(cfg: &McpServerConfig) -> Result<RemoteClient> {
        let url = cfg
            .remote_url()
            .ok_or_else(|| anyhow!("MCP server `{}` has no url", cfg.name))?;
        validate_endpoint(url)?;
        let headers = split_header_pairs(cfg);
        let bearer = cfg.bearer_token();
        let timeout = Duration::from_millis(cfg.operation_timeout_ms.unwrap_or(90_000));
        match cfg.transport_kind() {
            crate::config::McpTransport::Http => Ok(RemoteClient::Http {
                url: url.to_string(),
                static_headers: headers,
                bearer,
                protocol_version: HTTP_SUPPORTED_VERSIONS[0].to_string(),
                timeout,
            }),
            crate::config::McpTransport::Sse => {
                let conn = LegacySseConn::open(url, &headers, bearer.as_deref(), timeout)?;
                Ok(RemoteClient::Sse(conn))
            }
            crate::config::McpTransport::Stdio => {
                bail!("MCP server `{}` is stdio; use the stdio path", cfg.name)
            }
        }
    }

    /// One JSON-RPC round trip, negotiating protocol versions on -32022.
    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        match self {
            RemoteClient::Http {
                url,
                static_headers,
                bearer,
                protocol_version,
                timeout,
            } => {
                let mut last_err: Option<anyhow::Error> = None;
                for v in HTTP_SUPPORTED_VERSIONS {
                    let body = if method == "initialize" {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": method,
                            "params": {
                                "protocolVersion": v,
                                "capabilities": {},
                                "clientInfo": { "name": "fxrs", "version": crate::version::VERSION },
                            },
                        })
                    } else {
                        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                    };
                    {
                        let outcome = http_post(
                            url,
                            &body,
                            v,
                            None,
                            static_headers,
                            bearer.as_deref(),
                            *timeout,
                        )?;
                        if outcome.auth_rejection != AuthRejection::None {
                            bail!(
                                "MCP server requires authentication (HTTP {:?}); configure bearer_token_env or auth",
                                outcome.auth_rejection
                            );
                        }
                        if outcome.version_error {
                            last_err =
                                Some(anyhow!("server rejected protocol version {v} (-32022)"));
                            continue;
                        }
                        if let Some(spv) = &outcome.server_protocol_version {
                            if spv.starts_with("202") {
                                *protocol_version = spv.clone();
                            }
                        }
                        return Self::unwrap_response(outcome.value, method);
                    }
                }
                Err(last_err.unwrap_or_else(|| anyhow!("MCP {method} failed")))
            }
            RemoteClient::Sse(conn) => {
                let mut last_err: Option<anyhow::Error> = None;
                for v in SSE_SUPPORTED_VERSIONS {
                    let body = if method == "initialize" {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": method,
                            "params": {
                                "protocolVersion": v,
                                "capabilities": {},
                                "clientInfo": { "name": "fxrs", "version": crate::version::VERSION },
                            },
                        })
                    } else {
                        params.clone()
                    };
                    match conn.request(id, method, body) {
                        Ok(value) => return Ok(value),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap_or_else(|| anyhow!("MCP {method} failed")))
            }
        }
    }
    fn unwrap_response(value: Value, method: &str) -> Result<Value> {
        if let Some(err) = value.get("error") {
            bail!(
                "MCP {method} error: {}",
                serde_json::to_string(err).unwrap_or_default()
            );
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Build the Initialize params used by all transports (upstream handshake shape).
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": HTTP_SUPPORTED_VERSIONS[0],
        "capabilities": {},
        "clientInfo": { "name": "fxrs", "version": crate::version::VERSION },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpTransport;

    #[test]
    fn endpoint_validation() {
        assert!(validate_endpoint("https://mcp.example.com/sse").is_ok());
        assert!(validate_endpoint("https://example.com").is_ok());
        assert!(validate_endpoint("http://localhost:3000/mcp").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080").is_ok());
        assert!(validate_endpoint("http://[::1]:8080").is_ok());
        assert!(validate_endpoint("http://192.168.1.10:5000").is_err());
        assert!(validate_endpoint("http://evil.com").is_err());
        assert!(validate_endpoint("ftp://example.com").is_err());
        assert!(validate_endpoint("https://user:pass@example.com").is_err());
        assert!(validate_endpoint("https://example.com/#frag").is_err());
    }

    #[test]
    fn host_only_parses_ports_and_ipv6() {
        assert_eq!(host_only("localhost:3000"), "localhost");
        assert_eq!(host_only("[::1]:8080"), "[::1]");
        assert_eq!(host_only("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn sse_parses_basic_events() {
        let buf = "event: endpoint\ndata: https://example.com/message\n\nid: 42\ndata: hello\n\n";
        let events = parse_sse(buf.as_bytes()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "https://example.com/message");
        assert_eq!(events[1].id.as_deref(), Some("42"));
        assert_eq!(events[1].data, "hello");
    }

    #[test]
    fn sse_handles_crlf_and_multiline_data() {
        let buf = "data: line1\r\ndata: line2\r\n\r\n";
        let events = parse_sse(buf.as_bytes()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn sse_ignores_comments_and_unknown_fields() {
        let buf = ": ping\n\nfield: x\n\nretry: 30\n\ndata: hello\n\n";
        let events = parse_sse(buf.as_bytes()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry_ms, Some(30));
    }

    #[test]
    fn sse_carries_mcp_session_id() {
        let buf = "event: mcp-session-id\ndata: abc-123\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let (value, sid) = parse_event_stream_json(buf.as_bytes()).unwrap();
        assert_eq!(sid.as_deref(), Some("abc-123"));
        assert!(value.get("result").is_some());
    }

    #[test]
    fn version_error_detection() {
        let v = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32022,"message":"unsupported protocol version"}});
        assert!(json_version_error(&v));
        let v2 = json!({"jsonrpc":"2.0","id":1,"result":{}});
        assert!(!json_version_error(&v2));
    }

    #[test]
    fn transport_parse_aliases() {
        assert_eq!(McpTransport::parse(None), McpTransport::Stdio);
        assert_eq!(McpTransport::parse(Some("stdio")), McpTransport::Stdio);
        assert_eq!(McpTransport::parse(Some("http")), McpTransport::Http);
        assert_eq!(
            McpTransport::parse(Some("streamable-http")),
            McpTransport::Http
        );
        assert_eq!(McpTransport::parse(Some("sse")), McpTransport::Sse);
        assert_eq!(McpTransport::parse(Some("http-sse")), McpTransport::Sse);
    }

    #[test]
    fn env_expansion() {
        let environ = |k: &str| -> Option<String> {
            match k {
                "TOKEN" => Some("secret".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            McpServerConfig::expand_env_value("Bearer ${TOKEN}", &environ),
            "Bearer secret"
        );
        assert_eq!(
            McpServerConfig::expand_env_value("$TOKEN-x", &environ),
            "secret-x"
        );
        assert_eq!(
            McpServerConfig::expand_env_value("a $MISSING b", &environ),
            "a  b"
        );
        assert_eq!(
            McpServerConfig::expand_env_value("literal $${TOKEN}", &environ),
            "literal ${TOKEN}"
        );
    }
}
