//! Tool implementations, modeled on fx's tool set.
//!
//! Tools are executed by the agent loop after the permission gate. Each tool
//! returns a JSON value (or an error mapped into an error string result).
//!
//! Sensitive tools (require the permission gate): write_file, edit_file,
//! delete_file, rename_file, copy_file, create_folder, run_command,
//! open_file, install_skill, view_image.
//! Non-sensitive: read_file, list_files, glob_files, grep_files, file_info,
//! web_search, web_fetch, memory, skill, ask_user_question.

pub mod bash;
pub mod filesystem;
pub mod memory;
pub mod question;
pub mod skill;
pub mod vision;
pub mod web;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::Config;

/// Shared context passed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub max_result_bytes: usize,
    /// True when a human is present (interactive TTY) — enables
    /// ask_user_question and human permission fallbacks.
    pub interactive: bool,
    pub config: Arc<Config>,
}

impl ToolContext {
    /// Resolve a possibly-relative path against the workspace.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() { path.to_path_buf() } else { self.workspace.join(path) }
    }
    pub fn in_workspace(&self, p: &Path) -> bool {
        p.starts_with(&self.workspace)
    }
    pub fn truncate(&self, s: &str) -> String {
        if s.len() <= self.max_result_bytes {
            s.to_string()
        } else {
            let mut out: String = s.chars().take(self.max_result_bytes).collect();
            out.push_str(&format!("\n… [truncated {} bytes]", s.len() - self.max_result_bytes));
            out
        }
    }
}

/// Describe the permission target for a tool call: filesystem tools use the
/// resolved path, run_command uses the raw command string.
pub fn target_for(name: &str, args_json: &str) -> Option<String> {
    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    match name {
        "run_command" => s("command"),
        "read_file" | "write_file" | "edit_file" | "delete_file" | "rename_file"
        | "copy_file" | "create_folder" | "open_file" | "file_info" => {
            s("file_path").or_else(|| s("path")).or_else(|| s("folder_path"))
        }
        "glob_files" | "grep_files" | "list_files" => s("pattern").or_else(|| s("path")),
        n if n.starts_with("mcp__") => Some(n.to_string()),
        _ => s("path").or_else(|| s("url")).or_else(|| s("query")),
    }
}

