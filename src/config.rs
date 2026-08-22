//! Layered configuration, modeled after fx's precedence:
//! 1. command-line / process override  2. environment variable
//! 3. workspace entry in ~/.fx/settings.json  4. global entry in ~/.fx/settings.json
//! 5. <workspace>/.fx.json  6. built-in default
//!
//! Unknown JSON keys are ignored. Invalid values in known keys make a layer
//! unusable and produce a diagnostic (the layer is skipped).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::permissions::{PermissionMode, ToolRule};

pub const DEFAULT_MODEL: &str = "openai/gpt-5.4";
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 65536;
pub const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SettingsFile {
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Active mode id (`ask` | `code`; upstream builtins/modes.zig).
    pub mode: Option<String>,
    pub max_agent_steps: Option<usize>,
    pub max_tool_result_bytes: Option<usize>,
    pub first_call_tool_choice: Option<String>,
    pub context: Option<bool>,
    pub sandbox: Option<String>,
    pub input_appearance: Option<String>,
    pub presentation_mode: Option<String>,
    pub update_channel: Option<String>,
    pub permission: Option<BTreeMap<String, serde_json::Value>>,
    pub workspaces: Option<BTreeMap<String, WorkspaceFile>>,
    #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceFile {
    pub sandbox: Option<String>,
    pub input_appearance: Option<String>,
    pub presentation_mode: Option<String>,
    pub update_channel: Option<String>,
    pub permission: Option<BTreeMap<String, serde_json::Value>>,
    pub additional_directories: Option<Vec<String>>,
    #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// MCP transport, matching upstream fx's `McpTransport` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    /// Streamable HTTP transport (modern; `transport: "http"` / "streamable-http").
    Http,
    /// Legacy HTTP + SSE transport (`transport: "sse"` / "http-sse" / "streamable-http-sse").
    Sse,
}

impl McpTransport {
    /// Parse a config transport string. Missing/invalid values default to stdio,
    /// mirroring upstream's default `McpTransport = .stdio`.
    pub fn parse(s: Option<&str>) -> McpTransport {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("http") | Some("streamable-http") | Some("streamable_http") => McpTransport::Http,
            Some("sse") | Some("http-sse") | Some("http_sse") | Some("streamable-http-sse") => {
                McpTransport::Sse
            }
            _ => McpTransport::Stdio,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        }
    }
}

/// MCP OAuth/auth configuration, matching upstream fx's `McpAuthConfig`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct McpAuthConfig {
    pub resource: Option<String>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret_env: Option<String>,
    pub client_metadata_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// MCP server definition, matching upstream fx's McpServerConfig shape.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Additional headers pulled from the environment (`NAME` -> `${ENV_VAR}`).
    #[serde(default)]
    pub header_env: std::collections::BTreeMap<String, String>,
    /// Read a bearer token from the named environment variable.
    pub bearer_token_env: Option<String>,
    pub auth: Option<McpAuthConfig>,
    pub allow_stored_credentials: Option<bool>,
    pub required: Option<bool>,
    /// Explicitly disable a server without removing the config entry.
    pub enabled: Option<bool>,
    pub startup_timeout_ms: Option<u64>,
    pub operation_timeout_ms: Option<u64>,
}

impl McpServerConfig {
    pub fn transport_kind(&self) -> McpTransport {
        McpTransport::parse(self.transport.as_deref())
    }

