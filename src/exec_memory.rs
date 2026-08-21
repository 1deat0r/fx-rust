//! Execution memory (fx's `core/agent/execution_memory.zig` + the workspace
//! read tracker idea): a bounded record of what the agent has already done in
//! this run. Fed back to the model each turn as a compact context block so it
//! does not re-read/re-run identical work and does not silently lose track of
//! prior steps in long sessions.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

const CAP: usize = 240;

#[derive(Debug, Clone)]
pub struct Entry {
    pub tool: String,
    pub summary: String,
}

/// Bounded, dedup-aware record of executed tool calls.
#[derive(Debug, Clone, Default)]
pub struct ExecMemory {
    entries: VecDeque<Entry>,
    /// last summary per (tool, arg-key) for dedup bookkeeping
    last_same: HashMap<String, String>,
}

impl ExecMemory {
    pub fn record(&mut self, tool: &str, args: &Value, result_text: &str, ok: bool) {
        let key = self.dedup_key(tool, args);
        let summary = summarize(tool, args, result_text, ok);
        // Don't stack identical consecutive calls (e.g. read_file same path).
        let same_as_last = self
            .last_same
            .get(&key)
            .map(|prev| prev == &summary)
            .unwrap_or(false);
        self.last_same.insert(key, summary.clone());
        if same_as_last && self.entries.back().map(|e| e.summary == summary).unwrap_or(false) {
            return;
        }
        self.entries.push_back(Entry { tool: tool.to_string(), summary });
        while self.entries.len() > CAP {
            self.entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn dedup_key(&self, tool: &str, args: &Value) -> String {
        // For path tools the path (first relevant key) drives dedup; for
        // bash it's the exact command line.
        if tool == "run_command" {
            return format!("{tool}:{}", args.get("command").and_then(|v| v.as_str()).unwrap_or(""));
        }
        for k in ["file_path", "path", "old_path", "query", "url", "source"] {
            if let Some(v) = args.get(k).and_then(|v| v.as_str()) {
                return format!("{tool}:{k}={v}");
            }
        }
        format!("{tool}:{}", serde_json::to_string(args).unwrap_or_default())
    }

    /// Compact context block for the system prompt (single line per entry).
    pub fn snapshot(&self) -> String {
        let mut out = String::from("## Execution memory\n");
        for e in &self.entries {
            out.push_str("- ");
            out.push_str(&e.summary);
            out.push('\n');
        }
        out
    }
}

fn summarize(tool: &str, args: &Value, result_text: &str, ok: bool) -> String {
    let status = if ok { "ok" } else { "error" };
    let target = describe_target(tool, args);
    let result = first_line(result_text).chars().take(120).collect::<String>();
    match (tool, ok) {
        ("read_file", true) => format!("read_file {target} → {} chars", result.chars().count()),
        ("write_file", true) => format!("write_file {target} → written"),
        ("run_command", o) => {
            let tail = if result.is_empty() || !o {
                String::new()
            } else {
                format!(": {result}")
            };
            format!("run_command `{target}` → {status}{tail}")
        }
        ("web_search", _o) => format!("web_search \"{target}\" → {status}"),
        ("web_fetch", _o) => format!("web_fetch {target} → {status}: {}", short(result)),
        ("semantic_search", _o) => format!("semantic_search \"{target}\" → {status}: {}", short(result)),
        _ => format!("{tool} {target} → {status}{}", if result.is_empty() { String::new() } else { format!(": {}", short(result)) }),
    }
}

fn describe_target(tool: &str, args: &Value) -> String {
    if tool == "run_command" {
        return args.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
    }
    for k in ["file_path", "path", "old_path", "new_path", "query", "url", "name"] {
        if let Some(v) = args.get(k).and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }
    args.to_string()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn short(s: String) -> String {
    if s.chars().count() > 80 {
        s.chars().take(80).collect::<String>() + "…"
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_and_snapshot() {
        let mut m = ExecMemory::default();
        m.record("read_file", &json!({"file_path": "/ws/a.rs"}), "fn main() {}", true);
        m.record("run_command", &json!({"command": "cargo test"}), "ok. 3 passed", true);
        assert_eq!(m.len(), 2);
        let snap = m.snapshot();
        assert!(snap.contains("read_file /ws/a.rs"));
        assert!(snap.contains("run_command `cargo test` → ok"));
    }

    #[test]
    fn dedups_identical_consecutive_calls() {
        let mut m = ExecMemory::default();
        m.record("read_file", &json!({"file_path": "/ws/a.rs"}), "same", true);
        m.record("read_file", &json!({"file_path": "/ws/a.rs"}), "same", true);
        assert_eq!(m.len(), 1);
        m.record("read_file", &json!({"file_path": "/ws/b.rs"}), "same", true);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn reports_errors() {
        let mut m = ExecMemory::default();
        m.record("run_command", &json!({"command": "rm -rf /etc"}), "denied by rules", false);
        let snap = m.snapshot();
        assert!(snap.contains("→ error"));
    }
}