/// Execute one tool call. Returns the JSON result to hand back to the model.
/// Errors are surfaced as a structured error object (not an abort).
pub async fn execute(
    ctx: &ToolContext,
    name: &str,
    args: &Value,
) -> Result<Value> {
    match name {
        "run_command" => bash::run_command(ctx, args).await,
        "read_file" => filesystem::read_file(ctx, args),
        "write_file" => filesystem::write_file(ctx, args),
        "edit_file" => filesystem::edit_file(ctx, args),
        "delete_file" => filesystem::delete_file(ctx, args),
        "rename_file" => filesystem::rename_file(ctx, args),
        "copy_file" => filesystem::copy_file(ctx, args),
        "create_folder" => filesystem::create_folder(ctx, args),
        "file_info" => filesystem::file_info(ctx, args),
        "glob_files" => filesystem::glob_files(ctx, args),
        "grep_files" => filesystem::grep_files(ctx, args),
        "list_files" => filesystem::list_files(ctx, args),
        "open_file" => filesystem::open_file(ctx, args),
        "memory" => memory::memory(ctx, args),
        "web_fetch" => web::web_fetch(ctx, args).await,
        "web_search" => web::web_search(ctx, args).await,
        "ask_user_question" => question::ask_user_question(ctx, args),
        "skill" => skill::skill(ctx, args),
        "install_skill" => skill::install_skill(ctx, args),
        "view_image" => vision::view_image(ctx, args),
        "mcp" => {
            // Meta-tool: manage/look up connected MCP servers.
            let sub = arg(args, "action").unwrap_or("list");
            match sub {
                "list" => {
                    let servers = &ctx.config.mcp_servers;
                    let tools = crate::mcp::list_all_tools(servers);
                    Ok(serde_json::json!({
                        "servers": servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                        "tools": tools.iter().map(|t| format!("{} ({}): {}", crate::mcp::prefixed_name(&t.server, &t.name), t.server, t.description)).collect::<Vec<_>>(),
                    }))
                }
                "call" => {
                    let server = arg(args, "server").unwrap_or("").to_string();
                    let tool = arg(args, "tool").unwrap_or("").to_string();
                    let arguments = args.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
                    let full = format!("mcp__{server}__{tool}");
                    Ok(crate::mcp::execute_mcp(&full, &serde_json::json!({ "arguments": arguments }), &ctx.config.mcp_servers))
                }
                _ => Ok(err_json(format!("unknown mcp action: {sub}"))),
            }
        }
        other if other.starts_with("mcp__") => Ok(crate::mcp::execute_mcp(other, args, &ctx.config.mcp_servers)),
        other => bail!("unknown tool: {other}"),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

/// Tool schemas for the base toolkit plus any connected MCP servers.
/// MCP tools are published as `mcp__<server>__<tool>` entries so the model
/// can call them exactly like built-ins.
pub fn schemas_with_mcp(mcp_tools: &[crate::mcp::McpTool]) -> Vec<Value> {
    use serde_json::json;
    let mut out = schemas();
    for t in mcp_tools {
        out.push(json!({
            "type": "function",
            "function": {
                "name": crate::mcp::prefixed_name(&t.server, &t.name),
                "description": format!("[MCP {}] {}", t.server, t.description),
                "parameters": t.input_schema,
            }
        }));
    }
    out
}

pub fn err_json(msg: String) -> Value {
    json!({ "error": msg })
}

pub fn err_result(e: anyhow::Error) -> Value {
    json!({ "error": format!("{e:#}") })
}

pub fn ok_text(text: String) -> Value {
    json!({ "output": text })
}

/// Standard path-arg helper with helpful errors.
fn path_arg(ctx: &ToolContext, args: &Value, key: &str) -> Result<PathBuf> {
    let raw = arg(args, key).ok_or_else(|| anyhow::anyhow!("missing required argument `{key}`"))?;
    if raw.is_empty() {
        bail!("`{key}` must not be empty");
    }
    Ok(ctx.resolve(raw))
}

/// The full tool schema list sent to providers (OpenAI-style function schema).
pub fn schemas() -> Vec<Value> {
    use serde_json::json;
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the workspace using bash. Use for builds, tests, and git operations. Commands run in the project workspace directory. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The shell command to run."},
                        "description": {"type": "string", "description": "Why you are running this command."},
                        "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000)."}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file from the workspace. Use offset and limit for large files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "offset": {"type": "integer", "description": "Line offset (1-based)."},
                        "limit": {"type": "integer", "description": "Maximum lines to read."}
                    },
                    "required": ["file_path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write or overwrite a UTF-8 text file. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["file_path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace an exact unique string in a file with new text. The old string must match exactly once. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "Delete a file. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": ["file_path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rename_file",
                "description": "Move or rename a file. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "new_path": {"type": "string"}
                    },
                    "required": ["file_path", "new_path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "copy_file",
                "description": "Copy a file to a new path. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "destination": {"type": "string"}
                    },
                    "required": ["file_path", "destination"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "create_folder",
                "description": "Create a folder (and parents). Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {"folder_path": {"type": "string"}},
                    "required": ["folder_path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "open_file",
                "description": "Reveal a file in the system file manager / open it with the default application. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": ["file_path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "file_info",
                "description": "Metadata about a file or folder: size, modified time, type.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List entries in a directory (non-recursive), with sizes.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "description": "Directory; defaults to workspace root."}}
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob_files",
                "description": "Find files by glob pattern, relative to the workspace unless an absolute path is given.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern, e.g. 'src/**/*.rs'."},
                        "path": {"type": "string", "description": "Base directory; defaults to workspace root."}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep_files",
                "description": "Search file contents with a regex. Returns matching file:line:content. Defaults to workspace, skipping .git and node_modules.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regular expression."},
                        "path": {"type": "string"},
                        "glob": {"type": "string", "description": "Only search matching filenames."},
                        "output_mode": {"type": "string", "enum": ["content", "files_only", "count"]}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory",
                "description": "Persistent key-value memory scoped to this workspace. Actions: list, read, write, delete.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "read", "write", "delete"]},
                        "key": {"type": "string"},
                        "value": {"type": "string"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a web page and return its text content. Only http/https URLs reachable from this machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "prompt": {"type": "string", "description": "Optional guidance on what to extract."}
                    },
                    "required": ["url"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web and return top results (title, url, snippet).",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "ask_user_question",
                "description": "Ask the human a question when you need their input to proceed. Only available interactively.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "options": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["question"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "skill",
                "description": "List installed skills or read one skill's SKILL.md. Skills load at startup from ~/.fx/skills and <workspace>/.fx/skills.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "read"]},
                        "name": {"type": "string"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "install_skill",
                "description": "Install a skill from a local directory into the workspace skills folder. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Local directory containing SKILL.md."},
                        "name": {"type": "string"}
                    },
                    "required": ["source"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "view_image",
                "description": "Read an image file in the workspace and attach it to the conversation so a vision-capable model can see and describe it. Use for screenshots, diagrams, and photos the user references. Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path to the image file (png, jpg, gif, or webp, under 15 MB)"}
                    },
                    "required": ["path"]
                }
            }
        }),
    ]
}

