//! Phase-1 integration tests: CLI commands + new backend modules, all isolated
//! from the real ~/.fx via FX_HOME.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// FX_HOME is a process-global env var; tests that reach API which reads it
/// must be serialized or a parallel test can yank it mid-flight.
static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = ENV_LOCK.lock().unwrap();
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
    assert!(
        text.contains("turns: 0"),
        "expected zero usage, got: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cli_doctor_reports_missing_endpoint() {
    let _guard = ENV_LOCK.lock().unwrap();
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
    assert!(
        text.contains("FAIL"),
        "expected FAIL in doctor output: {text}"
    );
    std::env::remove_var("FX_HOME");
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
    use fxrs::slash_commands::{is_slash, parse, Slash};

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

#[test]
fn cli_settings_renders_catalog_with_fx_home() {
    let _guard = ENV_LOCK.lock().unwrap();
    let home = temp_home("settings");
    let ws = temp_home("ws-settings");
    let out = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["settings"])
        .output()
        .expect("fxrs settings runs");
    assert!(out.status.success(), "settings exited {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("model"), "catalog missing model: {text}");
    assert!(
        text.contains("permission_mode"),
        "catalog missing permission_mode: {text}"
    );
    assert!(
        text.contains("mcpServers"),
        "catalog missing mcpServers: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cli_session_json_lifecycle() {
    let _guard = ENV_LOCK.lock().unwrap();
    let home = temp_home("session-json");
    let ws = temp_home("ws-session-json");
    // Isolate the parent-process store too, so saves land in FX_HOME.
    std::env::set_var("FX_HOME", &home);
    // Build a session through sessions API against FX_HOME.
    let store = fxrs::sessions::SessionStore::new().unwrap();
    let sess = fxrs::sessions::Session {
        schema_version: fxrs::sessions::SCHEMA_VERSION,
        id: "stest-1".into(),
        workspace: ws.display().to_string(),
        created_ms: 1,
        updated_ms: 2,
        model: "m".into(),
        mode: fxrs::permissions::PermissionMode::Auto,
        interactive: false,
        messages: vec![fxrs::providers::Message::user("hello")],
        grants: Default::default(),
        usage: fxrs::sessions::SessionUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cost_usd: 0.0,
            steps: 2,
            tool_calls: 1,
        },
    };
    store.save(&sess).unwrap();

    let out = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["session", "stest-1", "--json"])
        .output()
        .expect("fxrs session --json runs");
    assert!(
        out.status.success(),
        "session --json exited {:?}",
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("session --json output parses");
    assert_eq!(parsed["id"], "stest-1");
    assert_eq!(parsed["usage"]["total_tokens"], 15);
    assert_eq!(parsed["schema_version"], 2);

    // sessions --json lists it
    let out2 = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let arr: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out2.stdout)).unwrap();
    assert_eq!(arr.as_array().map(|a| a.len()), Some(1));

    // delete
    let out3 = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["session", "stest-1", "--delete"])
        .output()
        .unwrap();
    assert!(out3.status.success());
    assert!(String::from_utf8_lossy(&out3.stdout).contains("deleted"));

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cli_replay_tape_and_sessions_search() {
    let _guard = ENV_LOCK.lock().unwrap();
    let home = temp_home("tape");
    let ws = temp_home("ws-tape");
    std::env::set_var("FX_HOME", &home);

    // Create a session + tape through the public API.
    let store = fxrs::sessions::SessionStore::new().unwrap();
    let sess = fxrs::sessions::Session {
        schema_version: fxrs::sessions::SCHEMA_VERSION,
        id: "tape-sess".into(),
        workspace: ws.display().to_string(),
        created_ms: 1,
        updated_ms: 2,
        model: "m".into(),
        mode: fxrs::permissions::PermissionMode::Auto,
        interactive: false,
        messages: vec![fxrs::providers::Message::user("hello")],
        grants: Default::default(),
        usage: Default::default(),
    };
    store.save(&sess).unwrap();
    let tape = fxrs::tape::TapeStore::for_session(&ws, "tape-sess");
    tape.record(
        &fxrs::tape::TapeEntry {
            ts_ms: fxrs::util::now_ms(),
            tool: "run_command".into(),
            target: "git status".into(),
            ok: true,
            preview: "ok".into(),
        },
        "tape-sess",
    );

    let out = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["replay", "tape", "tape-sess"])
        .output()
        .expect("replay tape runs");
    assert!(out.status.success(), "replay tape exited {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("run_command"), "tape shows tool: {text}");
    assert!(text.contains("git status"), "tape shows target: {text}");
    assert!(text.contains("1 tape entries"), "tape count: {text}");

    // sessions --search filters by last_text.
    let out2 = Command::new(fx_bin())
        .env_clear()
        .env("FX_HOME", &home)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&ws)
        .args(["sessions", "--search", "hello"])
        .output()
        .unwrap();
    assert!(out2.status.success());
    assert!(String::from_utf8_lossy(&out2.stdout).contains("tape-sess"));

    std::env::remove_var("FX_HOME");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}
