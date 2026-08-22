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
    /// Agent session that launched this process ("" when unknown). Used for
    /// restore-on-resume reporting: a resumed agent can see which daemons
    /// belong to it.
    #[serde(default)]
    pub session_id: Option<String>,
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
            session_id: None,
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
        self.start_with_session(command, cwd, name, None)
    }

    /// `start` plus an optional owning agent session id (restore-on-resume).
    pub fn start_with_session(
        &mut self,
        command: &str,
        cwd: &Path,
        name: Option<&str>,
        session_id: Option<&str>,
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
            session_id: session_id.map(|s| s.to_string()),
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

    /// Stop a running process: SIGTERM the process group (the double-fork
    /// launcher makes the stored pid a session/group leader, so one group
    /// signal covers the whole tree including `sh` children), wait up to
    /// `timeout_ms`, then SIGKILL the group. Falls back to pid-only signaling
    /// when the pid is not a group leader (e.g. the group is gone).
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

        let snapshot = process_table();
        let pgid = snapshot.iter().find(|p| p.pid == pid).map(|p| p.pgid);
        let group_leader = pgid == Some(pid);

        if group_leader {
            sync_group(pid, 15);
        } else {
            sync_signal(pid, 15);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if pid_alive(pid) {
            if group_leader {
                sync_group(pid, 9);
            }
            sync_signal(pid, 9);
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

/// One row of `ps` output captured for supervision (snapshot, no cross-snapshot
/// CPU sampling — `cpu_percent` is the lifetime average reported by ps).
#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub stat: String,
    pub rss_kb: u64,
    pub etimes_secs: u64,
    pub cpu_percent: f64,
    pub command: String,
}

/// Snapshot the live process table once (`ps -eo pid,ppid,pgid,stat,rss,etimes,pcpu,comm`).
/// A malformed row is skipped; a process table that cannot be read at all
/// returns an empty slice (callers degrade to pid-alive checks).
pub fn process_table() -> Vec<ProcessInfo> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,ppid=,pgid=,stat=,rss=,etimes=,pcpu=,comm="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let pid: u32 = match f[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ppid: u32 = f[1].parse().unwrap_or(0);
        let pgid: u32 = f[2].parse().unwrap_or(pid);
        let rss_kb: u64 = f[4].parse().unwrap_or(0);
        let etimes_secs: u64 = f[5].parse().unwrap_or(0);
        let cpu_percent: f64 = f[6].parse().unwrap_or(0.0);
        rows.push(ProcessInfo {
            pid,
            ppid,
            pgid,
            stat: f[3].to_string(),
            rss_kb,
            etimes_secs,
            cpu_percent,
            command: f[7..].join(" "),
        });
    }
    rows
}

/// Direct children of `pid` according to a process-table snapshot.
pub fn children_of(table: &[ProcessInfo], pid: u32) -> Vec<&ProcessInfo> {
    let mut out: Vec<&ProcessInfo> = table.iter().filter(|r| r.ppid == pid).collect();
    out.sort_by_key(|r| r.pid);
    out
}

/// All descendants of `pid`, breadth-first (children of each node ordered by
/// pid), so parents always appear before their children.
pub fn descendants(table: &[ProcessInfo], pid: u32) -> Vec<&ProcessInfo> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pid);
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    while let Some(cur) = queue.pop_front() {
        let kids = children_of(table, cur);
        for k in kids {
            if seen.insert(k.pid) {
                out.push(k);
                queue.push_back(k.pid);
            }
        }
    }
    out
}

/// A background record plus live supervision data (None when no longer alive).
#[derive(Debug, Clone, Serialize)]
pub struct SupervisedRecord {
    #[serde(flatten)]
    pub record: BackgroundRecord,
    pub alive: bool,
    pub rss_kb: Option<u64>,
    pub etimes_secs: Option<u64>,
    pub cpu_percent: Option<f64>,
    /// Number of live descendants at snapshot time.
    pub children_alive: usize,
}

impl BackgroundStore {
    /// Enrich every record with live process data (one `ps` snapshot shared
    /// across records) and counts of live children.
    pub fn supervise(&self) -> Vec<SupervisedRecord> {
        let table = process_table();
        self.records
            .iter()
            .map(|r| {
                let info = table.iter().find(|p| p.pid == r.pid).cloned();
                let alive = r.status == BgStatus::Running && info.is_some();
                let children_alive = if info.is_some() {
                    descendants(&table, r.pid).len()
                } else {
                    0
                };
                SupervisedRecord {
                    record: r.clone(),
                    alive,
                    rss_kb: info.as_ref().map(|i| i.rss_kb),
                    etimes_secs: info.as_ref().map(|i| i.etimes_secs),
                    cpu_percent: info.as_ref().map(|i| i.cpu_percent),
                    children_alive,
                }
            })
            .collect()
    }

