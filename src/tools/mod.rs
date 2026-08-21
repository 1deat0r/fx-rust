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

pub mod background;
pub mod bash;
pub mod filesystem;
pub mod html;
pub mod memory;
pub mod question;
pub mod search;
pub mod skill;
pub mod subagent;
pub mod vision;
pub mod web;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::{json, Value};

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
    /// Session store for nested agent runs (subagent tool).
    pub store: crate::sessions::SessionStore,
    /// Owning agent session id (empty outside an agent run). Tagged onto
    /// background processes for restore-on-resume reporting.
    pub session_id: String,
}

impl ToolContext {
    /// Resolve a possibly-relative path against the workspace.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        }
    }
    pub fn in_workspace(&self, p: &Path) -> bool {
        p.starts_with(&self.workspace)
    }
    pub fn truncate(&self, s: &str) -> String {
        if s.len() <= self.max_result_bytes {
            s.to_string()
        } else {
            let mut out: String = s.chars().take(self.max_result_bytes).collect();
            out.push_str(&format!(
                "\n… [truncated {} bytes]",
                s.len() - self.max_result_bytes
            ));
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
        "background_process" => {
            if args.get("action").and_then(|v| v.as_str()) == Some("start") {
                s("command")
            } else {
                s("process_id")
            }
        }
        "run_command" => s("command"),
        "read_file" | "write_file" | "edit_file" | "delete_file" | "rename_file" | "copy_file"
        | "create_folder" | "open_file" | "file_info" => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("folder_path")),
        "glob_files" | "grep_files" | "list_files" => s("pattern").or_else(|| s("path")),
        "semantic_search" => s("query"),
        n if n.starts_with("mcp__") => Some(n.to_string()),
        "subagent" => Some("subagent".to_string()),
        _ => s("path").or_else(|| s("url")).or_else(|| s("query")),
    }
}

/// Execute one tool call. Returns the JSON result to hand back to the model.
/// Errors are surfaced as a structured error object (not an abort).
pub async fn execute(ctx: &ToolContext, name: &str, args: &Value) -> Result<Value> {
    match name {
        "background_process" => background::background_process(ctx, args).await,
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
        "semantic_search" => search::semantic_search(ctx, args),
        "ask_user_question" => question::ask_user_question(ctx, args),
        "skill" => skill::skill(ctx, args),
        "install_skill" => skill::install_skill(ctx, args),
        "view_image" => vision::view_image(ctx, args),
        "subagent" => {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            subagent::run_subagent(ctx.config.clone(), ctx.store.clone(), prompt, model).await
        }
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
                    let arguments = args
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let full = format!("mcp__{server}__{tool}");
                    Ok(crate::mcp::execute_mcp(
                        &full,
                        &serde_json::json!({ "arguments": arguments }),
                        &ctx.config.mcp_servers,
                    ))
                }
                _ => Ok(err_json(format!("unknown mcp action: {sub}"))),
            }
        }
        other if other.starts_with("mcp__") => Ok(crate::mcp::execute_mcp(
            other,
            args,
            &ctx.config.mcp_servers,
        )),
        other => bail!("unknown tool: {other}"),
    }
}

