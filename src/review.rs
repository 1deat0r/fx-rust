//! `fxrs review` — review the current git changes with the model. Gathers a
//! git snapshot + unified diff (via `github` and `diff`), sends a review
//! prompt through the normal agent runtime, and prints the review.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::agent::{AgentRequest, FinishReason};
use crate::config;
use crate::sessions::SessionStore;
use crate::ui::QuietHuman;

const REVIEW_SYSTEM: &str = "You are a senior code reviewer. Review the following diff for correctness, security, and style. Be concrete and concise. Point out bugs and risks first, then smaller style notes. If the diff is clean, say so plainly.";

pub async fn run_review(rest: &[String], cwd: &Path) -> Result<i32> {
    let mut base: Option<String> = None;
    let mut extra_prompt: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--base" => {
                i += 1;
                base = rest.get(i).cloned();
            }
            "--help" | "-h" => {
                println!("usage: fxrs review [--base <ref>] [<extra prompt>]");
                return Ok(0);
            }
            _ => extra_prompt.push(a.clone()),
        }
        i += 1;
    }

    // git snapshot + diff
    let snap = crate::github::git_snapshot(cwd);
    if !snap.in_git_repo {
        eprintln!("fxrs review: not a git repository");
        return Ok(1);
    }
    let base_ref = base.unwrap_or_else(|| "HEAD".to_string());
    let diff = git_diff(cwd, &base_ref)?;

    let mut prompt = String::new();
    prompt.push_str("## Git snapshot\n");
    prompt.push_str(&snap.text);
    prompt.push('\n');
    prompt.push_str("## Diff\n");
    if diff.is_empty() {
        prompt.push_str("(no changes)\n");
    } else {
        let _ = diff.chars().take(60_000).collect::<String>();
        prompt.push_str(&diff.chars().take(60_000).collect::<String>());
        prompt.push('\n');
    }
    prompt.push_str("## Task\n");
    if extra_prompt.is_empty() {
        prompt.push_str("Review the changes above.\n");
    } else {
        prompt.push_str(&extra_prompt.join(" "));
        prompt.push('\n');
    }

    let cfg = Arc::new(config::resolve(cwd)?);
    let store = SessionStore::new()?;
    let human = QuietHuman;
    let req = AgentRequest {
        prompt: Some(prompt),
        system: Some(REVIEW_SYSTEM.to_string()),
        interactive: false,
        resume: None,
        messages: Vec::new(),
        images: Vec::new(),
    };
    let out = crate::agent::run(req, cfg, &human, &store).await?;
    if let Some(e) = &out.error {
        eprintln!("fxrs error: {e}");
        return Ok(1);
    }
    if out.finish_reason == FinishReason::MaxSteps {
        eprintln!("fxrs: stopped after {} steps (max_agent_steps)", out.steps);
    }
    for m in out.transcript.iter().rev() {
        if m.role == "assistant" {
            for block in &m.content {
                if let crate::providers::ContentBlock::Text(t) = block {
                    print!("{t}");
                }
            }
            break;
        }
    }
    println!();
    eprintln!(
        "[fxrs] review · {} steps · {} tool calls · {} tokens (${:.4})",
        out.steps, out.tool_calls, out.total_tokens, out.cost_usd
    );
    Ok(0)
}

/// `git diff <base>` text (working tree + staged changes vs the ref).
fn git_diff(cwd: &Path, base: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("diff")
        .arg(base)
        .current_dir(cwd)
        .output()?;
    if !out.status.success() {
        // Not a git repo or bad ref — best effort.
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
