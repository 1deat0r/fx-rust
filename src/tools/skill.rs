//! Skill tool: list installed skills and read SKILL.md documents, backed by
//! the ported skills subsystem (`core/skills/*` — contract parsing,
//! multi-root discovery, catalog). Faithful to fx's
//! `tools/skills/skill.zig` + `tools/skills/install_skill.zig` surface.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{arg, ToolContext};
use crate::skills::{managed_root, read_skill_md, Registry, SKILL_FILE_BYTES_DEFAULT};

/// `skill` tool: list the catalog, or read a skill (optionally a resource).
pub fn skill(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let action = arg(args, "action").unwrap_or("list");
    match action {
        "list" => {
            let registry = Registry::discover(&ctx.workspace);
            let mut skills: Vec<Value> = registry
                .catalog()
                .skills
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "description": s.description,
                        "path": s.path.display().to_string(),
                        "source": s.source.label(),
                        "managed_install": s.managed_install,
                    })
                })
                .collect();
            skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(json!({ "skills": skills }))
        }
        "read" => {
            let name = arg(args, "name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
            let registry = Registry::discover(&ctx.workspace);
            let skill = registry
                .find(name)
                .ok_or_else(|| anyhow::anyhow!("skill `{name}` not found"))?;
            let resource = arg(args, "resource");
            let text = match resource {
                Some(res) => {
                    let data =
                        crate::skills::open_resource(&skill.path, res, SKILL_FILE_BYTES_DEFAULT)
                            .map_err(|e| anyhow!("skill `{name}`: {e}"))?;
                    String::from_utf8_lossy(&data).into_owned()
                }
                None => read_skill_md(&skill.path, SKILL_FILE_BYTES_DEFAULT)
                    .map_err(|e| anyhow!("skill `{name}`: {e}"))?,
            };
            Ok(json!({
                "name": skill.name,
                "description": skill.description,
                "content": ctx.truncate(&text),
                "path": skill.path.display().to_string(),
                "source": skill.source.label(),
            }))
        }
        other => bail!("unknown skill action: {other}"),
    }
}

/// `install_skill` tool: install from a local directory into the managed
/// root (`~/.fx/skills`), optionally filtered to one skill.
pub fn install_skill(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let source = arg(args, "source").ok_or_else(|| anyhow::anyhow!("missing `source`"))?;
    let src = ctx.resolve(source);
    if !src.join("SKILL.md").is_file() && !src.is_dir() {
        bail!(
            "source must be a directory containing SKILL.md: {}",
            src.display()
        );
    }
    let filter = arg(args, "skill").map(|s| s.to_string());
    let skills_dir = managed_root();
    let registry = Registry::discover(&ctx.workspace);
    let names = crate::skills::commands::install_from_source(
        &skills_dir,
        &ctx.workspace,
        &registry,
        &src.display().to_string(),
        filter.as_deref(),
    )?;
    if names.is_empty() {
        bail!("install_skill: no skills found in {}", src.display());
    }
    Ok(json!({
        "result": "ok",
        "installed": names,
        "path": skills_dir.display().to_string(),
    }))
}