pub fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
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
                "name": "background_process",
                "description": "Start and manage long-running background processes: servers, watchers, builds. Commands run detached in their own session with output written to a log file, so they keep running after the tool returns. Actions: start (launch), list, get_output/log (tail the log), supervise (live ps data + counts), tree (descendant process tree), stop_tree (terminate the process and all its descendants), stop (terminate with a grace period). Sensitive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["start", "list", "get_output", "log", "supervise", "tree", "stop_tree", "stop"], "description": "What to do (default list)."},
                        "command": {"type": "string", "description": "The shell command to run. Required for start."},
                        "name": {"type": "string", "description": "Optional short label for the process."},
                        "cwd": {"type": "string", "description": "Working directory (relative to workspace). Defaults to the workspace."},
                        "process_id": {"type": "string", "description": "Background process id. Required for get_output/log/stop."},
                        "tail": {"type": "integer", "description": "For get_output: number of trailing lines to return."},
                        "max_bytes": {"type": "integer", "description": "For get_output: max bytes to return (default 16384)."},
                        "timeout_ms": {"type": "integer", "description": "For stop: grace period before SIGKILL (default 5000)."}
                    },
                    "required": ["action"]
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
                "name": "semantic_search",
                "description": "Ranked keyword search over text files in the workspace (BM25-lite). Use to locate code/docs by concept before reading files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "search terms, e.g. \"database connection pool\""},
                        "max_results": {"type": "number", "description": "1-10 results (default 5)"}
                    },
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
        json!({
            "type": "function",
            "function": {
                "name": "subagent",
                "description": "Delegate a self-contained task to a nested sub-agent that runs in this same workspace with its own loop and tools, then returns its final answer. Use for subtasks that are logically separate from the main thread (research, independent implementation, reading a document). The sub-agent starts with an empty tool history and its own permission checks. Sensitive, and nested depth is capped at 3.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "The self-contained task for the sub-agent, including any context it needs"},
                        "model": {"type": "string", "description": "Optional model override for the sub-agent"}
                    },
                    "required": ["prompt"]
                }
            }
        }),
    ]
}

