//! OpenAI-compatible Chat Completions client with streaming.
//! Serves the `gateway` (AI Gateway) and `openai` (any compatible endpoint)
//! provider kinds. Endpoint: {base_url}/chat/completions.

use anyhow::Context;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::sse::sse_events;
use super::{EventStream, Message, ProviderConfig, StreamEvent};

fn default_base() -> &'static str {
    "https://gateway.vercel.ai"
}

fn chat_url(p: &ProviderConfig) -> String {
    if let Ok(u) = std::env::var("FX_GATEWAY_CHAT_URL") {
        if !u.trim().is_empty() {
            return u;
        }
    }
    let base = p.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| default_base().to_string());
    let base = base.trim_end_matches('/').to_string();
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/chat/completions")
    }
}

fn serialize_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let mut parts = Vec::new();
                let mut text = String::new();
                for block in &m.content {
                    match block {
                        super::ContentBlock::Text(t) => text.push_str(t),
                        super::ContentBlock::ToolResult { id, content } => {
                            if !text.is_empty() {
                                parts.push(json!({"type": "text", "text": text}));
                                text.clear();
                            }
                            parts.push(json!({"type": "tool_result", "tool_call_id": id, "content": content}));
                        }
                        super::ContentBlock::Image { media_type, data } => {
                            if !text.is_empty() {
                                parts.push(json!({"type": "text", "text": text}));
                                text.clear();
                            }
                            let url = format!("data:{media_type};base64,{data}");
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    parts.push(json!({"type": "text", "text": text}));
                }
                out.push(json!({"role": "user", "content": parts}));
            }
            "assistant" => {
                let mut content = String::new();
                let mut tool_calls = Vec::new();
                for block in &m.content {
                    match block {
                        super::ContentBlock::Text(t) => content.push_str(t),
                        super::ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": input.to_string()}
                            }));
                        }
                        _ => {}
                    }
                }
                let mut msg = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(msg);
            }
            other => out.push(json!({"role": other, "content": m.text_content()})),
        }
    }
    out
}

/// Build the request body. Threads tool results after their corresponding
/// tool call, matching the OpenAI conversation contract.
pub fn build_body(
    p: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[Value]>,
    system: &str,
    max_tokens: Option<u32>,
) -> Value {
    let mut body = json!({
        "model": p.model,
        "messages": serialize_messages(messages),
        "stream": true,
    });
    if let Some(max) = max_tokens {
        body["max_tokens"] = json!(max);
    }
    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = "auto".into();
        }
    }
    if !system.trim().is_empty() {
        body["messages"] = {
            let mut arr = Vec::new();
            arr.push(json!({"role": "system", "content": system}));
            if let Value::Array(msgs) = body["messages"].clone() {
                arr.extend(msgs);
            }
            Value::Array(arr)
        };
    }
    body
}

