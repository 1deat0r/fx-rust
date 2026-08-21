//! Tool preparation (fx's `core/agent/tool_preparation.zig`): normalize and
//! validate tool arguments before the permission gate / execution so tools
//! never see malformed input. Relative file paths are resolved against the
//! workspace; required fields are enforced with structured errors; obviously
//! wrong types are coerced or rejected.

use serde_json::{json, Value};

use crate::tools::{arg, ToolContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub args: Value,
    /// Missing/invalid input; callers should short-circuit with this error.
    pub error: Option<String>,
}

impl Prepared {
    fn ok(args: Value) -> Self {
        Self { args, error: None }
    }
    fn fail(msg: impl Into<String>) -> Self {
        Self {
            args: Value::Null,
            error: Some(msg.into()),
        }
    }
}

const PATH_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    "rename_file",
    "copy_file",
    "create_folder",
    "open_file",
    "file_info",
];

const PATH_KEYS: &[&str] = &[
    "file_path",
    "path",
    "folder_path",
    "old_path",
    "new_path",
    "source",
    "destination",
    "target",
];

/// True when `resolved` stays inside `root` after lexically normalizing `..`.
fn resolved_under_workspace(resolved: &std::path::Path, root: &std::path::Path) -> bool {
    let mut out = std::path::PathBuf::new();
    use std::path::Component;
    for comp in resolved.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.starts_with(root)
}

fn as_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Prepare a tool call's arguments in place. Returns a structured error when
/// required input is missing or unusable (the agent feeds that back to the
/// model instead of executing).
pub fn prepare(name: &str, args: &Value, ctx: &ToolContext) -> Prepared {
    // 1. Rewrite relative paths to workspace-anchored absolute paths.
    if PATH_TOOLS.contains(&name) {
        return prepare_paths(args, ctx);
    }
    // 2. Required-field checks per tool.
    match name {
        "run_command" => match as_str(args.get("command")) {
            Some(c) if !c.trim().is_empty() => Prepared::ok(args.clone()),
            Some(_) => Prepared::fail("run_command requires a non-empty `command`"),
            None => Prepared::fail("run_command requires a `command` string"),
        },
        "web_search" => match as_str(args.get("query")) {
            Some(q) if !q.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("web_search requires a non-empty `query`"),
        },
        "web_fetch" => match as_str(args.get("url")) {
            Some(u) if !u.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("web_fetch requires a `url` string"),
        },
        "semantic_search" => match as_str(args.get("query")) {
            Some(q) if !q.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("semantic_search requires a non-empty `query`"),
        },
        "memory" => Prepared::ok(args.clone()),
        "ask_user_question" => match as_str(args.get("question")) {
            Some(q) if !q.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("ask_user_question requires a `question` string"),
        },
        "skill" | "install_skill" => match as_str(args.get("name")) {
            Some(n) if !n.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("skill requires a `name` string"),
        },
        "subagent" => match as_str(args.get("prompt")) {
            Some(p) if !p.trim().is_empty() => Prepared::ok(args.clone()),
            _ => Prepared::fail("subagent requires a `prompt` string"),
        },
        "view_image" => match as_str(args.get("path")).or_else(|| as_str(args.get("file_path"))) {
            Some(p) if !p.trim().is_empty() => {
                // Absolute it, matching path tools.
                let mut a = args.clone();
                if let Some(obj) = a.as_object_mut() {
                    if let Some(v) = obj.get_mut("path") {
                        *v = json!(ctx.resolve(v.as_str().unwrap_or("")).display().to_string());
                    }
                }
                Prepared::ok(a)
            }
            _ => Prepared::fail("view_image requires a `path` string"),
        },
        _ if name.starts_with("mcp__") => {
            // MCP tools get their own validation server-side; pass through.
            Prepared::ok(args.clone())
        }
        _ => Prepared::ok(args.clone()),
    }
}

