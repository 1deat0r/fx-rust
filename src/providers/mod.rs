//! Model providers, modeled after fx's model-agnostic design.
//!
//! Supported kinds:
//!   gateway   — OpenAI-compatible Chat Completions against an AI Gateway
//!               (default: https://gateway.vercel.ai). Credential: AI_GATEWAY_API_KEY.
//!   anthropic — Anthropic Messages API. Credential: ANTHROPIC_API_KEY.
//!   openai    — any OpenAI-compatible endpoint (local model servers, etc.).
//!               Base: AI_BASE_URL, optional key AI_API_KEY.
//!
//! Streaming uses Server-Sent Events with incremental text deltas and
//! tool-call accumulation; both wire formats normalize into `StreamEvent`.

pub mod anthropic;
pub mod gateway;
pub mod sse;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Gateway,
    Anthropic,
    OpenAi,
}

impl ProviderKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ContentBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        content: String,
    },
    /// Base64-encoded image attached to a user turn (vision).
    Image {
        media_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_text(text)
    }
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::tool_results(vec![(id.into(), content.into())])
    }
    pub fn role_str(&self) -> &str {
        &self.role
    }
    pub fn last_text(&self) -> Option<String> {
        if let Some(ContentBlock::Text(t)) = self.content.last() {
            Some(t.clone())
        } else {
            None
        }
    }
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some(ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::to_string(input).unwrap_or_default(),
                }),
                _ => None,
            })
            .collect()
    }
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: vec![ContentBlock::Text(text.into())],
        }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: vec![ContentBlock::Text(text.into())],
        }
    }
    pub fn assistant_tool_calls(calls: Vec<(String, String, Value)>) -> Self {
        Self {
            role: "assistant".into(),
            content: calls
                .into_iter()
                .map(|(id, name, input)| ContentBlock::ToolUse { id, name, input })
                .collect(),
        }
    }
    pub fn tool_results(results: Vec<(String, String)>) -> Self {
        Self {
            role: "user".into(),
            content: results
                .into_iter()
                .map(|(id, content)| ContentBlock::ToolResult { id, content })
                .collect(),
        }
    }

    /// Tool result with an image attached (vision tools like view_image).
    pub fn tool_with_image(id: String, content: String, media_type: String, data: String) -> Self {
        Self {
            role: "user".into(),
            content: vec![
                ContentBlock::ToolResult { id, content },
                ContentBlock::Image { media_type, data },
            ],
        }
    }
    pub fn plain_text(&self) -> Option<&str> {
        match self.content.first() {
            Some(ContentBlock::Text(t)) => Some(t),
            _ => None,
        }
    }
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text(t) = block {
                out.push_str(t);
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    /// Model chain-of-thought / reasoning text. Displayed as a subtle
    /// indicator (or traced to stderr); never counted as answer text.
    ReasoningDelta(String),
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallArgDelta {
        index: usize,
        delta: String,
    },
    ToolCallDone {
        index: usize,
        id: String,
        name: String,
        input: Value,
    },
    Finish,
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
}

pub type EventStream = futures_util::stream::BoxStream<'static, Result<StreamEvent>>;

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// Resolve provider kind/endpoint from configuration + environment.
pub fn resolve_provider(cfg: &Config) -> Result<ProviderConfig> {
    let model = cfg.model.clone();

    if let Ok(kind) = std::env::var("FX_PROVIDER") {
        let kind = match kind.as_str() {
            "gateway" => ProviderKind::Gateway,
            "anthropic" => ProviderKind::Anthropic,
            "openai" | "local" => ProviderKind::OpenAi,
            other => bail!("unknown FX_PROVIDER: {other} (expected gateway, anthropic, or openai)"),
        };
        let (base_url, api_key) = match kind {
            ProviderKind::Gateway => (
                std::env::var("FX_GATEWAY_BASE_URL")
                    .ok()
                    .or_else(|| std::env::var("AI_GATEWAY_BASE_URL").ok()),
                std::env::var("AI_GATEWAY_API_KEY")
                    .ok()
                    .or_else(|| std::env::var("FX_GATEWAY_API_KEY").ok()),
            ),
            ProviderKind::Anthropic => (
                std::env::var("ANTHROPIC_BASE_URL").ok(),
                std::env::var("ANTHROPIC_API_KEY").ok(),
            ),
            ProviderKind::OpenAi => (
                std::env::var("AI_BASE_URL").ok(),
                std::env::var("AI_API_KEY").ok(),
            ),
        };
        return Ok(ProviderConfig {
            provider: kind,
            model,
            base_url,
            api_key,
        });
    }

    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let gateway_key = std::env::var("AI_GATEWAY_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let base = std::env::var("AI_BASE_URL").ok().filter(|s| !s.is_empty());

    let is_anthropic_model = model.starts_with("anthropic/") || model.starts_with("claude-");
    if anthropic_key.is_some() && is_anthropic_model {
        return Ok(ProviderConfig {
            provider: ProviderKind::Anthropic,
            model,
            base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
            api_key: anthropic_key,
        });
    }
    if let Some(key) = gateway_key {
        return Ok(ProviderConfig {
            provider: ProviderKind::Gateway,
            model,
            base_url: std::env::var("FX_GATEWAY_BASE_URL")
                .ok()
                .or_else(|| std::env::var("AI_GATEWAY_BASE_URL").ok()),
            api_key: Some(key),
        });
    }
    if base.is_some() {
        return Ok(ProviderConfig {
            provider: ProviderKind::OpenAi,
            model,
            base_url: base,
            api_key: std::env::var("AI_API_KEY").ok().filter(|s| !s.is_empty()),
        });
    }

    // Credential fallback: fxrs auth store (~/.fx/auth.json).
    let store = crate::auth::load().unwrap_or_default();
    let provider_key = if is_anthropic_model {
        Some("anthropic")
    } else if gateway_key.is_some() || base.is_some() {
        None
    } else {
        Some("gateway")
    };
    if let Some(pk) = provider_key {
        let (key, base_url) = crate::auth::resolve_key(pk, &store);
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            let kind = if pk == "anthropic" {
                ProviderKind::Anthropic
            } else if pk == "openai" {
                ProviderKind::OpenAi
            } else {
                ProviderKind::Gateway
            };
            return Ok(ProviderConfig {
                provider: kind,
                model,
                base_url,
                api_key: Some(key),
            });
        }
    }

    bail!(
        "no model credentials found.\n\
         Configure one of:\n\
         \x20 AI_GATEWAY_API_KEY — Vercel AI Gateway key (fxrs setup, or `fxrs auth add gateway`)\n\
         \x20 ANTHROPIC_API_KEY — Anthropic API key (or `fxrs auth add anthropic`)\n\
         \x20 AI_BASE_URL — any OpenAI-compatible endpoint (optionally AI_API_KEY)\n\
         Model: {model}"
    )
}

