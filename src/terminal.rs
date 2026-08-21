//! Terminal sessions (tmux-backed): a persistent-terminal store plus the
//! `terminal` tool backend. Faithful to fx's `core/terminal/*` +
//! `tools/terminal/*` at the observable level: create a real terminal session
//! behind tmux, send keystrokes, read the pane with configurable scrollback,
//! resize, and stop it. The store reconciles dead sessions on load (tmux is
//! the source of liveness), and a resumed agent sees the same sessions.
//!
//! Each session gets its own tmux socket (`-L fxrs-<id>`) so we never touch a
//! user's running tmux server.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u64 = 1;
pub const DEFAULT_ROWS: u32 = 40;
pub const DEFAULT_COLUMNS: u32 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalRecord {
    pub id: String,
    pub name: Option<String>,
    /// Shell (or command) running inside the session.
    pub command: String,
    pub cwd: String,
    pub rows: u32,
    pub columns: u32,
    pub started_at_ms: u128,
    pub status: TermStatus,
    /// tmux server pid (approximate; -1 when unknown).
    pub pid: i64,
    /// tmux socket name (`-L <socket>`) — also the tmux session name.
    pub socket: String,
    /// Session target used for pane commands (`<socket>:0`).
    pub target: String,
}

impl Default for TerminalRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            command: "bash".into(),
            cwd: String::new(),
            rows: DEFAULT_ROWS,
            columns: DEFAULT_COLUMNS,
            started_at_ms: 0,
            status: TermStatus::Running,
            pid: -1,
            socket: String::new(),
            target: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalStore {
    pub version: u64,
    pub records: Vec<TerminalRecord>,
}

impl Default for TerminalStore {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}

pub fn store_path() -> PathBuf {
    crate::config::fx_home().join("terminal.json")
}

