//! Human-facing UI: the interactive REPL (Unix-shell form factor) and the
//! `Human` sink the agent reports into. Per-turn output goes to stdout as
//! plain text + occasional OSC terminal-title updates; status/approval
//! prompts go to stderr so piped stdout stays machine-parseable.

use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::agent::{self, AgentRequest};
use crate::config::Config;
use crate::providers::{self, ToolUse};
use crate::sessions::SessionStore;

// ------------------------------------------------------------------ Human
pub trait Human: Send + Sync {
    fn step_started(&self, _step: usize) {}
    fn text_delta(&self, _text: &str) {}
    fn reasoning_started(&self) {}
    fn reasoning_delta(&self, _text: &str) {}
    fn stream_done(&self) {}
    fn trace_tool(&self, _name: String) {}
    fn tool_result(&self, _name: &str, _result: &str) {}
    fn approve(&self, req: &crate::approval::ApprovalRequest) -> bool;
}

/// Drops output entirely (used for non-interactive / background runs).
pub struct QuietHuman;
impl Human for QuietHuman {
    fn approve(&self, _req: &crate::approval::ApprovalRequest) -> bool {
        false
    }
}

/// Streams model output to stdout, tool chatter to stderr, and prompts for
/// permission decisions on stdin.
pub struct InteractiveHuman {
    pub quiet: bool,
    pub trace: bool,
}

impl InteractiveHuman {
    fn trace_line(&self, line: &str) {
        if self.trace {
            let _ = writeln!(std::io::stderr(), "\x1b[2m[trace] {}\x1b[0m", shade(line));
        }
    }
}

fn shade(s: &str) -> String {
    if s.len() > 240 {
        format!("{}…", s.chars().take(240).collect::<String>())
    } else {
        s.to_string()
    }
}

impl Human for InteractiveHuman {
    fn step_started(&self, step: usize) {
        let _ = set_title(&format!("[{step}] fxrs …"));
    }