    /// A configured-but-disabled server is skipped entirely.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn requires_remote_url(&self) -> bool {
        self.transport_kind() != McpTransport::Stdio
    }

    /// The starting `command` for stdio servers (null for remote transports).
    pub fn stdio_command(&self) -> Option<&str> {
        if self.transport_kind() != McpTransport::Stdio {
            return None;
        }
        self.command.as_deref().filter(|c| !c.is_empty())
    }

    /// The `url` for remote transports (null for stdio servers).
    pub fn remote_url(&self) -> Option<&str> {
        if self.transport_kind() == McpTransport::Stdio {
            return None;
        }
        self.url.as_deref().filter(|u| !u.is_empty())
    }

    /// Expand `${VAR}` / `$VAR` references in an env value against the process
    /// environment. Unknown variables expand to empty, matching common MCP
    /// config conventions (a literal-value escape is available as `$${...}`).
    pub fn expand_env_value(value: &str, environ: &dyn Fn(&str) -> Option<String>) -> String {
        let mut out = String::with_capacity(value.len());
        let rest = value;
        let mut i = 0;
        while i < rest.len() {
            if let Some(cur) = rest[i..].find('$') {
                out.push_str(&rest[i..i + cur]);
                let after = i + cur;
                if let Some(_rest2) = rest[after..].strip_prefix("$$") {
                    out.push('$');
                    i = after + 2;
                    continue;
                }
                if let Some(rest2) = rest[after..].strip_prefix("${") {
                    if let Some(end) = rest2.find('}') {
                        let name = &rest2[..end];
                        out.push_str(&environ(name).unwrap_or_default());
                        i = after + 2 + end + 1;
                        continue;
                    }
                    // Unterminated `${` — leave as-is.
                    out.push_str("${");
                    i = after + 2;
                    continue;
                }
                if let Some(rest2) = rest[after..].strip_prefix('$') {
                    if let Some(name) = take_var_name(rest2) {
                        out.push_str(&environ(name).unwrap_or_default());
                        i = after + 1 + name.len();
                        continue;
                    }
                    // Lone trailing '$'.
                    out.push('$');
                    i = after + 1;
                    continue;
                }
                // Not a variable (`$` mid-word followed by nothing) — keep.
                out.push('$');
                i = after + 1;
            } else {
                out.push_str(&rest[i..]);
                break;
            }
        }
        out
    }

    /// Resolve the effective env map after `${VAR}` expansion.
    pub fn resolved_env(&self) -> std::collections::BTreeMap<String, String> {
        self.env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    Self::expand_env_value(v, &|n| std::env::var(n).ok()),
                )
            })
            .collect()
    }

    /// Resolve header_env entries against the environment.
    pub fn resolved_header_env(&self) -> std::collections::BTreeMap<String, String> {
        self.header_env
            .iter()
            .filter_map(|(k, v)| std::env::var(v).ok().map(|val| (k.clone(), val)))
            .collect()
    }

    /// The bearer token, if configured, read from `bearer_token_env`.
    pub fn bearer_token(&self) -> Option<String> {
        self.bearer_token_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    }
}

fn take_var_name(s: &str) -> Option<&str> {
    let end = s
        .find(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some(&s[..end])
}

/// Repository-safe project config (`.fx.json`). Only public fields are accepted.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
    pub mcp_servers: Vec<McpServerConfig>,
    pub max_agent_steps: Option<usize>,
    pub max_tool_result_bytes: Option<usize>,
    pub context: Option<bool>,
    pub sandbox: Option<String>,
    pub additional_directories: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    None,
    Auto,
    Os,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstCallToolChoice {
    Auto,
    None,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Workspace the config was resolved for.
    pub workspace: std::path::PathBuf,
    pub model: String,
    /// Active mode id (`ask` default; resolves via `modes::builtin_registry`).
    pub mode: String,
    pub permission_mode: PermissionMode,
    pub max_agent_steps: usize,
    pub max_tool_result_bytes: usize,
    pub first_call_tool_choice: FirstCallToolChoice,
    pub context: bool,
    pub sandbox: SandboxMode,
    /// Effective tool rules after merging all layers.
    pub permission_rules: BTreeMap<String, ToolRule>,
    /// Storage the resolved settings file lives in (for /settings).
    pub settings_path: Option<PathBuf>,
    /// Extra directories tools may act on (workspace entry / .fx.json),
    /// used by the permission sandbox.
    pub additional_directories: Vec<PathBuf>,
    /// Merged MCP servers (workspace layer wins by name).
    pub mcp_servers: Vec<McpServerConfig>,
    /// Context accounting limits (warn / hard stop, in estimated tokens).
    pub context_limits: crate::context::ContextLimits,
    /// Input styling hint (`auto` | `light` | `dark`).
    pub input_appearance: String,
    /// Presentation mode (`default` | `full`).
    pub presentation_mode: String,
    /// Upgrade channel (`stable` | `beta`).
    pub update_channel: String,
    /// Optional subagent tool filter: when set, only these tool names are
    /// published to the model and executed (subagent admission authority).
    /// Never read from settings files; internal to the agent runtime.
    pub tool_filter: Option<Vec<String>>,
    /// Optional reasoning-effort override for subagent runs.
    pub reasoning_effort: Option<String>,
}

impl Config {
    /// The active [`crate::modes::ModeSpec`] for this config (builtins only
    /// at v0.0.5 parity; unknown ids fall back to the builtin default).
    pub fn active_mode(&self) -> crate::modes::ModeSpec {
        crate::modes::builtin_registry()
            .lookup(&self.mode)
            .cloned()
            .unwrap_or_else(|| {
                crate::modes::builtin_registry()
                    .lookup(crate::modes::DEFAULT_MODE_ID)
                    .cloned()
                    .expect("builtin default mode always exists")
            })
    }

    /// Effective permission gate: the active mode's pinned permission mode
    /// wins; a custom/unknown mode falls back to the explicit setting.
    pub fn effective_permission_mode(&self) -> PermissionMode {
        let registry = crate::modes::builtin_registry();
        match registry.lookup(&self.mode) {
            Some(m) => m.permission_mode,
            None => self.permission_mode,
        }
    }
}

pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn fx_home() -> PathBuf {
    // FX_HOME overrides the data dir (tests / sandboxed runs); default ~/.fx.
    std::env::var("FX_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            home_dir()
                .map(|h| h.join(".fx"))
                .unwrap_or_else(|| PathBuf::from(".fx"))
        })
}

