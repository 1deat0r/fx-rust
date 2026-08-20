//! run_command: execute a shell command in the workspace.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use super::{ToolContext, arg, arg_i64};

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

pub async fn run_command(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let command = arg(args, "command")
        .ok_or_else(|| anyhow::anyhow!("missing required argument `command`"))?;
    if command.trim().is_empty() {
        anyhow::bail!("command must not be empty");
    }
    let description = arg(args, "description").unwrap_or("");

    let timeout_ms = arg_i64(args, "timeout_ms")
        .unwrap_or_else(|| DEFAULT_TIMEOUT_MS as i64)
        .clamp(1_000, MAX_TIMEOUT_MS as i64) as u64;

    let started = std::time::Instant::now();
    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(&ctx.workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn bash (is bash installed?)")?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let (status, combined) = match timeout(
        Duration::from_millis(timeout_ms),
        run_to_completion(&mut child, stdout_pipe, stderr_pipe),
    )
    .await
    {
        Ok(x) => x?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            (
                None,
                format!("… command exceeded {} ms timeout and was killed", timeout_ms),
            )
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let code = status.as_ref().and_then(|s| s.code());

    let output = ctx.truncate(&combined);
    let mut body = json!({
        "command": command,
        "exit_code": code,
        "elapsed_ms": elapsed_ms,
        "success": code == Some(0),
        "output": output,
    });
    if !description.is_empty() {
        body["description"] = json!(description);
    }
    Ok(body)
}

async fn run_to_completion(
    child: &mut tokio::process::Child,
    stdout_pipe: Option<tokio::process::ChildStdout>,
    stderr_pipe: Option<tokio::process::ChildStderr>,
) -> Result<(Option<std::process::ExitStatus>, String)> {
    let out_task = tokio::spawn(read_pipe(stdout_pipe));
    let err_task = tokio::spawn(read_pipe(stderr_pipe));
    let status = child.wait().await?;
    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();
    let combined = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    Ok((Some(status), combined))
}

async fn read_pipe<S>(pipe: Option<S>) -> Vec<u8>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf).await;
    }
    buf
}