    /// Process group id of a record's pid (0 when unknown/unreadable).
    pub fn pgid_of(&self, pid: u32) -> u32 {
        let table = process_table();
        table
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.pgid)
            .unwrap_or(0)
    }

    /// Stop a process and its whole tree: signal the process group first (the
    /// double-fork launcher makes the stored pid a session/group leader), then
    /// any descendants that escaped into their own groups, waiting `timeout_ms`
    /// between SIGTERM and SIGKILL.
    pub fn stop_tree(&mut self, id: &str, timeout_ms: u64) -> Result<BackgroundRecord> {
        let idx = self
            .records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| anyhow!("unknown background process id `{id}`"))?;
        if self.records[idx].status != BgStatus::Running {
            bail!("background process `{id}` is not running");
        }
        let pid = self.records[idx].pid;

        let snapshot = process_table();
        // The launcher starts the command under setsid, so pid is normally a
        // session+group leader: signal the whole group with kill -- -pid.
        let pgid = snapshot.iter().find(|p| p.pid == pid).map(|p| p.pgid);
        let mut targets: Vec<u32> = Vec::new();
        // All descendants, group members inclusive (children of the leader are
        // in its group unless they called setsid themselves).
        targets.extend(descendants(&snapshot, pid).iter().map(|p| p.pid));
        if !targets.contains(&pid) {
            targets.push(pid);
        }
        // Terminate: a group signal for the leader's group (one call, covers
        // every member); individual signals for descendants that escaped into
        // their own group (deepest first so parents can reap cleanly).
        let leader_alive = snapshot.iter().any(|p| p.pid == pid);
        let group_signal = pgid.filter(|g| *g == pid && leader_alive);
        let mut escaped: Vec<u32> = if group_signal.is_some() {
            // Anything whose group differs from the leader's group.
            snapshot
                .iter()
                .filter(|p| targets.contains(&p.pid) && p.pid != pid && p.pgid != pid)
                .map(|p| p.pid)
                .collect()
        } else {
            targets.clone()
        };
        escaped.sort_by_key(|p| std::cmp::Reverse(*p));
        if let Some(g) = group_signal {
            sync_group(g, 15);
        }
        for t in &escaped {
            sync_signal(*t, 15);
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let table = process_table();
            let any_alive = targets.iter().any(|t| {
                let info = table.iter().find(|p| p.pid == *t);
                info.is_some() || (info.is_none() && pid_alive(*t))
            });
            if !any_alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // SIGKILL survivors (group first, then individually).
        let table = process_table();
        let survivors: Vec<u32> = targets
            .iter()
            .copied()
            .filter(|t| table.iter().any(|p| p.pid == *t) || pid_alive(*t))
            .collect();
        if let Some(g) = group_signal {
            sync_group(g, 9);
        }
        for t in &survivors {
            sync_signal(*t, 9);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));

        let code = parse_exit_code(Path::new(&self.records[idx].log_path));
        self.records[idx].status = BgStatus::Exited;
        self.records[idx].exit_code = code;
        let record = self.records[idx].clone();
        self.save()?;
        Ok(record)
    }
}

/// Render a supervision table (used by `fxrs background supervise` and
/// `/background supervise`).
pub fn render_supervise(records: &[SupervisedRecord]) -> String {
    let mut lines = vec![format!(
        "{:<10} {:<8} {:<6} {:<7} {:<7} {:<6} {:<6} {}",
        "ID", "STATUS", "PID", "RSS-KB", "ELAPSED", "CPU%", "KIDS", "COMMAND"
    )];
    let mut sorted: Vec<&SupervisedRecord> = records.iter().collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.record.started_at_ms));
    let mut running = 0usize;
    for s in sorted {
        let (status, alive) = match (&s.record.status, s.alive) {
            (BgStatus::Running, true) => {
                running += 1;
                ("running", "")
            }
            (BgStatus::Running, false) => ("exited?", ""),
            (BgStatus::Exited, _) => ("exited", ""),
            (BgStatus::Failed, _) => ("failed", ""),
        };
        let rss = s
            .rss_kb
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        let elapsed = s
            .etimes_secs
            .map(render_elapsed)
            .unwrap_or_else(|| "—".into());
        let cpu = s
            .cpu_percent
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "—".into());
        let kids = s.children_alive.to_string();
        let name = s
            .record
            .name
            .as_deref()
            .map(|n| format!("[{n}] "))
            .unwrap_or_default();
        let cmd: String = s.record.command.chars().take(40).collect();
        lines.push(format!(
            "{:<10} {:<8} {:<6} {:<7} {:<7} {:<6} {:<6} {}{}{}",
            s.record.id, status, s.record.pid, rss, elapsed, cpu, kids, name, cmd, alive
        ));
    }
    lines.push(format!(
        "
{} running · {} total",
        running,
        records.len()
    ));
    lines.join("\n")
}

