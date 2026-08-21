//! File tools: read, write, edit, delete, rename, copy, create_folder,
//! file_info, glob_files, grep_files, list_files, open_file.

use std::fs;

use anyhow::{bail, Context, Result};
use globset::Glob;
use regex::Regex;
use serde_json::{json, Value};

use super::{arg, path_arg, ToolContext};

// ---------------------------------------------------------------- read_file
pub fn read_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "file_path")?;
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args.get("limit").and_then(|v| v.as_i64());

    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        if lineno < offset {
            continue;
        }
        if let Some(l) = limit {
            if lineno >= offset + l as usize {
                break;
            }
        }
        out.push_str(&format!("{lineno:>6} | {line}\n"));
    }
    if offset > 1 {
        out = format!("… (showing from line {offset})\n{out}");
    }
    Ok(json!({
        "path": path.display().to_string(),
        "content": ctx.truncate(&out),
        "bytes": text.len(),
    }))
}

// ---------------------------------------------------------------- write_file
pub fn write_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "file_path")?;
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument `content`"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {}", path.display()))?;
    }
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(json!({
        "result": "ok",
        "path": path.display().to_string(),
        "bytes": content.len()
    }))
}

// ---------------------------------------------------------------- edit_file
pub fn edit_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "file_path")?;
    let old = args
        .get("old_string")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument `old_string`"))?;
    let new = args
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if !replace_all {
        let matches = text.matches(old).count();
        if matches == 0 {
            bail!(
                "old_string not found in {} — the file may have changed ({} bytes). Use read_file to check current content.",
                path.display(), text.len()
            );
        }
        if matches > 1 {
            bail!(
                "old_string matches {matches} times in {} — pass replace_all or make old_string unique",
                path.display()
            );
        }
        let updated = text.replace(old, new);
        fs::write(&path, &updated).with_context(|| format!("writing {}", path.display()))?;
        Ok(
            json!({ "result": "ok", "path": path.display().to_string(), "replaced": matches, "bytes": updated.len() }),
        )
    } else {
        let count = text.matches(old).count();
        let updated = text.replace(old, new);
        fs::write(&path, &updated).with_context(|| format!("writing {}", path.display()))?;
        Ok(
            json!({ "result": "ok", "path": path.display().to_string(), "replaced": count, "bytes": updated.len() }),
        )
    }
}

// ---------------------------------------------------------------- delete/rename/copy/folder
pub fn delete_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "file_path")?;
    if !path.exists() {
        bail!("no such file: {}", path.display());
    }
    fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    Ok(json!({ "result": "ok", "path": path.display().to_string() }))
}

pub fn rename_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let from = path_arg(ctx, args, "file_path")?;
    let to_raw = arg(args, "new_path").ok_or_else(|| anyhow::anyhow!("missing `new_path`"))?;
    let to = ctx.resolve(to_raw);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {}", to.display()))?;
    }
    fs::rename(&from, &to)
        .with_context(|| format!("renaming {} -> {}", from.display(), to.display()))?;
    Ok(
        json!({ "result": "ok", "from": from.display().to_string(), "to": to.display().to_string() }),
    )
}

pub fn copy_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let from = path_arg(ctx, args, "file_path")?;
    let to_raw =
        arg(args, "destination").ok_or_else(|| anyhow::anyhow!("missing `destination`"))?;
    let to = ctx.resolve(to_raw);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {}", to.display()))?;
    }
    fs::copy(&from, &to)
        .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    Ok(
        json!({ "result": "ok", "from": from.display().to_string(), "to": to.display().to_string() }),
    )
}

pub fn create_folder(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "folder_path")?;
    fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(json!({ "result": "ok", "path": path.display().to_string() }))
}

// ---------------------------------------------------------------- open_file
pub fn open_file(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "file_path")?;
    if !path.exists() {
        bail!("no such file: {}", path.display());
    }
    open_path(&path)?;
    Ok(json!({ "result": "opened", "path": path.display().to_string() }))
}

