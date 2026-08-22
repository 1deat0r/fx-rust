//! Terminal sessions (tmux- or native-PTY-backed): a persistent-terminal
//! store plus the `terminal` tool backend. Faithful to fx's
//! `core/terminal/*` + `tools/terminal/*` at the observable level: create a
//! real terminal session, send keystrokes, read the pane with configurable
//! scrollback, resize, and stop it.
//!
//! Two backends mirror upstream `core/terminal/backend`:
//!
//! * **Native** (`Backend::Native`, the default for new sessions, matching
//!   upstream's `input.backend orelse .native`): a real PTY spawned through
//!   `portable-pty`. The session lives for the lifetime of this process (its
//!   host); the store keeps a record, and a fresh process reconciles those
//!   records to `lost` (host absent) through the recovery decision model in
//!   [`crate::terminal_recovery`].
//! * **Tmux** (`Backend::Tmux`): per-session tmux socket
//!   (`-L fxrs-<id>`) so we never touch a user's running tmux server.
//!   Sessions survive process restarts via the tmux server.
//!
//! The store reconciles liveness on load using the recovery decision model
//! (host + process evidence), and a resumed agent sees the same durable
//! records.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::terminal_recovery::ScreenUnavailableReason;

pub const SCHEMA_VERSION: u64 = 1;
pub const DEFAULT_ROWS: u32 = 40;
pub const DEFAULT_COLUMNS: u32 = 120;
/// Hard cap on native-session output retained per session (1 MiB).
const NATIVE_OUTPUT_CAP: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermStatus {
    Running,
    Exited,
    /// Recovery marked the session lost: its host/process is gone.
    Lost,
}

/// Which backend backs a terminal session (upstream `contracts.Backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Native,
    Tmux,
}

impl Default for Backend {
    /// Legacy records (pre-backend stores) were tmux-backed.
    fn default() -> Self {
        Backend::Tmux
    }
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Tmux => "tmux",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "native" => Some(Backend::Native),
            "tmux" => Some(Backend::Tmux),
            _ => None,
        }
    }
}

