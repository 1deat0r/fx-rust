//! `fxrs one-off` (alias `fxrs oneoff`) — run one non-interactive agent turn
//! and print ONLY the final assistant text. Stats go to stderr so stdout
//! stays machine-parseable. Shares the `ask` flag surface (--resume, --system,
//! -m/--message, --image).

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::agent::{AgentRequest, FinishReason};
use crate::config;
use crate::sessions::SessionStore;
use crate::ui::QuietHuman;

pub async fn run_oneoff(rest: &[String], cwd: &Path) -> Result<i32> {
    let mut resume: Option<String> = None;
    let mut system: Option<String> = None;
    let mut messages: Vec<String> = Vec::new();
    let mut images: Vec<crate::providers::ImageInput> = Vec::new();
    let mut prompt_parts: Vec<String> = Vec::new();

    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--resume" | "-r" => {
                i += 1;
                resume = rest.get(i).cloned();
            }
            "--system" => {
                i += 1;
                system = rest.get(i).cloned();
            }
            "--message" | "-m" => {
                i += 1;
                if let Some(m) = rest.get(i) {
                    messages.push(m.clone());
                }
            }
            "--image" => {
                i += 1;
                if let Some(path) = rest.get(i) {
                    if let Ok(img) = crate::cli::load_image_input_pub(Path::new(path)) {
                        images.push(img);
                    }
                }
            }
            "--help" | "-h" => {
                println!("usage: fxrs one-off [--resume <id>] [--system <s>] [-m <msg>] [--image PATH] <prompt>");
                return Ok(0);
            }
            _ => prompt_parts.push(a.clone()),
        }
        i += 1;
    }

    let prompt = if prompt_parts.is_empty() {
        use std::io::Read;
        let mut buf = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin().read_to_string(&mut buf)?;
        }
        if buf.trim().is_empty() {
            anyhow::bail!("fxrs one-off: pass a prompt: fxrs one-off \"explain this repo\"");
        }
        Some(buf)
    } else {
        Some(prompt_parts.join(" "))
    };

    let cfg = Arc::new(config::resolve(cwd)?);
    let store = SessionStore::new()?;
    let human = QuietHuman;
    let req = AgentRequest {
        prompt,
        system,
        interactive: false,
        resume,
        messages,
        images,
    };
    let out = crate::agent::run(req, cfg, &human, &store).await?;
    if let Some(e) = &out.error {
        eprintln!("fxrs error: {e}");
        return Ok(1);
    }
    if out.finish_reason == FinishReason::MaxSteps {
        eprintln!("fxrs: stopped after {} steps (max_agent_steps)", out.steps);
    }
    // Print the final assistant text only.
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
    // Ensure newline terminates stdout.
    println!();
    eprintln!(
        "[fxrs] session {} · {} steps · {} tool calls · {} tokens (${:.4})",
        out.session_id, out.steps, out.tool_calls, out.total_tokens, out.cost_usd
    );
    Ok(0)
}