/// Parse model-emitted text tool calls of the form:
///   <invoke name="tool_name"><parameter name="arg">value</parameter>...</invoke>
///   <│DSML│invoke name="tool_name"><│DSML│parameter name="arg" string="true">value</│DSML│parameter></│DSML│invoke>
/// Returns (name, arguments) pairs. Values are parsed as JSON when possible,
/// otherwise treated as plain strings. Used as a fallback for models/endpoints
/// that cannot emit structured tool_calls (many local/OpenAI-compatible models).
///
/// DeepSeek-style DSML markup is normalized away first so `<│DSML│invoke ...>`
/// blocks parse identically to `<invoke ...>` blocks. Parameter/attribute
/// shapes like `<parameter name="x" string="true">` are accepted unchanged.
pub fn parse_text_tool_calls(text: &str) -> Vec<(String, Value)> {
    let text = normalize_dsml(text);
    let b = text.as_bytes();
    let find_tag =
        |from: usize, tag: &str| -> Option<usize> { text[from..].find(tag).map(|r| from + r) };
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(open) = find_tag(pos, "<invoke name=\"") {
        let Some(name_end) = find_tag(open + "<invoke name=\"".len(), "\"") else {
            break;
        };
        let name = xml_unescape(&text[open + "<invoke name=\"".len()..name_end]);
        let Some(body_open) = find_tag(name_end + 1, ">") else {
            break;
        };
        let Some(body_close) = find_tag(body_open + 1, "</invoke>") else {
            break;
        };
        let body = &text[body_open + 1..body_close];

        let mut args = serde_json::Map::new();
        let mut ppos = 0usize;
        while let Some(popen) = body[ppos..].find("<parameter name=\"") {
            let pabs = ppos + popen;
            let pname_start = pabs + "<parameter name=\"".len();
            let Some(pname_end) = body[pname_start..].find('"') else {
                break;
            };
            let key = xml_unescape(&body[pname_start..pname_start + pname_end]);
            let Some(pgt) = body[pname_start + pname_end..].find('>') else {
                break;
            };
            let val_start = pname_start + pname_end + pgt + 1;
            let Some(pclose) = body[val_start..].find("</parameter>") else {
                break;
            };
            let raw = xml_unescape(&body[val_start..val_start + pclose]);
            let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
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

/// Strip DeepSeek DSML markers (`│DSML│` / `|DSML|`) from tool-call markup so
/// `<│DSML│invoke name="t">...` is parsed like `<invoke name="t">...`.
/// The marker is redundant wrapping: `<│DSML│invoke` -> `<invoke`,
/// `</│DSML│parameter>` -> `</parameter>`.
fn normalize_dsml(text: &str) -> String {
    // │ is U+FF5C FULLWIDTH VERTICAL LINE; also accept ASCII `|DSML|` and
    // any surrounding-marker variants some models emit.
    let mut out = text.to_string();
    for marker in [
        "\u{ff5c}DSML\u{ff5c}",
        "|DSML|",
        "\u{ff5c}DSML|",
        "|DSML\u{ff5c}",
    ] {
        out = out.replace(marker, "");
    }
    out
}

/// Streaming display mask for text-blob tool-call markup.
///
/// Models without native tool-calling stream their tool calls as ordinary text
/// in one of these dialects, which must be hidden from the user's terminal and
/// from the committed transcript (the raw text still goes to
/// [`parse_text_tool_calls`] unchanged):
///
///   • DeepSeek DSML:  `<│DSML│invoke ...><│DSML│parameter ...>...</│DSML│invoke>`
///   • fx/Claude style: `<tool_calls>...<invoke name="t"><parameter ...>...</invoke>...</tool_calls>`
///
/// Feed every streamed delta through [`ToolMarkupMask::filter`]. Markers may be
/// split across deltas; the mask holds back any run that could be a partial
/// marker until it resolves.
pub struct ToolMarkupMask {
    pending: String,
    inside: bool,
    /// Nesting depth of block elements (`invoke`/`call`/`tool_calls`).
    /// `parameter`/`arg` opens and closes are depth-neutral; only the outer
    /// block close drops the depth back to 0 and un-hides text.
    depth: usize,
}

/// Open markers: (literal, element name). `parameter`/`arg` opens are
/// depth-neutral; the rest start hidden blocks.
const MARKUP_OPEN: [(&str, &str); 12] = [
    ("<\u{ff5c}DSML\u{ff5c}invoke", "invoke"),
    ("<|DSML|invoke", "invoke"),
    ("<\u{ff5c}DSML\u{ff5c}call", "call"),
    ("<|DSML|call", "call"),
    // Some models wrap the block in a `tool_calls` element too.
    ("<\u{ff5c}DSML\u{ff5c}tool_calls", "tool_calls"),
    ("<|DSML|tool_calls", "tool_calls"),
    ("<\u{ff5c}DSML\u{ff5c}tool_call", "tool_calls"),
    ("<|DSML|tool_call", "tool_calls"),
    ("<tool_calls", "tool_calls"),
    ("<invoke", "invoke"),
    // Depth-neutral (kept explicit so `parameter` opens are recognized if they
    // ever appear at top level or for wrapper bookkeeping).
    ("<\u{ff5c}DSML\u{ff5c}parameter", "parameter"),
    ("<|DSML|parameter", "parameter"),
];

/// Close markers: (literal, element name). `parameter`/`arg` closes are
/// depth-neutral; only block closes decrement depth.
const MARKUP_CLOSE: [(&str, &str); 11] = [
    ("</\u{ff5c}DSML\u{ff5c}invoke", "invoke"),
    ("</|DSML|invoke", "invoke"),
    ("</\u{ff5c}DSML\u{ff5c}call", "call"),
    ("</|DSML|call", "call"),
    ("</\u{ff5c}DSML\u{ff5c}tool_calls", "tool_calls"),
    ("</|DSML|tool_calls", "tool_calls"),
    ("</\u{ff5c}DSML\u{ff5c}tool_call", "tool_calls"),
    ("</|DSML|tool_call", "tool_calls"),
    ("</tool_calls", "tool_calls"),
    ("</invoke", "invoke"),
    ("</\u{ff5c}DSML\u{ff5c}parameter", "parameter"),
];

#[derive(Debug)]
enum Mark {
    Open { name: &'static str, len: usize },
    Close { name: &'static str, len: usize },
}

/// Earliest marker occurrence in `s` among the given marker list.
/// On offset ties (one marker is a prefix of another) the longer literal wins.
fn find_markers(
    s: &str,
    markers: &'static [(&'static str, &'static str)],
) -> Option<(usize, Mark)> {
    let mut best: Option<(usize, usize, &'static str)> = None;
    for (marker, name) in markers {
        if let Some(i) = s.find(marker) {
            let better = match best {
                Some((bi, bl, _)) => i < bi || (i == bi && marker.len() > bl),
                None => true,
            };
            if better {
                best = Some((i, marker.len(), name));
            }
        }
    }
    best.map(|(i, l, n)| (i, Mark::Open { name: n, len: l }))
}

/// Close-marker form of [`find_markers`].
fn find_close_markers(s: &str) -> Option<(usize, Mark)> {
    let mut best: Option<(usize, usize, &'static str)> = None;
    for (marker, name) in MARKUP_CLOSE {
        if let Some(i) = s.find(marker) {
            let better = match best {
                Some((bi, bl, _)) => i < bi || (i == bi && marker.len() > bl),
                None => true,
            };
            if better {
                best = Some((i, marker.len(), name));
            }
        }
    }
    best.map(|(i, l, n)| (i, Mark::Close { name: n, len: l }))
}

impl Default for ToolMarkupMask {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolMarkupMask {
    pub fn new() -> Self {
        ToolMarkupMask {
            pending: String::new(),
            inside: false,
            depth: 0,
        }
    }

    /// Feed one streamed text delta; returns the portion safe to display.
    pub fn filter(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let mut out = String::new();
        loop {
            if self.inside {
                match self.next_inside() {
                    Some((idx, Mark::Open { name, len })) => {
                        self.pending.drain(..idx + len);
                        if name != "parameter" {
                            self.depth += 1;
                        }
                    }
                    Some((idx, Mark::Close { name, len })) => {
                        let marker_end = idx + len;
                        match self.pending[marker_end..].find('>') {
                            Some(g) => {
                                self.pending.drain(..marker_end + g + 1);
                                if name != "parameter" {
                                    self.depth = self.depth.saturating_sub(1);
                                    if self.depth == 0 {
                                        self.inside = false;
                                        // Blocks usually end with a newline
                                        // after the close tag; drop one so the
                                        // following prose doesn't start on a
                                        // blank line.
                                        if self.pending.starts_with("\r\n") {
                                            self.pending.drain(..2);
                                        } else if self.pending.starts_with('\n') {
                                            self.pending.drain(..1);
                                        }
                                    }
                                }
                            }
                            None => {
                                // '>' likely arrives in the next delta.
                                return out;
                            }
                        }
                    }
                    None => {
                        // Nothing to emit while inside; if the tail could be a
                        // partial marker, hold it for the next delta.
                        if ends_with_partial(&self.pending, &ALL_MARKERS) {
                            return out;
                        }
                        self.pending.clear();
                        return out;
                    }
                }
            } else {
                match find_markers(&self.pending, &MARKUP_OPEN) {
                    Some((idx, Mark::Open { len, .. })) => {
                        // Only treat as an entry when not part of a close tag
                        // (`</invoke>` also contains `<invoke`).
                        if idx > 0 && self.pending.as_bytes()[idx - 1] == b'/' {
                            // A close tag in normal prose: flush one char and
                            // keep scanning so the text eventually appears.
                            let first = self.pending.remove(0);
                            out.push(first);
                            continue;
                        }
                        out.push_str(&self.pending[..idx]);
                        self.pending.drain(..idx + len);
                        self.inside = true;
                        self.depth = 1;
                    }
                    _ => {
                        if ends_with_partial(&self.pending, &ALL_MARKERS) {
                            return out;
                        }
                        out.push_str(&self.pending);
                        self.pending.clear();
                        return out;
                    }
                }
            }
        }
    }

    /// While inside a hidden region, find the next open or close marker
    /// (closes win at earlier offsets; prefix ties prefer longer literals).
    fn next_inside(&self) -> Option<(usize, Mark)> {
        let o = find_markers(&self.pending, &MARKUP_OPEN);
        let c = find_close_markers(&self.pending);
        match (o, c) {
            (Some((oi, om)), Some((ci, cm))) => {
                let ol = marker_len(&om);
                let cl = marker_len(&cm);
                if ci < oi {
                    Some((ci, cm))
                } else if oi < ci {
                    Some((oi, om))
                } else if cl > ol {
                    Some((ci, cm))
                } else {
                    Some((oi, om))
                }
            }
            (Some(o), None) => Some(o),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }

    /// Finalize: flush any residual pending text (used at stream end).
    pub fn finish(self) -> String {
        if self.inside {
            String::new()
        } else {
            self.pending
        }
    }
}

fn marker_len(m: &Mark) -> usize {
    match m {
        Mark::Open { len, .. } | Mark::Close { len, .. } => *len,
    }
}

/// Concatenated marker list used for partial-hold detection inside a block.
static ALL_MARKERS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&'static str> = Vec::new();
    for (m, _) in MARKUP_OPEN.iter() {
        v.push(m);
    }
    for (m, _) in MARKUP_CLOSE.iter() {
        v.push(m);
    }
    v
});

fn ends_with_partial(s: &str, needles: &[&str]) -> bool {
    // True when the tail of `s` could be the *beginning* of one of `needles`.
    // Only the text from the last '<' can begin a marker — plain prose before
    // it must not prevent the hold (byte offsets, UTF-8 safe via rfind on char
    // boundary: '<' is ASCII so any position works).
    match s.rfind('<') {
        Some(i) => {
            let tail = &s[i..];
            !tail.is_empty() && needles.iter().any(|n| n.starts_with(tail))
        }
        None => false,
    }
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

    // ---- DeepSeek DSML dialect (real output from bitdeer/deepseek-v4-flash) ----

    #[test]
    fn parses_dsml_invoke_blocks() {
        let t = "I'll look that up.\n\n<\u{ff5c}DSML\u{ff5c}invoke name=\"web_search\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"query\" string=\"true\">trending AI news today</\u{ff5c}DSML\u{ff5c}parameter>\n<\u{ff5c}DSML\u{ff5c}parameter name=\"num_results\" string=\"false\">10</\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>";
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web_search");
        assert_eq!(calls[0].1["query"], "trending AI news today");
        assert_eq!(calls[0].1["num_results"], 10);
    }

    #[test]
    fn parses_dsml_with_ascii_pipe() {
        let t = "<|DSML|invoke name=\"bash\"><|DSML|parameter name=\"cmd\" string=\"true\">echo hi</|DSML|parameter></|DSML|invoke>";
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bash");
        assert_eq!(calls[0].1["cmd"], "echo hi");
    }

    #[test]
    fn parses_tool_calls_wrapper_with_dsml() {
        // Model sometimes wraps the whole blob in a <tool_calls> element.
        let t = "<tool_calls>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"web_search\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"query\" string=\"true\">news</\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n</tool_calls>";
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web_search");
        assert_eq!(calls[0].1["query"], "news");
    }

    #[test]
    fn truncated_dsml_still_yields_first_call() {
        // The live stream got cut mid-parameter on a second invoke; the first
        // complete block must still parse and execute.
        let t = "<\u{ff5c}DSML\u{ff5c}invoke name=\"web_search\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"query\" string=\"true\">today</\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"exec_command\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"tty\" string";
        let calls = parse_text_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web_search");
    }

    // ---- DsmlMask streaming display mask ----

    #[test]
    fn markup_mask_hides_dsml_keeps_prose() {
        let mut m = ToolMarkupMask::new();
        let shown: String = [
            "I'll look that up.\n\n",
            "<\u{ff5c}DSML\u{ff5c}invoke ",
            "name=\"web_search\">\n<\u{ff5c}DSML\u{ff5c}param",
            "eter name=\"query\" string=\"true\">news<",
            "/\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n",
            "Done. ",
        ]
        .iter()
        .map(|d| m.filter(d))
        .collect();
        let tail = m.finish();
        let shown = format!("{shown}{tail}");
        assert_eq!(shown, "I'll look that up.\n\nDone. ");
    }

    #[test]
    fn markup_mask_split_marker_across_deltas() {
        let mut m = ToolMarkupMask::new();
        let mut shown = String::new();
        for d in [
            "a<\u{ff5c}DSM",
            "L\u{ff5c}invoke name=\"x\">",
            "v</\u{ff5c}DSM",
            "L\u{ff5c}invoke>b",
        ] {
            shown.push_str(&m.filter(d));
        }
        shown.push_str(&m.finish());
        assert_eq!(shown, "ab");
    }

    // ---- plain fx/Claude text-blob dialect (<tool_calls>/<invoke>) ----

    #[test]
    fn markup_mask_hides_plain_invoke_block() {
        let mut m = ToolMarkupMask::new();
        let mut shown = String::new();
        for d in [
            "Let me check.\n\n",
            "<tool_calls>\n",
            "<invoke name=\"web_search\">\n",
            "<parameter name=\"query\">trending AI news today</parameter>\n",
            "</invoke>\n",
            "</tool_calls>\n",
            "Done.",
        ] {
            shown.push_str(&m.filter(d));
        }
        shown.push_str(&m.finish());
        assert_eq!(shown, "Let me check.\n\nDone.");
    }

    #[test]
    fn markup_mask_hides_invoke_without_wrapper() {
        let mut m = ToolMarkupMask::new();
        let shown: String = ["a<invoke name=\"x\"><parameter name=\"p\">1</parameter></invoke>b"]
            .iter()
            .map(|d| m.filter(d))
            .collect();
        let shown = format!("{shown}{}", m.finish());
        assert_eq!(shown, "ab");
    }

    #[test]
    fn markup_mask_ignores_stray_close_in_prose() {
        // A lone `</invoke>` with no opening block is prose, not markup.
        let mut m = ToolMarkupMask::new();
        let shown = format!("{}{}", m.filter("look at </invoke> here"), m.finish());
        assert_eq!(shown, "look at </invoke> here");
    }

    #[test]
    fn markup_mask_hides_dsml_tool_calls_wrapper() {
        // Real live output: model wraps the block in `<│DSML│tool_calls>`.
        let mut m = ToolMarkupMask::new();
        let shown: String = [
            "I'll search for trending AI news by fetching from a few live sources.\n\n",
            "<\u{ff5c}DSML\u{ff5c}tool_calls>\n",
            "<invoke name=\"web_search\"><parameter name=\"query\">trending AI news today</parameter></invoke>\n",
            "</\u{ff5c}DSML\u{ff5c}tool_calls>",
        ]
        .iter()
        .map(|d| m.filter(d))
        .collect();
        let shown = format!("{shown}{}", m.finish());
        assert_eq!(
            shown,
            "I'll search for trending AI news by fetching from a few live sources.\n\n"
        );
    }

    #[test]
    fn markup_mask_hides_empty_dsml_tool_calls_wrapper() {
        // Truncated/empty wrapper: nothing between the open and close tags.
        let mut m = ToolMarkupMask::new();
        let shown: String = [
            "a",
            "<\u{ff5c}DSML\u{ff5c}tool_calls>\n",
            "</\u{ff5c}DSML\u{ff5c}tool_calls>",
            "b",
        ]
        .iter()
        .map(|d| m.filter(d))
        .collect();
        let shown = format!("{shown}{}", m.finish());
        assert_eq!(shown, "ab");
    }
}