/// Optional knobs for [`TerminalStore::create_backend`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalCreateOptions<'a> {
    /// Optional short label for the terminal.
    pub name: Option<&'a str>,
    /// Terminal height in rows (defaults to [`DEFAULT_ROWS`]).
    pub rows: Option<u32>,
    /// Terminal width in columns (defaults to [`DEFAULT_COLUMNS`]).
    pub columns: Option<u32>,
    /// Extra argv arguments for the native backend (tmux ignores these;
    /// used by `exec` to run `bash -c <command>`).
    pub argv: &'a [String],
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
    /// Session-leader pid (native: pty child; tmux: tmux server).
    pub pid: i64,
    /// Backend: `native` (this process) or `tmux` (persistent tmux server).
    pub backend: Backend,
    /// tmux socket name (`-L <socket>`); empty for native sessions.
    pub socket: String,
    /// tmux session target (`<socket>:0`); empty for native sessions.
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
            backend: Backend::default(),
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
    /// Load the store and reconcile liveness against the backend hosts.
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

    /// Reconcile every running record against live evidence using the
    /// recovery decision model (see [`crate::terminal_recovery`]).
    ///
    /// Backend hosts:
    /// * tmux — the tmux server on the record's socket is the host; the
    ///   recorded `pid` is matched against the live server pid.
    /// * native — this process is the host. A live handle means the host is
    ///   present; child `try_wait` provides termination evidence. A missing
    ///   handle (fresh process) means the host is absent → lost.
    fn reconcile(&mut self) {
        for record in &mut self.records {
            if record.status != TermStatus::Running {
                continue;
            }
            match record.backend {
                Backend::Tmux => reconcile_tmux(record),
                Backend::Native => reconcile_native(record),
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

    /// Create a terminal session on the default backend (native; upstream
    /// `backend orelse .native`). See [`TerminalStore::create_backend`].
    pub fn create(
        &mut self,
        cwd: &Path,
        command: Option<&str>,
        name: Option<&str>,
        rows: Option<u32>,
        columns: Option<u32>,
    ) -> Result<TerminalRecord> {
        let opts = TerminalCreateOptions {
            name,
            rows,
            columns,
            ..TerminalCreateOptions::default()
        };
        self.create_backend(Backend::Native, cwd, command, &opts)
    }

    /// Create a terminal session on an explicit backend.
    ///
    /// * `tmux`: spawns `tmux -L <socket> new-session -d ...`; failures
    ///   surface as errors and leave no record.
    /// * `native`: spawns `<command>` (plus `argv` arguments) under a real
    ///   PTY in this process and registers a live handle. The shell becomes
    ///   the session leader of the pty; when this process exits the session
    ///   is lost (host absent).
    pub fn create_backend(
        &mut self,
        backend: Backend,
        cwd: &Path,
        command: Option<&str>,
        opts: &TerminalCreateOptions<'_>,
    ) -> Result<TerminalRecord> {
        match backend {
            Backend::Tmux => self.create_tmux(cwd, command, opts),
            Backend::Native => self.create_native(cwd, command, opts),
        }
    }

    fn create_tmux(
        &mut self,
        cwd: &Path,
        command: Option<&str>,
        opts: &TerminalCreateOptions<'_>,
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
        let rows = opts.rows.unwrap_or(DEFAULT_ROWS).clamp(4, 500);
        let columns = opts.columns.unwrap_or(DEFAULT_COLUMNS).clamp(20, 500);

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
            name: opts.name.map(str::to_string),
            command: command.to_string(),
            cwd: cwd.display().to_string(),
            rows,
            columns,
            started_at_ms: crate::util::now_ms(),
            status: TermStatus::Running,
            pid,
            backend: Backend::Tmux,
            socket: socket.clone(),
            target: format!("{id}:0"),
        };
        self.records.push(record.clone());
        self.save()?;
        Ok(record)
    }

    fn create_native(
        &mut self,
        cwd: &Path,
        command: Option<&str>,
        opts: &TerminalCreateOptions<'_>,
    ) -> Result<TerminalRecord> {
        let program = command.unwrap_or("bash").trim();
        if program.is_empty() {
            bail!("terminal create: command must not be empty");
        }
        let id = self.next_id();
        let rows = opts.rows.unwrap_or(DEFAULT_ROWS).clamp(4, 500);
        let columns = opts.columns.unwrap_or(DEFAULT_COLUMNS).clamp(20, 500);
        let handle = spawn_native(&id, cwd, program, opts.argv, rows, columns)?;
        let record = TerminalRecord {
            id: id.clone(),
            name: opts.name.map(str::to_string),
            command: program.to_string(),
            cwd: cwd.display().to_string(),
            rows,
            columns,
            started_at_ms: crate::util::now_ms(),
            status: TermStatus::Running,
            pid: handle
                .child
                .lock()
                .unwrap()
                .process_id()
                .map(|p| p as i64)
                .unwrap_or(-1),
            backend: Backend::Native,
            socket: String::new(),
            target: String::new(),
        };
        native_registry().lock().unwrap().insert(id.clone(), handle);
        self.records.push(record.clone());
        self.save()?;
        Ok(record)
    }

    /// Send input to the session.
    ///
    /// * native: writes the literal bytes to the pty master (`\r` when
    ///   `enter` is true — the canonical line discipline terminator).
    /// * tmux: `tmux send-keys -l <input>` then `Enter` when requested.
    pub fn send(&self, id: &str, input: &str, enter: bool) -> Result<()> {
        let record = self.require_running(id)?;
        match record.backend {
            Backend::Native => {
                let handle = require_native_handle(id)?;
                handle.write(input, enter)
            }
            Backend::Tmux => self.send_tmux(record, input, enter),
        }
    }

    fn send_tmux(&self, record: &TerminalRecord, input: &str, enter: bool) -> Result<()> {
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

    /// Read output from the session.
    ///
    /// * native: the retained output buffer (bounded ring), limited to the
    ///   last `scrollback` lines when `scrollback > 0`.
    /// * tmux: visible pane plus up to `scrollback` history lines.
    ///
    /// ANSI escapes are stripped unless `raw`; trailing blank lines trimmed.
    pub fn read(
        &self,
        id: &str,
        scrollback: usize,
        max_bytes: usize,
        raw: bool,
        clear_after: bool,
    ) -> Result<String> {
        let record = self.require_running(id)?;
        match record.backend {
            Backend::Native => {
                let handle = require_native_handle(id)?;
                Ok(handle.read_text(scrollback, max_bytes, raw, clear_after))
            }
            Backend::Tmux => self.read_tmux(record, scrollback, max_bytes, raw, clear_after),
        }
    }

    fn read_tmux(
        &self,
        record: &TerminalRecord,
        scrollback: usize,
        max_bytes: usize,
        raw: bool,
        clear_after: bool,
    ) -> Result<String> {
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

    /// Resize the session to `rows` x `columns` (native: pty winsize ioctl;
    /// tmux: resize-window).
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
        match self.records[idx].backend {
            Backend::Native => {
                require_native_handle(id)?.resize(rows, columns)?;
            }
            Backend::Tmux => {
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
            }
        }
        self.records[idx].rows = rows;
        self.records[idx].columns = columns;
        let record = self.records[idx].clone();
        self.save()?;
        Ok(record)
    }

    /// Stop a terminal session (native: kill + reap the pty child; tmux:
    /// kill the tmux session). Live native handles are dropped from the
    /// registry.
    pub fn stop(&mut self, id: &str) -> Result<TerminalRecord> {
        let idx = self
            .records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown terminal id `{id}`"))?;
        if self.records[idx].status != TermStatus::Running {
            bail!("terminal `{id}` is not running");
        }
        let backend = self.records[idx].backend;
        match backend {
            Backend::Native => {
                let handle = require_native_handle(id)?;
                handle.terminate()?;
                native_registry().lock().unwrap().remove(id);
            }
            Backend::Tmux => {
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
            }
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

/// Reconcile a tmux-backed record against live host/process evidence.
fn reconcile_tmux(record: &mut TerminalRecord) {
    if !tmux_has_session(&record.socket) {
        // Host absent — never restart, never fabricate a screen.
        if recovery_marks_lost(false) {
            record.status = TermStatus::Lost;
        }
        return;
    }
    // Host present. The recorded host process must still own the session.
    let live_pid = tmux_server_pid(&record.socket);
    let matched = record.pid > 0 && live_pid == Some(record.pid);
    if !matched && recovery_marks_lost(true) {
        record.status = TermStatus::Lost;
    }
}

/// Reconcile a native-backed record against this process's live handles.
fn reconcile_native(record: &mut TerminalRecord) {
    let handle = native_handle(&record.id);
    let Some(handle) = handle else {
        // Fresh process (or session never materialized): host absent.
        if recovery_marks_lost(false) {
            record.status = TermStatus::Lost;
        }
        return;
    };
    // Host present and matched. Termination evidence?
    if handle.try_exit().is_some() {
        record.status = TermStatus::Exited;
        native_registry().lock().unwrap().remove(&record.id);
    }
}

/// Run the recovery decision model on a valid running record's evidence.
/// `host_present` distinguishes host-absent from host-present-but-mismatched.
fn recovery_marks_lost(host_present: bool) -> bool {
    use crate::terminal_recovery::{
        reconcile, CheckpointEvidence, HostEvidence, Input, Lifecycle, ProcessEvidence,
        RecordEvidence,
    };
    let decision = reconcile(Input {
        record: RecordEvidence::Valid,
        lifecycle: Lifecycle::Running,
        termination_present: false,
        host: if host_present {
            HostEvidence::PresentSame
        } else {
            HostEvidence::Absent
        },
        process: if host_present {
            ProcessEvidence::Mismatched
        } else {
            ProcessEvidence::Unavailable
        },
        checkpoint: CheckpointEvidence::Missing,
    });
    matches!(
        decision.disposition,
        crate::terminal_recovery::Disposition::MarkLost
    )
}

/// A real PTY session hosted by this process (upstream `native_session`).
pub(crate) struct NativeSession {
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    output: Arc<Mutex<NativeOutput>>,
    state: Arc<Mutex<NativeState>>,
}

#[derive(Clone, Copy)]
struct NativeState {
    status: TermStatus,
    exit_code: Option<u32>,
}

/// Bounded byte ring retaining raw pty output.
struct NativeOutput {
    data: Vec<u8>,
    cap: usize,
}

impl NativeOutput {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap.min(8192)),
            cap,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            self.data = bytes[bytes.len() - self.cap..].to_vec();
            return;
        }
        if self.data.len() + bytes.len() > self.cap {
            let excess = self.data.len() + bytes.len() - self.cap;
            self.data.drain(..excess);
        }
        self.data.extend_from_slice(bytes);
    }

    fn clear(&mut self) {
        self.data.clear();
    }
}

pub(crate) type NativeHandle = Arc<NativeSession>;

fn native_registry() -> &'static Mutex<HashMap<String, NativeHandle>> {
    static REG: OnceLock<Mutex<HashMap<String, NativeHandle>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn native_handle(id: &str) -> Option<NativeHandle> {
    native_registry().lock().unwrap().get(id).cloned()
}

fn require_native_handle(id: &str) -> Result<NativeHandle> {
    native_handle(id)
        .ok_or_else(|| anyhow!("terminal `{id}` has no live native session in this process"))
}

/// Whether this platform/process can open a native pty.
pub fn native_pty_available() -> bool {
    spawn_probe("true").is_ok()
}

/// Spawn `<program> [argv..]` under a real pty and return a live handle with
/// a reader thread draining the master into a bounded buffer.
fn spawn_native(
    id: &str,
    cwd: &Path,
    program: &str,
    argv: &[String],
    rows: u32,
    columns: u32,
) -> Result<NativeHandle> {
    let pty_system = portable_pty::native_pty_system();
    let size = portable_pty::PtySize {
        rows: rows as u16,
        cols: columns as u16,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .context("open native pty (is /dev/ptmx available?)")?;
    let mut cmd = portable_pty::CommandBuilder::new(program);
    cmd.args(argv);
    cmd.cwd(cwd);
    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn command in native pty")?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone native pty reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("take native pty writer")?;

    let output = Arc::new(Mutex::new(NativeOutput::new(NATIVE_OUTPUT_CAP)));
    let state = Arc::new(Mutex::new(NativeState {
        status: TermStatus::Running,
        exit_code: None,
    }));
    let session = Arc::new(NativeSession {
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        child: Mutex::new(child),
        output: output.clone(),
        state: state.clone(),
    });

    // Reader thread: drain pty output until EOF, then mark the session
    // exited (best-effort; the store reconcile is authoritative).
    let drain_output = output.clone();
    let drain_state = state.clone();
    std::thread::Builder::new()
        .name(format!("fxrs-native-{id}"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if let Ok(mut out) = drain_output.lock() {
                    out.append(&buf[..n]);
                }
            }
            if let Ok(mut st) = drain_state.lock() {
                if st.status == TermStatus::Running {
                    st.status = TermStatus::Exited;
                }
            }
        })
        .context("spawn native reader thread")?;

    Ok(session)
}

/// Probe helper: spawn a trivial command under a pty and wait for it.
fn spawn_probe(program: &str) -> Result<()> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(portable_pty::PtySize::default())?;
    let cmd = portable_pty::CommandBuilder::new(program);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut deadline = 0u32;
    loop {
        if let Some(_status) = child.try_wait()? {
            return Ok(());
        }
        deadline += 1;
        if deadline > 200 {
            let _ = child.kill();
            return Err(anyhow!("native pty probe timed out"));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

impl NativeSession {
    fn write(&self, input: &str, enter: bool) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow!("native terminal writer poisoned"))?;
        w.write_all(input.as_bytes())
            .context("write to native pty")?;
        if enter {
            w.write_all(b"\r").context("write enter to native pty")?;
        }
        w.flush().ok();
        Ok(())
    }

    pub(crate) fn read_text(
        &self,
        scrollback: usize,
        max_bytes: usize,
        raw: bool,
        clear_after: bool,
    ) -> String {
        let mut text = {
            let out = self.output.lock().unwrap();
            let data = out.data.as_slice();
            let full = String::from_utf8_lossy(data).to_string();
            if scrollback > 0 {
                tail_lines(&full, scrollback)
            } else {
                full
            }
        };
        if !raw {
            text = strip_ansi(&text);
        }
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
            if let Ok(mut out) = self.output.lock() {
                out.clear();
            }
        }
        text
    }

    fn resize(&self, rows: u32, columns: u32) -> Result<()> {
        let size = portable_pty::PtySize {
            rows: rows as u16,
            cols: columns as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master
            .lock()
            .map_err(|_| anyhow!("native terminal master poisoned"))?
            .resize(size)
            .context("resize native pty")
    }

    /// Kill (if needed) and reap the pty child.
    fn terminate(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow!("native terminal child poisoned"))?;
        let mut st = self
            .state
            .lock()
            .map_err(|_| anyhow!("native terminal state poisoned"))?;
        if st.status == TermStatus::Running {
            let _ = child.kill();
            let _ = child.wait();
        }
        st.status = TermStatus::Exited;
        Ok(())
    }

    /// Non-blocking child-exit poll. On exit, records the exit code and
    /// returns it.
    pub(crate) fn try_exit(&self) -> Option<u32> {
        let mut child = self.child.lock().ok()?;
        match child.try_wait().ok()? {
            Some(status) => {
                if let Ok(mut st) = self.state.lock() {
                    st.status = TermStatus::Exited;
                    st.exit_code = Some(status.exit_code());
                }
                Some(status.exit_code())
            }
            None => None,
        }
    }

    /// Whether the retained output buffer is non-empty (used by exec
    /// `return_when: started`).
    pub(crate) fn has_output(&self) -> bool {
        self.output
            .lock()
            .map(|o| !o.data.is_empty())
            .unwrap_or(false)
    }

    /// Whether the pty child has exited (reader-thread state).
    pub(crate) fn has_exited(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.status != TermStatus::Running)
            .unwrap_or(false)
    }
}

