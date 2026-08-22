//! Human-facing UI: the interactive REPL (Unix-shell form factor) and the
//! `Human` sink the agent reports into. Per-turn output goes to stdout as
//! plain text + occasional OSC terminal-title updates; status/approval
//! prompts go to stderr so piped stdout stays machine-parseable.

use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

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
    // Phase 5 TUI: opt in with FXRS_TUI=1 (the `fxrs tui` command arms it
    // directly). The REPL remains the default stdout form factor.
    if std::env::var("FXRS_TUI")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return crate::tui::run_tui(config, store, resume, trace).await;
    }
    let human = InteractiveHuman {
        quiet: false,
        trace,
    };
    let provider = providers::resolve_provider(&config)?;
    eprintln!(
        "\x1b[1;36mƒx\x1b[0m rust port (\x1b[36m{}\x1b[0m) — workspace \x1b[90m{}\x1b[0m\n  model \x1b[36m{}\x1b[0m · permissions \x1b[36m{:?}\x1b[0m · type \x1b[32m/help\x1b[0m",
        crate::version::VERSION,
        config.workspace.display(),
        provider.model,
        config.permission_mode.to_string(),
    );

    // Restore-on-resume: surface background daemons and terminal sessions that
    // are still alive so a resumed agent (and its human) immediately see the
    // supervisor state.
    if let Ok(bg) = crate::background::BackgroundStore::open() {
        let running = bg
            .list()
            .iter()
            .filter(|r| r.status == crate::background::BgStatus::Running)
            .count();
        if running > 0 {
            eprintln!(
                "\x1b[90m{}\x1b[0m background process(es) running — \x1b[36m/background supervise\x1b[0m",
                running
            );
        }
    }
    if let Ok(term) = crate::terminal::TerminalStore::open() {
        let running = term
            .list()
            .iter()
            .filter(|r| r.status == crate::terminal::TermStatus::Running)
            .count();
        if running > 0 {
            eprintln!(
                "\x1b[90m{}\x1b[0m terminal session(s) running — \x1b[36m/terminal\x1b[0m",
                running
            );
        }
    }

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
                Slash::History(limit) => {
                    let n = limit
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(20)
                        .min(200);
                    let recs = crate::history::HistoryStore::new().query(None, n);
                    if recs.is_empty() {
                        println!("no history");
                    }
                    for r in &recs {
                        println!(
                            "{} {}",
                            r.timestamp_ms,
                            crate::sessions::workspace_name(&r.workspace_root)
                        );
                        println!("    {}", r.text.chars().take(240).collect::<String>());
                    }
                    println!("({} prompts)", recs.len());
                    continue;
                }
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
                    println!(
                        "additional dirs: {}",
                        config
                            .additional_directories
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
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
                        println!(
                            "{}\t{}\t{} msgs\t{}",
                            s.id, s.model, s.messages, s.last_text
                        );
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
                                    println!(
                                        "-- {}: {}",
                                        m.role_str(),
                                        m.last_text().unwrap_or_default()
                                    );
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
                                images: Vec::new(),
                            };
                            run_one(&config, store, &interactive_human, req).await?;
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
                    println!(
                        "  AI_BASE_URL=... AI_API_KEY=...    (OpenAI-compatible local server)"
                    );
                    println!(
                        "  FX_MODEL=...                      (model id, default openai/gpt-5.4)"
                    );
                    println!("  FX_PERMISSION_MODE=ask|auto|yolo  (default auto)");
                    continue;
                }
                Slash::Trace => {
                    interactive_human.trace = !interactive_human.trace;
                    println!(
                        "trace {}",
                        if interactive_human.trace { "on" } else { "off" }
                    );
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
                        println!(
                            "AGENTS.md: {} file(s), {} chars",
                            ag.len(),
                            ag.iter().map(|s| s.len()).sum::<usize>()
                        );
                    }
                    continue;
                }
                Slash::Background(arg) => {
                    let mut store = match crate::background::BackgroundStore::open() {
                        Ok(s) => s,
                        Err(e) => {
                            println!("background store error: {e:#}");
                            continue;
                        }
                    };
                    let (sub, id_arg) =
                        match arg.as_deref().unwrap_or("").split_once(char::is_whitespace) {
                            Some((s, rest)) => (s, rest.trim().to_string()),
                            None => (arg.as_deref().unwrap_or("list"), String::new()),
                        };
                    let id: Option<&str> = if id_arg.is_empty() {
                        None
                    } else {
                        Some(id_arg.as_str())
                    };
                    match (sub, id) {
                        ("list" | "l", _) | ("get" | "stop" | "supervise" | "tree" | "stop-tree", None) => {
                            match sub {
                                "list" | "l" => {
                                    let records = store.list().to_vec();
                                    if records.is_empty() {
                                        println!("no background processes");
                                    } else {
                                        println!("{}", crate::background::render_table(&records));
                                    }
                                }
                                "supervise" => {
                                    if store.list().is_empty() {
                                        println!("no background processes");
                                    } else {
                                        println!(
                                            "{}",
                                            crate::background::render_supervise(&store.supervise())
                                        );
                                    }
                                }
                                _ => println!("usage: /background {sub} <id>"),
                            }
                        }
                        ("get", Some(id)) => match store.log_text(id, 16 * 1024, None) {
                            Ok(text) => println!("{text}"),
                            Err(e) => println!("{e:#}"),
                        },
                        ("tree", Some(id)) => match store.get(id) {
                            Some(record) => {
                                let table = crate::background::process_table();
                                println!(
                                    "{}",
                                    crate::background::render_tree(record, &table)
                                );
                            }
                            None => println!("unknown background process id `{id}`"),
                        },
                        ("stop", Some(id)) => match store.stop(id, 5000) {
                            Ok(r) => println!("stopped {} (pid {})", r.id, r.pid),
                            Err(e) => println!("{e:#}"),
                        },
                        ("stop-tree" | "stop_tree", Some(id)) => match store.stop_tree(id, 5000) {
                            Ok(r) => println!("stopped {} (pid {}) and descendants", r.id, r.pid),
                            Err(e) => println!("{e:#}"),
                        },
                        (other, _) => println!(
                            "unknown background subcommand `{other}` (list | supervise | tree <id> | get <id> | stop <id> | stop-tree <id>)"
                        ),
                    }
                }
                Slash::Terminal(arg) => {
                    let mut store = match crate::terminal::TerminalStore::open() {
                        Ok(s) => s,
                        Err(e) => {
                            println!("terminal store error: {e:#}");
                            continue;
                        }
                    };
                    let (sub, id_arg) =
                        match arg.as_deref().unwrap_or("").split_once(char::is_whitespace) {
                            Some((s, rest)) => (s, rest.trim().to_string()),
                            None => (arg.as_deref().unwrap_or("list"), String::new()),
                        };
                    let id: Option<&str> = if id_arg.is_empty() {
                        None
                    } else {
                        Some(id_arg.as_str())
                    };
                    match (sub, id) {
                        ("list" | "l", _) => {
                            let records = store.list().to_vec();
                            if records.is_empty() {
                                println!("no terminal sessions");
                            } else {
                                println!("{}", crate::terminal::render_table(&records));
                            }
                        }
                        ("get" | "read", Some(id)) => match store.read(id, 200, 16 * 1024, false, false) {
                            Ok(text) => println!("{text}"),
                            Err(e) => println!("{e:#}"),
                        },
                        ("send", Some(id)) => {
                            let rest = arg.as_deref().unwrap_or("");
                            let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
                            if parts.len() < 2 || parts[1].trim().is_empty() {
                                println!("usage: /terminal send <id> <text>");
                                continue;
                            }
                            match store.send(id, parts[1].trim(), true) {
                                Ok(()) => println!("sent to {id}"),
                                Err(e) => println!("{e:#}"),
                            }
                        }
                        ("stop", Some(id)) => match store.stop(id) {
                            Ok(r) => println!("stopped {} (pid {})", r.id, r.pid),
                            Err(e) => println!("{e:#}"),
                        },
                        (other, _) => println!(
                            "unknown terminal subcommand `{other}` (list | get <id> | send <id> <text> | stop <id>)"
                        ),
                    }
                }
                Slash::Skills(arg) => {
                    let line = arg.unwrap_or_default();
                    let command = crate::skills::commands::parse_command(&line);
                    match crate::skills::commands::execute_command(&config.workspace, &command) {
                        Ok(result) => {
                            print!("{}", result.render());
                            if !result.render().ends_with('\n') {
                                println!();
                            }
                        }
                        Err(e) => println!("fxrs skills: {e:#}"),
                    }
                    continue;
                }
                Slash::Compact => println!("(not ready: context compaction is a later phase)"),
                Slash::Login(arg) => {
                    let line = arg.unwrap_or_default();
                    let mut parts = line.split_whitespace();
                    let provider = parts.next().map(|s| s.to_string());
                    let rest: Vec<String> = parts.map(|s| s.to_string()).collect();
                    let mut key = None;
                    let mut base_url = None;
                    let mut rest = rest;
                    while !rest.is_empty() {
                        let tok = rest.remove(0);
                        match tok.as_str() {
                            "--key" => {
                                key = rest.first().cloned();
                                if key.is_some() {
                                    rest.remove(0);
                                }
                            }
                            "--base-url" => {
                                base_url = rest.first().cloned();
                                if base_url.is_some() {
                                    rest.remove(0);
                                }
                            }
                            _ => {}
                        }
                    }
                    let provider = provider.unwrap_or_else(|| {
                        if config.model.starts_with("anthropic/")
                            || config.model.starts_with("claude-")
                        {
                            "anthropic".into()
                        } else {
                            "gateway".into()
                        }
                    });
                    let key = key.or_else(|| std::env::var("FX_API_KEY").ok());
                    match crate::auth::set_key(
                        &provider,
                        key.as_deref().unwrap_or(""),
                        base_url.as_deref(),
                    ) {
                        Ok(()) => {
                            if key.is_some() {
                                println!("saved API key for provider `{provider}` (fxrs auth remove {provider} to clear)");
                            } else {
                                println!("no key provided: set FX_API_KEY or pass `--key`");
                            }
                        }
                        Err(e) => println!("login failed: {e:#}"),
                    }
                    continue;
                }
                Slash::Logout(arg) => {
                    let provider = arg
                        .unwrap_or_default()
                        .split_whitespace()
                        .next()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let provider = if provider.is_empty() {
                        if config.model.starts_with("anthropic/")
                            || config.model.starts_with("claude-")
                        {
                            "anthropic".to_string()
                        } else {
                            "gateway".to_string()
                        }
                    } else {
                        provider
                    };
                    match crate::auth::remove_key(&provider) {
                        Ok(true) => println!("removed API key for provider `{provider}`"),
                        Ok(false) => println!("no stored key for provider `{provider}`"),
                        Err(e) => println!("logout failed: {e:#}"),
                    }
                    continue;
                }
                Slash::Credits => {
                    let store = match crate::auth::load() {
                        Ok(s) => s,
                        Err(e) => {
                            println!("auth error: {e:#}");
                            continue;
                        }
                    };
                    let (key, _) = crate::auth::resolve_key("gateway", &store);
                    match crate::gateway::fetch_credits(
                        key.filter(|k| !k.is_empty()).as_deref(),
                        None,
                    ) {
                        crate::gateway::CreditsResult::Loaded(snap) => {
                            println!("credits: {}", snap.balance.as_deref().unwrap_or("unknown"));
                            if let Some(used) = &snap.used {
                                println!("used: {used}");
                            }
                            if let Some(plan) = &snap.plan {
                                println!("plan: {plan}");
                            }
                        }
                        crate::gateway::CreditsResult::Failed { .. } => {
                            println!("credits: failed to fetch (set FX_GATEWAY_API_KEY or run `fxrs login`)");
                        }
                    }
                    continue;
                }
                Slash::Stats => {
                    let period = "7d";
                    let totals = crate::usage::UsageStore::new()
                        .aggregate(crate::usage::parse_period(period));
                    println!("usage (last {period}):");
                    println!(
                        "  turns: {} · sessions: {} · input: {} · output: {} · total: {}",
                        totals.turns,
                        totals.sessions.len(),
                        totals.input_tokens,
                        totals.output_tokens,
                        totals.total_tokens
                    );
                    println!(
                        "  tool calls: {} · steps: {} · est. cost: ${:.4}",
                        totals.tool_calls, totals.steps, totals.cost_usd
                    );
                    continue;
                }
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
            images: Vec::new(),
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

        run_one(&config, store, &interactive_human, request).await?;
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
    if out.finish_reason == agent::FinishReason::MaxSteps {
        eprintln!(
            "\x1b[33mƒx stopped: max agent steps reached ({})\x1b[0m",
            out.steps
        )
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

// ---------------------------------------------------------------- TUI helpers
//
// The full-screen TUI reuses the interactive command logic by asking for a
// rendered string instead of println. These mirrors keep the two UIs honest.

/// Rendered output for `/background <args>` (also `fxrs background`).
pub fn render_slash_background(_config: &Config, arg: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut store = match crate::background::BackgroundStore::open() {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "background store error: {e:#}");
            return out;
        }
    };
    let (sub, id_arg) = match arg.unwrap_or("").split_once(char::is_whitespace) {
        Some((s, rest)) => (s, rest.trim().to_string()),
        None => (arg.unwrap_or("list"), String::new()),
    };
    let id: Option<&str> = if id_arg.is_empty() {
        None
    } else {
        Some(id_arg.as_str())
    };
    match (sub, id) {
        ("list" | "l", _) | ("get" | "stop" | "supervise" | "tree" | "stop-tree", None) => {
            match sub {
                "list" | "l" => {
                    let records = store.list().to_vec();
                    if records.is_empty() {
                        let _ = writeln!(out, "no background processes");
                    } else {
                        let _ = writeln!(out, "{}", crate::background::render_table(&records));
                    }
                }
                "supervise" => {
                    if store.list().is_empty() {
                        let _ = writeln!(out, "no background processes");
                    } else {
                        let _ = writeln!(
                            out,
                            "{}",
                            crate::background::render_supervise(&store.supervise())
                        );
                    }
                }
                _ => {
                    let _ = writeln!(out, "usage: /background {sub} <id>");
                }
            }
        }
        ("get", Some(id)) => match store.log_text(id, 16 * 1024, None) {
            Ok(text) => {
                let _ = writeln!(out, "{text}");
            }
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
            }
        },
        ("tree", Some(id)) => match store.get(id) {
            Some(record) => {
                let table = crate::background::process_table();
                let _ = writeln!(out, "{}", crate::background::render_tree(record, &table));
            }
            None => {
                let _ = writeln!(out, "unknown background process id `{id}`");
            }
        },
        ("stop", Some(id)) => match store.stop(id, 5000) {
            Ok(r) => {
                let _ = writeln!(out, "stopped {} (pid {})", r.id, r.pid);
            }
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
            }
        },
        ("stop-tree" | "stop_tree", Some(id)) => match store.stop_tree(id, 5000) {
            Ok(r) => {
                let _ = writeln!(out, "stopped {} (pid {}) and descendants", r.id, r.pid);
            }
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
            }
        },
        (other, _) => {
            let _ = writeln!(
                out,
                "unknown background subcommand `{other}` (list | supervise | tree <id> | get <id> | stop <id> | stop-tree <id>)"
            );
        }
    }
    out
}

