//! Permission runtime, modeled on fx's four-gate decision:
//!   1. Whether the tool needs approval at all (sensitive tools set).
//!   2. Deny/allow rules matched against tool + target.
//!   3. Session grants created earlier in the session ("don't ask again").
//!   4. The permission mode, deciding unresolved calls (ask / auto / yolo).
//!
//! Modes:
//!   ask  -> prompt before unresolved sensitive tool calls.
//!   auto -> apply rules, then automatically review unresolved calls
//!           (default). Unresolved review falls back to human approval in
//!           interactive mode; in noninteractive mode the call is blocked.
//!   yolo -> disable permission checks, no sandbox.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Ask,
    Auto,
    Yolo,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ask" => Ok(Self::Ask),
            "auto" => Ok(Self::Auto),
            "yolo" => Ok(Self::Yolo),
            other => bail!("invalid permission mode: {other} (expected ask, auto, or yolo)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Allow,
    Deny,
    Ask,
}

pub fn parse_rule(s: &str) -> Result<Rule> {
    match s {
        "allow" => Ok(Rule::Allow),
        "deny" => Ok(Rule::Deny),
        "ask" => Ok(Rule::Ask),
        other => bail!("invalid rule: {other} (expected allow, deny, or ask)"),
    }
}

/// A rule attached to a permission key: either the whole tool or a set of
/// target glob patterns (paths for file tools, command prefixes for bash).
#[derive(Debug, Clone)]
pub enum ToolRule {
    Whole(Rule),
    Patterns(Vec<(String, Rule)>),
}

/// Coarse permission key -> fine tool name mapping (fx's naming).
pub fn tool_kind(tool_name: &str) -> &str {
    match tool_name {
        "write_file" | "edit_file" | "delete_file" | "rename_file" | "copy_file"
        | "create_folder" | "open_file" => "edit",
        "run_command" => "bash",
        "read_file" | "list_files" | "glob_files" | "grep_files" | "file_info" => "read",
        "web_search" | "web_fetch" => "web",
        "memory" => "memory",
        "install_skill" => "skill",
        "vision" => "vision",
        other => other,
    }
}

/// Sensitive tools that require approval when no rule/grant resolves.
pub fn needs_approval(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "write_file"
            | "edit_file"
            | "delete_file"
            | "rename_file"
            | "copy_file"
            | "create_folder"
            | "run_command"
            | "open_file"
            | "install_skill"
            | "vision"
    )
}

/// Outcome of the permission gate for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
    /// No rule or grant resolved; the caller must consult the mode
    /// (human prompt or automatic review).
    Unresolved,
}

pub struct PermissionRequest<'a> {
    pub tool_name: &'a str,
    /// Target for rule matching: absolute path for file tools, command text
    /// for run_command.
    pub target: &'a str,
    /// The raw tool call input (command string etc.) for review/prompt text.
    pub input_text: String,
    pub workspace: &'a Path,
}

static DENY_TOOL: &str = "denied by rule";
static DENY_MISSING_GRANT: &str = "no session grant";

#[derive(Debug, Default)]
pub struct GrantStore {
    /// (tool kind, target glob) entries created by "don't ask again".
    grants: Vec<(String, String)>,
}

impl GrantStore {
    pub fn allow(&mut self, tool: &str, target: &str) {
        // A granular target can follow a prefix grant; most specific match later
        // in the vector wins when inserted afterwards.
        self.grants.push((tool.to_string(), target.to_string()));
    }
    pub fn reset(&mut self) {
        self.grants.clear();
    }
    pub fn is_allowed(&self, tool: &str, target: &str) -> bool {
        self.grants
            .iter()
            .any(|(t, pattern)| t == tool && glob_matches(pattern, target))
    }
}