/// Keep the last `lines` newline-delimited lines of `text`, including any
/// trailing partial line.
fn tail_lines(text: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let mut kept: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    let mut piece_start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            kept.push_back(&text[piece_start..=i]);
            if kept.len() > lines {
                kept.pop_front();
            }
            piece_start = i + ch.len_utf8();
        }
    }
    let trailing = &text[piece_start..];
    if !trailing.is_empty() {
        kept.push_back(trailing);
        if kept.len() > lines {
            kept.pop_front();
        }
    }
    kept.iter().copied().collect::<Vec<&str>>().join("")
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
        "{:<10} {:<8} {:<8} {:<7} {:<6} {:<4} {}",
        "ID", "STATUS", "PID", "BACKEND", "COLS", "ROWS", "COMMAND"
    )];
    let mut sorted: Vec<&TerminalRecord> = records.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
    for r in sorted {
        let status = match r.status {
            TermStatus::Running => "running",
            TermStatus::Exited => "exited",
            TermStatus::Lost => "lost",
        };
        let name = r
            .name
            .as_deref()
            .map(|n| format!("[{n}] "))
            .unwrap_or_default();
        let cmd: String = r.command.chars().take(36).collect();
        lines.push(format!(
            "{:<10} {:<8} {:<8} {:<7} {:<6} {:<4} {}{}",
            r.id,
            status,
            r.pid,
            r.backend.as_str(),
            r.columns,
            r.rows,
            name,
            cmd
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
    fn tail_lines_keeps_last_n_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_lines("a\nb\nc\n", 5), "a\nb\nc\n");
        assert_eq!(tail_lines("single", 0), "");
        assert_eq!(tail_lines("x\ny", 1), "y");
    }

    #[test]
    fn backend_roundtrip_and_legacy_default_tmux() {
        assert_eq!(Backend::parse("native"), Some(Backend::Native));
        assert_eq!(Backend::parse("tmux"), Some(Backend::Tmux));
        assert_eq!(Backend::parse("bogus"), None);
        let legacy = serde_json::from_value::<TerminalRecord>(serde_json::json!({
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
        }))
        .unwrap();
        assert_eq!(legacy.backend, Backend::Tmux);
        let rec: TerminalRecord = serde_json::from_value(serde_json::json!({
            "id": "n1",
            "command": "bash",
            "cwd": ".",
            "rows": 40,
            "columns": 120,
            "started_at_ms": 0,
            "status": "running",
            "pid": 1,
            "backend": "native",
            "socket": "",
            "target": ""
        }))
        .unwrap();
        assert_eq!(rec.backend, Backend::Native);
        assert_eq!(rec.backend.as_str(), "native");
    }

    #[test]
    fn store_roundtrip_and_reconcile_legacy() {
        let dir = std::env::temp_dir().join(format!("fxrs-term-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("terminal.json");
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
                "pid": 0,
                "socket": "fxrs-ab1",
                "target": "ab1:0"
            }]
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
        let store: TerminalStore = serde_json::from_value(legacy).unwrap();
        assert_eq!(store.records[0].name, None);
        assert_eq!(store.records[0].backend, Backend::Tmux);
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
        let mut store = TerminalStore::default();
        let err = store.create(Path::new("."), Some(""), None, None, None);
        assert!(err.is_err());
        assert!(store.records.is_empty());
    }

    #[test]
    fn tmux_available_reports_bool() {
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
            backend: Backend::Native,
            socket: String::new(),
            target: String::new(),
        });
        let text = render_table(store.list());
        assert!(text.contains("t1"));
        assert!(text.contains("[api]"));
        assert!(text.contains("native"));
    }

    #[test]
    fn native_lifecycle_probe() {
        // The native backend must spawn, run, and report output + exit.
        if !native_pty_available() {
            eprintln!("native pty unavailable; skipping");
            return;
        }
        let handle = spawn_native(
            "probe",
            Path::new("."),
            "bash",
            &["-c".into(), "printf native-probe-ok".into()],
            24,
            80,
        )
        .unwrap();
        let mut exit = None;
        for _ in 0..200 {
            exit = handle.try_exit();
            if exit.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(exit.is_some(), "native child should exit");
        let text = handle.read_text(10, 65536, false, false);
        assert!(
            text.contains("native-probe-ok"),
            "native output missing: {text:?}"
        );
        handle.terminate().unwrap();
    }

    #[test]
    fn native_loss_after_fresh_process_is_marked_lost() {
        // A native record with no live handle (this process played host in
        // an earlier run) must reconcile to `lost` — never to a fake screen.
        let mut record = TerminalRecord {
            id: "native-gone".into(),
            status: TermStatus::Running,
            backend: Backend::Native,
            ..TerminalRecord::default()
        };
        assert!(native_handle("native-gone").is_none());
        reconcile_native(&mut record);
        assert_eq!(record.status, TermStatus::Lost);
    }
}