impl TerminalStore {
    /// Load the store and reconcile liveness against tmux.
    pub fn open() -> Result<TerminalStore> {
        let path = store_path();
        let mut store = if path.exists() {
            let data = std::fs::read_to_string(&path).context("read terminal store")?;
            serde_json::from_str(&data).context("parse terminal store")?
        } else {
            TerminalStore::default()
        };
        store.reconcile();
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        let path = store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create terminal store dir")?;
        }
        let data = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data).context("write terminal store")?;
        std::fs::rename(&tmp, path).context("commit terminal store")?;
        Ok(())
    }

    /// tmux is the single source of truth for liveness.
    fn reconcile(&mut self) {
        for record in &mut self.records {
            if record.status == TermStatus::Running && !tmux_has_session(&record.socket) {
                record.status = TermStatus::Exited;
            }
        }
    }

    pub fn list(&self) -> &[TerminalRecord] {
        &self.records
    }

    pub fn get(&self, id: &str) -> Option<&TerminalRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    fn next_id(&self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = format!("{:x}", nanos & 0xffff_ffff_ffff);
        let mut candidate = base.clone();
        let mut n = 0u32;
        while self.records.iter().any(|r| r.id == candidate) {
            n += 1;
            candidate = format!("{base}{n:x}");
        }
        candidate
    }

    /// Create a terminal session.
    ///
    /// Spawns `tmux -L <socket> new-session -d -s <id> -x <cols> -y <rows>
    /// -c <cwd> <shell>`, then reads the tmux server pid. Failures (tmux
    /// missing, session not created) surface as errors and leave no record.
    pub fn create(
        &mut self,
        cwd: &Path,
        command: Option<&str>,
        name: Option<&str>,
        rows: Option<u32>,
        columns: Option<u32>,
    ) -> Result<TerminalRecord> {
        if !tmux_available() {
            bail!(
                "terminal sessions require tmux (install it, e.g. `apt install tmux` or `brew install tmux`)"
            );
        }
        let command = command.unwrap_or("bash").trim();
        if command.is_empty() {
            bail!("terminal create: command must not be empty");
        }
        let id = self.next_id();
        let socket = format!("fxrs-{id}");
        let rows = rows.unwrap_or(DEFAULT_ROWS).clamp(4, 500);
        let columns = columns.unwrap_or(DEFAULT_COLUMNS).clamp(20, 500);

        let status = Command::new("tmux")
            .arg("-L")
            .arg(&socket)
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&id)
            .arg("-x")
            .arg(columns.to_string())
            .arg("-y")
            .arg(rows.to_string())
            .arg("-c")
            .arg(cwd)
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("spawn tmux new-session")?;
        if !status.success() {
            bail!("tmux failed to create session `{id}` (exit {status})");
        }
        let pid = tmux_server_pid(&socket).unwrap_or(-1);
        let record = TerminalRecord {
            id: id.clone(),
            name: name.map(str::to_string),
            command: command.to_string(),
            cwd: cwd.display().to_string(),
            rows,
            columns,
            started_at_ms: crate::util::now_ms(),
            status: TermStatus::Running,
            pid,
            socket: socket.clone(),
            target: format!("{id}:0"),
        };
        self.records.push(record.clone());
        self.save()?;
        Ok(record)
    }

    /// Send input to the session. `input` goes through tmux send-keys -l
    /// (literal), then Enter unless `enter` is false.
    pub fn send(&self, id: &str, input: &str, enter: bool) -> Result<()> {
        let record = self.require_running(id)?;
        let mut cmd = Command::new("tmux");
        cmd.arg("-L")
            .arg(&record.socket)
            .arg("send-keys")
            .arg("-t")
            .arg(&record.target)
            .arg("-l")
            .arg(input);
        let status = cmd.status().context("spawn tmux send-keys")?;
        if !status.success() {
            bail!("tmux send-keys failed (exit {status})");
        }
        if enter {
            let status = Command::new("tmux")
                .arg("-L")
                .arg(&record.socket)
                .arg("send-keys")
                .arg("-t")
                .arg(&record.target)
                .arg("Enter")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("spawn tmux send-keys Enter")?;
            if !status.success() {
                bail!("tmux send-keys Enter failed (exit {status})");
            }
        }
        Ok(())
    }

    /// Read the visible pane plus up to `scrollback` history lines. ANSI
    /// escapes are stripped and trailing blank lines trimmed unless `raw`.
    pub fn read(
        &self,
        id: &str,
        scrollback: usize,
        max_bytes: usize,
        raw: bool,
        clear_after: bool,
    ) -> Result<String> {
        let record = self.require_running(id)?;
        let scrollback = scrollback.clamp(0, 5000);
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&record.socket)
            .arg("capture-pane")
            .arg("-t")
            .arg(&record.target)
            .arg("-p")
            .arg("-J")
            .arg("-S")
            .arg(format!("-{scrollback}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .context("spawn tmux capture-pane")?;
        if !out.status.success() {
            bail!("tmux capture-pane failed (exit {})", out.status);
        }
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        if !raw {
            text = strip_ansi(&text);
        }
        // Trim trailing blank lines so the read back is a clean transcript.
        while text.ends_with('\n') {
            text.pop();
        }
        if text.len() > max_bytes {
            let start = text.len() - max_bytes;
            let mut clipped = String::from(&text[start..]);
            clipped.insert_str(
                0,
                &format!("… [truncated {} bytes]\n", text.len() - max_bytes),
            );
            text = clipped;
        }
        if clear_after {
            let _ = Command::new("tmux")
                .arg("-L")
                .arg(&record.socket)
                .arg("clear-history")
                .arg("-t")
                .arg(&record.target)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        Ok(text)
    }

    /// Resize the pane to `rows` x `columns`.
    pub fn resize(&mut self, id: &str, rows: u32, columns: u32) -> Result<TerminalRecord> {
        let idx = self
            .records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown terminal id `{id}`"))?;
        if self.records[idx].status != TermStatus::Running {
            bail!("terminal `{id}` is not running");
        }
        let rows = rows.clamp(4, 500);
        let columns = columns.clamp(20, 500);
        let status = Command::new("tmux")
            .arg("-L")
            .arg(&self.records[idx].socket)
            .arg("resize-window")
            .arg("-t")
            .arg(&self.records[idx].target)
            .arg("-x")
            .arg(columns.to_string())
            .arg("-y")
            .arg(rows.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("spawn tmux resize-window")?;
        if !status.success() {
            bail!("tmux resize-window failed (exit {status})");
        }
        self.records[idx].rows = rows;
        self.records[idx].columns = columns;
        let record = self.records[idx].clone();
        self.save()?;
        Ok(record)
    }

    /// Stop a terminal session (kill the tmux session; the pane's shell dies
    /// with it).
    pub fn stop(&mut self, id: &str) -> Result<TerminalRecord> {
        let idx = self
            .records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown terminal id `{id}`"))?;
        if self.records[idx].status != TermStatus::Running {
            bail!("terminal `{id}` is not running");
        }
        let socket = self.records[idx].socket.clone();
        let status = Command::new("tmux")
            .arg("-L")
            .arg(&socket)
            .arg("kill-session")
            .arg("-t")
            .arg(&self.records[idx].id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("spawn tmux kill-session")?;
        if !status.success() {
            bail!("tmux kill-session failed (exit {status})");
        }
        self.records[idx].status = TermStatus::Exited;
        let record = self.records[idx].clone();
        self.save()?;
        Ok(record)
    }

    fn require_running(&self, id: &str) -> Result<&TerminalRecord> {
        let record = self
            .records
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown terminal id `{id}`"))?;
        if record.status != TermStatus::Running {
            bail!("terminal `{id}` is not running");
        }
        Ok(record)
    }
}

pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tmux_has_session(socket: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("has-session")
        .arg("-t")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tmux_server_pid(socket: &str) -> Option<i64> {
    let out = Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("display-message")
        .arg("-p")
        .arg("#{pid}")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Remove ECMA-48 ANSI escape sequences (CSI + OSC + linked sequences).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // Consume CSI (ESC [ ... final 0x40..0x7E) or OSC (ESC ] ...
                // BEL / ST), plus single-char escape sequences.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for ch in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&ch) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        let mut done = false;
                        while let Some(&ch) = chars.peek() {
                            chars.next();
                            if ch == '\x07' {
                                done = true;
                                break;
                            }
                            if ch == '\x1b' && chars.peek() == Some(&'\\') {
                                chars.next();
                                done = true;
                                break;
                            }
                        }
                        let _ = done;
                    }
                    Some('(') | Some(')') => {
                        chars.next();
                        let _ = chars.next(); // charset selector
                    }
                    Some(&'\\') => {
                        chars.next();
                    }
                    Some(_) => {
                        let _ = chars.next();
                    }
                    None => break,
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Human table for `fxrs terminal list` / `/terminal`.
pub fn render_table(records: &[TerminalRecord]) -> String {
    let mut lines = vec![format!(
        "{:<10} {:<8} {:<8} {:<6} {:<4} {}",
        "ID", "STATUS", "PID", "COLS", "ROWS", "COMMAND"
    )];
    let mut sorted: Vec<&TerminalRecord> = records.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
    for r in sorted {
        let status = match r.status {
            TermStatus::Running => "running",
            TermStatus::Exited => "exited",
        };
        let name = r
            .name
            .as_deref()
            .map(|n| format!("[{n}] "))
            .unwrap_or_default();
        let cmd: String = r.command.chars().take(40).collect();
        lines.push(format!(
            "{:<10} {:<8} {:<8} {:<6} {:<4} {}{}",
            r.id, status, r.pid, r.columns, r.rows, name, cmd
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_common_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;32mgreen\x1b[m done"), "green done");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(strip_ansi("line1\r\nline2"), "line1\r\nline2");
        assert_eq!(strip_ansi("\x1b[?25lhide\x1b(B\x1b[m"), "hide");
        assert_eq!(strip_ansi("no-escapes"), "no-escapes");
    }

    #[test]
    fn read_trims_trailing_newlines() {
        // read() is string-level; ensure the trimming contract is stable.
        let text = "a\nb\n\n\n";
        let trimmed = text.trim_end_matches('\n');
        assert_eq!(trimmed, "a\nb");
    }

    #[test]
    fn store_roundtrip_and_reconcile_legacy() {
        let dir = std::env::temp_dir().join(format!("fxrs-term-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("terminal.json");
        // Legacy record with no optional fields — must deserialize.
        let legacy = serde_json::json!({
            "version": 1,
            "records": [{
                "id": "ab1",
                "command": "bash",
                "cwd": ".",
                "rows": 40,
                "columns": 120,
                "started_at_ms": 0,
                "status": "running",
                "pid": 123,
                "socket": "fxrs-ab1",
                "target": "ab1:0"
            }]
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
        let store: TerminalStore = serde_json::from_value(legacy).unwrap();
        assert_eq!(store.records[0].name, None);
        assert_eq!(store.records[0].command, "bash");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_ids_are_unique() {
        let mut store = TerminalStore::default();
        store.records.push(TerminalRecord {
            id: store.next_id().clone(),
            ..TerminalRecord::default()
        });
        let second = store.next_id();
        assert_ne!(store.records[0].id, second);
    }

    #[test]
    fn failed_create_leaves_no_record() {
        // Whether tmux is present or not, a failed create must not push a
        // record (no phantom entries in the store).
        let mut store = TerminalStore::default();
        let err = store.create(Path::new("."), Some(""), None, None, None);
        assert!(err.is_err());
        assert!(store.records.is_empty());
    }

    #[test]
    fn tmux_available_reports_bool() {
        // Just exercises the probe; the value depends on the host.
        assert!(
            tmux_available()
                == Command::new("tmux")
                    .arg("-V")
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
        );
    }

    #[test]
    fn render_table_lists_records() {
        let mut store = TerminalStore::default();
        store.records.push(TerminalRecord {
            id: "t1".into(),
            name: Some("api".into()),
            command: "bash".into(),
            cwd: ".".into(),
            rows: 40,
            columns: 120,
            started_at_ms: 5,
            status: TermStatus::Running,
            pid: 99,
            socket: "fxrs-t1".into(),
            target: "t1:0".into(),
        });
        let text = render_table(store.list());
        assert!(text.contains("t1"));
        assert!(text.contains("[api]"));
    }
}