pub fn stream(
    p: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[Value]>,
    system: &str,
    max_tokens: Option<u32>,
) -> EventStream {
    let url = chat_url(p);
    let body = build_body(p, messages, tools, system, max_tokens);
    let key = p.api_key.clone();
    let model = p.model.clone();

    Box::pin(async_stream::stream! {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            // Bound the whole request so a gateway that stops sending events
            // mid-stream cannot hang the agent forever.
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("building http client")?;

        let mut req = client.post(&url).json(&body);
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                yield Err(anyhow::anyhow!("request to {url} failed: {e}"));
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            yield Err(anyhow::anyhow!("{status} from {model} at {url}: {}", truncate(&text, 500)));
            return;
        }

        // Track tool call fragments: index -> (id, name, args).
        let mut calls: Vec<(usize, Option<String>, Option<String>, String)> = Vec::new();
        let mut saw_usage = false;

        let mut events = sse_events(resp);
        while let Some(ev) = events.next().await {
            let ev = match ev {
                Ok(e) => e,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            if ev.data == "[DONE]" {
                break;
            }
            let chunk: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue, // keep-alive comments etc.
            };
            if let Some(err) = chunk.get("error") {
                yield Err(anyhow::anyhow!("provider error: {err}"));
                return;
            }
            // AI-SDK-style typed events (local gateways, Vercel AI SDK proxies):
            //   {"type":"text-delta","delta":...}
            //   {"type":"reasoning-delta","delta":...}   (thinking; skipped)
            //   {"type":"finish","finishReason":...,"usage":{...}}
            if let Some(ty) = chunk.get("type").and_then(|v| v.as_str()) {
                match ty {
                    "text-delta" => {
                        if let Some(d) = chunk.get("delta").and_then(|v| v.as_str()) {
                            if !d.is_empty() {
                                yield Ok(StreamEvent::TextDelta(d.to_string()));
                            }
                        }
                    }
                    "reasoning-delta" => {
                        if let Some(d) = chunk.get("delta").and_then(|v| v.as_str()) {
                            if !d.is_empty() {
                                yield Ok(StreamEvent::ReasoningDelta(d.to_string()));
                            }
                        }
                    }
                    "tool-call-start" | "tool-call-delta" | "tool-call-end" => {
                        // AI SDK tool-call events: {"type":"tool-call-start","toolCallId","toolName","args":{...}}
                        // Accumulate like OpenAI tool_calls.
                        let idx = chunk.get("index").and_then(|v| v.as_u64()).unwrap_or(calls.len() as u64) as usize;
                        while calls.len() <= idx {
                            calls.push((idx, None, None, String::new()));
                        }
                        let slot = &mut calls[idx];
                        if let Some(id) = chunk.get("toolCallId").and_then(|v| v.as_str()) {
                            slot.1 = Some(id.to_string());
                        }
                        if let Some(name) = chunk.get("toolName").and_then(|v| v.as_str()) {
                            slot.2 = Some(name.to_string());
                        }
                        if let Some(args) = chunk.get("args") {
                            let rendered = serde_json::to_string(args).unwrap_or_default();
                            if !rendered.is_empty() {
                                slot.3 = rendered;
                            }
                        }
                    }
                    "finish" => {
                        if let Some(u) = chunk.get("usage") {
                            let input = u.get("inputTokens").and_then(|v| v.as_u64())
                                .or_else(|| u.get("prompt_tokens").and_then(|v| v.as_u64()));
                            let output = u.get("outputTokens").and_then(|v| v.as_u64())
                                .or_else(|| u.get("completion_tokens").and_then(|v| v.as_u64()));
                            yield Ok(StreamEvent::Usage { input_tokens: input, output_tokens: output });
                        }
                        break;
                    }
                    _ => {} // reasoning-delta and anything else: skip
                }
                continue;
            }
            if let Some(usage) = chunk.get("usage") {
                saw_usage = true;
                let input = usage.get("prompt_tokens").and_then(|v| v.as_u64());
                let output = usage.get("completion_tokens").and_then(|v| v.as_u64());
                yield Ok(StreamEvent::Usage { input_tokens: input, output_tokens: output });
            }
            let choices = chunk
                .get("choices")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for choice in choices {
                let _ = choice.get("finish_reason");
                let delta = &choice["delta"];
                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        yield Ok(StreamEvent::TextDelta(text.to_string()));
                    }
                }
                if let Some(tools) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tools {
                        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        while calls.len() <= index {
                            calls.push((index, None, None, String::new()));
                        }
                        let slot = &mut calls[index];
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            slot.1 = Some(id.to_string());
                        }
                        if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
                            slot.2 = Some(name.to_string());
                            yield Ok(StreamEvent::ToolCallStart {
                                index,
                                id: slot.1.clone().unwrap_or_default(),
                                name: name.to_string(),
                            });
                        }
                        if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()) {
                            if !args.is_empty() {
                                slot.3.push_str(args);
                                yield Ok(StreamEvent::ToolCallArgDelta { index, delta: args.to_string() });
                            }
                        }
                    }
                }
            }
        }

        // Finalize pending tool calls (id may be absent on some providers).
        for (index, id, name, args) in calls {
            if let Some(name) = name {
                let id = id.unwrap_or_else(|| format!("call_{index}"));
                let input: Value = serde_json::from_str(&args).unwrap_or(Value::String(args));
                yield Ok(StreamEvent::ToolCallDone { index, id, name, input });
            }
        }

        let _ = saw_usage;
        yield Ok(StreamEvent::Finish);
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push_str("…");
        out
    }
}
