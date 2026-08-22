//! TUI application: owns the screen, transcript, composer, and the event
//! loop, and runs the agent as a background task with a `TuiHuman` sink.
//!
//! Layout (rows):
//!   0            header / status line
//!   1..H-3       transcript viewport
//!   H-2          composer (input line) or approval prompt
//!   H-1          footer
//!
//! The agent streams events through a channel; the loop drains the channel
//! every frame, so rendering stays responsive while the model streams.

use std::sync::Arc;
use std::sync::mpsc;
use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};

use crate::agent::AgentRequest;
use crate::approval::ApprovalRequest;
use crate::config::Config;
use crate::sessions::SessionStore;

use super::composer::{Composer, ComposerAction};
use super::keys::{self, Key};
use super::screen::{CellStyle, Screen};
use super::theme::{self, Theme};
use super::transcript::{LineKind, Transcript};
use super::widgets::{self, FooterInfo};

/// Events the agent pushes into the UI.
pub enum Ev {
    Step(usize),
    Text(String),
    ReasoningStart,
    ReasoningDelta(String),
    Done,
    Tool(String, String),
    Banner(String),
    Error(String),
    Approve(ApprovalRequest, std::sync::mpsc::Sender<bool>),
}

/// A `Human` sink that forwards agent output into the TUI event channel.
/// `approve` parks the agent task until the UI answers y/n/a.
pub struct TuiHuman {
    tx: mpsc::Sender<Ev>,
}

impl TuiHuman {
    pub fn new(tx: mpsc::Sender<Ev>) -> Self {
        TuiHuman { tx }
    }
}

impl crate::ui::Human for TuiHuman {
    fn step_started(&self, step: usize) {
        let _ = self.tx.send(Ev::Step(step));
    }
    fn text_delta(&self, text: &str) {
        let _ = self.tx.send(Ev::Text(text.to_string()));
    }
    fn reasoning_started(&self) {
        let _ = self.tx.send(Ev::ReasoningStart);
    }
    fn reasoning_delta(&self, text: &str) {
        let _ = self.tx.send(Ev::ReasoningDelta(text.to_string()));
    }
    fn stream_done(&self) {
        let _ = self.tx.send(Ev::Done);
    }
    fn trace_tool(&self, name: String) {
        let _ = self.tx.send(Ev::Tool(name, String::new()));
    }
    fn tool_result(&self, name: &str, result: &str) {
        let _ = self.tx.send(Ev::Tool(name.to_string(), result.to_string()));
    }
    fn approve(&self, req: &crate::approval::ApprovalRequest) -> bool {
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        if self.tx.send(Ev::Approve(req.clone(), answer_tx)).is_err() {
            return false;
        }
        answer_rx
            .recv_timeout(Duration::from_secs(600))
            .unwrap_or_default()
        
    }
}

// ------------------------------------------------------------------ app

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    Confirm,
}

pub struct App {
    config: Arc<Config>,
    store: SessionStore,
    theme: Theme,
    screen: Screen,
    transcript: Transcript,
    composer: Composer,
    mode: Mode,
    running: Option<tokio::task::JoinHandle<()>>,
    agent_rx: Option<mpsc::Receiver<Ev>>,
    pending_approval: Option<std::sync::mpsc::Sender<bool>>,
    approval_req: Option<ApprovalRequest>,
    unread: usize,
    exit_requested: bool,
    trace: bool,
}

impl App {
    pub fn new(config: Arc<Config>, store: SessionStore, resume: Option<String>, trace: bool) -> Result<Self> {
        let (cols, rows) = super::screen::terminal_size();
        let theme = theme::resolve(None);
        let transcript = Transcript::new(cols as usize);
        let mut composer = Composer::new();
        composer.set_prompt_prefix("ƒ> ");
        let mut app = App {
            config,
            store,
            theme,
            screen: Screen::new(cols, rows, theme),
            transcript,
            composer,
            mode: Mode::Normal,
            running: None,
            agent_rx: None,
            pending_approval: None,
            approval_req: None,
            unread: 0,
            exit_requested: false,
            trace,
        };
        app.init_banner(resume)?;
        Ok(app)
    }