fn render_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render a process tree for one background record (used by `fxrs background
/// tree <id>` and the tool's `tree` action).
pub fn render_tree(record: &BackgroundRecord, table: &[ProcessInfo]) -> String {
    let mut lines = vec![format!(
        "{} (pid {}){}",
        record.id,
        record.pid,
        if record.status == BgStatus::Running {
            " · running"
        } else {
            " · not running"
        }
    )];
    let kids = children_of(table, record.pid);
    if kids.is_empty() {
        lines.push("  └─ (no children)".into());
    } else {
        render_tree_nodes(&mut lines, table, &kids, "");
    }
    lines.join("\n")
}

fn render_tree_nodes(
    lines: &mut Vec<String>,
    table: &[ProcessInfo],
    kids: &[&ProcessInfo],
    indent: &str,
) {
    for (i, kid) in kids.iter().enumerate() {
        let last = i == kids.len() - 1;
        let branch = if last { "└─ " } else { "├─ " };
        lines.push(format!("{indent}{branch}{}{}", kid.pid, kid.command));
        let sub = children_of(table, kid.pid);
        let next_indent = format!("{indent}{}", if last { "   " } else { "│  " });
        render_tree_nodes(lines, table, &sub, &next_indent);
    }
}

fn sync_group(pgid: u32, signal: i32) {
    // Correct argv: `kill -<signal> -- -<pgid>` (negative pid targets the whole
    // process group). Passing `-- -15` as one token previously made the group
    // signal a no-op and left children orphaned.
    let _: std::io::Result<std::process::ExitStatus> = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
            session_id: Some("sess-123".into()),
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

    #[test]
    fn old_store_files_without_session_id_still_load() {
        let dir = std::env::temp_dir().join(format!("fxrs-bg-sessid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("background.json");
        // A record serialized before session_id existed.
        let legacy = serde_json::json!({
            "version": 1,
            "records": [{
                "id": "abc",
                "name": null,
                "command": "true",
                "cwd": ".",
                "pid": 1,
                "started_at_ms": 0,
                "status": "exited",
                "exit_code": 0,
                "log_path": "/dev/null"
            }]
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
        // Load goes through fx_home; we can't override config here, so parse
        // directly through serde (the load path in production uses the same
        // deserializer, which must tolerate the missing field).
        let store: BackgroundStore = serde_json::from_value(legacy).unwrap();
        assert_eq!(store.records[0].session_id, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn descendants_and_children_work() {
        let mk = |pid: u32, ppid: u32| ProcessInfo {
            pid,
            ppid,
            pgid: 1,
            stat: "S".into(),
            rss_kb: 0,
            etimes_secs: 0,
            cpu_percent: 0.0,
            command: format!("p{pid}"),
        };
        let table = vec![mk(1, 0), mk(2, 1), mk(3, 1), mk(4, 2), mk(5, 4), mk(6, 3)];
        let kids = children_of(&table, 1);
        let pids: Vec<u32> = kids.iter().map(|k| k.pid).collect();
        assert_eq!(pids, vec![2, 3]);
        let desc = descendants(&table, 1);
        let dpids: Vec<u32> = desc.iter().map(|d| d.pid).collect();
        // breadth-first, ordered by pid: 2,3,4,6,5 — 5 is deepest so appears last
        // but BFS visits 2's children (4) before 3's children (6), then 4's child (5).
        assert_eq!(dpids, vec![2, 3, 4, 6, 5]);
        assert!(descendants(&table, 6).is_empty());
    }

    #[test]
    fn supervise_marks_missing_pids_not_alive() {
        let mut store = BackgroundStore::default();
        store.records.push(BackgroundRecord {
            id: "ghost".into(),
            command: "true".into(),
            cwd: ".".into(),
            pid: u32::MAX - 1,
            started_at_ms: 0,
            status: BgStatus::Running,
            exit_code: None,
            log_path: "/dev/null".into(),
            name: None,
            session_id: None,
        });
        // process_table() reads real ps; a pid that cannot exist reports none.
        let supervised = store.supervise();
        assert_eq!(supervised.len(), 1);
        assert!(!supervised[0].alive);
    }

    #[test]
    fn render_supervise_never_empty_and_totals() {
        let mut store = BackgroundStore::default();
        store.records.push(BackgroundRecord {
            id: "r1".into(),
            command: "x".into(),
            cwd: ".".into(),
            pid: 1,
            started_at_ms: 5,
            status: BgStatus::Exited,
            exit_code: Some(0),
            log_path: "/dev/null".into(),
            name: None,
            session_id: None,
        });
        let text = render_supervise(&store.supervise());
        assert!(text.contains("r1"));
        assert!(text.contains("1 total"));
    }
}
