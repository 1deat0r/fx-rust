//! Usage accounting: a JSONL sidecar under `~/.fx/usage.jsonl` recording one
//! record per agent turn, mirroring fx's usage.jsonl + usage CLI. Records are
//! appended (never rewritten); `fxrs usage` aggregates them.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub ts_ms: u128,
    pub workspace: String,
    pub session_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub steps: usize,
    pub tool_calls: usize,
    pub interactive: bool,
}

#[derive(Clone, Default)]
pub struct UsageStore {
    pub path: Option<PathBuf>,
}

impl UsageStore {
    pub fn new() -> Self {
        Self {
            path: Some(fx_home().join("usage.jsonl")),
        }
    }

    /// Append one record to the sidecar. Failures are non-fatal (logged).
    pub fn record(&self, rec: &UsageRecord) -> Result<()> {
        let path = self.path.as_ref().context("usage store has no path")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let line = serde_json::to_string(rec)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// Read all records that are parseable (corrupt lines are skipped).
    pub fn read_all(&self) -> Vec<UsageRecord> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let Ok(data) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        data.lines()
            .filter_map(|l| serde_json::from_str::<UsageRecord>(l.trim()).ok())
            .collect()
    }

    /// Aggregate records newer than `since_ms` (0 = all).
    pub fn aggregate(&self, since_ms: u128) -> UsageTotals {
        let mut t = UsageTotals::default();
        for rec in self.read_all() {
            if rec.ts_ms < since_ms {
                continue;
            }
            t.records += 1;
            t.turns += 1;
            t.input_tokens += rec.input_tokens;
            t.output_tokens += rec.output_tokens;
            t.total_tokens += rec.total_tokens;
            t.cost_usd += rec.cost_usd;
            t.steps += rec.steps;
            t.tool_calls += rec.tool_calls;
            t.sessions.extend([rec.session_id]);
        }
        t
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub records: usize,
    pub turns: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub steps: usize,
    pub tool_calls: usize,
    pub sessions: std::collections::BTreeSet<String>,
}

/// Millisecond timestamp now (matches session code).
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Parse a `--period`-style duration (`24h`, `7d`, `30d`, `session`) into a
/// since-ms cutoff. 0 means "no cutoff".
pub fn parse_period(period: &str) -> u128 {
    let p = period.trim().to_ascii_lowercase();
    if p.is_empty() || p == "all" || p == "0" || p == "session" {
        return 0;
    }
    let now = now_ms();
    let mult = |c: char| -> Option<u128> {
        match c {
            'h' => Some(3_600_000),
            'd' => Some(86_400_000),
            'w' => Some(604_800_000),
            'm' => Some(2_592_000_000), // 30 days
            's' => Some(1_000),
            _ => None,
        }
    };
    let (num, unit) = if p.ends_with('h')
        || p.ends_with('d')
        || p.ends_with('w')
        || p.ends_with('m')
        || p.ends_with('s')
    {
        (p[..p.len() - 1].to_string(), p.chars().last().unwrap())
    } else {
        (p.clone(), 'd')
    };
    let n: u128 = num.parse().unwrap_or(7);
    let Some(m) = mult(unit) else { return 0 };
    now.saturating_sub(n * m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_counts_records() {
        let dir = std::env::temp_dir().join(format!("fxrs-usage-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = UsageStore {
            path: Some(dir.join("usage.jsonl")),
        };
        for i in 0..3 {
            store
                .record(&UsageRecord {
                    ts_ms: now_ms(),
                    workspace: "/ws".into(),
                    session_id: format!("s{i}"),
                    model: "m".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    cost_usd: 0.001,
                    steps: 2,
                    tool_calls: 1,
                    interactive: false,
                })
                .unwrap();
        }
        let t = store.aggregate(0);
        assert_eq!(t.records, 3);
        assert_eq!(t.total_tokens, 45);
        assert_eq!(t.sessions.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_period_hours_and_days() {
        let h = parse_period("24h");
        let d = parse_period("7d");
        assert!(h > 0 && d < h); // 7d cutoff is earlier (smaller) than 24h
        assert_eq!(parse_period("all"), 0);
    }
}
