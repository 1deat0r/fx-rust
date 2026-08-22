//! Integration: the read_tool_result tool reads back a stored result handle.

use serde_json::{json, Value};
use std::path::PathBuf;

// Use std temp dirs like the unit tests (tempfile isn't a dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("fxrs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn read_tool_result_tool_lifecycle() {
    let _results = TempDir::new("results");
    let workspace = TempDir::new("workspace");
    let home = std::env::temp_dir().join(format!("fxrs-home-{}", std::process::id()));
    std::env::set_var("FX_HOME", &home);

    // Store a large fake "tool" result directly through the store, under the
    // same FX_HOME the tool resolves.
    let dup: String = "line\n".repeat(12_000); // > 16KB
    let store_dir = fxrs::result_store::result_dir();
    let prepared = fxrs::result_store::prepare(
        Some(&store_dir),
        "call-integration-1",
        "run_command",
        dup.as_bytes(),
        4096,
    );
    let handle = prepared.output_handle.expect("large result should store");
    assert!(prepared.model_output.contains("<tool_result_preview"));
    assert!(prepared.model_output.contains("Use read_tool_result"));

    // Now invoke the tool with that handle.
    let cfg = fxrs::config::resolve(workspace.path()).expect("resolve config");
    let ctx = fxrs::tools::ToolContext {
        workspace: workspace.path().to_path_buf(),
        max_result_bytes: 64 * 1024,
        interactive: false,
        config: std::sync::Arc::new(cfg),
        store: fxrs::sessions::SessionStore::new().unwrap(),
        session_id: "s-test".to_string(),
    };
    let args = json!({ "handle": handle });
    let result: Value = fxrs::tools::read_tool_result::call(&ctx, &args).unwrap();
    let out = result.get("output").and_then(|v| v.as_str()).unwrap_or("");
    assert!(out.contains("<tool_result handle="));
    assert!(out.contains("line"));

    // Query read.
    let qargs = json!({ "handle": handle.clone(), "query": "line" });
    let qres = fxrs::tools::read_tool_result::call(&ctx, &qargs).unwrap();
    assert!(qres
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("<tool_result"));

    // Unknown handle yields the exact upstream not-found message.
    let bad = json!({ "handle": "unknown-dogfood-handle" });
    let bres = fxrs::tools::read_tool_result::call(&ctx, &bad).unwrap();
    let err = bres.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(err.contains("ResultHandleNotFound"), "got: {err}");
    assert!(err.contains("handles are session-scoped"));
    let _ = std::fs::remove_dir_all(&home);
}
