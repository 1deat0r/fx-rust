//! E2E terminal-session tests: create a real tmux-backed terminal, send
//! input, read the pane, resize, and stop it.
//!
//! Uses a unique FX_HOME per test so the store never touches the real
//! profile. Skips when tmux is unavailable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use fxrs::terminal::{TermStatus, TerminalStore};

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
fn create_send_read_resize_stop() {
    if !tmux_present() {
        eprintln!("tmux unavailable; skipping");
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = TerminalStore::open().unwrap();
    let rec = store
        .create(&cwd, Some("bash"), Some("demo"), Some(24), Some(80))
        .unwrap();
    let id = rec.id.clone();
    assert_eq!(rec.status, TermStatus::Running);
    assert!(rec.pid > 0);
    assert!(store.get(&id).is_some());

    // send a command and read the pane
    store.send(&id, "echo terminal-e2e-works", true).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = store.read(&id, 50, 65536, false, false).unwrap();
    assert!(
        out.contains("terminal-e2e-works"),
        "pane did not show output: {out:?}"
    );

    // resize and verify the record updates
    let resized = store.resize(&id, 30, 100).unwrap();
    assert_eq!(resized.rows, 30);
    assert_eq!(resized.columns, 100);
    let rec2 = store.get(&id).unwrap().clone();
    assert_eq!(rec2.rows, 30);
    assert_eq!(rec2.columns, 100);

    // stop: shell dies, tmux session gone, record exited
    let stopped = store.stop(&id).unwrap();
    assert_eq!(stopped.status, TermStatus::Exited);
    // creating a second store reconciles via tmux has-session
    let store2 = TerminalStore::open().unwrap();
    assert_eq!(store2.get(&id).unwrap().status, TermStatus::Exited);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn read_strips_ansi_and_respects_raw() {
    if !tmux_present() {
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
