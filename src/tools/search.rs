//! `semantic_search` tool: ranked keyword search over workspace text files.
//! Uses a BM25-lite scorer (no embedding dependency) over locally indexed
//! files — a pragmatic stand-in for the embeddings backend upstream can use.
//! Scope and honesty: lexical, not neural; good for "find the code that…".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use super::{arg, ToolContext};

const MAX_FILES: usize = 5000;
const MAX_BYTES_PER_FILE: usize = 512 * 1024;
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".fx",
    "target",
    "node_modules",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    ".next",
    ".cache",
    "coverage",
];

fn is_text_ext(name: &str) -> bool {
    let Some(ext) = name.rsplit('.').next() else {
        return false;
    };
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "java"
            | "kt"
            | "rb"
            | "php"
            | "swift"
            | "sh"
            | "zsh"
            | "bash"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "md"
            | "txt"
            | "html"
            | "css"
            | "scss"
            | "sql"
            | "vue"
            | "svelte"
            | "zig"
            | "lua"
            | "r"
            | "dart"
            | "cs"
            | "scala"
            | "ex"
            | "exs"
            | "erl"
            | "fs"
            | "ml"
    )
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c.to_ascii_lowercase());
            if cur.len() > 40 {
                out.push(std::mem::take(&mut cur));
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

struct Doc {
    path: String,
    terms: HashMap<String, usize>, // term -> freq
    len: usize,
    text: String,
}

fn walk_files(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_FILES {
                return out;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIRS.contains(&name) {
                    continue;
                }
                stack.push(path);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_text_ext(name) {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// BM25-lite score (k1 = 1.2, b = 0.75) of `query` against `docs`.
fn rank(docs: &[Doc], query: &[String], avg_len: f64) -> Vec<(usize, f64)> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let n = docs.len() as f64;
    let mut scored = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        let mut score = 0.0;
        for qt in query {
            let df = docs.iter().filter(|d| d.terms.contains_key(qt)).count() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.0);
            let tf = *doc.terms.get(qt).unwrap_or(&0) as f64;
            if tf == 0.0 {
                continue;
            }
            let len_norm = 1.0 - B + B * (doc.len as f64 / avg_len.max(1.0));
            score += idf * (tf * (K1 + 1.0)) / (tf + K1 * len_norm);
        }
        scored.push((i, score));
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

fn snippet(doc_text: &str, query: &[String], max: usize) -> String {
    let lowercase = doc_text.to_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for line in doc_text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let ll = l.to_lowercase();
        let has = query.iter().any(|q| ll.contains(q.as_str()));
        if has {
            let pos = lowercase.find(&l.to_lowercase()).unwrap_or(0);
            if best.map(|(bp, _)| pos < bp).unwrap_or(true) {
                best = Some((pos, line));
            }
        }
    }
    match best {
        Some((_, line)) => {
            let line = line.trim();
            if line.chars().count() <= max {
                line.to_string()
            } else {
                format!("{}…", line.chars().take(max).collect::<String>())
            }
        }
        None => {
            let head = doc_text.trim();
            if head.chars().count() <= max {
                head.to_string()
            } else {
                format!("{}…", head.chars().take(max).collect::<String>())
            }
        }
    }
}

pub fn semantic_search(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let query = arg(args, "query").unwrap_or("").trim().to_string();
    if query.is_empty() {
        return Ok(json!({ "error": "semantic_search requires a query" }));
    }
    let max_results = arg(args, "max_results")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5)
        .clamp(1, 10);
    let qt = tokenize(&query);
    if qt.is_empty() {
        return Ok(json!({ "results": [], "note": "query had no indexable tokens" }));
    }
    let root = ctx.workspace.clone();
    let files = walk_files(&root);
    let mut docs: Vec<Doc> = Vec::new();
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_BYTES_PER_FILE as u64 {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        if data.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        let tokens = tokenize(&text);
        if tokens.is_empty() {
            continue;
        }
        let mut terms: HashMap<String, usize> = HashMap::new();
        for t in &tokens {
            *terms.entry(t.clone()).or_insert(0) += 1;
        }
        let rel = path.strip_prefix(&root).unwrap_or(&path);
        docs.push(Doc {
            path: rel.display().to_string(),
            terms,
            len: tokens.len(),
            text,
        });
    }
    if docs.is_empty() {
        return Ok(json!({ "results": [], "query": query }));
    }
    let avg_len = docs.iter().map(|d| d.len as f64).sum::<f64>() / docs.len() as f64;
    let scored = rank(&docs, &qt, avg_len);
    let results: Vec<Value> = scored
        .into_iter()
        .take(max_results)
        .filter(|(_, s)| *s > 0.0)
        .map(|(i, score)| {
            let d = &docs[i];
            json!({
                "path": d.path,
                "score": (score * 1000.0).round() / 1000.0,
                "snippet": snippet(&d.text, &qt, 220),
            })
        })
        .collect();
    Ok(json!({ "results": results, "query": query }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx(ws: &str) -> ToolContext {
        ToolContext {
            workspace: ws.into(),
            max_result_bytes: 65536,
            interactive: false,
            session_id: String::new(),
            config: Arc::new(crate::config::Config {
                mode: "ask".into(),
                workspace: ws.into(),
                model: "m".into(),
                permission_mode: crate::permissions::PermissionMode::Auto,
                max_agent_steps: 0,
                max_tool_result_bytes: 65536,
                first_call_tool_choice: crate::config::FirstCallToolChoice::Auto,
                context: true,
                sandbox: crate::config::SandboxMode::None,
                permission_rules: Default::default(),
                settings_path: None,
                additional_directories: vec![],
                mcp_servers: vec![],
                context_limits: crate::context::ContextLimits::default(),
                input_appearance: "auto".into(),
                presentation_mode: "default".into(),
                update_channel: "stable".into(),
                tool_filter: None,
                reasoning_effort: None,
            }),
            store: crate::sessions::SessionStore::new().unwrap(),
        }
    }

    fn write(path: &std::path::Path, s: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, s).unwrap();
    }

    #[test]
    fn finds_relevant_file_and_snippet() {
        let dir = std::env::temp_dir().join(format!("fxrs-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir.join("src/main.rs"),
            "fn main() { println!(\"hello world\"); }",
        );
        write(
            &dir.join("src/util.rs"),
            "pub fn parse_config(cfg: &str) -> u32 { 42 }",
        );
        write(
            &dir.join("README.md"),
            "# project\nnothing here about parsing.",
        );
        let c = ctx(&dir.display().to_string());
        let res = semantic_search(&c, &json!({"query": "parse cfg", "max_results": 3})).unwrap();
        let results = res["results"].as_array().expect("results array");
        assert!(!results.is_empty());
        let top = results[0]["path"].as_str().unwrap();
        assert!(top.contains("util.rs"), "expected util.rs, got {top}");
        let snip = results[0]["snippet"].as_str().unwrap_or("");
        assert!(
            snip.contains("parse_config"),
            "snippet should show the hit: {snip}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokenize_is_lowercased() {
        assert_eq!(
            tokenize("FooBar Baz_1!"),
            vec!["foobar".to_string(), "baz_1".to_string()]
        );
    }
}
