//! Prompt history (fx's `core/session/prompt_history_store.zig`): an
//! append-only JSONL of prompts under `~/.fx/history.jsonl` with the same
//! record shape as upstream (`schema_version`, `timestamp_ms`,
//! `workspace_root`, `text`), size-capped by compacting to the most recent
//! 1000 records / 1 MiB.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::fx_home;
use crate::util::now_ms;

const SCHEMA_VERSION: u8 = 1;
const COMPACTION_RECORD_LIMIT: usize = 1000;
const COMPACTION_BYTE_LIMIT: u64 = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub schema_version: u8,
    pub timestamp_ms: u128,
    pub workspace_root: String,
    pub text: String,
}

#[derive(Clone)]
pub struct HistoryStore {
    pub path: PathBuf,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryStore {
    pub fn new() -> Self {
        Self {
            path: fx_home().join("history.jsonl"),
        }
    }

    /// Append a prompt, compacting when the file outgrows its limits.
    pub fn record(&self, workspace_root: &str, text: &str) -> Result<()> {
        if text.trim().is_empty() || text.trim().len() > MAX_RECORD_BYTES {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let rec = HistoryRecord {
            schema_version: SCHEMA_VERSION,
            timestamp_ms: now_ms(),
            workspace_root: workspace_root.to_string(),
            text: text.trim().to_string(),
        };
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        drop(f);
        self.maybe_compact();
        Ok(())
    }

    pub fn read(&self) -> Vec<HistoryRecord> {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        data.lines()
            .filter_map(|l| serde_json::from_str::<HistoryRecord>(l.trim()).ok())
            .collect()
    }

    /// Most recent first, filtered by optional substring term, capped.
    pub fn query(&self, term: Option<&str>, limit: usize) -> Vec<HistoryRecord> {
        let mut recs: Vec<_> = self
            .read()
            .into_iter()
            .filter(|r| match term {
                Some(t) => {
                    let q = t.to_lowercase();
                    r.text.to_lowercase().contains(&q)
                        || r.workspace_root.to_lowercase().contains(&q)
                }
                None => true,
            })
            .collect();
        recs.sort_by_key(|r| std::cmp::Reverse(r.timestamp_ms));
        recs.truncate(limit.max(1));
        recs
    }

    /// Compaction: if the file exceeds record/byte limits, rewrite keeping the
    /// most recent records (oldest dropped). Never fails the caller.
    fn maybe_compact(&self) {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let mut records = self.read();
        let over = meta.len() > COMPACTION_BYTE_LIMIT || records.len() > COMPACTION_RECORD_LIMIT;
        if !over {
            return;
        }
        if records.len() > COMPACTION_RECORD_LIMIT {
            records.sort_by_key(|r| std::cmp::Reverse(r.timestamp_ms));
            records.truncate(COMPACTION_RECORD_LIMIT);
        }
        let mut body = String::new();
        // Re-write in chronological (append) order.
        records.sort_by_key(|a| a.timestamp_ms);
        for r in &records {
            if let Ok(line) = serde_json::to_string(r) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        // Write to a temp then rename so appends never see a half-written file.
        let tmp = self.path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> HistoryStore {
        let home = std::env::temp_dir().join(format!("fxrs-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("FX_HOME", &home);
        let s = HistoryStore::new();
        let _ = home;
        s
    }

    #[test]
    fn record_and_query() {
        let s = store("rq");
        s.record("/ws", "hello world").unwrap();
        s.record("/ws", "how are you").unwrap();
        let all = s.query(None, 10);
        assert_eq!(all.len(), 2);
        let found = s.query(Some("hello"), 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "hello world");
        std::env::remove_var("FX_HOME");
    }

    #[test]
    fn compaction_keeps_most_recent() {
        let s = store("cmp");
        for i in 0..1050 {
            s.record("/ws", &format!("prompt {i}")).unwrap();
        }
        let recs = s.read();
        assert!(recs.len() <= COMPACTION_RECORD_LIMIT, "got {}", recs.len());
        // The most recent prompt survives.
        assert!(recs.iter().any(|r| r.text == "prompt 1049"));
        std::env::remove_var("FX_HOME");
    }
}
