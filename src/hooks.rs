//! Lifecycle hook runtime mirroring fx's four hook kinds:
//! PreToolUse, Stop, PostTurnEnd, AttentionRequired.
//!
//! Hooks are executable scripts discovered under `~/.fx/hooks/<Event>` and
//! `<workspace>/.fx/hooks/<Event>` (also tried lowercase and `.sh`). Each
//! script receives a JSON input object on stdin and may write a JSON reply
//! on stdout. PreToolUse hooks may block or rewrite a tool call; the other
//! events are side effects (their exit status is logged, not acted on).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    PreToolUse,
    Stop,
    PostTurnEnd,
    AttentionRequired,
}

impl HookKind {
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::Stop => "Stop",
            Self::PostTurnEnd => "PostTurnEnd",
            Self::AttentionRequired => "AttentionRequired",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    /// Proceed (or side-effect hook that did not object).
    Allow,
    /// PreToolUse hook blocked the call.
    Block { reason: String },
    /// PreToolUse hook rewrote the tool arguments (JSON object).
    Rewrite { args: Value },
}

/// Discover hook scripts for `kind` in both the user hook dir and the
/// workspace hook dir. Later dirs (workspace) take precedence in ordering.
pub fn discover(kind: HookKind, workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [crate::config::fx_home().join("hooks"), workspace.join(".fx/hooks")] {
        for cand in [
            base.join(kind.event_name()),
            base.join(kind.event_name().to_lowercase()),
            base.join(format!("{}.sh", kind.event_name())),
        ] {
            if cand.is_file() {
                out.push(cand);
            }
        }
    }
    out
}

/// Run every hook script for `kind`. `input` is the root JSON object written
/// to each hook's stdin. `base_input` is augmented with hook_event_name.
/// Returns aggregated outcomes; a Block or Rewrite ends evaluation early.
pub fn run(
    kind: HookKind,
    input: Value,
    workspace: &Path,
    timeout_secs: u64,
) -> Vec<HookOutcome> {
    let mut outcomes = Vec::new();
    for script in discover(kind, workspace) {
        let mut payload = input.clone();
        if let Value::Object(map) = &mut payload {
            map.insert("hook_event_name".into(), Value::String(kind.event_name().into()));
            map.insert("hook_script".into(), Value::String(script.display().to_string()));
        }
        let result = run_one(&script, payload, timeout_secs);
        match result {
            Ok(outcome) => {
                let blocked = matches!(outcome, HookOutcome::Block { .. });
                let rewrote = matches!(outcome, HookOutcome::Rewrite { .. });
                outcomes.push(outcome);
                if kind == HookKind::PreToolUse && (blocked || rewrote) {
                    break;
                }
            }
            Err(e) => {
                // A failing hook logs but never hard-fails the agent.
                eprintln!("[fxrs] hook {} failed: {e:#}", script.display());
            }
        }
    }
    outcomes
}

fn run_one(script: &Path, input: Value, timeout_secs: u64) -> Result<HookOutcome> {
    let mut child = Command::new(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", script.display()))?;
    {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(serde_json::to_string(&input)?.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    let mut child = child;
    let output = if timeout_secs > 0 {
        use wait_timeout::ChildExt;
        match child.wait_timeout(std::time::Duration::from_secs(timeout_secs))? {
            Some(_) => child.wait_with_output()?,
            None => {
                let _ = child.kill();
                anyhow::bail!("hook {} timed out after {timeout_secs}s", script.display());
            }
        }
    } else {
        child.wait_with_output()?
    };
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        let parsed: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
        let decision = parsed.get("decision").and_then(|d| d.as_str()).unwrap_or("allow");
        match decision {
            "block" => Ok(HookOutcome::Block {
                reason: parsed.get("reason").and_then(|r| r.as_str()).unwrap_or("blocked by hook").into(),
            }),
            "rewrite" => Ok(HookOutcome::Rewrite {
                args: parsed.get("args").cloned().unwrap_or(Value::Object(Default::default())),
            }),
            _ => Ok(HookOutcome::Allow),
        }
    } else {
        anyhow::bail!("hook {} exited {:?}", script.display(), output.status.code());
    }
}

/// Convenience for building the PreToolUse input object.
pub fn pre_tool_use_input(tool_name: &str, args: &Value, workspace: &Path, session_id: Option<&str>) -> Value {
    json!({
        "tool_name": tool_name,
        "tool_input": args,
        "workspace": workspace.display().to_string(),
        "session_id": session_id,
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

fn base_input(workspace: &Path, session_id: Option<&str>) -> Value {
    json!({
        "workspace": workspace.display().to_string(),
        "session_id": session_id,
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

/// Build the Stop event input (fx: assistant's final message for the turn).
pub fn stop_input(workspace: &Path, session_id: Option<&str>, assistant_text: &str) -> Value {
    let mut v = base_input(workspace, session_id);
    v["assistant_text"] = Value::String(assistant_text.to_string());
    v
}

/// Build the PostTurnEnd event input (fx: turn accounting).
pub fn post_turn_end_input(workspace: &Path, session_id: Option<&str>, steps: usize) -> Value {
    let mut v = base_input(workspace, session_id);
    v["steps"] = json!(steps);
    v
}

/// Build the AttentionRequired event input (fx: an unresolved permission /
/// interrupt the user must resolve).
pub fn attention_required_input(
    workspace: &Path,
    session_id: Option<&str>,
    reason: &str,
) -> Value {
    let mut v = base_input(workspace, session_id);
    v["reason"] = Value::String(reason.to_string());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_matches_event_names() {
        let ws = std::path::Path::new("/tmp/fxrs-hooks-test");
        let _ = ws.join(".fx/hooks");
        // No scripts -> empty discovery (sanity: no panic on missing dirs).
        assert!(discover(HookKind::PreToolUse, ws).is_empty());
    }
}