/// Parse model-emitted text tool calls of the form:
///   <invoke name="tool_name"><parameter name="arg">value</parameter>...</invoke>
/// Returns (name, arguments) pairs. Values are parsed as JSON when possible,
/// otherwise treated as plain strings. Used as a fallback for models/endpoints
/// that cannot emit structured tool_calls (many local/OpenAI-compatible models).
pub fn parse_text_tool_calls(text: &str) -> Vec<(String, Value)> {
    let b = text.as_bytes();
    let find_tag = |from: usize, tag: &str| -> Option<usize> {
        text[from..].find(tag).map(|r| from + r)
    };
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(open) = find_tag(pos, "<invoke name=\"") {
        let Some(name_end) = find_tag(open + "<invoke name=\"".len(), "\"") else { break };
        let name = xml_unescape(&text[open + "<invoke name=\"".len()..name_end]);
        let Some(body_open) = find_tag(name_end + 1, ">") else { break };
        let Some(body_close) = find_tag(body_open + 1, "</invoke>") else { break };
        let body = &text[body_open + 1..body_close];

        let mut args = serde_json::Map::new();
        let mut ppos = 0usize;
        while let Some(popen) = body[ppos..].find("<parameter name=\"") {
            let pabs = ppos + popen;
            let pname_start = pabs + "<parameter name=\"".len();
            let Some(pname_end) = body[pname_start..].find('"') else { break };
            let key = xml_unescape(&body[pname_start..pname_start + pname_end]);
            let Some(pgt) = body[pname_start + pname_end..].find('>') else { break };
            let val_start = pname_start + pname_end + pgt + 1;
            let Some(pclose) = body[val_start..].find("</parameter>") else { break };
            let raw = xml_unescape(&body[val_start..val_start + pclose]);
            let parsed = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::String(raw));
            args.insert(key, parsed);
            ppos = val_start + pclose + "</parameter>".len();
        }
        if !name.is_empty() {
            out.push((name, Value::Object(args)));
        }
        pos = body_close + "</invoke>".len();
        let _ = b;
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod text_tool_tests {
    use super::*;

    #[test]
    fn parses_invoke_blocks() {
        let t = r#"Sure! Let me look.
<invoke name="list_files"><parameter name="path">/tmp</parameter></invoke>
Done."#;
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "list_files");
        assert_eq!(calls[0].1["path"], "/tmp");
    }

    #[test]
    fn parses_json_escaped_values() {
        let t = r#"<invoke name="grep_files"><parameter name="pattern">TODO</parameter><parameter name="path">/a/b</parameter></invoke>"#;
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["pattern"], "TODO");
        assert_eq!(calls[0].1["path"], "/a/b");
    }

    #[test]
    fn no_invoke_means_empty() {
        assert!(parse_text_tool_calls("hello there").is_empty());
    }
}
