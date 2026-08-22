//! Local command executor — faithful port of upstream
//! `core/execution/local_executor.zig`, with the direct-output projector
//! from `core/permissions/direct_command.zig` and the foreground envelope
//! from `core/execution/command_roller`'s result contract.
//!
//! A prepared command is routed one of two ways:
//!
//! * **Direct read-only** — a safe inspection command (e.g. `ls`, `git
//!   status`) that needs no approval. Its output is *projected*: newlines
//!   kept, ASCII control bytes escaped as `\xNN`, C1 controls escaped as
//!   `\u{00NN}`, invalid UTF-8 escaped as `\xNN`, multi-byte UTF-8 passed
//!   through (with chunk-boundary resumes), and the total capped at
//!   [`DIRECT_OUTPUT_LIMIT_BYTES`].
//! * **Approved shell** — any admitted shell command executed in its own
//!   working directory/environment. Output is enveloped as
//!   `exit_code=N\n<stdout>\n…\n</stdout>` (+ `<stderr>` when non-empty),
//!   truncated in the middle with `[… N bytes truncated …]` above the
//!   caller's comparison limit.
//!
//! Upstream's `devbox_executor` no longer exists at the v0.0.5 reference, so
//! this module is the complete faithful local-execution surface.

use std::time::Instant;

use anyhow::{Context, Result};

/// Direct read-only output cap (upstream `direct_output_limit_bytes`).
pub const DIRECT_OUTPUT_LIMIT_BYTES: usize = 65_536;
const DIRECT_OUTPUT_READ_CHUNK_BYTES: usize = 4096;

/// Which route a prepared command takes (upstream `RouteKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    DirectReadOnly,
    ApprovedShell,
}

/// A command prepared for local execution (upstream `PreparedCommand`).
#[derive(Debug, Clone)]
pub enum PreparedCommand {
    DirectReadOnly {
        command: String,
        cwd: String,
    },
    ApprovedShell {
        command: String,
        cwd: String,
        environment: Vec<(String, String)>,
    },
}

impl PreparedCommand {
    pub fn command(&self) -> &str {
        match self {
            PreparedCommand::DirectReadOnly { command, .. }
            | PreparedCommand::ApprovedShell { command, .. } => command,
        }
    }

    pub fn cwd(&self) -> &str {
        match self {
            PreparedCommand::DirectReadOnly { cwd, .. }
            | PreparedCommand::ApprovedShell { cwd, .. } => cwd,
        }
    }
}

/// Foreground execution result (mirrors the upstream
/// `ForegroundCommandResult` JSON envelope).
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub route: RouteKind,
    pub command: String,
    pub cwd: String,
    pub exit_code: Option<i64>,
    pub signal: Option<u32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub truncated: bool,
    pub output: String,
}

/// Whether an effect qualifies for the direct read-only route: a
/// read-only-class command with no writes, network, or destructive effects.
pub fn is_direct_read_only_effect(effect: &crate::shell_command::CommandEffect) -> bool {
    use crate::shell_command::CommandClass;
    effect.class == CommandClass::ReadOnly
        && !effect.writes
        && !effect.network
        && !effect.destructive
}

/// Upper bound for raw callback bytes still representable by the command's
/// ordinary foreground result (upstream `foregroundResultComparisonLimit`).
pub fn foreground_result_comparison_limit(
    command: &PreparedCommand,
    approved_shell_limit: usize,
) -> usize {
    match command {
        PreparedCommand::DirectReadOnly { .. } => DIRECT_OUTPUT_LIMIT_BYTES,
        PreparedCommand::ApprovedShell { .. } => approved_shell_limit,
    }
}

/// Stateful projection of raw command output into safe display text
/// (upstream `DirectOutputProjector`): newlines pass through, ASCII control
/// bytes escape as `\xNN`, C1 controls escape as `\u{00NN}`, invalid UTF-8
/// bytes escape as `\xNN`, and a multi-byte sequence split across chunk
/// boundaries is resumed on the next chunk.
#[derive(Debug, Default)]
pub struct DirectOutputProjector {
    utf8_pending: [u8; 3],
    utf8_pending_len: usize,
    pub escaped_controls: usize,
    pub escaped_invalid: usize,
}