fn glob_matches(pattern: &str, target: &str) -> bool {
    // Treat the target as a path or command string; "*" sugar common in rules.
    let pat = if pattern.ends_with('/') && target_starts_with_dir(pattern, target) {
        return target_starts_with_dir(pattern, target);
    } else {
        pattern
    };
    match Glob::new(pat) {
        Ok(g) => g.compile_matcher().is_match(target),
        Err(_) => pattern == target,
    }
}

fn target_starts_with_dir(pattern: &str, target: &str) -> bool {
    let prefix = pattern.trim_end_matches('/');
    target == prefix || target.starts_with(&format!("{prefix}/"))
}

/// The permission engine: rules are matched most-specifically; a deny always
/// wins over an allow for an overlapping target, and session grants override
/// static rules (they represent explicit human confirmation).
pub struct Permissions {
    pub mode: PermissionMode,
    pub rules: BTreeMap<String, ToolRule>,
    pub grants: GrantStore,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Auto,
            rules: BTreeMap::new(),
            grants: GrantStore::default(),
        }
    }
}

fn matches_rule(rule: &ToolRule, target: &str) -> Option<Rule> {
    match rule {
        ToolRule::Whole(r) => Some(*r),
        ToolRule::Patterns(patterns) => {
            let mut matched: Option<Rule> = None;
            for (pattern, r) in patterns {
                if glob_matches(pattern, target) {
                    matched = Some(*r);
                }
            }
            matched
        }
    }
}

