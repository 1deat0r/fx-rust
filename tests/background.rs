//! E2E background-process tests: start a real detached process, tail its
//! output, reconcile status after exit, and stop it.
//!
//! Uses a unique FX_HOME per test so the store/logs never touch the real
//! profile. Skips when `bash` is unavailable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use fxrs::background::{BackgroundStore, BgStatus};

static COUNTER: AtomicU32 = AtomicU32::new(0);
// FX_HOME is process-global; these tests must not run concurrently.
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fxrs-bg-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("FX_HOME", &dir);
    dir
}

#[test]
fn start_output_exit_and_stop() {
    if std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("bash unavailable; skipping");
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = BackgroundStore::open().unwrap();
    let rec = store
        .start(
            "printf 'hello background\\n'; sleep 0.4; echo done",
            &cwd,
            Some("demo"),
        )
        .unwrap();
    let id = rec.id.clone();
    assert_eq!(rec.status, BgStatus::Running);
    assert!(rec.pid > 0);
    assert!(store.get(&id).is_some());

    let list = store.list().to_vec();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);

    // give it a moment, then read output
    std::thread::sleep(std::time::Duration::from_millis(700));
    let log = store.log_text(&id, 8192, None).unwrap();
    assert!(log.contains("hello background"), "log: {log}");

    // reconcile -> process should have exited with code 0
    store = BackgroundStore::open().unwrap();
    let rec = store.get(&id).unwrap().clone();
    assert_eq!(
        rec.status,
        BgStatus::Exited,
        "log: {}",
        store.log_text(&id, 8192, None).unwrap()
    );
    assert_eq!(rec.exit_code, Some(0));

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn stop_terminates_and_reports() {
    if std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let cwd = std::env::current_dir().unwrap();

    let mut store = BackgroundStore::open().unwrap();
    let rec = store.start("sleep 30", &cwd, None).unwrap();
    let id = rec.id.clone();
    assert!(store.get(&id).unwrap().status == BgStatus::Running);

    // stop with a short grace: SIGTERM should kill `sleep` immediately
    let stopped = store.stop(&id, 2000).unwrap();
    assert!(
        !crate_alive(stopped.pid),
        "process should be gone after stop"
    );
    assert!(stopped.status == BgStatus::Exited);

    let _ = std::fs::remove_dir_all(&home);
}

#[allow(clippy::redundant_closure_call)]
fn crate_alive(pid: u32) -> bool {
    (|| {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })()
}