/// Stream a completion, normalizing wire events into `StreamEvent`.
pub fn stream(
    p: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[Value]>,
    system: &str,
    max_tokens: Option<u32>,
) -> EventStream {
    match p.provider {
        ProviderKind::Gateway | ProviderKind::OpenAi => {
            gateway::stream(p, messages, tools, system, max_tokens)
        }
        ProviderKind::Anthropic => anthropic::stream(p, messages, tools, system, max_tokens),
    }
}

/// Run a single streaming completion and return (text, tool calls, usage).
/// `tools` is the JSON schema array; pass None to disable tool use.
pub async fn chat(
    p: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[Value]>,
    system: &str,
    max_tokens: Option<u32>,
) -> Result<(String, Vec<(String, String, Value)>, Usage)> {
    use futures_util::StreamExt;
    let mut text = String::new();
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    let mut usage = Usage::default();

    let mut events = stream(p, messages, tools, system, max_tokens);
    while let Some(ev) = events.next().await {
        match ev? {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::ReasoningDelta(_) => {} // reasoning never enters chat() output
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                pending.push((index, id, name, String::new()));
            }
            StreamEvent::ToolCallArgDelta { index, delta } => {
                if let Some(c) = pending.iter_mut().find(|c| c.0 == index) {
                    c.3.push_str(&delta);
                }
            }
            StreamEvent::ToolCallDone {
                index,
                id,
                name,
                input,
            } => {
                if let Some(c) = pending.iter_mut().find(|c| c.0 == index) {
                    c.1 = id;
                    c.2 = name;
                    c.3 = serde_json::to_string(&input).unwrap_or_default();
                }
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                usage.input_tokens = input_tokens.unwrap_or(0);
                usage.output_tokens = output_tokens.unwrap_or(0);
            }
            StreamEvent::Finish => {}
        }
    }

    let mut tool_calls = Vec::new();
    for (_, id, name, arg_json) in pending {
        let input: Value = serde_json::from_str(&arg_json).unwrap_or(Value::Null);
        if input.is_null() && !arg_json.trim().is_empty() {
            tool_calls.push((id, name, Value::String(arg_json)));
        } else {
            tool_calls.push((id, name, input));
        }
    }

    Ok((text, tool_calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_resolution_prefers_explicit() {
        let _g = crate::test_env::lock().lock().unwrap();
        std::env::set_var("FX_PROVIDER", "openai");
        std::env::set_var("AI_BASE_URL", "http://localhost:11434/v1");
        let cfg = Config {
            mode: "ask".into(),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            model: "qwen3-coder".into(),
            permission_mode: crate::permissions::PermissionMode::Auto,
            max_agent_steps: 0,
            max_tool_result_bytes: 65536,
            first_call_tool_choice: crate::config::FirstCallToolChoice::Auto,
            context: true,
            sandbox: crate::config::SandboxMode::None,
            permission_rules: Default::default(),
            settings_path: None,
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            context_limits: crate::context::ContextLimits::default(),
            input_appearance: "auto".into(),
            presentation_mode: "default".into(),
            update_channel: "stable".into(),
        };
        let p = resolve_provider(&cfg).unwrap();
        assert_eq!(p.provider, ProviderKind::OpenAi);
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:11434/v1"));
        std::env::remove_var("FX_PROVIDER");
        std::env::remove_var("AI_BASE_URL");
        drop(_g);
    }
}
