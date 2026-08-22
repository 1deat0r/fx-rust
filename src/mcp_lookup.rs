//! `fxrs mcp-lookup <query>` — search for an MCP server by name/description.
//! Tries the public fx registry first, then a local registry fixture, and
//! always degrades gracefully (exit 0) when the network is unavailable.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use crate::config::fx_home;

const REGISTRY_URL: &str = "https://registry.fx.sh/api/servers?query=";

pub fn run_mcp_lookup(args: &[String]) -> Result<i32> {
    let wants_json = args.iter().any(|a| a == "--json");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let query = match positional.first() {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => {
            eprintln!("usage: fxrs mcp-lookup <query> [--json]");
            return Ok(2);
        }
    };

    if let Some(results) = fetch_registry(&query) {
        render(&results, &query, wants_json);
        return Ok(0);
    }

    // Offline fallback: a locally maintained registry fixture.
    let local = fx_home().join("mcp-registry.json");
    if let Ok(raw) = std::fs::read_to_string(&local) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            let results = filter_local(&v, &query);
            render(&results, &query, wants_json);
            if !wants_json {
                eprintln!("fxrs mcp-lookup: offline fallback ({})", local.display());
            }
            return Ok(0);
        }
    }

    eprintln!(
        "fxrs mcp-lookup: registry unreachable and no local registry at {}",
        local.display()
    );
    Ok(0)
}

fn fetch_registry(query: &str) -> Option<Vec<Value>> {
    let url = format!("{REGISTRY_URL}{}", urlencode(query));
    let req = ureq::get(&url).timeout(std::time::Duration::from_secs(6));
    let resp = req.call().ok()?;
    let body: String = resp.into_string().ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    // Accept {servers: [...]} or a bare array.
    let arr = v
        .get("servers")
        .cloned()
        .or_else(|| v.get("results").cloned())
        .unwrap_or(v);
    arr.as_array().cloned()
}

fn filter_local(v: &Value, query: &str) -> Vec<Value> {
    let q = query.to_lowercase();
    match v.as_array() {
        Some(arr) => arr
            .iter()
            .filter(|row| {
                let hay = row.to_string().to_lowercase();
                hay.contains(&q)
            })
            .cloned()
            .collect(),
        None => Vec::new(),
    }
}

fn render(results: &[Value], query: &str, wants_json: bool) {
    if wants_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "query": query,
                "results": results,
            }))
            .unwrap_or_else(|_| "[]".to_string())
        );
        return;
    }
    if results.is_empty() {
        println!("no MCP servers found for `{query}`");
        return;
    }
    for r in results {
        let name = r
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed)");
        let desc = r
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect::<String>();
        let id = r
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| r.get("key").and_then(|v| v.as_str()))
            .unwrap_or("");
        println!("{name}");
        if !id.is_empty() {
            println!("  id: {id}");
        }
        if !desc.is_empty() {
            println!("  {desc}");
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[allow(dead_code)]
fn registry_cache_path() -> PathBuf {
    fx_home().join("mcp-registry.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_spaces_and_slashes() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a/b?c"), "a%2Fb%3Fc");
    }

    #[test]
    fn filter_local_matches_name() {
        let v: Value = serde_json::json!([
            {"name": "github", "description": "Repo tools"},
            {"name": "slack", "description": "Messages"}
        ]);
        let out = filter_local(&v, "git");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "github");
    }
}
