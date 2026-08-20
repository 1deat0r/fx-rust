//! E2E stdio MCP test against a tiny inline Python server that speaks the
//! real protocol (Content-Length frames, initialize, tools/list, tools/call).

use fxrs::config::McpServerConfig;
use fxrs::mcp;

const PY: &str = r#"
import sys, json
def send(o):
    b = json.dumps(o).encode()
    sys.stdout.write(f"Content-Length: {len(b)}\r\n\r\n")
    sys.stdout.write(b.decode())
    sys.stdout.flush()
def recv():
    hdr = b""
    while b"\r\n\r\n" not in hdr:
        c = sys.stdin.buffer.read(1)
        if not c: raise SystemExit
        hdr += c
    head, _, _ = hdr.partition(b"\r\n\r\n")
    ln = int([l for l in head.split(b"\r\n") if l.lower().startswith(b"content-length")][0].split(b":")[1])
    body = sys.stdin.buffer.read(ln)
    return json.loads(body)
while True:
    msg = recv()
    if msg.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mini","version":"1"}}})
    elif msg.get("method") == "notifications/initialized":
        pass
    elif msg.get("method") == "tools/list":
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"tools":[{"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}})
    elif msg.get("method") == "tools/call":
        text = msg["params"]["arguments"].get("text","")
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":text}]}})
"#;

fn mini_server() -> McpServerConfig {
    McpServerConfig {
        name: "mini".into(),
        transport: Some("stdio".into()),
        command: Some("python3".into()),
        args: vec!["-c".into(), PY.into()],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        required: Some(false),
    }
}

#[test]
fn stdio_handshake_list_and_call() {
    let cfg = mini_server();
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 unavailable; skipping");
        return;
    }
    let tools = mcp::list_tools(&cfg);
    assert_eq!(tools.len(), 1, "expected echo tool");
    assert_eq!(tools[0].name, "echo");

    let result = mcp::call(&cfg, "echo", serde_json::json!({"text": "hello mcp"})).unwrap();
    assert_eq!(result["content"], "hello mcp");
    assert_eq!(result["is_error"], false);

    // Prefix naming helpers.
    assert_eq!(mcp::prefixed_name("mini", "echo"), "mcp__mini__echo");
    assert_eq!(mcp::parse_prefixed("mcp__mini__echo"), Some(("mini".to_string(), "echo".to_string())));
}
