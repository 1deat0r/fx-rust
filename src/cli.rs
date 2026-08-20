//! CLI surface: `fxrs [command]` — mirrors fx's Unix-shell CLI.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};

use crate::agent::{AgentOutput, AgentRequest, FinishReason};
use crate::config;
use crate::sessions::SessionStore;
use crate::ui::QuietHuman;

pub async fn run_main(args: Vec<String>) -> Result<i32> {
    let mut args = args.iter().skip(1); // drop argv[0]
    let cmd = args.next();

    match cmd.map(|s| s.as_str()) {
        None | Some("") | Some("repl") => {
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            crate::ui::run_interactive(cfg, &store, None, false).await?;
            Ok(0)
        }
        Some("ask") | Some("a") => {
            let rest: Vec<String> = args.map(|s| s.to_string()).collect();
            run_ask(&rest).await
        }
        Some("resume") | Some("r") => {
            let id = args.next().map(|s| s.to_string());
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            let id = match id {
                Some(i) if i == "last" => None,
                Some(i) => Some(i),
                None => None,
            };
            crate::ui::run_interactive(cfg, &store, id, false).await?;
            Ok(0)
        }
        Some("sessions") => {
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            let sessions = store.list(Some(&cfg.workspace))?;
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                println!("{}\t{}\t{}\t{} msgs\t{}", s.id, s.updated_ms, s.model, s.messages, s.last_text);
            }
            Ok(0)
        }
        Some("session") => {
            let id = args.next().ok_or_else(|| anyhow::anyhow!("usage: fxrs session <id>"))?.to_string();
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            match store.load(&cfg.workspace, &id)? {
                Some(sess) => {
                    println!("id: {}", sess.id);
                    println!("workspace: {}", sess.workspace);
                    println!("updated_ms: {}", sess.updated_ms);
                    println!("model: {}", sess.model);
                    println!("mode: {:?}", sess.mode);
                    for m in &sess.messages {
                        println!("-- {}: {}", m.role_str(), m.last_text().unwrap_or_default());
                    }
                }
                None => bail!("no session `{id}`"),
            }
            Ok(0)
        }
        Some("status") => {
            let cfg = config::resolve(&cwd())?;
            println!("fxrs {}", crate::version::VERSION);
            println!("workspace: {}", cfg.workspace.display());
            println!("model: {}", cfg.model);
            println!("permission mode: {:?}", cfg.permission_mode.to_string());
            println!("max_agent_steps: {}", cfg.max_agent_steps);
            println!("max_tool_result_bytes: {}", cfg.max_tool_result_bytes);
            println!("context: {}", cfg.context);
            println!("sandbox: {:?}", cfg.sandbox);
            Ok(0)
        }
        Some("hooks") => {
            let cfg = config::resolve(&cwd())?;
            use crate::hooks::{HookKind, discover};
            for kind in [HookKind::PreToolUse, HookKind::Stop, HookKind::PostTurnEnd, HookKind::AttentionRequired] {
                let found = discover(kind, &cfg.workspace);
                println!("{}:", kind.event_name());
                if found.is_empty() {
                    println!("  (none)");
                }
                for f in found {
                    println!("  {}", f.display());
                }
            }
            Ok(0)
        }
        Some("permissions") => {
            let cfg = config::resolve(&cwd())?;
            println!("permission mode: {:?}", cfg.permission_mode.to_string());
            println!("rules: {}", if cfg.permission_rules.is_empty() { "(default)" } else { "see settings" });
            for (k, v) in &cfg.permission_rules {
                println!("  {k}: {v:?}");
            }
            Ok(0)
        }
        Some("setup") => {
            println!("fxrs needs a model endpoint. Configure one of:");
            println!("  AI_GATEWAY_API_KEY=...            (Vercel AI Gateway, default)");
            println!("  FX_GATEWAY_BASE_URL=...           (gateway base, default https://gateway.vercel.ai)");
            println!("  ANTHROPIC_API_KEY=...             (native Anthropic)");
            println!("  AI_BASE_URL=... AI_API_KEY=...    (OpenAI-compatible local server)");
            println!("  FX_MODEL=...                      (model id, default openai/gpt-5.4)");
            println!("  FX_PERMISSION_MODE=ask|auto|yolo  (default auto)");
            println!("\nExample for a local server:");
            println!("  export AI_BASE_URL=http://localhost:11434/v1 AI_API_KEY=ollama FX_MODEL=llama3.1");
            Ok(0)
        }
        Some("models") => {
            let cfg = config::resolve(&cwd())?;
            println!("resolved model: {}", cfg.model);
            println!("provider: {}", providers_summary(&cfg));
            println!("\nSet FX_MODEL to choose. Examples:");
            println!("  openai/gpt-5.4        (AI Gateway default)");
            println!("  claude-sonnet-4-6      (Anthropic API)");
            println!("  ollama/llama3.1        (OpenAI-compatible base URL)");
            Ok(0)
        }
        Some("help") | Some("-h") | Some("--help") => {
            show_cli_help();
            Ok(0)
        }
        Some("version") | Some("-v") | Some("--version") => {
            println!("fxrs {}", crate::version::VERSION);
            Ok(0)
        }
        Some("upgrade") => {
            println!("fxrs: `upgrade` is a no-op for this build; update via your package manager.");
            Ok(0)
        }
        Some(other) => {
            eprintln!("fxrs: unknown command `{other}`");
            show_cli_help();
            bail!("unknown command")
        }
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().context("getting current directory").unwrap_or_else(|_| PathBuf::from("."))
}

