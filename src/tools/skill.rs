//! Skill tool: list installed skills and read SKILL.md documents.
//! Skills live in ~/.fx/skills/<name>/SKILL.md and
//! <workspace>/.fx/skills/<name>/SKILL.md.

use std::fs;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{arg, ToolContext};

pub fn skill(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let action = arg(args, "action").unwrap_or("list");
    match action {
        "list" => {
            let mut skills = Vec::new();
            for dir in skill_dirs(ctx) {
                if !dir.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let skill_md = entry.path().join("SKILL.md");
                    let description = fs::read_to_string(&skill_md)
                        .ok()
                        .and_then(|t| extract_description(&t))
                        .unwrap_or_default();
                    skills.push(json!({ "name": name, "description": description }));
                }
            }
            skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(json!({ "skills": skills }))
        }
        "read" => {
            let name = arg(args, "name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
            for dir in skill_dirs(ctx) {
                let p = dir.join(name).join("SKILL.md");
                if p.is_file() {
                    let text = fs::read_to_string(&p)
                        .with_context(|| format!("reading {}", p.display()))?;
                    return Ok(json!({
                        "name": name,
                        "content": ctx.truncate(&text),
                        "path": p.display().to_string(),
                    }));
                }
            }
            bail!("skill `{name}` not found in ~/.fx/skills or <workspace>/.fx/skills")
        }
        other => bail!("unknown skill action: {other}"),
    }
}

pub fn install_skill(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let source = arg(args, "source").ok_or_else(|| anyhow::anyhow!("missing `source`"))?;
    let src = ctx.resolve(source);
    if !src.join("SKILL.md").is_file() {
        bail!(
            "source must be a directory containing SKILL.md: {}",
            src.display()
        );
    }
    let name = arg(args, "name").map(|n| n.to_string()).unwrap_or_else(|| {
        src.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".into())
    });
    let dest = ctx.workspace.join(".fx").join("skills").join(&name);
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    copy_dir(&src, &dest)?;
    Ok(json!({
        "result": "ok",
        "name": name,
        "path": dest.display().to_string(),
    }))
}

fn skill_dirs(ctx: &ToolContext) -> Vec<std::path::PathBuf> {
    vec![
        crate::config::fx_home().join("skills"),
        ctx.workspace.join(".fx").join("skills"),
    ]
}

fn extract_description(text: &str) -> Option<String> {
    let front = text.lines().take(15).collect::<Vec<_>>().join("\n");
    let desc_start = front.find("description:")?;
    let after = &front[desc_start + "description:".len()..];
    let desc = after
        .lines()
        .next()?
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    if desc.is_empty() {
        None
    } else {
        Some(desc.to_string())
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
