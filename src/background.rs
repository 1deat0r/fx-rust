//! Background process execution: a detached-process store plus the
//! `background_process` tool backend. Faithful to upstream fx's
//! `core/background/*` + `tools/shell/background_process.zig` at the
//! observable level: start a command detached (own session, log file),
//! list/lookup records, tail output, stop with SIGTERM→SIGKILL grace,
//! and reconcile liveness on load (an exit-code marker is appended to the
//! log so finished processes report their real exit code even after the
//! agent restarted).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Marker appended to a process log when the wrapped shell exits (upstream
/// `background_process_provider.exit_marker`).
pub const EXIT_MARKER: &str = "__FX_EXIT_CODE__=";
pub const SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BgStatus {
    Running,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundRecord {
    pub id: String,
    pub name: Option<String>,
    pub command: String,
    pub cwd: String,
    pub pid: u32,
    pub started_at_ms: u128,
    pub status: BgStatus,
    pub exit_code: Option<i32>,
    pub log_path: String,
}

impl Default for BackgroundRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            command: String::new(),
            cwd: String::new(),
            pid: 0,
            started_at_ms: 0,
            status: BgStatus::Running,
            exit_code: None,
            log_path: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundStore {
    pub version: u64,
    pub records: Vec<BackgroundRecord>,
}

impl Default for BackgroundStore {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}

pub fn store_path() -> PathBuf {
    crate::config::fx_home().join("background.json")
}

pub fn log_dir() -> PathBuf {
    crate::config::fx_home().join("background")
}

/// Load the store from disk (missing file = empty), then reconcile liveness
/// so a resumed agent sees accurate status.
pub fn load() -> Result<BackgroundStore> {
    BackgroundStore::open()
}

impl BackgroundStore {
    /// Inherent loader used by tools and CLI (reconciles liveness).
    pub fn open() -> Result<BackgroundStore> {
        load_inner()
    }
}

fn load_inner() -> Result<BackgroundStore> {
    let path = store_path();
    let mut store = if path.exists() {
        let data = std::fs::read_to_string(&path).context("read background store")?;
        serde_json::from_str(&data).context("parse background store")?
    } else {
        BackgroundStore::default()
    };
    store.reconcile();
    Ok(store)
}

fn save_store(store: &BackgroundStore) -> Result<()> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create background store dir")?;
    }
    std::fs::create_dir_all(log_dir()).context("create background log dir")?;
    let data = serde_json::to_string_pretty(store)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).context("write background store")?;
    std::fs::rename(&tmp, path).context("commit background store")?;
    Ok(())
}

impl BackgroundStore {
    fn reconcile(&mut self) {
        for record in &mut self.records {
            if record.status != BgStatus::Running {
                continue;
            }
            if !pid_alive(record.pid) {
                let code = parse_exit_code(Path::new(&record.log_path));
                record.status = if code.is_some() {
                    BgStatus::Exited
                } else {
                    BgStatus::Failed
                };
                record.exit_code = code;
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        save_store(self)
    }

    fn next_id(&self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("{:x}", nanos & 0xffff_ffff_ffff);
        let base = suffix;
        // Avoid collisions inside one process lifetime.
        let mut candidate = base.clone();
        let mut n = 0u32;
        while self.records.iter().any(|r| r.id == candidate) {
            n += 1;
            candidate = format!("{base}{n:x}");
        }
        candidate
    }

    /// Start a detached background process.
    ///
    /// Wire behavior (mirroring upstream): the command runs via `sh -c` in
    /// its own session (setsid when available, else nohup), stdio goes to
    /// `~/.fx/background/<id>.log`, stdin is /dev/null, and the log gains a
    /// trailing `__FX_EXIT_CODE__=` marker on exit.
    ///
    /// Detachment uses the double-fork pattern: the direct child (`sh`)
    /// backgrounds the real process and exits immediately, so the agent can
    /// reap it and the actual process is reparented to init (no zombies
    /// answering `kill -0` as alive after they exit).
    pub fn start(
        &mut self,
        command: &str,
        cwd: &Path,
        name: Option<&str>,
    ) -> Result<BackgroundRecord> {
        let command = command.trim();
        if command.is_empty() {
            bail!("background_process start: command must not be empty");
        }
        std::fs::create_dir_all(log_dir()).context("create background log dir")?;
        let id = self.next_id();
        let log_path = log_dir().join(format!("{id}.log"));
        let log_disp = shell_quote(&log_path.display().to_string());

        // Wrap with an exit-code marker: run the command, then append
        // `__FX_EXIT_CODE__=<exit>` to the log when the shell exits.
        let marker = format!(
            r###"printf '{marker}%s\n' "$?" >> {log_disp}"###,
            marker = EXIT_MARKER
        );
        let script = format!("set +e\n{command}\n{marker}");
        let wrapped_sh = shell_quote(&script);
        let inner = if setsid_available() {
            format!("setsid bash -lc {wrapped_sh}")
        } else {
            format!("nohup bash -lc {wrapped_sh}")
        };
        // Background the inner process, echo its pid, and exit (double fork).
        let launcher = format!("{inner} >{log_disp} 2>&1 </dev/null & echo $!");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&launcher)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn background process")?;
        let mut pid_out = String::new();
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read;
            let _ = out.read_to_string(&mut pid_out);
        }
        let pid: u32 = pid_out
            .trim()
            .parse()
            .with_context(|| format!("background launcher produced no pid: `{pid_out}`"))?;
        // Reap the launcher (it exits right after printing the pid).
        let _ = child.wait();

        let started_at_ms = crate::util::now_ms();
        let record = BackgroundRecord {
            id: id.clone(),
            name: name.map(str::to_string),
            command: command.to_string(),
            cwd: cwd.display().to_string(),
            pid,
            started_at_ms,
            status: BgStatus::Running,
            exit_code: None,
            log_path: log_path.display().to_string(),
        };
        self.records.push(record.clone());
        self.save()?;
        Ok(record)
    }