fn providers_summary(_cfg: &config::Config) -> String {
    if std::env::var("AI_GATEWAY_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
        "AI Gateway".into()
    } else if std::env::var("ANTHROPIC_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
        "Anthropic".into()
    } else if std::env::var("AI_BASE_URL").map(|v| !v.is_empty()).unwrap_or(false) {
        "OpenAI-compatible base URL".into()
    } else {
        "not configured (run `fxrs setup`)".into()
    }
}

fn show_cli_help() {
    println!(
        "fxrs — Rust port of fx (Vercel Labs) — a tiny terminal coding agent\n\n\
         usage:\n\
         \x1b[32m  fxrs\x1b[0m                    interactive shell\n\
         \x1b[32m  fxrs ask <prompt>\x1b[0m       one-shot prompt (reads prompt from argv or stdin)\n\
         \x1b[32m  fxrs resume [last|<id>]\x1b[0m resume a session (default: latest)\n\
         \x1b[32m  fxrs sessions\x1b[0m           list sessions\n\
         \x1b[32m  fxrs session <id>\x1b[0m       show a session\n\
         \x1b[32m  fxrs status\x1b[0m             show config status\n\
         \x1b[32m  fxrs permissions\x1b[0m        show permission rules\n\
         \x1b[32m  fxrs models\x1b[0m             show resolved model / provider\n\
         \x1b[32m  fxrs setup\x1b[0m              provider configuration guide\n\
         \x1b[32m  fxrs version\x1b[0m            version info\n\
         \x1b[32m  fxrs help\x1b[0m               this help\n\n\
         Environment: FX_MODEL, FX_PERMISSION_MODE, AI_GATEWAY_API_KEY,\n\
         FX_GATEWAY_BASE_URL, ANTHROPIC_API_KEY, AI_BASE_URL, AI_API_KEY\n\
         Config: ~/.fx/settings.json, <workspace>/.fx.json (see README)"
    );
}

async fn run_ask(rest: &[String]) -> Result<i32> {
    let args = rest.to_vec();
    let mut resume: Option<String> = None;
    let mut prompt_parts: Vec<String> = vec![];
    let mut system: Option<String> = None;
    let mut messages: Vec<String> = vec![];

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--resume" | "-r" => {
                i += 1;
                resume = args.get(i).cloned();
            }
            "--system" => {
                i += 1;
                system = args.get(i).cloned();
            }
            "--message" | "-m" => {
                i += 1;
                if let Some(m) = args.get(i) {
                    messages.push(m.clone());
                }
            }
            "--json" => {
                // accepted for fx CLI compat; output is already structured-ish
            }
            "--help" | "-h" => {
                println!("usage: fxrs ask [--resume <id>] [--system <s>] [-m <msg>] <prompt>");
                return Ok(0);
            }
            _ => prompt_parts.push(a.clone()),
        }
        i += 1;
    }

    let prompt = if prompt_parts.is_empty() {
        // Read from stdin if piped, else prompt.
        use std::io::Read;
        let mut buf = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin().read_to_string(&mut buf)?;
            if buf.trim().is_empty() {
                bail!("fxrs ask: empty prompt (pass text or pipe stdin)");
            }
        } else {
            bail!("fxrs ask: pass a prompt: fxrs ask \"explain this repo\"");
        }
        Some(buf)
    } else {
        Some(prompt_parts.join(" "))
    };

    let cfg = Arc::new(config::resolve(&cwd())?);
    let store = SessionStore::new()?;
    let human = QuietHuman;

    // Non-interactive mode: a one-shot agent turn. Reuse the interactive where
    // resume requested — fall back to one-shot semantics via non-interactive.
    let req = AgentRequest {
        prompt,
        system,
        interactive: false,
        resume,
        messages,
    };
    let out: AgentOutput = crate::agent::run(req, cfg.clone(), &human, &store).await?;
    if let Some(e) = &out.error {
        eprintln!("fxrs error: {e}");
        return Ok(1);
    }
    match out.finish_reason {
        FinishReason::MaxSteps => eprintln!(
            "fxrs: stopped after {} steps (max_agent_steps)",
            out.steps
        ),
        _ => {}
    }
    println!(
        "\n[fxrs] session {} · {} steps · {} tool calls · {} tokens (${:.4})",
        out.session_id, out.steps, out.tool_calls, out.total_tokens, out.cost_usd
    );
    Ok(0)
}

#[allow(dead_code)]
fn _p(_: &Path) {}
