//! Manual integration check: call a remote MCP server through fxrs's mcp module.
use serde_json::Value;

fn main() {
    let url = std::env::var("FXRS_MCP_URL").expect("FXRS_MCP_URL");
    let tool = std::env::args().nth(1).unwrap_or_else(|| "echo".into());
    let args_text = std::env::args()
        .nth(2)
        .unwrap_or_else(|| r#"{"text":"hi"}"#.into());
    let args: Value = serde_json::from_str(&args_text).expect("args json");

    let cfg = fxrs::config::McpServerConfig {
        name: "demo".into(),
        transport: Some("http".into()),
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
        operation_timeout_ms: None,
    };

    match fxrs::mcp::call(&cfg, &tool, args, std::path::Path::new(".")) {
        Ok(v) => println!("OK {}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("ERR {e:#}");
            std::process::exit(1);
        }
    }
}