    fn init_banner(&mut self, resume: Option<String>) -> Result<()> {
        self.transcript.push(
            LineKind::System,
            format!(
                "ƒx rust port ({}) — workspace {}",
                crate::version::VERSION,
                self.config.workspace.display()
            ),
        );
        self.transcript.push(
            LineKind::System,
            format!(
                "model {} · permissions {} · type /help · esc for shortcuts",
                self.model_label(),
                self.config.permission_mode,
            ),
        );

        // Restore-on-resume: surface running background processes and
        // terminal sessions, mirroring the REPL.
        if let Ok(bg) = crate::background::BackgroundStore::open() {
            let running = bg
                .list()
                .iter()
                .filter(|r| r.status == crate::background::BgStatus::Running)
                .count();
            if running > 0 {
                self.transcript.push(
                    LineKind::System,
                    format!("{running} background process(es) running — /background supervise"),
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
                self.transcript.push(
                    LineKind::System,
                    format!("{running} terminal session(s) running — /terminal"),
                );
            }
        }

        if let Some(id) = resume {
            match id.as_str() {
                "" | "last" => {
                    if let Some(sess) = self
                        .store
                        .list(Some(&self.config.workspace))
                        .ok()
                        .and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0)) })
                    {
                        self.resume_session(&sess.id)?;
                    }
                }
                _ => self.resume_session(&id)?,
            }
        }
        Ok(())
    }

    fn resume_session(&mut self, id: &str) -> Result<()> {
        let sess = self.store.load_or_error(&self.config.workspace, id)?;
        self.transcript.push(
            LineKind::System,
            format!("resuming session {} ({})", sess.id, sess.model),
        );
        // Replay history into the transcript.
        for msg in &sess.messages {
            match msg.role.as_str() {
                "user" => {
                    for block in &msg.content {
                        if let crate::providers::ContentBlock::Text(t) = block {
                            self.transcript.push(LineKind::User, t.clone());
                        }
                    }
                }
                "assistant" => {
                    for block in &msg.content {
                        match block {
                            crate::providers::ContentBlock::Text(t) => {
                                self.transcript.push(LineKind::Assistant, t.clone());
                            }
                            crate::providers::ContentBlock::ToolUse { name, input, .. } => {
                                let cmd = input
                                    .as_object()
                                    .and_then(|m| m.get("command"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.chars().take(120).collect::<String>())
                                    .unwrap_or_default();
                                self.transcript.push(LineKind::Tool, format!("⎿ {} {cmd}", name));
                            }
                            _ => {}
                        }
                    }
                }
                "tool" => {
                    for block in &msg.content {
                        if let crate::providers::ContentBlock::Text(t) = block {
                            self.transcript.push(LineKind::Tool, t.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn model_label(&self) -> String {
        crate::providers::resolve_provider(&self.config)
            .map(|p| p.model)
            .unwrap_or_else(|_| "unset".to_string())
    }

    fn running_task(&self) -> bool {
        self.running.is_some()
    }

    // ---------------- agent execution

    fn spawn_agent(&mut self, req: AgentRequest) {
        let (tx, rx) = mpsc::channel::<Ev>();
        let human = TuiHuman::new(tx);
        let config = self.config.clone();
        let store = self.store.clone();
        let is_trace = self.trace;

        let task = tokio::task::spawn_local(async move {
            let out = match crate::agent::run(req, config.clone(), &human, &store).await {
                Ok(o) => o,
                Err(e) => {
                    let _ = human.tx.send(Ev::Error(format!("{e:#}")));
                    return;
                }
            };
            if let Some(e) = &out.error {
                let _ = human.tx.send(Ev::Error(e.clone()));
            }
            let _ = human.tx.send(Ev::Banner(format!(
                "done · session {} · {} steps · {} tool calls · {} tokens (${:.4})",
                short_id(&out.session_id),
                out.steps,
                out.tool_calls,
                out.total_tokens,
                out.cost_usd
            )));
            let _ = human.tx.send(Ev::Done);
            let _ = is_trace;
        });

        self.running = Some(task);
        self.agent_rx = Some(rx);
    }

    fn interrupt(&mut self) {
        if let Some(task) = self.running.take() {
            task.abort();
            self.agent_rx = None;
            self.mode = Mode::Normal;
            self.pending_approval = None;
            self.approval_req = None;
            self.transcript.push(LineKind::System, "interrupted");
        }
    }

    // ---------------- event handlers

    fn handle_key(&mut self, key: Key) {
        match self.mode {
            Mode::Help => {
                if let Key::Escape | Key::Char('q') | Key::Char('Q') | Key::Enter = key {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Confirm => {
                match key {
                    Key::Char('y') | Key::Char('Y') | Key::Enter => self.answer_approval(true),
                    Key::Char('a') | Key::Char('A') => self.answer_approval(true),
                    Key::Char('n') | Key::Char('N') | Key::Escape => self.answer_approval(false),
                    _ => {}
                }
                return;
            }
            Mode::Normal => {}
        }

        match key {
            Key::Ctrl('d') => {
                self.exit_requested = true;
            }
            Key::Escape => {
                if self.running_task() {
                    self.interrupt();
                } else {
                    self.mode = Mode::Help;
                }
            }
            Key::Ctrl('l') => {
                // repaint: next frame redraws everything.
            }
            Key::PageUp => {
                let rows = self.transcript_view_rows();
                self.transcript.page_up(rows as usize);
            }
            Key::PageDown => {
                let rows = self.transcript_view_rows();
                self.transcript.page_down(rows as usize);
            }
            Key::Up if self.running_task() => {
                let rows = self.transcript_view_rows();
                self.transcript.scroll_by(-1, rows as usize);
            }
            Key::Down if self.running_task() => {
                let rows = self.transcript_view_rows();
                self.transcript.scroll_by(1, rows as usize);
            }
            other => match self.composer.handle_key(other) {
                ComposerAction::Submit(text) => {
                    self.handle_submit(text);
                }
                ComposerAction::Interrupt => {
                    if self.running_task() {
                        self.interrupt();
                    }
                }
                ComposerAction::Cancel => {
                    if !self.running_task() {
                        self.mode = Mode::Help;
                    }
                }
                ComposerAction::None => {}
            },
        }
    }

    fn handle_submit(&mut self, text: String) {
        if text.starts_with('/') {
            self.handle_slash(&text);
            return;
        }
        self.transcript.push(LineKind::User, text.clone());
        self.unread = 0;
        let req = AgentRequest {
            prompt: Some(text),
            system: None,
            interactive: true,
            resume: None,
            messages: Vec::new(),
            images: Vec::new(),
        };
        self.spawn_agent(req);
    }

    fn push_multi(&mut self, text: &str) {
        for l in text.lines() {
            if !l.trim().is_empty() {
                self.transcript.push(LineKind::Tool, l.to_string());
            }
        }
    }

    fn handle_slash(&mut self, line: &str) {
        use crate::slash_commands::{parse, Slash};
        let Some(cmd) = parse(line) else {
            self.transcript.push(LineKind::System, format!("unknown slash command: {line}"));
            return;
        };
        match cmd {
            Slash::History(limit) => {
                let n = limit
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(20)
                    .min(200);
                let recs = crate::history::HistoryStore::new().query(None, n);
                if recs.is_empty() {
                    self.transcript.push(LineKind::System, "no history");
                } else {
                    for r in &recs {
                        self.transcript.push(
                            LineKind::Tool,
                            format!(
                                "{} {}\n    {}",
                                r.timestamp_ms,
                                crate::sessions::workspace_name(&r.workspace_root),
                                r.text.chars().take(240).collect::<String>(),
                            ),
                        );
                    }
                    self.transcript
                        .push(LineKind::System, format!("({} prompts)", recs.len()));
                }
            }
            Slash::Exit => self.exit_requested = true,
            Slash::Help => self.mode = Mode::Help,
            Slash::Clear => self.transcript.clear(),
            Slash::Version => {
                self.transcript
                    .push(LineKind::System, format!("fxrs {}", crate::version::VERSION));
            }
            Slash::Status => {
                self.transcript.push(
                    LineKind::System,
                    format!(
                        "model: {} · permissions: {} · workspace: {} · max steps: {}",
                        self.model_label(),
                        self.config.permission_mode,
                        self.config.workspace.display(),
                        self.config.max_agent_steps,
                    ),
                );
            }
            Slash::Model => {
                self.transcript.push(LineKind::System, format!("model: {}", self.model_label()));
            }
            Slash::Permissions => {
                self.transcript.push(
                    LineKind::System,
                    format!("permission mode: {}", self.config.permission_mode),
                );
                for (pat, action) in &self.config.permission_rules {
                    let action = match action {
                        crate::permissions::ToolRule::Whole(r) => format!("{r:?}"),
                        crate::permissions::ToolRule::Patterns(ps) => ps
                            .iter()
                            .map(|(g, r)| format!("{g}->{r:?}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    };
                    self.transcript
                        .push(LineKind::Tool, format!("  {pat}: {action}"));
                }
            }
            Slash::Settings => {
                let rendered = crate::settings_catalog::render(&self.config);
                self.push_multi(&rendered);
            }
            Slash::Doctor => {
                let issues = crate::cli::doctor_checks(&self.config);
                if issues.is_empty() {
                    self.transcript.push(LineKind::System, "all checks passed ✓".to_string());
                } else {
                    for (sev, msg) in issues {
                        self.transcript.push(
                            LineKind::Error,
                            format!("[{}] {msg}", if sev == 'w' { "warn" } else { "fail" }),
                        );
                    }
                }
            }
            Slash::Sessions => {
                match self.store.list(Some(&self.config.workspace)) {
                    Ok(list) if list.is_empty() => {
                        self.transcript.push(LineKind::System, "no sessions".to_string());
                    }
                    Ok(list) => {
                        for s in list.iter().take(20) {
                            self.transcript.push(
                                LineKind::Tool,
                                format!(
                                    "{} {} msgs · {}k tok · {}",
                                    s.id,
                                    s.messages,
                                    s.tokens / 1000,
                                    s.last_text.chars().take(80).collect::<String>(),
                                ),
                            );
                        }
                    }
                    Err(e) => self.transcript.push(LineKind::Error, format!("{e:#}")),
                }
            }
            Slash::Session(id) => {
                let target = self.resolve_session_arg(id.as_deref());
                match target {
                    Some(rid) => {
                        if let Err(e) = self.resume_session(&rid) {
                            self.transcript.push(LineKind::Error, format!("{e:#}"));
                        }
                    }
                    None => self.transcript.push(LineKind::System, "no sessions".to_string()),
                }
            }
            Slash::Resume(id) => {
                let target = self.resolve_session_arg(id.as_deref());
                match target {
                    Some(rid) => {
                        if self.running_task() {
                            self.transcript.push(
                                LineKind::System,
                                "busy — finish the current run first".to_string(),
                            );
                        } else if let Err(e) = self.resume_session(&rid) {
                            self.transcript.push(LineKind::Error, format!("{e:#}"));
                        } else {
                            let req = AgentRequest {
                                prompt: None,
                                system: None,
                                interactive: true,
                                resume: Some(rid),
                                messages: Vec::new(),
                                images: Vec::new(),
                            };
                            self.spawn_agent(req);
                        }
                    }
                    None => self.transcript.push(LineKind::System, "no sessions".to_string()),
                }
            }
            Slash::Usage(period) => {
                let period = period.unwrap_or_else(|| "7d".into());
                let since = crate::usage::parse_period(&period);
                let totals = crate::usage::UsageStore::new().aggregate(since);
                self.transcript.push(
                    LineKind::System,
                    format!(
                        "usage ({period}): {} turns · {}k in / {}k out tokens · {} tool calls · ${:.4}",
                        totals.turns,
                        totals.input_tokens / 1000,
                        totals.output_tokens / 1000,
                        totals.tool_calls,
                        totals.cost_usd,
                    ),
                );
            }
            Slash::Setup => {
                for l in [
                    "fxrs needs a model endpoint. Configure one of:",
                    "  AI_GATEWAY_API_KEY=...            (Vercel AI Gateway, default)",
                    "  ANTHROPIC_API_KEY=...             (native Anthropic)",
                    "  AI_BASE_URL=... AI_API_KEY=...    (OpenAI-compatible local server)",
                    "  FX_MODEL=...                      (model id, default openai/gpt-5.4)",
                    "  FX_PERMISSION_MODE=ask|auto|yolo  (default auto)",
                ] {
                    self.transcript.push(LineKind::Tool, l.to_string());
                }
            }
            Slash::Trace => {
                self.trace = !self.trace;
                self.transcript.push(
                    LineKind::System,
                    format!("trace {}", if self.trace { "on" } else { "off" }),
                );
            }
            Slash::Feedback => {
                self.transcript.push(
                    LineKind::System,
                    "fxrs feedback: open an issue at github.com/1deat0r/fx-rust".to_string(),
                );
            }
            Slash::Workspace => {
                self.transcript.push(
                    LineKind::System,
                    format!("workspace: {}", self.config.workspace.display()),
                );
                let ag = crate::config::load_project_instructions(&self.config.workspace);
                if ag.is_empty() {
                    self.transcript
                        .push(LineKind::System, "AGENTS.md: (none loaded)".to_string());
                } else {
                    self.transcript.push(
                        LineKind::System,
                        format!(
                            "AGENTS.md: {} file(s), {} chars",
                            ag.len(),
                            ag.iter().map(|s| s.len()).sum::<usize>()
                        ),
                    );
                }
            }
            Slash::Background(arg) => {
                let text = crate::ui::render_slash_background(&self.config, arg.as_deref());
                self.push_multi(&text);
            }
            Slash::Terminal(arg) => {
                let text = crate::ui::render_slash_terminal(arg.as_deref());
                self.push_multi(&text);
            }
            Slash::Skills(arg) => {
                let text = crate::ui::render_slash_skills(&self.config.workspace, arg.as_deref());
                self.push_multi(&text);
            }
            Slash::Compact => {
                self.transcript.push(LineKind::System, "compact is not supported in the TUI yet".to_string());
            }
            Slash::Login(_) => {
                self.transcript
                    .push(LineKind::System, "run `fxrs login` outside the TUI to authenticate".to_string());
            }
            Slash::Logout(_) => {
                self.transcript
                    .push(LineKind::System, "run `fxrs logout` outside the TUI".to_string());
            }
            Slash::Credits => {
                self.transcript
                    .push(LineKind::System, "run `fxrs credits` outside the TUI".to_string());
            }
            Slash::Stats => {
                self.transcript
                    .push(LineKind::System, "run `fxrs credits` outside the TUI".to_string());
            }
            Slash::Unknown(u) => {
                self.transcript
                    .push(LineKind::System, format!("unknown slash command: {u}"));
            }
        }
    }

    fn resolve_session_arg(&self, id: Option<&str>) -> Option<String> {
        match id {
            Some("last") | None => self
                .store
                .list(Some(&self.config.workspace))
                .ok()
                .and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0).id) }),
            Some(t) => Some(t.to_string()),
        }
    }

    fn answer_approval(&mut self, allow: bool) {
        if let Some(tx) = self.pending_approval.take() {
            let _ = tx.send(allow);
        }
        self.approval_req = None;
        self.mode = Mode::Normal;
    }

    /// Drain all available agent events into the transcript. Takes the
    /// receiver so mutation of `self` stays borrow-safe; returns it when the
    /// channel is still open (agent still running).
    fn drain_events(&mut self) {
        let Some(rx) = self.agent_rx.take() else { return };
        let mut still_open = true;
        loop {
            match rx.try_recv() {
                Ok(ev) => self.handle_ev(ev),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    still_open = false;
                    break;
                }
            }
        }
        if still_open {
            self.agent_rx = Some(rx);
        }
    }

    fn handle_ev(&mut self, ev: Ev) {
        match ev {
            Ev::Step(_n) => {
                self.unread = 0;
            }
            Ev::Text(text) => {
                self.append_assistant(&text);
                self.unread = 0;
            }
            Ev::ReasoningStart => {
                self.transcript.push(LineKind::System, "… reasoning");
            }
            Ev::ReasoningDelta(text) => {
                self.append_assistant(&text);
            }
            Ev::Tool(name, res) => {
                self.unread = 0;
                if res.is_empty() {
                    self.transcript.push(LineKind::Tool, format!("⎿ {name}"));
                } else {
                    let preview: String = res
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(160)
                        .collect();
                    self.transcript
                        .push(LineKind::Tool, format!("⎿ {name} {preview}"));
                }
            }
            Ev::Banner(text) => {
                self.transcript.push(LineKind::System, text);
            }
            Ev::Error(e) => {
                self.transcript.push(LineKind::Error, format!("ƒx error: {e}"));
            }
            Ev::Done => {
                self.running = None;
                self.agent_rx = None;
                self.transcript.push(LineKind::System, "─".repeat(30));
            }
            Ev::Approve(req, answer_tx) => {
                self.pending_approval = Some(answer_tx);
                self.approval_req = Some(req);
                self.mode = Mode::Confirm;
            }
        }
    }

    fn append_assistant(&mut self, text: &str) {
        if self.transcript.lines.is_empty() {
            self.transcript.push(LineKind::Assistant, text.to_string());
            return;
        }
        let last_idx = self.transcript.lines.len() - 1;
        if self.transcript.lines[last_idx].kind == LineKind::Assistant {
            let w = self.transcript.width();
            self.transcript.lines[last_idx].text.push_str(text);
            self.transcript.lines[last_idx].rewrap(w);
            self.transcript.total_rows = self.transcript.lines.iter().map(|l| l.rows).sum();
            if self.transcript.follow {
                self.transcript.scroll_line = usize::MAX;
            }
            return;
        }
        self.transcript.push(LineKind::Assistant, text.to_string());
    }

    fn transcript_view_rows(&self) -> u16 {
        self.screen.rows.saturating_sub(4).max(1)
    }

    // ---------------- rendering

    fn render(&mut self) -> std::io::Result<()> {
        use crossterm::cursor::MoveTo;
        use crossterm::QueueableCommand;
        let mut out = std::io::stdout();
        let theme = self.theme;
        let rows = self.screen.rows;
        let _cols = self.screen.cols;

        self.screen.clear();

        // Header.
        let header = format!(
            "{} · {}",
            self.model_label(),
            self.config.permission_mode
        );
        self.screen
            .putstr_styled(0, 0, &header, CellStyle::dim(theme.dim));

        // Transcript.
        let view_rows = self.transcript_view_rows();
        self.transcript
            .render_into(&mut self.screen, &theme, 1, view_rows, theme.assistant);

        // Composer row.
        let composer_row = rows.saturating_sub(2);
        if self.mode == Mode::Confirm {
            if let Some(req) = &self.approval_req {
                let brief = req.prompt();
                let line: String = brief.chars().take(140).collect();
                self.screen
                    .putstr_styled(composer_row, 0, &line, CellStyle::bold(theme.error));
            }
            let allow = "  allow? (y)es / (n)o / (a)lways  ";
            self.screen
                .putstr_styled(composer_row, 0, allow, CellStyle::bold(theme.selection));
        } else {
            let _ = self
                .composer
                .render(&mut self.screen, &theme, composer_row, false);
        }

        // Footer.
        let mode_text = self.config.permission_mode.to_string();
        let running = self.running_task();
        let hints = if running {
            "esc interrupt · ctrl-d exit"
        } else {
            "esc help · ctrl-d exit · ↑↓ history"
        };
        let ws_name = self
            .config
            .workspace
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.config.workspace.display().to_string());
        let model = self.model_label();
        let unread = self.unread;
        widgets::footer(
            &mut self.screen,
            &theme,
            rows.saturating_sub(1),
            &FooterInfo {
                workspace: Some(ws_name),
                model: Some(model),
                permission_mode: Some(mode_text),
                running,
                unread,
                hints: hints.to_string(),
            },
        );

        // Help overlay.
        if self.mode == Mode::Help {
            widgets::help_overlay(&mut self.screen, &theme);
        }

        self.screen.flush(&mut out)?;

        // Place the cursor.
        match self.mode {
            Mode::Confirm => {
                out.queue(MoveTo(0, composer_row))?;
                out.flush()?;
            }
            Mode::Help => {
                let _ = Hide;
                out.queue(MoveTo(0, 0))?;
                out.flush()?;
            }
            Mode::Normal => {
                let (cur_row, cur_col) = self.composer.cursor_position_single(composer_row);
                out.queue(MoveTo(cur_col, cur_row))?;
                out.flush()?;
            }
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;

        loop {
            let _ = self.render();
            self.drain_events();

            if self.exit_requested {
                break;
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(kev) => {
                        let key = keys::from_crossterm(kev);
                        self.handle_key(key);
                    }
                    Event::Resize(w, h) => {
                        self.screen.resize(w, h);
                        self.transcript.set_width(w as usize);
                    }
                    _ => {}
                }
            }

            // Reap a finished task (in case Done raced with the poll).
            if let Some(task) = &self.running {
                if task.is_finished() {
                    self.running = None;
                    let _ = self.agent_rx.take();
                }
            }
        }

        // Restore terminal state before returning.
        let mut out = std::io::stdout();
        execute!(out, LeaveAlternateScreen, Show)?;
        disable_raw_mode()?;

        println!(
            "\n[fxrs] {} · workspace {}",
            crate::version::VERSION,
            self.config.workspace.display()
        );
        Ok(())
    }
}

/// Entry point: run the full-screen TUI. `resume` mirrors the REPL's
/// `Some("last") | Some(id)` semantics.
pub async fn run_tui(
    config: Arc<Config>,
    store: &SessionStore,
    resume: Option<String>,
    trace: bool,
) -> Result<()> {
    let mut app = App::new(config, store.clone(), resume, trace)?;
    // Agent tasks are !Send (agent::run awaits a boxed non-Send future in the
    // subagent tool), so the whole UI runs inside a LocalSet on the main
    // thread — the same arrangement the ACP server relies on.
    let local = tokio::task::LocalSet::new();
    local.run_until(app.run()).await
}

fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}