impl DirectOutputProjector {
    fn append_byte_escape(out: &mut String, byte: u8) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        out.push('\\');
        out.push('x');
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }

    fn append_c1_escape(out: &mut String, scalar: u32) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        out.push_str("\\u{00");
        out.push(DIGITS[(scalar >> 4) as usize] as char);
        out.push(DIGITS[(scalar & 0x0f) as usize] as char);
        out.push('}');
    }

    pub fn push(&mut self, raw: &[u8], projected: &mut String) {
        let mut combined = [0u8; DIRECT_OUTPUT_READ_CHUNK_BYTES + 3];
        let pending_len = self.utf8_pending_len;
        combined[..pending_len].copy_from_slice(&self.utf8_pending[..pending_len]);
        combined[pending_len..pending_len + raw.len()].copy_from_slice(raw);
        let bytes = &combined[..pending_len + raw.len()];
        self.utf8_pending_len = 0;

        let mut index = 0usize;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte < 0x80 {
                if byte == b'\n' {
                    projected.push('\n');
                } else if byte < 0x20 || byte == 0x7f {
                    self.escaped_controls += 1;
                    Self::append_byte_escape(projected, byte);
                } else {
                    projected.push(byte as char);
                }
                index += 1;
                continue;
            }
            // Multi-byte sequence.
            let sequence_len = utf8_sequence_length(byte);
            if let Some(seq_len) = sequence_len {
                if bytes.len() - index < seq_len {
                    let remainder = &bytes[index..];
                    self.utf8_pending[..remainder.len()].copy_from_slice(remainder);
                    self.utf8_pending_len = remainder.len();
                    break;
                }
                let sequence = &bytes[index..index + seq_len];
                match std::str::from_utf8(sequence) {
                    Ok(s) => {
                        let scalar = s.chars().next().unwrap() as u32;
                        if (0x80..=0x9f).contains(&scalar) {
                            self.escaped_controls += 1;
                            Self::append_c1_escape(projected, scalar);
                        } else {
                            projected.push_str(s);
                        }
                    }
                    Err(_) => {
                        self.escaped_invalid += 1;
                        Self::append_byte_escape(projected, byte);
                    }
                }
                index += seq_len;
            } else {
                self.escaped_invalid += 1;
                Self::append_byte_escape(projected, byte);
                index += 1;
            }
        }
    }

    /// Flush any partial UTF-8 sequence left at the end of the stream as
    /// invalid bytes.
    pub fn finish(&mut self, projected: &mut String) {
        for i in 0..self.utf8_pending_len {
            self.escaped_invalid += 1;
            Self::append_byte_escape(projected, self.utf8_pending[i]);
        }
        self.utf8_pending_len = 0;
    }
}

/// Length of a UTF-8 lead-byte sequence, or `None` for a continuation/invalid
/// byte.
fn utf8_sequence_length(byte: u8) -> Option<usize> {
    if byte & 0x80 == 0 {
        return Some(1);
    }
    if byte & 0xe0 == 0xc0 {
        return Some(2);
    }
    if byte & 0xf0 == 0xe0 {
        return Some(3);
    }
    if byte & 0xf8 == 0xf0 {
        return Some(4);
    }
    None
}

/// Truncation marker used by both routes (upstream uses `[… N bytes
/// truncated …]`).
fn truncate_middle(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    // Keep head and tail, marking the dropped span.
    let dropped = text.len() - max_bytes;
    let head_bytes = max_bytes / 2;
    let tail_start = text.len() - (max_bytes - head_bytes);
    let mut out = String::with_capacity(max_bytes + 40);
    // Avoid splitting a UTF-8 char at the cut points.
    let head_end = safe_char_boundary(&text, head_bytes);
    let tail_begin = safe_char_boundary(&text, tail_start);
    out.push_str(&text[..head_end]);
    out.push_str(&format!("[… {dropped} bytes truncated …]"));
    out.push_str(&text[tail_begin..]);
    (out, true)
}

fn safe_char_boundary(s: &str, mut index: usize) -> usize {
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Execute a prepared foreground command without taking ownership of it
/// (upstream `executePreparedCommand`).
pub async fn execute_prepared_command(
    command: &PreparedCommand,
    approved_shell_limit: usize,
) -> Result<CommandResult> {
    match command {
        PreparedCommand::DirectReadOnly { command, cwd } => {
            run_direct_read_only(command, cwd).await
        }
        PreparedCommand::ApprovedShell {
            command,
            cwd,
            environment,
        } => run_approved_shell(command, cwd, environment, approved_shell_limit).await,
    }
}

async fn spawn_bash(
    command: &str,
    cwd: &str,
    environment: &[(String, String)],
) -> Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-lc").arg(command).current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in environment {
        cmd.env(k, v);
    }
    cmd.spawn()
        .context("failed to spawn bash (is bash installed?)")
}

