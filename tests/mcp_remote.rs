//! E2E remote MCP test against the bundled fake streamable-HTTP server
//! (`tests/fake_mcp_server.py`). Skips when python3 is unavailable or the
//! server is not running (set FXRS_TEST_MCP_PORT / FXRS_TEST_MCP_URL).

use fxrs::config::McpServerConfig;

fn remote_cfg(name: &str, transport: &str, url: String) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        transport: Some(transport.into()),
        command: None,
        args: vec![],
        env: Default::default(),
        url: Some(url),
        headers: Default::default(),
        header_env: Default::default(),
        bearer_token_env: None,
        auth: None,
        allow_stored_credentials: None,
        required: Some(true),
        enabled: Some(true),
        startup_timeout_ms: None,
        operation_timeout_ms: Some(5000),
    }
}

fn server_url() -> Option<String> {
    if let Ok(u) = std::env::var("FXRS_TEST_MCP_URL") {
        if !u.is_empty() {
            return Some(u);
        }
    }
    let port = std::env::var("FXRS_TEST_MCP_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(18765);
    // Only use the default port if something is actually listening there.
    let probe = std::env::var("FXRS_TEST_MCP_PORT").is_ok()
        || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
    probe.then(|| format!("http://127.0.0.1:{port}/mcp"))
}

#[test]
fn streamable_http_handshake_list_and_call() {
    let Some(url) = server_url() else {
        eprintln!("fake MCP server not running; skipping (see tests/fake_mcp_server.py)");
        return;
    };
    let cfg = remote_cfg("demo", "http", url);

    let tools = fxrs::mcp::list_tools(&cfg);
    assert_eq!(tools.len(), 2, "expected echo+add tools");
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].name, "add");

    let result = fxrs::mcp::call(
        &cfg,
        "echo",
        serde_json::json!({"text": "hello remote"}),
        std::path::Path::new("."),
    )
    .unwrap();
    assert_eq!(result["content"], "hello remote");
    assert_eq!(result["is_error"], false);

    let result = fxrs::mcp::call(
        &cfg,
        "add",
        serde_json::json!({"a": 2, "b": 41}),
        std::path::Path::new("."),
    )
    .unwrap();
    assert_eq!(result["content"], "43");
}

#[test]
fn streamable_http_rejects_invalid_arguments() {
    let Some(url) = server_url() else {
        return;
    };
    let cfg = remote_cfg("demo", "http", url);
    let err = fxrs::mcp::call(
        &cfg,
        "add",
        serde_json::json!({"a": 1}),
        std::path::Path::new("."),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("missing required property `b`"),
        "{err}"
    );

    let err = fxrs::mcp::call(
        &cfg,
        "add",
        serde_json::json!({"a": "x", "b": 1}),
        std::path::Path::new("."),
    )
    .unwrap_err();
    assert!(err.to_string().contains("type `integer`"), "{err}");
}

#[test]
fn endpoint_validation_rejects_remote_http() {
    let cfg = remote_cfg("bad", "http", "http://192.168.1.5:5000/mcp".into());
    let tools = fxrs::mcp::list_tools(&cfg);
    assert_eq!(tools.len(), 0, "non-loopback http must be rejected");
}