fn prepare_paths(args: &Value, ctx: &ToolContext) -> Prepared {
    let Some(obj) = args.as_object() else {
        return Prepared::fail("file tools require an object of arguments");
    };
    let mut out = serde_json::Map::<String, Value>::new();
    let mut escapes = false;
    for (k, v) in obj {
        if PATH_KEYS.contains(&k.as_str()) {
            if let Some(s) = v.as_str() {
                let resolved = ctx.resolve(s);
                // Lexical escape guard: `..` components must not leave the
                // workspace (defense in depth alongside the sandbox check).
                if !resolved_under_workspace(&resolved, &ctx.workspace) {
                    escapes = true;
                }
                out.insert(k.clone(), json!(resolved.display().to_string()));
                continue;
            }
        }
        out.insert(k.clone(), v.clone());
    }
    if escapes {
        return Prepared::fail("file path escapes the workspace");
    }
    let a = Value::Object(out);
    // Required path presence.
    let has_path = PATH_KEYS.iter().any(|k| {
        a.get(*k)
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    if has_path {
        Prepared::ok(a)
    } else {
        let _ = arg(args, "file_path"); // silence unused-arg lint on helper
        Prepared::fail(format!(
            "{name_placeholder} requires a file path (file_path/path/folder_path)",
            name_placeholder = "this tool"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx(ws: &str) -> ToolContext {
        ToolContext {
            workspace: PathBuf::from(ws),
            max_result_bytes: 1024,
            interactive: false,
            config: Arc::new(Config {
                workspace: ws.into(),
                model: "m".into(),
                permission_mode: crate::permissions::PermissionMode::Auto,
                max_agent_steps: 0,
                max_tool_result_bytes: 65536,
                first_call_tool_choice: crate::config::FirstCallToolChoice::Auto,
                context: true,
                sandbox: crate::config::SandboxMode::None,
                permission_rules: Default::default(),
                settings_path: None,
                additional_directories: vec![],
                mcp_servers: vec![],
                context_limits: crate::context::ContextLimits::default(),
                input_appearance: "auto".into(),
                presentation_mode: "default".into(),
                update_channel: "stable".into(),
            }),
            store: crate::sessions::SessionStore::new().unwrap(),
            session_id: String::new(),
        }
    }

    #[test]
    fn relative_paths_absolutize() {
        let c = ctx("/ws");
        let p = prepare("read_file", &json!({"file_path": "src/main.rs"}), &c);
        assert!(p.error.is_none(), "{:?}", p.error);
        assert_eq!(p.args["file_path"], json!("/ws/src/main.rs"));
    }

    #[test]
    fn required_fields_enforced() {
        let c = ctx("/ws");
        assert!(prepare("run_command", &json!({}), &c).error.is_some());
        assert!(prepare("web_search", &json!({"query": ""}), &c)
            .error
            .is_some());
        assert!(prepare("semantic_search", &json!({}), &c).error.is_some());
        assert!(prepare("run_command", &json!({"command": "ls"}), &c)
            .error
            .is_none());
    }

    #[test]
    fn mcp_passthrough() {
        let c = ctx("/ws");
        let p = prepare("mcp__srv__tool", &json!({"foo": 1}), &c);
        assert!(p.error.is_none());
        assert_eq!(p.args["foo"], json!(1));
    }

    #[test]
    fn workspace_escape_rejected() {
        let c = ctx("/ws");
        let p = prepare("read_file", &json!({"file_path": "../../etc/passwd"}), &c);
        assert!(p.error.is_some(), "escape should fail, got {:?}", p.error);
        assert!(p.error.as_deref().unwrap().contains("escapes"));
        // Same-directory .. is fine.
        let q = prepare("read_file", &json!({"file_path": "a/../b.txt"}), &c);
        assert!(q.error.is_none());
    }

    #[test]
    fn path_renames_absolutize_all_keys() {
        let c = ctx("/ws");
        let p = prepare(
            "rename_file",
            &json!({"old_path": "a.txt", "new_path": "b.txt"}),
            &c,
        );
        assert_eq!(p.args["old_path"], json!("/ws/a.txt"));
        assert_eq!(p.args["new_path"], json!("/ws/b.txt"));
    }
}
