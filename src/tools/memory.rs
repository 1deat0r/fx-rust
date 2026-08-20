//! Persistent key-value memory scoped to a workspace (mirrors fx's memory
//! tool shape). Stored as JSON at ~/.fx/memory.json.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ToolContext, arg};

const STORE_NAME: &str = "memory.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryStore {
    #[serde(default)]
    entries: BTreeMap<String, serde_json::Value>,
}

fn store_path() -> PathBuf {
    crate::config::fx_home().join(STORE_NAME)
}

fn workspace_prefix(ctx: &ToolContext) -> String {
    // Namespace by canonical workspace path so memories don't leak across repos.
    let ws = ctx.workspace.canonicalize().unwrap_or_else(|_| ctx.workspace.clone());
    format!("ws:{}", ws.to_string_lossy())
}

fn load() -> Result<MemoryStore> {
    let path = store_path();
    if !path.exists() {
        return Ok(MemoryStore::default());
    }
    let data = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&data).unwrap_or_default())
}

fn save(store: &MemoryStore) -> Result<()> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(store)?;
    fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn memory(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let action = arg(args, "action").unwrap_or("list");
    let mut store = load()?;
    let prefix = workspace_prefix(ctx);

    match action {
        "list" => {
            let keys: Vec<&String> = store
                .entries
                .keys()
                .filter(|k| k.starts_with(&format!("{prefix}:")))
                .collect();
            Ok(json!({
                "keys": keys.iter().map(|k| {
                    let key = k.strip_prefix(&format!("{prefix}:")).unwrap_or(k);
                    json!(key)
                }).collect::<Vec<_>>()
            }))
        }
        "read" => {
            let key = arg(args, "key").ok_or_else(|| anyhow::anyhow!("missing `key`"))?;
            let full = format!("{prefix}:{key}");
            match store.entries.get(&full) {
                Some(v) => Ok(json!({ "key": key, "value": v })),
                None => Ok(json!({ "key": key, "value": null, "found": false })),
            }
        }
        "write" => {
            let key = arg(args, "key").ok_or_else(|| anyhow::anyhow!("missing `key`"))?;
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            store.entries.insert(format!("{prefix}:{key}"), value.clone());
            save(&store)?;
            Ok(json!({ "result": "ok", "key": key, "value": value }))
        }
        "delete" => {
            let key = arg(args, "key").ok_or_else(|| anyhow::anyhow!("missing `key`"))?;
            let removed = store.entries.remove(&format!("{prefix}:{key}")).is_some();
            save(&store)?;
            Ok(json!({ "result": "ok", "removed": removed }))
        }
        other => anyhow::bail!("unknown memory action: {other}"),
    }
}
