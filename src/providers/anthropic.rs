//! Anthropic Messages API client with streaming.

use anyhow::Context;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::sse::sse_events;
use super::{EventStream, Message, ProviderConfig, StreamEvent};

const DEFAULT_BASE: &str = "https://api.anthropic.com";

fn messages_url(p: &ProviderConfig) -> String {
    let base = p
        .base_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_BASE);
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

fn serialize_content(m: &Message) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in &m.content {
        match block {
            super::ContentBlock::Text(t) => parts.push(json!({"type": "text", "text": t})),
            super::ContentBlock::ToolUse { id, name, input } => {
                parts.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            }
            super::ContentBlock::ToolResult { id, content } => {
                parts.push(json!({"type": "tool_result", "tool_use_id": id, "content": content}));
            }
            super::ContentBlock::Image { media_type, data } => {
                parts.push(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}));
            }
        }
    }
    parts
}

fn build_body(p: &ProviderConfig, messages: &[Message], tools: Option<&[Value]>, system: &str, max_tokens: Option<u32>) -> Value {
    let mut body = json!({
        "model": p.model,
        "messages": messages
            .iter()
            .filter(|m| !matches!(m.role.as_str(), "system"))
            .map(|m| json!({"role": m.role, "content": serialize_content(m)}))
            .collect::<Vec<_>>(),
        "stream": true,
        "max_tokens": max_tokens.unwrap_or(4096),
    });
    if !system.trim().is_empty() {
        body["system"] = json!(system);
    }
    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
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
    let url = messages_url(p);
    let body = build_body(p, messages, tools, system, max_tokens);
    let key = p.api_key.clone();

    Box::pin(async_stream::stream! {
        let key = match key {
            Some(k) => k,
            None => {
                yield Err(anyhow::anyhow!("ANTHROPIC_API_KEY is not set"));
                return;
            }
        };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("building http client")?;
        let resp = match client
            .post(&url)
            .bearer_auth(&key)
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                yield Err(anyhow::anyhow!("request to {url} failed: {e}"));
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            yield Err(anyhow::anyhow!("{status} from {url}: {}", truncate(&text, 500)));
            return;
        }

        // content block state by index: (block_type, id, name, arg_json)
        let mut blocks: std::collections::BTreeMap<usize, (String, String, String, String)> =
            std::collections::BTreeMap::new();

        let mut events = sse_events(resp);
        while let Some(ev) = events.next().await {
            let ev = match ev {
                Ok(e) => e,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let data: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let etype = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match etype {
                "content_block_start" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let block = &data["content_block"];
                    let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if btype == "tool_use" {
                        blocks.insert(index, ("tool_use".into(), id.clone(), name.clone(), String::new()));
                        yield Ok(StreamEvent::ToolCallStart { index, id, name });
                    } else if !text.is_empty() {
                        yield Ok(StreamEvent::TextDelta(text));
                    }
                }
                "content_block_delta" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &data["delta"];
                    let dtype = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match dtype {
                        "text_delta" => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                yield Ok(StreamEvent::TextDelta(t.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    yield Ok(StreamEvent::ReasoningDelta(t.to_string()));
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(j) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                if let Some(slot) = blocks.get_mut(&index) {
                                    slot.3.push_str(j);
                                }
                                yield Ok(StreamEvent::ToolCallArgDelta { index, delta: j.to_string() });
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    if let Some((_, id, name, args)) = blocks.remove(&index) {
                        if !name.is_empty() {
                            let input: Value = serde_json::from_str(&args).unwrap_or(Value::String(args));
                            yield Ok(StreamEvent::ToolCallDone { index, id, name, input });
                        }
                    }
                }
                "message_delta" => {
                    let usage = &data["usage"];
                    let input = usage.get("input_tokens").and_then(|v| v.as_u64());
                    let output = usage.get("output_tokens").and_then(|v| v.as_u64());
                    yield Ok(StreamEvent::Usage { input_tokens: input, output_tokens: output });
                }
                "error" => {
                    yield Err(anyhow::anyhow!("provider error: {data}"));
                    return;
                }
                "message_stop" => break,
                _ => {}
            }
        }

        // Finalize any blocks that stopped without explicit stop events.
        for (index, (_, id, name, args)) in blocks {
            if !name.is_empty() {
                let input: Value = serde_json::from_str(&args).unwrap_or(Value::String(args));
                yield Ok(StreamEvent::ToolCallDone { index, id, name, input });
            }
        }
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