async fn read_pipe<S>(pipe: Option<S>) -> Vec<u8>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf).await;
    }
    buf
}

async fn run_direct_read_only(command: &str, cwd: &str) -> Result<CommandResult> {
    let started = Instant::now();
    let mut child = spawn_bash(command, cwd, &[]).await?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let status = child.wait().await?;
    let out = read_pipe(stdout).await;
    let err = read_pipe(stderr).await;

    // Project both streams through the direct output projector, then cap at
    // the direct limit. Projection first (escaping invalid/control bytes),
    // capping second (byte-accurate truncation).
    let mut projector = DirectOutputProjector::default();
    let mut projected = String::new();
    projector.push(&out, &mut projected);
    projector.push(&err, &mut projected);
    projector.finish(&mut projected);
    let (output, truncated) = truncate_middle(projected, DIRECT_OUTPUT_LIMIT_BYTES);

    Ok(CommandResult {
        route: RouteKind::DirectReadOnly,
        command: command.to_string(),
        cwd: cwd.to_string(),
        exit_code: status.code().map(|c| c as i64),
        signal: signal_of(&status),
        timed_out: false,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout_bytes: out.len(),
        stderr_bytes: err.len(),
        truncated,
        output,
    })
}

fn signal_of(status: &std::process::ExitStatus) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| s as u32)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

