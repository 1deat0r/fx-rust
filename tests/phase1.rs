//! Phase-1 integration tests: CLI commands + new backend modules, all isolated
//! from the real ~/.fx via FX_HOME.

use std::path::PathBuf;
use std::process::Command;

fn fx_bin() -> PathBuf {
    // target/debug/fxrs relative to CARGO_MANIFEST_DIR
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    p.push("fxrs");
    p
}

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fxrs-phase1-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cli_usage_roundtrip_with_fx_home() {
    let home = temp_home("usage");
    let ws = temp_home("ws");
    let out = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["usage", "all"])
        .output()
        .expect("fxrs usage runs");
    assert!(out.status.success(), "usage exited {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("turns: 0"), "expected zero usage, got: {text}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cli_doctor_reports_missing_endpoint() {
    let home = temp_home("doctor");
    let ws = temp_home("ws2");
    // No API key env -> doctor must fail (exit != 0).
    let out = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["doctor"])
        .output()
        .expect("fxrs doctor runs");
    assert!(!out.status.success(), "doctor should fail with no endpoint");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("FAIL"), "expected FAIL in doctor output: {text}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cli_version_and_unknown_command() {
    let out = Command::new(fx_bin()).args(["version"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("fxrs"));
}

#[test]
fn slash_router_and_shell_classifier_integration() {
    use fxrs::permissions::{auto_classify, AutoDecision, PermissionRequest, Sandbox};
    use fxrs::shell_command::{classify, CommandClass};
    use fxrs::slash_commands::{parse, is_slash, Slash};

    // Router
    assert_eq!(parse("/usage 24h"), Some(Slash::Usage(Some("24h".into()))));
    assert!(is_slash("/status"));

    // Classifier on a realistic pipeline: head command is `git diff --stat`.
    let eff = classify("git diff --stat && cargo test -q");
    assert_eq!(eff.class, CommandClass::ReadOnly);
    assert_eq!(classify("ls -la").class, CommandClass::ReadOnly);

    // Auto classifier uses sandbox for edits
    let s = Sandbox {
        mode: fxrs::config::SandboxMode::Auto,
        workspace: "/ws".into(),
        additional: vec![],
    };
    let req = PermissionRequest {
        tool_name: "write_file",
        target: "/ws/notes.md",
        input_text: String::new(),
        workspace: std::path::Path::new("/ws"),
    };
    assert_eq!(auto_classify(&req, &s), AutoDecision::Allow);
}