impl Permissions {
    pub fn new(mode: PermissionMode, rules: BTreeMap<String, ToolRule>) -> Self {
        Self { mode, rules, grants: GrantStore::default() }
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Decides whether a tool call may execute.
    pub fn decide(&self, req: &PermissionRequest) -> Decision {
        if self.mode == PermissionMode::Yolo {
            return Decision::Allow;
        }
        let kind = tool_kind(req.tool_name);
        let target = req.target;

        // 1. Session grants: explicit human confirmations first.
        if self.grants.is_allowed(&kind.to_string(), target) {
            return Decision::Allow;
        }
        // Also match the raw tool name via grants (for bash exact commands).
        if self.grants.is_allowed(req.tool_name, target) {
            return Decision::Allow;
        }

        // 2. Static rules (deny wins over allow on overlap).
        let mut matched_deny = false;
        let mut matched_allow = false;
        if let Some(rule) = self.rules.get("*") {
            match matches_rule(rule, target) {
                Some(Rule::Allow) => matched_allow = true,
                Some(Rule::Deny) => matched_deny = true,
                _ => {}
            }
        }
        for key in [kind, req.tool_name] {
            if let Some(rule) = self.rules.get(key) {
                match matches_rule(rule, target) {
                    Some(Rule::Allow) => matched_allow = true,
                    Some(Rule::Deny) => matched_deny = true,
                    _ => {}
                }
            }
        }
        if matched_deny {
            return Decision::Deny(DENY_TOOL);
        }
        if matched_allow {
            return Decision::Allow;
        }

        // 3. Non-sensitive tools: always allowed.
        if !needs_approval(req.tool_name) {
            return Decision::Allow;
        }

        // 4. Unresolved sensitive call -> mode decides.
        match self.mode {
            PermissionMode::Yolo => Decision::Allow,
            PermissionMode::Ask | PermissionMode::Auto => Decision::Unresolved,
        }
    }

    /// Present a human prompt for an unresolved call. Returns whether to run.
    /// `interactive` is false for non-TTY contexts where we must not wait.
    pub fn human_decide(
        &mut self,
        req: &PermissionRequest,
        interactive: bool,
        approve_all: impl FnOnce(&mut Self, &PermissionRequest),
        deny: impl FnOnce(),
    ) -> bool {
        if !interactive {
            return false;
        }
        eprintln!();
        eprintln!(
            "\x1b[33m⚠ Permission needed\x1b[0m: {} — {}\x1b[0m",
            req.tool_name, req.input_text
        );
        loop {
            let mut line = String::new();
            match std::io::Write::flush(&mut std::io::stdout()) {
                Ok(_) => {}
                Err(_) => return false,
            }
            eprint!("\x1b[33mAllow?\x1b[0m (y)es / (n)o / (a)lways for this scope: ");
            use std::io::BufRead;
            let _ = std::io::stdin().lock().read_line(&mut line);
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return true,
                "a" | "always" | "yesall" => {
                    approve_all(self, req);
                    return true;
                }
                "n" | "no" => {
                    deny();
                    return false;
                }
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, target: &str, ws: &str) -> PermissionRequest<'_> {
        PermissionRequest {
            tool_name: tool,
            target,
            input_text: target.to_string(),
            workspace: Path::new(ws),
        }
    }

    #[test]
    fn read_tools_are_always_allowed_in_default_policy() {
        let p = Permissions::default();
        assert_eq!(p.decide(&req("read_file", "/ws/src/main.rs", "/ws")), Decision::Allow);
        assert_eq!(p.decide(&req("glob_files", "/ws/**", "/ws")), Decision::Allow);
        assert_eq!(p.decide(&req("web_search", "x", "/ws")), Decision::Allow);
    }

    #[test]
    fn sensitive_tools_are_unresolved_by_default() {
        let p = Permissions::default();
        assert_eq!(p.decide(&req("run_command", "rm -rf /", "/ws")), Decision::Unresolved);
        assert_eq!(p.decide(&req("write_file", "/ws/a.txt", "/ws")), Decision::Unresolved);
    }

    #[test]
    fn edit_pattern_rules_apply() {
        let mut rules = BTreeMap::new();
        rules.insert(
            "edit".to_string(),
            ToolRule::Patterns(vec![
                ("docs/*".to_string(), Rule::Allow),
                ("*".to_string(), Rule::Deny),
            ]),
        );
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(p.decide(&req("edit_file", "/ws/docs/readme.md", "/ws")), Decision::Allow);
        assert_eq!(p.decide(&req("edit_file", "/ws/src/main.rs", "/ws")), Decision::Deny(DENY_TOOL));
    }

    #[test]
    fn bash_prefix_rule_allows_git_commands() {
        let mut rules = BTreeMap::new();
        rules.insert(
            "bash".to_string(),
            ToolRule::Patterns(vec![("git *".to_string(), Rule::Allow)]),
        );
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(p.decide(&req("run_command", "git status", "/ws")), Decision::Allow);
        assert_eq!(p.decide(&req("run_command", "npm install", "/ws")), Decision::Unresolved);
    }

    #[test]
    fn deny_wins_over_allow() {
        let mut rules = BTreeMap::new();
        rules.insert(
            "bash".to_string(),
            ToolRule::Patterns(vec![
                ("git *".to_string(), Rule::Allow),
                ("git push *".to_string(), Rule::Deny),
            ]),
        );
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(p.decide(&req("run_command", "git push origin main", "/ws")), Decision::Deny(DENY_TOOL));
        assert_eq!(p.decide(&req("run_command", "git status", "/ws")), Decision::Allow);
    }

    #[test]
    fn star_rule_applies_to_all_sensitive_tools() {
        let mut rules = BTreeMap::new();
        rules.insert("*".to_string(), ToolRule::Whole(Rule::Ask));
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(p.decide(&req("write_file", "/ws/a", "/ws")), Decision::Unresolved);
    }

    #[test]
    fn session_grant_resolves_unresolved() {
        let mut p = Permissions::default();
        p.grants.allow("bash", "npm *");
        assert_eq!(p.decide(&req("run_command", "npm install", "/ws")), Decision::Allow);
        assert_eq!(p.decide(&req("run_command", "rm -rf /", "/ws")), Decision::Unresolved);
    }

    #[test]
    fn directory_grant_sugar() {
        let mut p = Permissions::default();
        p.grants.allow("edit", "/ws/");
        assert_eq!(p.decide(&req("edit_file", "/ws/src/main.rs", "/ws")), Decision::Allow);
    }
}
