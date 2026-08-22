//! Phase 8 hardening — CLI surface e2e: the AB parity contract in Rust.
//!
//! Mirrors `scripts/parity_check.sh` for the commands that can run safely in
//! a test environment: every registered top-level command is listed by
//! `fxrs --commands`, and a read-only probe (`--help`, `--version`, or a
//! bare stat) must return exit 0 or a well-formed non-zero that proves the
//! command exists and does not panic.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_fxrs");

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn fxrs binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn commands_surface_is_exhaustive_and_probeable() {
    let (code, list) = run(&["--commands"]);
    assert_eq!(code, 0, "fxrs --commands must succeed");
    let commands: Vec<&str> = list
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert!(!commands.is_empty());

    // Every expected command is registered (the AB parity contract).
    for cmd in [
        "repl",
        "ask",
        "resume",
        "sessions",
        "session",
        "status",
        "permissions",
        "models",
        "modes",
        "subagent",
        "tui",
        "teams",
        "workspace",
        "capture",
        "one-off",
        "review",
        "mcp-lookup",
        "doctor",
        "usage",
        "settings",
        "replay",
        "setup",
        "version",
        "help",
        "auth",
        "login",
        "logout",
        "upgrade",
        "hooks",
        "mcp",
        "gh",
        "pr",
        "issue",
        "credits",
        "provider",
        "diff",
        "skills",
        "background",
        "terminal",
        "sound",
    ] {
        assert!(
            commands.contains(&cmd),
            "expected `{cmd}` in `fxrs --commands` surface"
        );
    }
}

#[test]
fn safe_commands_probe_cleanly() {
    // Commands that accept --help / --version / a bare safe invocation.
    for (cmd, args) in [
        ("help", &[][..]),
        ("version", &[][..]),
        ("status", &[][..]),
        ("permissions", &[][..]),
        ("settings", &[][..]),
        ("models", &["--offline"][..]),
        ("modes", &["--json"][..]),
        ("doctor", &[][..]),
        ("setup", &[][..]),
        ("sound", &["status"][..]),
        ("repl", &["--help"][..]),
        ("skills", &["--json"][..]),
        ("background", &["list"][..]),
        ("terminal", &["list"][..]),
        ("hooks", &["--help"][..]),
        ("mcp", &["--help"][..]),
        ("gh", &["--help"][..]),
        ("pr", &["--help"][..]),
        ("issue", &["--help"][..]),
        ("credits", &["--help"][..]),
        ("provider", &["--help"][..]),
        ("upgrade", &["--help"][..]),
        ("auth", &["status"][..]),
        ("subagent", &["list"][..]),
        ("teams", &["list"][..]),
        ("workspace", &["list"][..]),
    ] {
        let mut all: Vec<&str> = Vec::with_capacity(args.len() + 1);
        all.push(cmd);
        all.extend_from_slice(args);
        let (code, _) = run(&all);
        assert_eq!(
            code,
            0,
            "`fxrs {cmd} {}` should probe cleanly (exit 0), got {code}",
            args.join(" ")
        );
    }
}

#[test]
fn unknown_command_fails_with_help() {
    let (code, out) = run(&["definitely-not-a-command"]);
    assert_ne!(code, 0);
    assert!(out.contains("unknown command") || out.to_lowercase().contains("usage"));
}

#[test]
fn version_matches_constant() {
    let (code, out) = run(&["version"]);
    assert_eq!(code, 0);
    assert!(out.trim_start().starts_with("fxrs "));
}
