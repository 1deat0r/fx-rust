//! Modes — faithful port of upstream `core/modes/*` + `builtins/modes.zig`.
//!
//! A *mode* is a named runtime profile (`id`, display name, description)
//! that pins a permission mode and, optionally, a tool policy. The registry
//! defaults to the built-in `ask` mode; `code` (auto permission, full tools)
//! and `ask` (ask permission, full tools) are the only built-ins upstream
//! ships at v0.0.5. A read-only tool policy restricts the gateway tool
//! projection to a supplied read-only name set and blocks denied tool calls
//! with a structured pre-tool-use-denied error.

use serde_json::{json, Value};

use crate::permissions::PermissionMode;

/// Tool exposure policy for a mode (upstream `mode_contract.ToolPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    Full,
    ReadOnly,
}

/// One named runtime profile (upstream `mode_contract.ModeSpec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub permission_mode: PermissionMode,
    pub tool_policy: ToolPolicy,
    pub tool_policy_denial_message: Option<&'static str>,
}

/// The mode registry (upstream `mode_registry.Registry`).
#[derive(Debug, Clone)]
pub struct Registry {
    pub default_mode_id: &'static str,
    pub modes: Vec<ModeSpec>,
}

impl Registry {
    pub fn lookup(&self, id: &str) -> Option<&ModeSpec> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// Upstream `toolAllowed`: a tool outside the supplied set, a missing
    /// mode, or a full-policy mode always allows; a read-only mode allows
    /// only names in `read_only_tool_names`.
    pub fn tool_allowed(
        &self,
        tool_set_registry_has: impl Fn(&str) -> bool,
        id: &str,
        tool_name: &str,
        read_only_tool_names: &[&str],
    ) -> bool {
        if !tool_set_registry_has(tool_name) {
            return true;
        }
        let Some(mode) = self.lookup(id) else {
            return true;
        };
        match mode.tool_policy {
            ToolPolicy::Full => true,
            ToolPolicy::ReadOnly => read_only_tool_names.contains(&tool_name),
        }
    }

    /// Upstream `toolPolicyDeniedJson`: structured pre-tool-use denial.
    pub fn tool_policy_denied_json(&self, id: &str, tool_name: &str) -> Option<Value> {
        self.lookup(id)?;
        let reason = self
            .lookup(id)
            .and_then(|m| m.tool_policy_denial_message)
            .unwrap_or("Tool blocked by the active mode policy.");
        Some(blocked_tool_json(tool_name, reason))
    }
}

/// Structured pre-tool-use blocked result (ports `preToolUseBlockedJson`).
pub fn blocked_tool_json(tool_name: &str, reason: &str) -> Value {
    json!({
        "error": {
            "type": "tool_execution_failed",
            "tool_name": tool_name,
            "message": reason,
            "suggestion": "Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative."
        }
    })
}

pub const DEFAULT_MODE_ID: &str = "ask";

/// The read-only name set used by a read-only tool policy. Mirrors the
/// upstream gateway set's inspection tools.
pub const READ_ONLY_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_files",
    "glob_files",
    "grep_files",
    "file_info",
    "web_search",
    "web_fetch",
    "semantic_search",
    "memory",
    "skill",
    "read_tool_result",
];

/// The built-in registry: exact upstream `builtins/modes.zig` order.
pub fn builtin_registry() -> Registry {
    Registry {
        default_mode_id: DEFAULT_MODE_ID,
        modes: vec![
            ModeSpec {
                id: "code",
                name: "Code",
                description: "Write and modify code with full tool access",
                permission_mode: PermissionMode::Auto,
                tool_policy: ToolPolicy::Full,
                tool_policy_denial_message: None,
            },
            ModeSpec {
                id: "ask",
                name: "Ask",
                description: "Request permission before making any changes",
                permission_mode: PermissionMode::Ask,
                tool_policy: ToolPolicy::Full,
                tool_policy_denial_message: None,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_modes_register_exact_order_and_permission_policy() {
        let registry = builtin_registry();
        let ids: Vec<&str> = registry.modes.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["code", "ask"]);
        assert_eq!(registry.default_mode_id, "ask");
        assert_eq!(
            registry.lookup("code").unwrap().permission_mode,
            PermissionMode::Auto
        );
        assert_eq!(
            registry.lookup("ask").unwrap().permission_mode,
            PermissionMode::Ask
        );
        assert_eq!(
            registry.lookup("code").unwrap().tool_policy,
            ToolPolicy::Full
        );
        assert!(registry.lookup("unknown").is_none());
    }

    #[test]
    fn mode_registry_looks_up_modes_by_id() {
        let modes = vec![
            ModeSpec {
                id: "ask",
                name: "Ask",
                description: "",
                permission_mode: PermissionMode::Ask,
                tool_policy: ToolPolicy::Full,
                tool_policy_denial_message: None,
            },
            ModeSpec {
                id: "code",
                name: "Code",
                description: "",
                permission_mode: PermissionMode::Auto,
                tool_policy: ToolPolicy::Full,
                tool_policy_denial_message: None,
            },
        ];
        let registry = Registry {
            default_mode_id: "ask",
            modes,
        };
        assert_eq!(registry.lookup("code").unwrap().name, "Code");
        assert!(registry.lookup("missing").is_none());
    }

    #[test]
    fn mode_registry_applies_tool_policy_to_the_supplied_tool_set() {
        let modes = vec![
            ModeSpec {
                id: "full",
                name: "Full",
                description: "",
                permission_mode: PermissionMode::Ask,
                tool_policy: ToolPolicy::Full,
                tool_policy_denial_message: None,
            },
            ModeSpec {
                id: "inspect",
                name: "Inspect",
                description: "",
                permission_mode: PermissionMode::Ask,
                tool_policy: ToolPolicy::ReadOnly,
                tool_policy_denial_message: Some("Inspection mode blocks mutations."),
            },
        ];
        let registry = Registry {
            default_mode_id: "full",
            modes,
        };
        let read_only_names = ["inspect"];
        let has = |name: &str| matches!(name, "inspect" | "mutate");

        assert!(registry.tool_allowed(has, "full", "mutate", &read_only_names));
        assert!(registry.tool_allowed(has, "inspect", "inspect", &read_only_names));
        assert!(!registry.tool_allowed(has, "inspect", "mutate", &read_only_names));
        // Missing mode: everything allowed.
        assert!(registry.tool_allowed(has, "missing", "mutate", &read_only_names));
        // Tool outside the supplied set: allowed (MCP/unknown names).
        assert!(registry.tool_allowed(has, "inspect", "dynamic_tool", &read_only_names));

        let denied = registry
            .tool_policy_denied_json("inspect", "mutate")
            .expect("denial json");
        assert!(denied
            .to_string()
            .contains("Inspection mode blocks mutations."));
        assert!(registry
            .tool_policy_denied_json("missing", "mutate")
            .is_none());
    }

    #[test]
    fn blocked_json_uses_the_upstream_error_shape() {
        let v = blocked_tool_json("run_command", "Tool blocked by the active mode policy.");
        assert_eq!(v["error"]["type"], "tool_execution_failed");
        assert_eq!(v["error"]["tool_name"], "run_command");
        assert_eq!(
            v["error"]["message"],
            "Tool blocked by the active mode policy."
        );
        assert!(v["error"]["suggestion"]
            .as_str()
            .unwrap()
            .contains("Do not retry"));
    }
}
