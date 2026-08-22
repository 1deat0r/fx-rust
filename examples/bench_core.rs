//! Phase 8 benchmark milestone — core hot-path micro benchmark.
//!
//! Runs a battery of synthetic workloads through the pure-Rust hot paths
//! (diff engine, shell lexer, transcript wrapping, settings render, MCP
//! schema validation) and prints ns/op + op/s. Not a criterion framework —
//! plain std timing so it needs no dev-dependencies.
//!
//! Usage: cargo run --release --example bench_core

use std::hint::black_box;
use std::time::Instant;

fn bench<F: FnMut()>(name: &str, iters: usize, mut f: F) {
    // Warm up.
    for _ in 0..iters.min(20) {
        black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    let elapsed = start.elapsed();
    let per = elapsed.as_nanos() / iters as u128;
    let ops = (1_000_000_000f64 / per as f64) as u64;
    println!("{name:<34} {per:>10} ns/op  {ops:>12} op/s");
}

fn main() {
    println!("fxrs core benchmark (release build)");

    // 1) Diff engine on two ~200-line files with a handful of edits.
    let old_text: String = (0..200)
        .map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let new_text: String = (0..200)
        .map(|i| {
            if i % 50 == 0 {
                format!("edited line {i}: a NEW body for this row")
            } else {
                format!("line {i}: the quick brown fox jumps over the lazy dog {i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    bench("diff::compute (200 ln)", 2000, move || {
        black_box(fxrs::diff::compute(&old_text, &new_text));
    });

    // 2) Shell-command lexer on a representative command.
    let cmd = "cd src && cargo build --release -j 8 > /tmp/log 2>&1 && echo done";
    bench("shell_command::classify", 50_000, move || {
        black_box(fxrs::shell_command::classify(cmd));
    });

    // 3) Transcript wrapping of a 4 KiB paragraph at 100 columns.
    let para: String = "word ".repeat(700);
    bench("transcript wrapping 4KiB@100", 20_000, move || {
        black_box(fxrs::tui::transcript::TranscriptLine::new(
            fxrs::tui::transcript::LineKind::User,
            para.clone(),
            100,
        ));
    });

    // 4) Settings render.
    let cfg = fxrs::config::resolve(std::path::Path::new(".")).expect("resolve cwd config");
    bench("settings_catalog::render", 20_000, move || {
        black_box(fxrs::settings_catalog::render(&cfg));
    });

    // 5) MCP json-schema validation (simple object schema).
    let schema_json = r#"{"type":"object","required":["command"],"properties":{"command":{"type":"string"},"timeoutMs":{"type":"integer","minimum":1}},"additionalProperties":false}"#;
    let schema: serde_json::Value = serde_json::from_str(schema_json).unwrap();
    let args = serde_json::json!({"command":"ls -la","timeoutMs":5000});
    bench("mcp_schema validate object", 100_000, move || {
        black_box(fxrs::mcp_schema::validate(&schema, &args));
    });

    // 6) Operation-id / subagent control-record round trip.
    let invocation = "fxop-8a3f2c1e-0000-4000-8000-000000000000";
    bench("operation_id generate", 500_000, move || {
        black_box(fxrs::operation_id::operation_id(invocation));
    });

    // 7) Approval auto-classify sandbox (permission fast path).
    let sandbox = fxrs::permissions::Sandbox {
        mode: fxrs::config::SandboxMode::Auto,
        workspace: std::path::PathBuf::from("."),
        additional: Vec::new(),
    };
    bench("permissions sandbox allows tmp", 200_000, move || {
        black_box(sandbox.allows("/tmp/x.txt"));
    });
}
