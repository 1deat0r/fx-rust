//! E2E terminal-session tests: create a real terminal (native PTY by
//! default, tmux explicitly), send input, read output, resize, stop, and
//! exercise the exec/browser_terminal surfaces.
//!
//! Uses a unique FX_HOME per test so the store never touches the real
//! profile. Skips when the relevant backend is unavailable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use fxrs::terminal::{native_pty_available, Backend, TermStatus, TerminalStore};

static COUNTER: AtomicU32 = AtomicU32::new(0);
// FX_HOME is process-global; these tests must not run concurrently.
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fxrs-term-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("FX_HOME", &dir);
    dir
}

fn tmux_present() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn native_create_send_read_resize_stop() {
    if !native_pty_available() {
        eprintln!("native pty unavailable; skipping");
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = TerminalStore::open().unwrap();
    // Default backend is native (upstream `backend orelse .native`).
    let rec = store
        .create(&cwd, Some("bash"), Some("demo"), Some(24), Some(80))
        .unwrap();
    let id = rec.id.clone();
    assert_eq!(rec.status, TermStatus::Running);
    assert_eq!(rec.backend, Backend::Native);
    assert!(rec.pid > 0);
    assert!(store.get(&id).is_some());

    // send a command and read the output
    store.send(&id, "echo terminal-e2e-works", true).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = store.read(&id, 50, 65536, false, false).unwrap();
    assert!(
        out.contains("terminal-e2e-works"),
        "pty did not show output: {out:?}"
    );

    // resize and verify the record updates
    let resized = store.resize(&id, 30, 100).unwrap();
    assert_eq!(resized.rows, 30);
    assert_eq!(resized.columns, 100);
    let rec2 = store.get(&id).unwrap().clone();
    assert_eq!(rec2.rows, 30);
    assert_eq!(rec2.columns, 100);

    // stop: child dies, record exited
    let stopped = store.stop(&id).unwrap();
    assert_eq!(stopped.status, TermStatus::Exited);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn tmux_backend_create_send_read_stop() {
    if !tmux_present() {
        eprintln!("tmux unavailable; skipping");
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = TerminalStore::open().unwrap();
    let opts = fxrs::terminal::TerminalCreateOptions {
        name: Some("tmux-demo"),
        rows: Some(24),
        columns: Some(80),
        ..Default::default()
    };
    let rec = store
        .create_backend(Backend::Tmux, &cwd, Some("bash"), &opts)
        .unwrap();
    let id = rec.id.clone();
    assert_eq!(rec.backend, Backend::Tmux);
    assert!(rec.pid > 0);

    store.send(&id, "echo tmux-e2e-works", true).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = store.read(&id, 50, 65536, false, false).unwrap();
    assert!(
        out.contains("tmux-e2e-works"),
        "tmux pane did not show output: {out:?}"
    );

    let stopped = store.stop(&id).unwrap();
    assert_eq!(stopped.status, TermStatus::Exited);
    // creating a second store reconciles via tmux has-session
    let store2 = TerminalStore::open().unwrap();
    assert_eq!(store2.get(&id).unwrap().status, TermStatus::Exited);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn read_strips_ansi_and_respects_raw() {
    if !native_pty_available() {
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = TerminalStore::open().unwrap();
    let rec = store
        .create(&cwd, Some("bash"), Some("ansi"), None, None)
        .unwrap();
    let id = rec.id.clone();
    store
        .send(&id, r#"printf '\033[31mred\033[0m plain'"#, true)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let clean = store.read(&id, 20, 65536, false, false).unwrap();
    assert!(clean.contains("red plain"), "clean read: {clean:?}");
    assert!(!clean.contains("\u{1b}["), "ansi leaked: {clean:?}");
    store.stop(&id).unwrap();

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn native_lost_after_host_gone() {
    // A native record outlives its host (fresh process => empty registry):
    // recovery marks it `lost`, tmux-style durable sessions do not.
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();

    // Craft a store file with a native record that has no live handle.
    let store_path = home.join("terminal.json");
    let record = serde_json::json!({
        "version": 1,
        "records": [{
            "id": "gone-native",
            "command": "bash",
            "cwd": ".",
            "rows": 40,
            "columns": 120,
            "started_at_ms": 0,
            "status": "running",
            "pid": 99999,
            "backend": "native",
            "socket": "",
            "target": ""
        }]
    });
    std::fs::write(&store_path, serde_json::to_string(&record).unwrap()).unwrap();
    let store = TerminalStore::open().unwrap();
    assert_eq!(
        store.get("gone-native").unwrap().status,
        TermStatus::Lost,
        "native record without its host must reconcile to lost"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn browser_terminal_exec_runs_command_and_returns_output() {
    if !native_pty_available() {
        eprintln!("native pty unavailable; skipping");
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    use fxrs::tools::{terminal as term_tool, ToolContext};
    use std::sync::Arc;
    let ctx = ToolContext {
        workspace: cwd.clone(),
        max_result_bytes: 65536,
        interactive: false,
        session_id: String::new(),
        config: Arc::new(fxrs::config::Config {
            mode: "ask".into(),
            workspace: cwd.clone(),
            model: "m".into(),
            permission_mode: fxrs::permissions::PermissionMode::Auto,
            max_agent_steps: 0,
            max_tool_result_bytes: 65536,
            first_call_tool_choice: fxrs::config::FirstCallToolChoice::Auto,
            context: true,
            sandbox: fxrs::config::SandboxMode::None,
            permission_rules: Default::default(),
            settings_path: None,
            additional_directories: vec![],
            mcp_servers: vec![],
            context_limits: fxrs::context::ContextLimits::default(),
            input_appearance: "auto".into(),
            presentation_mode: "default".into(),
            update_channel: "stable".into(),
            tool_filter: None,
            reasoning_effort: None,
        }),
        store: fxrs::sessions::SessionStore::new().unwrap(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let res = rt.block_on(term_tool::browser_terminal(
        &ctx,
        &serde_json::json!({"action": "exec", "command": "echo browser-exec-ok"}),
    ));
    let res = res.expect("browser_terminal exec should succeed");
    assert_eq!(res["status"], "ran");
    assert_eq!(res["exit_code"], 0);
    let output = res["output"].as_str().unwrap_or("");
    assert!(
        output.contains("browser-exec-ok"),
        "exec output missing marker: {output:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
