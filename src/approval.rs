//! Approval flow (fx's `core/permissions/approval_*` + `permission_prompter`):
//! the typed request/decision pair that drives interactive permission
//! prompts, plus the display copy rendered at the terminal.

/// Outcome of a human approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow this one call.
    Allow,
    /// Allow this call and remember a grant for the same tool+scope
    /// ("don't ask again").
    AllowAlways { remember: bool },
    /// Refuse the call.
    Deny,
}

/// One permission decision the human must make.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool_name: String,
    /// Permission target (absolute path for file tools, command line for bash).
    pub target: String,
    /// Raw tool-call text for the preview (may be long; prompt truncates).
    pub input_text: String,
    /// Workspace root — used to render targets workspace-relative.
    pub workspace: std::path::PathBuf,
}

impl ApprovalRequest {
    /// Target rendered relative to the workspace when possible (`src/a.rs`
    /// instead of `/abs/ws/src/a.rs`) so prompts stay short.
    pub fn display_target(&self) -> String {
        let ws = self.workspace.to_string_lossy().to_string();
        let t = &self.target;
        if let Some(rel) = t.strip_prefix(&format!("{ws}/")) {
            if !rel.is_empty() {
                return rel.to_string();
            }
        }
        t.to_string()
    }

    /// 160-char preview of the raw input for the prompt.
    pub fn preview(&self) -> String {
        let body: String = self.input_text.chars().take(160).collect();
        if body.len() < self.input_text.len() {
            format!("{body}…")
        } else {
            body
        }
    }

    /// The prompt copy shown to the human. fn because it is cheap and keeps
    /// the UI string here with the domain type.
    pub fn prompt(&self) -> String {
        let target = self.display_target();
        let preview = self.preview();
        if preview.is_empty() {
            format!(
                "\x1b[33mƒ permission needed\x1b[0m: {} \x1b[90m{}\x1b[0m",
                self.tool_name, target
            )
        } else {
            format!(
                "\x1b[33mƒ permission needed\x1b[0m: {} \x1b[90m{}\x1b[0m\n  \x1b[2m{}\x1b[0m",
                self.tool_name, target, preview
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "run_command".into(),
            target: "/ws/tmp/x.sh".into(),
            input_text: "echo hello".into(),
            workspace: "/ws".into(),
        }
    }

    #[test]
    fn relative_target_when_inside_workspace() {
        assert_eq!(req().display_target(), "tmp/x.sh");
        let outside = ApprovalRequest {
            target: "/etc/passwd".into(),
            ..req()
        };
        assert_eq!(outside.display_target(), "/etc/passwd");
    }

    #[test]
    fn preview_truncates_long_input() {
        let long = ApprovalRequest {
            input_text: "x".repeat(500),
            ..req()
        };
        assert!(long.preview().ends_with('…'));
        assert!(long.preview().chars().count() <= 165);
    }

    #[test]
    fn prompt_includes_tool_and_target() {
        let p = req().prompt();
        assert!(p.contains("run_command"));
        assert!(p.contains("tmp/x.sh"));
    }
}