async fn run_approved_shell(
    command: &str,
    cwd: &str,
    environment: &[(String, String)],
    approved_shell_limit: usize,
) -> Result<CommandResult> {
    let started = Instant::now();
    let mut child = spawn_bash(command, cwd, environment).await?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let status = child.wait().await?;
    let out = read_pipe(stdout).await;
    let err = read_pipe(stderr).await;

    let (stdout_text, stderr_text) = {
        let so = String::from_utf8_lossy(&out).to_string();
        let se = String::from_utf8_lossy(&err).to_string();
        (so, se)
    };

    // Envelope (upstream command_contract.formatForegroundCommandResult):
    // status line, `<stdout>`/`<stderr>` sections with whitespace-trimmed
    // text, or `(no output)` when both are empty.
    let stdout_text = stdout_text.trim_matches([' ', '\r', '\n', '\t']);
    let stderr_text = stderr_text.trim_matches([' ', '\r', '\n', '\t']);
    let mut body = String::new();
    if let Some(code) = status.code() {
        body.push_str(&format!("exit_code={code}\n"));
    } else if let Some(sig) = signal_of(&status) {
        body.push_str(&format!("signal={sig}\n"));
    } else {
        body.push_str("process finished\n");
    }
    if !stdout_text.is_empty() {
        body.push_str(&format!("<stdout>\n{stdout_text}\n</stdout>\n"));
    }
    if !stderr_text.is_empty() {
        body.push_str(&format!("<stderr>\n{stderr_text}\n</stderr>\n"));
    }
    if stdout_text.is_empty() && stderr_text.is_empty() {
        body.push_str("(no output)\n");
    }
    let (output, truncated) = truncate_middle(body, approved_shell_limit);

    Ok(CommandResult {
        route: RouteKind::ApprovedShell,
        command: command.to_string(),
        cwd: cwd.to_string(),
        exit_code: status.code().map(|c| c as i64),
        signal: signal_of(&status),
        timed_out: false,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout_bytes: out.len(),
        stderr_bytes: err.len(),
        truncated,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_limits_follow_prepared_kind() {
        let direct = PreparedCommand::DirectReadOnly {
            command: "ls".into(),
            cwd: "/tmp".into(),
        };
        assert_eq!(
            foreground_result_comparison_limit(&direct, 1024),
            DIRECT_OUTPUT_LIMIT_BYTES
        );
        let approved = PreparedCommand::ApprovedShell {
            command: "ls".into(),
            cwd: "/tmp".into(),
            environment: vec![],
        };
        assert_eq!(foreground_result_comparison_limit(&approved, 1024), 1024);
    }

    #[test]
    fn direct_route_requires_a_clean_read_only_effect() {
        let ro = crate::shell_command::classify("git status --short");
        assert!(is_direct_read_only_effect(&ro));
        let writes = crate::shell_command::classify("git add .");
        assert!(!is_direct_read_only_effect(&writes));
        let net = crate::shell_command::classify("git fetch");
        assert!(!is_direct_read_only_effect(&net));
        let dangerous = crate::shell_command::classify("sudo rm -rf /");
        assert!(!is_direct_read_only_effect(&dangerous));
        let plain = crate::shell_command::classify("ls -la");
        assert!(is_direct_read_only_effect(&plain));
    }

    #[test]
    fn projector_keeps_newlines_and_utf8_but_escapes_controls() {
        let mut p = DirectOutputProjector::default();
        let mut out = String::new();
        p.push(b"hello\nworld", &mut out);
        p.finish(&mut out);
        assert_eq!(out, "hello\nworld");
        assert_eq!(p.escaped_controls, 0);

        let mut p = DirectOutputProjector::default();
        let mut out = String::new();
        // \x1b (ESC) escapes; \x07 (BEL) escapes; UTF-8 café passes through.
        p.push("a\x1b[b\x07café".as_bytes(), &mut out);
        p.finish(&mut out);
        assert_eq!(out, "a\\x1b[b\\x07café");
        assert_eq!(p.escaped_controls, 2);
    }

    #[test]
    fn projector_resumes_utf8_split_across_chunks() {
        // "é" = 0xC3 0xA9. Split the lead byte and continuation.
        let mut p = DirectOutputProjector::default();
        let mut out = String::new();
        p.push(b"caf\xc3", &mut out);
        p.push(b"\xa9 noir", &mut out);
        p.finish(&mut out);
        assert_eq!(out, "café noir");
    }

    #[test]
    fn projector_escapes_invalid_bytes_and_c1_controls() {
        let mut p = DirectOutputProjector::default();
        let mut out = String::new();
        p.push(b"a\xffb", &mut out);
        p.push(b"\xc2\x85", &mut out); // U+0085 (NEL, C1 control)
        p.finish(&mut out);
        assert_eq!(out, "a\\xffb\\u{0085}");
        assert_eq!(p.escaped_invalid, 1);
        assert_eq!(p.escaped_controls, 1);
    }

    #[test]
    fn truncate_middle_marks_dropped_span() {
        let long = "x".repeat(10_000);
        let (out, truncated) = truncate_middle(long.clone(), 100);
        assert!(truncated);
        assert!(out.len() <= 100 + 40);
        assert!(out.contains("bytes truncated"));
        // Both head and tail survive.
        assert!(out.starts_with(&"x".repeat(50)));
        assert!(out.ends_with("xxxxx"));
        // No truncation when within limit.
        let (out, truncated) = truncate_middle(long.clone(), 20_000);
        assert!(!truncated);
        assert_eq!(out.len(), long.len());
    }

    #[tokio::test]
    async fn direct_execute_runs_and_projects() {
        let result = execute_prepared_command(
            &PreparedCommand::DirectReadOnly {
                command: "printf 'local-executor\n'".into(),
                cwd: "/tmp".into(),
            },
            1024,
        )
        .await
        .unwrap();
        assert_eq!(result.route, RouteKind::DirectReadOnly);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("local-executor"));
    }

    #[tokio::test]
    async fn approved_execute_envelopes_stdout_and_stderr() {
        let result = execute_prepared_command(
            &PreparedCommand::ApprovedShell {
                command: "echo hello; echo err >&2".into(),
                cwd: "/tmp".into(),
                environment: vec![],
            },
            1024,
        )
        .await
        .unwrap();
        assert_eq!(result.route, RouteKind::ApprovedShell);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("<stdout>\nhello\n</stdout>"));
        assert!(result.output.contains("<stderr>\nerr\n</stderr>"));
    }

    #[tokio::test]
    async fn approved_execute_prints_no_output_marker() {
        let result = execute_prepared_command(
            &PreparedCommand::ApprovedShell {
                command: "true".into(),
                cwd: "/tmp".into(),
                environment: vec![],
            },
            1024,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(
            result.output.ends_with("(no output)\n"),
            "got {:?}",
            result.output
        );
    }

    #[tokio::test]
    async fn approved_execute_respects_environment_and_cwd() {
        let result = execute_prepared_command(
            &PreparedCommand::ApprovedShell {
                command: "printf '%s/%s' \"$FXRS_EXEC_TEST\" \"$(basename \"$PWD\")\"".into(),
                cwd: "/tmp".into(),
                environment: vec![("FXRS_EXEC_TEST".into(), "env-ok".into())],
            },
            1024,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("env-ok/tmp"));
    }
}
