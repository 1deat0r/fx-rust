//! Settings catalog (fx's `core/config/settings_catalog.zig`): the typed,
//! documented inventory of every known setting key, its env override, its
//! default, and how to render the effective configuration. Powering
//! `fxrs settings` and `/settings`.

use crate::config::{Config, SandboxMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    String,
    Number,
    Bool,
    Enum,
    PathList,
}

#[derive(Debug, Clone, Copy)]
pub struct SettingKey {
    pub name: &'static str,
    pub env: Option<&'static str>,
    pub kind: SettingKind,
    pub default: &'static str,
    pub description: &'static str,
}

pub fn catalog() -> &'static [SettingKey] {
    &[
        SettingKey {
            name: "model",
            env: Some("FX_MODEL"),
            kind: SettingKind::String,
            default: "openai/gpt-5.4",
            description: "model id (gateway / anthropic / openai-compatible)",
        },
        SettingKey {
            name: "permission_mode",
            env: Some("FX_PERMISSION_MODE"),
            kind: SettingKind::Enum,
            default: "auto",
            description: "ask | auto | yolo — gate for unresolved tool calls",
        },
        SettingKey {
            name: "max_agent_steps",
            env: Some("FX_MAX_AGENT_STEPS"),
            kind: SettingKind::Number,
            default: "0 (unlimited)",
            description: "maximum agent loop steps per turn",
        },
        SettingKey {
            name: "max_tool_result_bytes",
            env: None,
            kind: SettingKind::Number,
            default: "65536",
            description: "tool result text truncated to this many bytes",
        },
        SettingKey {
            name: "first_call_tool_choice",
            env: None,
            kind: SettingKind::Enum,
            default: "auto",
            description: "auto | none — whether the first model call may use tools",
        },
        SettingKey {
            name: "context",
            env: None,
            kind: SettingKind::Bool,
            default: "true",
            description: "load AGENTS.md / CLAUDE.md project instructions",
        },
        SettingKey {
            name: "sandbox",
            env: None,
            kind: SettingKind::Enum,
            default: "none",
            description: "none | auto | os — path confinement for file/shell writes",
        },
        SettingKey {
            name: "additional_directories",
            env: Some("FX_ADDITIONAL_DIRECTORIES"),
            kind: SettingKind::PathList,
            default: "[]",
            description: "extra dirs the sandbox may write into (colon/JSON list)",
        },
        SettingKey {
            name: "context_limit",
            env: Some("FX_CONTEXT_LIMIT"),
            kind: SettingKind::Number,
            default: "220000",
            description: "estimated token ceiling before the agent stops",
        },
        SettingKey {
            name: "context_warn_at",
            env: Some("FX_CONTEXT_WARN_AT"),
            kind: SettingKind::Number,
            default: "180000",
            description: "estimated tokens at which a context warning is printed",
        },
        SettingKey {
            name: "input_appearance",
            env: Some("FX_INPUT_APPEARANCE"),
            kind: SettingKind::Enum,
            default: "auto",
            description: "auto | light | dark — input styling hint",
        },
        SettingKey {
            name: "presentation_mode",
            env: Some("FX_PRESENTATION_MODE"),
            kind: SettingKind::Enum,
            default: "default",
            description: "default | full — presentation mode",
        },
        SettingKey {
            name: "update_channel",
            env: Some("FX_UPDATE_CHANNEL"),
            kind: SettingKind::Enum,
            default: "stable",
            description: "stable | beta — upgrade channel",
        },
        SettingKey {
            name: "mcpServers",
            env: None,
            kind: SettingKind::String,
            default: "[]",
            description: "MCP stdio server definitions",
        },
    ]
}

pub fn known_keys() -> Vec<&'static str> {
    catalog().iter().map(|k| k.name).collect()
}

/// Render the effective configuration (with resolved values) for display.
pub fn render(cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str("effective configuration\n");
    out.push_str("----------------------\n");
    for k in catalog() {
        let value = resolved(cfg, k.name);
        out.push_str(&format!(
            "  {:<26} {:<45} {}\n",
            k.name,
            value,
            k.env.map(|e| format!("({e})")).unwrap_or_default()
        ));
    }
    out
}

fn resolved(cfg: &Config, name: &str) -> String {
    match name {
        "model" => cfg.model.clone(),
        "permission_mode" => cfg.permission_mode.to_string(),
        "max_agent_steps" => cfg.max_agent_steps.to_string(),
        "max_tool_result_bytes" => cfg.max_tool_result_bytes.to_string(),
        "first_call_tool_choice" => format!("{:?}", cfg.first_call_tool_choice).to_lowercase(),
        "context" => cfg.context.to_string(),
        "sandbox" => match cfg.sandbox {
            SandboxMode::None => "none".into(),
            SandboxMode::Auto => "auto".into(),
            SandboxMode::Os => "os".into(),
        },
        "additional_directories" => {
            if cfg.additional_directories.is_empty() {
                "[]".into()
            } else {
                cfg.additional_directories
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            }
        }
        "context_limit" => cfg.context_limits.max_tokens.to_string(),
        "context_warn_at" => cfg.context_limits.warn_at_tokens.to_string(),
        "input_appearance" => cfg.input_appearance.clone(),
        "presentation_mode" => cfg.presentation_mode.clone(),
        "update_channel" => cfg.update_channel.clone(),
        "mcpServers" => format!("[{} servers]", cfg.mcp_servers.len()),
        _ => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionMode;

    fn cfg() -> Config {
        Config {
            mode: "ask".into(),
            workspace: "/ws".into(),
            model: "m".into(),
            permission_mode: PermissionMode::Auto,
            max_agent_steps: 0,
            max_tool_result_bytes: 65536,
            first_call_tool_choice: crate::config::FirstCallToolChoice::Auto,
            context: true,
            sandbox: SandboxMode::None,
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
        }
    }

    #[test]
    fn catalog_is_ordered_and_unique() {
        let names = known_keys();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(names.len(), uniq.len());
    }

    #[test]
    fn render_contains_resolved_values() {
        let out = render(&cfg());
        assert!(out.contains("model"));
        assert!(out.contains("220000"));
        assert!(out.contains("auto"));
    }
}
