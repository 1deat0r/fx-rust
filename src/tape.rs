//! Replay tape (fx's `core/workspace/record_tape.zig` +
//! `core/session/command_replay_store.zig`): a per-session JSONL log of every
//! executed tool call, so `fxrs replay tape <id>` can reproduce what the
//! agent actually did. Append-only, size-capped.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;

const MAX_TAPE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    pub ts_ms: u128,
    pub tool: String,
    pub target: String,
    pub ok: bool,
    /// First ~400 chars of the result, enough to remember the outcome.
    pub preview: String,
}

pub struct TapeStore {
    root: PathBuf,
}

fn session_dir(workspace: &Path, id: &str) -> PathBuf {
    let canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let key = canon.to_string_lossy().to_string();
    let hash = simple_hash(&key);
    fx_home()
        .join("sessions")
        .join(format!("ws-{hash}"))
        .join(id)
}

impl TapeStore {
    pub fn for_session(workspace: &Path, id: &str) -> Self {
        Self {
            root: session_dir(workspace, id),
        }
    }

    pub fn tap_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.tape.jsonl"))
    }

    pub fn record(&self, entry: &TapeEntry, id: &str) {
        if let Err(e) = self.append(entry, id) {
            eprintln!("[fxrs] tape record failed: {e:#}");
        }
    }

    fn append(&self, entry: &TapeEntry, id: &str) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.tap_path(id);
        // Size cap: stop appending (drop the tape) rather than grow forever.
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_TAPE_BYTES {
            return Ok(());
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let line = serde_json::to_string(entry)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    pub fn read(&self, id: &str) -> Vec<TapeEntry> {
        let path = self.tap_path(id);
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        data.lines()
            .filter_map(|l| serde_json::from_str::<TapeEntry>(l.trim()).ok())
            .collect()
    }
}

fn simple_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    }

    #[test]
    fn tape_roundtrip_via_fx_home() {
        let home = std::env::temp_dir().join(format!("fxrs-tape-{}", std::process::id()));
        std::env::set_var("FX_HOME", &home);
        let _ = std::fs::remove_dir_all(&home);
        let ws = Path::new("/tmp/fxrs-tape-ws");
        let store = TapeStore::for_session(ws, "tape-1");
        store.record(
            &TapeEntry {
                ts_ms: now(),
                tool: "run_command".into(),
                target: "cargo test".into(),
                ok: true,
                preview: "ok. 3 passed".into(),
            },
            "tape-1",
        );
        store.record(
            &TapeEntry {
                ts_ms: now(),
                tool: "write_file".into(),
                target: "/tmp/fxrs-tape-ws/a.rs".into(),
                ok: false,
                preview: "error: denied".into(),
            },
            "tape-1",
        );
        let tape = store.read("tape-1");
        assert_eq!(tape.len(), 2);
        assert_eq!(tape[0].tool, "run_command");
        assert!(tape[0].ok);
        assert!(!tape[1].ok);
        std::env::remove_var("FX_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
