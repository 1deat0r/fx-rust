//! E2E gateway-credits test: serve a fake `/coding-agent/v1/credits`
//! response on loopback and verify `fxrs credits` renders the snapshot.
//!
//! Uses a unique FX_HOME per test and restores env vars afterwards.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

static COUNTER: AtomicU32 = AtomicU32::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fxrs-credits-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("FX_HOME", &dir);
    dir
}

/// Serve one HTTP response and close.
fn serve_once(body: &str, status: u16) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

#[test]
fn credits_cli_renders_snapshot() {
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let addr = serve_once(r#"{"balance":"42.7","used":"12","plan":"hobby"}"#, 200);
    let url = format!("http://{addr}/coding-agent/v1/credits");
    std::env::set_var("FX_E2E_GATEWAY_CREDITS_URL", &url);
    std::env::set_var("AI_GATEWAY_API_KEY", "test-key");

    let bin = env!("CARGO_BIN_EXE_fxrs");
    let out = std::process::Command::new(bin)
        .args(["credits"])
        .env("FX_HOME", &home)
        .env("FX_E2E_GATEWAY_CREDITS_URL", &url)
        .env("AI_GATEWAY_API_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("42.7"), "stdout: {text}");
    assert!(text.contains("plan: hobby"), "stdout: {text}");

    std::env::remove_var("FX_E2E_GATEWAY_CREDITS_URL");
    std::env::remove_var("AI_GATEWAY_API_KEY");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn credits_cli_handles_failure() {
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let addr = serve_once("nope", 500);
    let url = format!("http://{addr}/coding-agent/v1/credits");
    let bin = env!("CARGO_BIN_EXE_fxrs");
    let out = std::process::Command::new(bin)
        .args(["credits"])
        .env("FX_HOME", &home)
        .env("FX_E2E_GATEWAY_CREDITS_URL", &url)
        .env("AI_GATEWAY_API_KEY", "test-key")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "stdout: {text} stderr: {err}");
    assert!(err.contains("credits"), "stderr: {err}");

    std::env::remove_var("FX_E2E_GATEWAY_CREDITS_URL");
    std::env::remove_var("AI_GATEWAY_API_KEY");
    let _ = std::fs::remove_dir_all(&home);
}