    fn text_delta(&self, text: &str) {
        if self.quiet {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    fn reasoning_started(&self) {
        if self.quiet {
            return;
        }
        // One faint line so a long thinking phase never reads as a hang.
        let _ = writeln!(std::io::stderr().lock(), "\x1b[2mƒ thinking…\x1b[0m");
    }

    fn reasoning_delta(&self, text: &str) {
        if self.quiet || !self.trace {
            return;
        }
        let mut e = std::io::stderr().lock();
        let _ = e.write_all(b"\x1b[2m");
        let _ = e.write_all(text.as_bytes());
        let _ = e.write_all(b"\x1b[0m");
        let _ = e.flush();
    }

    fn stream_done(&self) {
        if self.quiet {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\n");
        let _ = out.flush();
        let _ = set_title("fxrs");
    }

    fn approve(&self, req: &crate::approval::ApprovalRequest) -> bool {
        if self.quiet {
            return false;
        }
        eprintln!("{}", req.prompt());
        let mut line = String::new();
        loop {
            eprint!("  allow? \x1b[1m(y)\x1b[0mes / \x1b[1m(n)\x1b[0mo / \x1b[1m(a)\x1b[0mllow always for this scope\x1b[0m: ");
            let _ = std::io::stderr().flush();
            line.clear();
            let mut buf = String::new();
            if std::io::stdin().lock().read_line(&mut buf).unwrap_or(0) == 0 {
                return false;
            }
            line = buf.trim().to_string();
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return true,
                "a" | "always" => return true,
                "n" | "no" => {
                    self.trace_line(&format!("denied {} {}", req.tool_name, req.target));
                    return false;
                }
                _ => continue,
            }
        }
    }
}

fn set_title(title: &str) -> std::io::Result<()> {
    // OSC 0 / OSC 2 title; ignore failures (non-TTY).
    let mut e = std::io::stderr().lock();
    e.write_all(b"\x1b]0;")?;
    e.write_all(title.as_bytes())?;
    e.write_all(b"\x07")?;
    e.flush()
}

// ------------------------------------------------------------- interactive
pub async fn run_interactive(
    config: Arc<Config>,
    store: &SessionStore,
    resume: Option<String>,
    trace: bool,
) -> Result<()> {
    let human = InteractiveHuman { quiet: false, trace };
    let provider = providers::resolve_provider(&config)?;
    eprintln!(
        "\x1b[1;36mƒx\x1b[0m rust port (\x1b[36m{}\x1b[0m) — workspace \x1b[90m{}\x1b[0m\n  model \x1b[36m{}\x1b[0m · permissions \x1b[36m{:?}\x1b[0m · type \x1b[32m/help\x1b[0m",
        crate::version::VERSION,
        config.workspace.display(),
        provider.model,
        config.permission_mode.to_string(),
    );

    let mut interactive_human = human;
    let rl = rustyline::DefaultEditor::new().context("creating readline")?;
    let mut rl = rl;
    let prompt = "ƒ> ";
    let mut first = true;

    loop {
        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => bail!("readline error: {e}"),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line.clone());

        if let Some(cmd) = crate::slash_commands::parse(trimmed) {
            use crate::slash_commands::Slash;
            match cmd {
                Slash::Settings => {
                    print!("{}", crate::settings_catalog::render(&config));
                    continue;
                }
                Slash::Exit => break,
                Slash::Help => {
                    println!("{}", crate::slash_commands::render_help());
                    continue;
                }
                Slash::Clear => {
                    let _ = std::io::stdout().lock().write_all(b"\x1b[2J\x1b[H");
                    continue;
                }
                Slash::Version => {
                    println!("fxrs {}", crate::version::VERSION);
                    continue;
                }
                Slash::Status => {
                    println!("model: {}", provider.model);
                    println!("permissions: {:?}", config.permission_mode.to_string());
                    println!("workspace: {}", config.workspace.display());
                    println!("max steps: {}", config.max_agent_steps);
                    println!("max tool result bytes: {}", config.max_tool_result_bytes);
                    println!("sandbox: {:?}", config.sandbox);
                    println!("additional dirs: {}", config.additional_directories.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
                    continue;
                }
                Slash::Model => {
                    println!("current model: {}", provider.model);
                    println!("set with FX_MODEL (e.g. openai/gpt-5.4 or claude-sonnet-4-6)");
                    continue;
                }
                Slash::Permissions => {
                    println!("permission mode: {:?}", config.permission_mode.to_string());
                    println!("set with FX_PERMISSION_MODE=ask|auto|yolo");
                    continue;
                }
                Slash::Sessions => {
                    let sessions = store.list(Some(&config.workspace))?;
                    for s in &sessions {
                        println!("{}\t{}\t{} msgs\t{}", s.id, s.model, s.messages, s.last_text);
                    }
                    println!("({} sessions)", sessions.len());
                    continue;
                }
                Slash::Session(id) => {
                    let sessions = store.list(Some(&config.workspace))?;
                    let target = id
                        .as_deref()
                        .map(|s| s.to_string())
                        .or_else(|| sessions.first().map(|s| s.id.clone()));
                    match target {
                        Some(t) => match store.load(&config.workspace, &t)? {
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
                            None => println!("no session `{t}`"),
                        },
                        None => println!("no sessions"),
                    }
                    continue;
                }
                Slash::Resume(id) => {
                    let sessions = store.list(Some(&config.workspace))?;
                    let target = match id.as_deref() {
                        Some("last") | None => sessions.first().map(|s| s.id.clone()),
                        Some(t) => Some(t.to_string()),
                    };
                    match target {
                        Some(rid) => {
                            println!("resuming {rid}");
                            let req = AgentRequest {
                                prompt: None,
                                system: None,
                                interactive: true,
                                resume: Some(rid),
                                messages: Vec::new(),
                            };
                            run_one(&config, &store, &interactive_human, req).await?;
                        }
                        None => println!("no sessions to resume"),
                    }
                    continue;
                }
                Slash::Usage(period) => {
                    let period = period.unwrap_or_else(|| "7d".into());
                    let since = crate::usage::parse_period(&period);
                    let totals = crate::usage::UsageStore::new().aggregate(since);
                    println!(
                        "usage ({period}): {} turns · {}k in / {}k out / {}k total tokens · {} tool calls · ${:.4}",
                        totals.turns,
                        totals.input_tokens / 1000,
                        totals.output_tokens / 1000,
                        totals.total_tokens / 1000,
                        totals.tool_calls,
                        totals.cost_usd,
                    );
                    continue;
                }
                Slash::Doctor => {
                    let issues = crate::cli::doctor_checks(&config);
                    if issues.is_empty() {
                        println!("all checks passed ✓");
                    } else {
                        for (sev, msg) in &issues {
                            println!("[{}] {}", if *sev == 'w' { "warn" } else { "fail" }, msg);
                        }
                    }
                    continue;
                }
                Slash::Setup => {
                    println!("fxrs needs a model endpoint. Configure one of:");
                    println!("  AI_GATEWAY_API_KEY=...            (Vercel AI Gateway, default)");
                    println!("  ANTHROPIC_API_KEY=...             (native Anthropic)");
                    println!("  AI_BASE_URL=... AI_API_KEY=...    (OpenAI-compatible local server)");
                    println!("  FX_MODEL=...                      (model id, default openai/gpt-5.4)");
                    println!("  FX_PERMISSION_MODE=ask|auto|yolo  (default auto)");
                    continue;
                }
                Slash::Trace => {
                    interactive_human.trace = !interactive_human.trace;
                    println!("trace {}", if interactive_human.trace { "on" } else { "off" });
                    continue;
                }
                Slash::Feedback => {
                    println!("fxrs feedback: open an issue at github.com/1deat0r/fx-rust");
                    continue;
                }
                Slash::Workspace => {
                    println!("workspace: {}", config.workspace.display());
                    let ag = crate::config::load_project_instructions(&config.workspace);
                    if ag.is_empty() {
                        println!("AGENTS.md: (none loaded)");
                    } else {
                        println!("AGENTS.md: {} file(s), {} chars", ag.len(), ag.iter().map(|s| s.len()).sum::<usize>());
                    }
                    continue;
                }
                Slash::Compact => println!("(not ready: context compaction is a later phase)"),
                Slash::Login => println!("(not ready: OAuth login is a later phase)"),
                Slash::Logout => println!("(not ready: OAuth logout is a later phase)"),
                Slash::Unknown(name) => {
                    println!("unknown slash command `/{name}` — try /help");
                    continue;
                }
            }
        }

        let mut request = AgentRequest {
            prompt: Some(trimmed.to_string()),
            system: None,
            interactive: true,
            resume: None,
            messages: Vec::new(),
        };
        if first && resume.is_none() {
            // nothing to seed; run normally.
        }
        if let Some(r) = &resume {
            if first {
                request.resume = Some(r.clone());
                request.prompt = Some(format!("\n(continued) {trimmed}"));
            }
        }
        first = false;

        run_one(&config, &store, &interactive_human, request).await?;
    }
    Ok(())
}

async fn run_one(
    config: &Arc<Config>,
    store: &SessionStore,
    human: &InteractiveHuman,
    req: AgentRequest,
) -> Result<()> {
    let out = agent::run(req, config.clone(), human, store).await?;
    if let Some(e) = &out.error {
        eprintln!("\x1b[31mƒx error: {e}\x1b[0m");
    }
    match out.finish_reason {
        agent::FinishReason::MaxSteps => eprintln!(
            "\x1b[33mƒx stopped: max agent steps reached ({})\x1b[0m",
            out.steps
        ),
        _ => {}
    }
    eprintln!(
        "\x1b[90mƒx done · session {} · {} steps · {} tool calls · {} tokens (${:.4})\x1b[0m",
        short_id(&out.session_id),
        out.steps,
        out.tool_calls,
        out.total_tokens,
        out.cost_usd,
    );
    Ok(())
}

fn short_id(id: &str) -> String {
    if id.len() > 12 {
        id[..12].to_string()
    } else {
        id.to_string()
    }
}



#[allow(dead_code)]
fn _tool_use_json(t: &ToolUse) -> String {
    serde_json::json!({ "id": t.id, "name": t.name, "arguments": t.arguments }).to_string()
}