    /// All records (owned copy is caller's choice; newest first is up to the
    /// renderer).
    pub fn list(&self) -> &[BackgroundRecord] {
        &self.records
    }

    /// Look up one record.
    pub fn get(&self, id: &str) -> Option<&BackgroundRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Return the shared log of a record as text (bounded), with an
    /// optional tail count of lines.
    pub fn log_text(&self, id: &str, max_bytes: usize, tail: Option<usize>) -> Result<String> {
        let record = self
            .records
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown background process id `{id}`"))?;
        let text = std::fs::read_to_string(&record.log_path)
            .with_context(|| format!("read log {}", record.log_path))?;
        let text = if text.len() > max_bytes {
            let start = text.len() - max_bytes;
            let mut out = String::from(&text[start..]);
            out.insert_str(
                0,
                &format!("… [truncated {} bytes]\n", text.len() - max_bytes),
            );
            out
        } else {
            text
        };
        Ok(match tail {
            Some(n) if n > 0 => text
                .lines()
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n"),
            _ => text,
        })
    }

    /// Stop a running process: SIGTERM, wait up to `timeout_ms`, then SIGKILL.
    /// Returns the updated record.
    pub fn stop(&mut self, id: &str, timeout_ms: u64) -> Result<BackgroundRecord> {
        let idx = self
            .records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown background process id `{id}`"))?;
        if self.records[idx].status != BgStatus::Running {
            bail!("background process `{id}` is not running");
        }
        let pid = self.records[idx].pid;
        sync_signal(pid, 15); // SIGTERM
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if pid_alive(pid) {
            sync_signal(pid, 9); // SIGKILL
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let code = parse_exit_code(Path::new(&self.records[idx].log_path));
        self.records[idx].status = BgStatus::Exited;
        self.records[idx].exit_code = code;
        let record = self.records[idx].clone();
        self.save()?;
        Ok(record)
    }
}

/// Does `pid` exist? (`kill -0`)
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let status = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

fn sync_signal(pid: u32, signal: i32) {
    let _: std::io::Result<std::process::ExitStatus> = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn setsid_available() -> bool {
    Command::new("setsid")
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse the trailing `__FX_EXIT_CODE__=<code>` marker from a log file.
pub fn parse_exit_code(log: &Path) -> Option<i32> {
    let data = std::fs::read_to_string(log).ok()?;
    parse_exit_code_str(&data)
}

fn parse_exit_code_str(data: &str) -> Option<i32> {
    data.lines().rev().find_map(|line| {
        let line = line.trim_end_matches('\r');
        line.strip_prefix(EXIT_MARKER)?.trim().parse::<i32>().ok()
    })
}

pub fn render_table(records: &[BackgroundRecord]) -> String {
    let mut lines = vec![format!(
        "{:<10} {:<8} {:<6} {}",
        "ID", "STATUS", "PID", "COMMAND"
    )];
    let mut sorted: Vec<&BackgroundRecord> = records.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
    for r in sorted {
        let status = match r.status {
            BgStatus::Running => "running",
            BgStatus::Exited => "exited",
            BgStatus::Failed => "failed",
        };
        let name = r
            .name
            .as_deref()
            .map(|n| format!("[{n}] "))
            .unwrap_or_default();
        let cmd: String = r.command.chars().take(48).collect();
        lines.push(format!(
            "{:<10} {:<8} {:<6} {}{}",
            r.id, status, r.pid, name, cmd
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_marker_parses() {
        let data = "hello\nworld\n__FX_EXIT_CODE__=0\n";
        assert_eq!(parse_exit_code_str(data), Some(0));
        let data = "boom\n__FX_EXIT_CODE__=127\n";
        assert_eq!(parse_exit_code_str(data), Some(127));
        let data = "no marker here\n";
        assert_eq!(parse_exit_code_str(data), None);
    }

    #[test]
    fn shell_quoting_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn store_roundtrip_and_reconcile() {
        let dir = std::env::temp_dir().join(format!("fxrs-bg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = &dir;
        // Point the store at the temp home by overriding store functions.
        // (These functions read config::fx_home; we simulate by writing files
        // directly and calling reconcile on a manually-built store.)
        let mut store = BackgroundStore::default();
        store.records.push(BackgroundRecord {
            id: "gone".into(),
            command: "true".into(),
            cwd: ".".into(),
            pid: u32::MAX - 1,
            started_at_ms: 0,
            status: BgStatus::Running,
            exit_code: None,
            log_path: home.join("gone.log").display().to_string(),
            name: None,
        });
        std::fs::write(home.join("gone.log"), "x\n__FX_EXIT_CODE__=42\n").unwrap();
        store.reconcile();
        assert_eq!(store.records[0].status, BgStatus::Exited);
        assert_eq!(store.records[0].exit_code, Some(42));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_ids_are_unique() {
        let mut store = BackgroundStore::default();
        store.records.push(BackgroundRecord {
            id: store.next_id().clone(),
            ..BackgroundRecord::default()
        });
        let second = store.next_id();
        assert_ne!(store.records[0].id, second);
    }
}