pub fn settings_path() -> PathBuf {
    fx_home().join("settings.json")
}

fn parse_tool_rule(v: &serde_json::Value) -> Result<ToolRule> {
    match v {
        serde_json::Value::String(s) => Ok(ToolRule::Whole(crate::permissions::parse_rule(s)?)),
        serde_json::Value::Object(map) => {
            let mut patterns = Vec::new();
            for (k, val) in map {
                let rule = crate::permissions::parse_rule(
                    val.as_str()
                        .context("permission pattern value must be a string")?,
                )?;
                patterns.push((k.clone(), rule));
            }
            Ok(ToolRule::Patterns(patterns))
        }
        _ => bail!("permission rule must be a string or an object of patterns"),
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_SETTINGS_BYTES {
        eprintln!("fxrs: warning: {path:?} exceeds 64 KiB; ignoring layer");
        return Ok(None);
    }
    let data =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed =
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

/// Resolve the effective configuration for a workspace directory.
pub fn resolve(workspace: &Path) -> Result<Config> {
    // Layer 5: project .fx.json (public fields only).
    let project: ProjectConfig = read_json_file(&workspace.join(".fx.json"))?.unwrap_or_default();

    // Layers 3+4: user profile with per-workspace overrides.
    let settings: SettingsFile = read_json_file(&settings_path())?.unwrap_or_default();

    let workspace_key = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let ws_entry = settings
        .workspaces
        .as_ref()
        .and_then(|w| {
            w.iter()
                .find(|(k, _)| Path::new(k) == workspace || Path::new(k) == workspace_key)
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_default();

    // ---- fields with full precedence resolution ----

    let model = std::env::var("FX_MODEL")
        .ok()
        .flatten_nonempty()
        .or(settings.model)
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let permission_mode = std::env::var("FX_PERMISSION_MODE")
        .ok()
        .flatten_nonempty()
        .or(settings.permission_mode)
        .map(|m| PermissionMode::parse(&m))
        .transpose()?
        .unwrap_or(PermissionMode::Auto);

    let mode = std::env::var("FX_MODE")
        .ok()
        .flatten_nonempty()
        .or(settings.mode)
        .unwrap_or_else(|| crate::modes::DEFAULT_MODE_ID.to_string());

    let max_agent_steps = std::env::var("FX_MAX_AGENT_STEPS")
        .ok()
        .flatten_nonempty()
        .and_then(|v| v.parse::<usize>().ok())
        .or(settings.max_agent_steps)
        .or(project.max_agent_steps)
        .unwrap_or(0);

    let max_tool_result_bytes = settings
        .max_tool_result_bytes
        .or(project.max_tool_result_bytes)
        .unwrap_or(DEFAULT_MAX_TOOL_RESULT_BYTES)
        .max(1024);

    let first_call_tool_choice = match settings.first_call_tool_choice.as_deref().unwrap_or("auto")
    {
        "none" => FirstCallToolChoice::None,
        _ => FirstCallToolChoice::Auto,
    };

    let context = settings.context.or(project.context).unwrap_or(true);

    let sandbox = settings
        .sandbox
        .clone()
        .or(ws_entry.sandbox.clone())
        .or(project.sandbox.clone())
        .unwrap_or_else(|| "none".to_string());
    let sandbox = match sandbox.as_str() {
        "os" | "macos" => SandboxMode::Os,
        "auto" => SandboxMode::Auto,
        _ => SandboxMode::None,
    };

    let input_appearance = std::env::var("FX_INPUT_APPEARANCE")
        .ok()
        .flatten_nonempty()
        .or(settings.input_appearance)
        .unwrap_or_else(|| "auto".to_string());
    let presentation_mode = std::env::var("FX_PRESENTATION_MODE")
        .ok()
        .flatten_nonempty()
        .or(settings.presentation_mode)
        .unwrap_or_else(|| "default".to_string());
    let update_channel = std::env::var("FX_UPDATE_CHANNEL")
        .ok()
        .flatten_nonempty()
        .or(settings.update_channel)
        .unwrap_or_else(|| "stable".to_string());

    // ---- permission rules: merge global settings, then workspace overrides ----
    let mut permission_rules: BTreeMap<String, ToolRule> = BTreeMap::new();
    if let Some(map) = &settings.permission {
        for (k, v) in map {
            if let Ok(rule) = parse_tool_rule(v) {
                permission_rules.insert(k.clone(), rule);
            }
        }
    }
    if let Some(map) = &ws_entry.permission {
        for (k, v) in map {
            if let Ok(rule) = parse_tool_rule(v) {
                permission_rules.insert(k.clone(), rule);
            }
        }
    }

    let additional_directories: Vec<PathBuf> = std::env::var("FX_ADDITIONAL_DIRECTORIES")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(':').map(PathBuf::from).collect())
        .or_else(|| {
            // Layered: workspace/project settings first, then the persistent
            // per-workspace store from `fxrs workspace add` (env wins above).
            let mut dirs: Vec<PathBuf> = ws_entry
                .additional_directories
                .iter()
                .flatten()
                .map(PathBuf::from)
                .collect();
            if let Some(proj) = project.additional_directories {
                dirs.extend(proj.into_iter().map(PathBuf::from));
            }
            if let Ok(store) = crate::workspace_cli::WorkspaceDirsStore::open() {
                dirs.extend(store.dirs_for(workspace));
            }
            if dirs.is_empty() {
                None
            } else {
                Some(dirs)
            }
        })
        .unwrap_or_default();

    Ok(Config {
        workspace: workspace.to_path_buf(),
        model,
        mode,
        permission_mode,
        max_agent_steps,
        max_tool_result_bytes,
        first_call_tool_choice,
        context,
        sandbox,
        permission_rules,
        settings_path: Some(settings_path()),
        additional_directories,
        context_limits: crate::context::ContextLimits::from_env(),
        input_appearance,
        presentation_mode,
        update_channel,
        tool_filter: None,
        reasoning_effort: None,
        mcp_servers: merge_mcp_servers(
            &merge_mcp_servers(&settings.mcp_servers, &ws_entry.mcp_servers),
            &project.mcp_servers,
        ),
    })
}

/// Merge MCP server definitions: workspace entries override same-named
/// profile entries by name; order is stable (profile first, then workspace).
pub fn merge_mcp_servers(
    profile: &[McpServerConfig],
    workspace: &[McpServerConfig],
) -> Vec<McpServerConfig> {
    let mut out: Vec<McpServerConfig> = profile.to_vec();
    for ws in workspace {
        if let Some(existing) = out.iter_mut().find(|m| m.name == ws.name) {
            *existing = ws.clone();
        } else {
            out.push(ws.clone());
        }
    }
    out
}

/// Load project instructions (AGENTS.md / CLAUDE.md etc.) if `context` is enabled.
pub fn load_project_instructions(workspace: &Path) -> Vec<String> {
    const CANDIDATES: &[&str] = &[
        "AGENTS.md",
        "CLAUDE.md",
        "agent.md",
        ".agents.md",
        ".fx/AGENTS.md",
    ];
    let mut out = Vec::new();
    for name in CANDIDATES {
        let p = workspace.join(name);
        if p.is_file() {
            if let Ok(text) = std::fs::read_to_string(&p) {
                if !text.trim().is_empty() {
                    out.push(format!("# {name}\n{}", text.trim()));
                }
            }
        }
    }
    out
}

trait OptionStringExt {
    fn flatten_nonempty(self) -> Option<String>;
}
impl OptionStringExt for Option<String> {
    fn flatten_nonempty(self) -> Option<String> {
        self.filter(|s| !s.trim().is_empty())
    }
}