/// Rendered output for `/terminal <args>`.
pub fn render_slash_terminal(arg: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut store = match crate::terminal::TerminalStore::open() {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "terminal store error: {e:#}");
            return out;
        }
    };
    let (sub, id_arg) = match arg.unwrap_or("").split_once(char::is_whitespace) {
        Some((s, rest)) => (s, rest.trim().to_string()),
        None => (arg.unwrap_or("list"), String::new()),
    };
    let id: Option<&str> = if id_arg.is_empty() {
        None
    } else {
        Some(id_arg.as_str())
    };
    match (sub, id) {
        ("list" | "l", _) => {
            let records = store.list().to_vec();
            if records.is_empty() {
                let _ = writeln!(out, "no terminal sessions");
            } else {
                let _ = writeln!(out, "{}", crate::terminal::render_table(&records));
            }
        }
        ("get" | "read", Some(id)) => match store.read(id, 200, 16 * 1024, false, false) {
            Ok(text) => {
                let _ = writeln!(out, "{text}");
            }
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
            }
        },
        ("send", Some(id)) => {
            let rest = arg.unwrap_or("");
            let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
            if parts.len() < 2 || parts[1].trim().is_empty() {
                let _ = writeln!(out, "usage: /terminal send <id> <text>");
            } else if let Err(e) = store.send(id, parts[1].trim(), true) {
                let _ = writeln!(out, "{e:#}");
            } else {
                let _ = writeln!(out, "sent to {id}");
            }
        }
        ("stop", Some(id)) => match store.stop(id) {
            Ok(r) => {
                let _ = writeln!(out, "stopped {} (pid {})", r.id, r.pid);
            }
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
            }
        },
        (other, _) => {
            let _ = writeln!(
                out,
                "unknown terminal subcommand `{other}` (list | get <id> | send <id> <text> | stop <id>)"
            );
        }
    }
    out
}

/// Rendered output for `/skills <args>`.
pub fn render_slash_skills(workspace: &std::path::Path, arg: Option<&str>) -> String {
    use std::fmt::Write as _;
    let line = arg.unwrap_or_default();
    let command = crate::skills::commands::parse_command(line);
    let mut out = String::new();
    match crate::skills::commands::execute_command(workspace, &command) {
        Ok(result) => {
            let _ = write!(out, "{}", result.render());
            if !result.render().ends_with('\n') {
                let _ = writeln!(out);
            }
        }
        Err(e) => {
            let _ = writeln!(out, "fxrs skills: {e:#}");
        }
    }
    out
}