#[cfg(target_os = "macos")]
fn open_path(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("open").arg(path).spawn()?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn open_path(path: &std::path::Path) -> Result<()> {
    // Best effort: xdg-open (reveal in file manager).
    let _ = std::process::Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_path(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- file_info
pub fn file_info(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let path = path_arg(ctx, args, "path")?;
    let meta = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "exists": true,
        "is_file": meta.is_file(),
        "is_dir": meta.is_dir(),
        "size": meta.len(),
        "modified_ms": meta.modified().map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)).unwrap_or(0),
    }))
}

// ---------------------------------------------------------------- list_files
pub fn list_files(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let dir = match arg(args, "path") {
        Some(p) => ctx.resolve(p),
        None => ctx.workspace.clone(),
    };
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = entry.file_type()?;
        let meta = entry.metadata().ok();
        entries.push(json!({
            "name": name,
            "type": if ft.is_dir() { "dir" } else if ft.is_symlink() { "symlink" } else { "file" },
            "size": meta.map(|m| m.len()).unwrap_or(0),
        }));
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({ "path": dir.display().to_string(), "entries": entries }))
}

// ---------------------------------------------------------------- glob_files
pub fn glob_files(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let pattern = arg(args, "pattern").ok_or_else(|| anyhow::anyhow!("missing `pattern`"))?;
    let base = match arg(args, "path") {
        Some(p) => ctx.resolve(p),
        None => ctx.workspace.clone(),
    };
    let glob = Glob::new(pattern).map_err(|e| anyhow::anyhow!("invalid glob `{pattern}`: {e}"))?;
    let matcher = glob.compile_matcher();

    let mut matched = Vec::new();
    walk(&base, &base, &matcher, &mut matched, 0)?;
    matched.sort();
    Ok(json!({ "pattern": pattern, "base": base.display().to_string(), "matches": matched }))
}

fn walk(
    base: &std::path::Path,
    dir: &std::path::Path,
    matcher: &globset::GlobMatcher,
    out: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > 24 {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if (rel == ".git"
            || rel == "node_modules"
            || rel == "target"
            || rel == "vendor"
            || rel == ".venv")
            && path.is_dir()
        {
            continue;
        }
        if matcher.is_match(&rel) || matcher.is_match(path.to_string_lossy().to_string()) {
            out.push(rel.clone());
        }
        if path.is_dir() {
            walk(base, &path, matcher, out, depth + 1)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- grep_files
pub fn grep_files(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let pattern = arg(args, "pattern").ok_or_else(|| anyhow::anyhow!("missing `pattern`"))?;
    let re = Regex::new(pattern).map_err(|e| anyhow::anyhow!("invalid regex `{pattern}`: {e}"))?;
    let base = match arg(args, "path") {
        Some(p) => ctx.resolve(p),
        None => ctx.workspace.clone(),
    };
    let glob_filter =
        arg(args, "glob").and_then(|g| Glob::new(g).ok().map(|g| g.compile_matcher()));
    let output_mode = arg(args, "output_mode").unwrap_or("content");

    let mut results = Vec::new();
    let mut count_total = 0usize;
    grep_walk(
        &base,
        &base,
        &re,
        &glob_filter,
        &mut results,
        &mut count_total,
        0,
    )?;

    let summary = json!({
        "pattern": pattern,
        "base": base.display().to_string(),
        "mode": output_mode,
        "matches": results,
        "files": results.len(),
    });
    Ok(summary)
}

fn grep_walk(
    base: &std::path::Path,
    dir: &std::path::Path,
    re: &Regex,
    glob_filter: &Option<globset::GlobMatcher>,
    out: &mut Vec<Value>,
    _count: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > 24 {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".git" | "node_modules" | "target" | "vendor" | ".venv" | ".fx"
            ) {
                continue;
            }
            grep_walk(base, &path, re, glob_filter, out, _count, depth + 1)?;
            continue;
        }
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if let Some(gm) = glob_filter {
            if !gm.is_match(&rel) {
                continue;
            }
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push(json!({
                    "file": rel,
                    "line": i + 1,
                    "content": truncate_line(line)
                }));
                *_count += 1;
            }
        }
    }
    Ok(())
}

fn truncate_line(s: &str) -> String {
    if s.len() > 300 {
        let mut out: String = s.chars().take(300).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}
