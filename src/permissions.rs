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

use anyhow::{bail, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Ask,
    Auto,
    Yolo,
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        };
        f.write_str(s)
    }
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
        "run_command" | "background_process" | "terminal" | "browser_terminal" => "bash",
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
            | "background_process"
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

#[allow(dead_code)] // input_text kept for reviewer/prompt text
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
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.grants.clear();
    }
    pub fn is_allowed(&self, tool: &str, target: &str) -> bool {
        self.grants
            .iter()
            .any(|(t, pattern)| t == tool && glob_matches(pattern, target, None))
    }
}

fn glob_matches(pattern: &str, target: &str, rel_target: Option<&str>) -> bool {
    // Directory sugar: pattern "docs/" matches the tree under docs/.
    if pattern.ends_with('/') && target_starts_with_dir(pattern, target) {
        return true;
    }
    if pattern.ends_with('/') {
        if let Some(rel) = rel_target {
            if target_starts_with_dir(pattern, rel) {
                return true;
            }
        }
    }
    let ok_abs = match Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(target),
        Err(_) => pattern == target,
    };
    if ok_abs {
        return true;
    }
    if let Some(rel) = rel_target {
        match Glob::new(pattern) {
            Ok(g) => g.compile_matcher().is_match(rel),
            Err(_) => pattern == rel,
        }
    } else {
        false
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

struct Candidate {
    spec: usize,
    key_rank: u8,
    rule: Rule,
}

fn consider(best: &mut Option<Candidate>, cand: Candidate) {
    let replace = match best {
        None => true,
        Some(b) => {
            (cand.spec, cand.key_rank, rule_priority(cand.rule))
                > (b.spec, b.key_rank, rule_priority(b.rule))
        }
    };
    if replace {
        *best = Some(cand);
    }
}

fn rule_priority(r: Rule) -> u8 {
    match r {
        Rule::Deny => 2,
        Rule::Ask => 1,
        Rule::Allow => 0,
    }
}

impl Permissions {
    pub fn new(mode: PermissionMode, rules: BTreeMap<String, ToolRule>) -> Self {
        Self {
            mode,
            rules,
            grants: GrantStore::default(),
        }
    }

    #[allow(dead_code)]
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Decides whether a tool call may execute.
    pub fn decide(&self, req: &PermissionRequest) -> Decision {
        let kind = tool_kind(req.tool_name);
        if self.mode == PermissionMode::Yolo {
            return Decision::Allow;
        }
        let sensitive = needs_approval(req.tool_name);
        if !sensitive
            && !self.rules.contains_key("*")
            && !self.rules.contains_key(kind)
            && !self.rules.contains_key(req.tool_name)
        {
            return Decision::Allow;
        }

        let rel_target = req
            .target
            .strip_prefix(&req.workspace.to_string_lossy().to_string())
            .map(|r| r.trim_start_matches('/'))
            .filter(|r| !r.is_empty());

        // Best candidate: patterns with higher specificity win; on equal
        // specificity, the more specific rule key wins (tool name > kind > "*");
        // ties on both resolve to deny first, then ask, then allow.
        let mut best: Option<Candidate> = None;
        for (key, key_rank) in [("*", 0u8), (kind, 1u8), (req.tool_name, 2u8)] {
            if let Some(rule) = self.rules.get(key) {
                match rule {
                    ToolRule::Whole(r) => {
                        consider(
                            &mut best,
                            Candidate {
                                spec: 0,
                                key_rank,
                                rule: *r,
                            },
                        );
                    }
                    ToolRule::Patterns(patterns) => {
                        for (pattern, r) in patterns {
                            if glob_matches(pattern, req.target, rel_target) {
                                consider(
                                    &mut best,
                                    Candidate {
                                        spec: pattern.chars().count(),
                                        key_rank,
                                        rule: *r,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        // Session grants resolve unresolved calls ("don't ask again").
        let granted = self.grants.is_allowed(req.tool_name, req.target)
            || self.grants.is_allowed(kind, req.target);
        if let Some(c) = &best {
            if matches!(c.rule, Rule::Ask) && granted {
                return Decision::Allow;
            }
        } else if granted {
            return Decision::Allow;
        }

        match best {
            Some(Candidate {
                rule: Rule::Allow, ..
            }) => Decision::Allow,
            Some(Candidate {
                rule: Rule::Deny, ..
            }) => Decision::Deny(DENY_TOOL),
            Some(Candidate {
                rule: Rule::Ask, ..
            })
            | None
                if sensitive =>
            {
                Decision::Unresolved
            }
            Some(Candidate {
                rule: Rule::Ask, ..
            })
            | None => Decision::Allow,
        }
    }

    #[allow(dead_code)]
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

    fn req<'a>(tool: &'a str, target: &'a str, ws: &'a str) -> PermissionRequest<'a> {
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
        assert_eq!(
            p.decide(&req("read_file", "/ws/src/main.rs", "/ws")),
            Decision::Allow
        );
        assert_eq!(
            p.decide(&req("glob_files", "/ws/**", "/ws")),
            Decision::Allow
        );
        assert_eq!(p.decide(&req("web_search", "x", "/ws")), Decision::Allow);
    }

    #[test]
    fn sensitive_tools_are_unresolved_by_default() {
        let p = Permissions::default();
        assert_eq!(
            p.decide(&req("run_command", "rm -rf /", "/ws")),
            Decision::Unresolved
        );
        assert_eq!(
            p.decide(&req("write_file", "/ws/a.txt", "/ws")),
            Decision::Unresolved
        );
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
        assert_eq!(
            p.decide(&req("edit_file", "/ws/docs/readme.md", "/ws")),
            Decision::Allow
        );
        assert_eq!(
            p.decide(&req("edit_file", "/ws/src/main.rs", "/ws")),
            Decision::Deny(DENY_TOOL)
        );
    }

    #[test]
    fn bash_prefix_rule_allows_git_commands() {
        let mut rules = BTreeMap::new();
        rules.insert(
            "bash".to_string(),
            ToolRule::Patterns(vec![("git *".to_string(), Rule::Allow)]),
        );
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(
            p.decide(&req("run_command", "git status", "/ws")),
            Decision::Allow
        );
        assert_eq!(
            p.decide(&req("run_command", "npm install", "/ws")),
            Decision::Unresolved
        );
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
        assert_eq!(
            p.decide(&req("run_command", "git push origin main", "/ws")),
            Decision::Deny(DENY_TOOL)
        );
        assert_eq!(
            p.decide(&req("run_command", "git status", "/ws")),
            Decision::Allow
        );
    }

    #[test]
    fn star_rule_applies_to_all_sensitive_tools() {
        let mut rules = BTreeMap::new();
        rules.insert("*".to_string(), ToolRule::Whole(Rule::Ask));
        let p = Permissions::new(PermissionMode::Ask, rules);
        assert_eq!(
            p.decide(&req("write_file", "/ws/a", "/ws")),
            Decision::Unresolved
        );
    }

    #[test]
    fn session_grant_resolves_unresolved() {
        let mut p = Permissions::default();
        p.grants.allow("bash", "npm *");
        assert_eq!(
            p.decide(&req("run_command", "npm install", "/ws")),
            Decision::Allow
        );
        assert_eq!(
            p.decide(&req("run_command", "rm -rf /", "/ws")),
            Decision::Unresolved
        );
    }

    #[test]
    fn directory_grant_sugar() {
        let mut p = Permissions::default();
        p.grants.allow("edit", "/ws/");
        assert_eq!(
            p.decide(&req("edit_file", "/ws/src/main.rs", "/ws")),
            Decision::Allow
        );
    }
}

// ------------------------------------------------------------------ sandbox

/// Set of directories a tool may act on. When sandbox mode is `None`
/// (`sandbox: none`) every path is allowed by the sandbox (the permission
/// gates still apply). `Auto` sandbox confines to the workspace plus any
/// additional directories from config; `Os` confines even harder (nothing
/// outside the workspace is writable).
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub mode: crate::config::SandboxMode,
    pub workspace: std::path::PathBuf,
    pub additional: Vec<std::path::PathBuf>,
}

impl Sandbox {
    pub fn allows(&self, target: &str) -> bool {
        if self.mode == crate::config::SandboxMode::None {
            return true;
        }
        let p = std::path::Path::new(target);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.workspace.join(p)
        };
        // Lexically resolve `.`/`..`; paths that escape the mount point are
        // not in the sandbox. Without this, `/ws/../../etc/passwd` passes
        // a naive `starts_with("/ws")` check.
        let Some(abs) = sandbox_normalize(&abs) else {
            return false;
        };
        let mut base =
            std::iter::once(self.workspace.clone()).chain(self.additional.iter().cloned());
        base.any(|allowed| abs.starts_with(&allowed))
    }
}

/// Resolve `.` / `..` components lexically. Returns `None` when the path
/// would escape the root (a parent dir above `/`).
fn sandbox_normalize(p: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

// ------------------------------------------------------------ auto classifier

/// Deterministic decision for unresolved calls in `auto` mode. Keeps the fast
/// common cases (read-only bash, in-sandbox edits) moving without a prompt;
/// dangerous/unknown calls fall back to the review stage or are denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoDecision {
    Allow,
    Deny(&'static str),
    /// The classifier cannot decide: defer to the model reviewer (or human).
    Undetermined,
}

/// Classify an unresolved tool call under `auto` mode, using the sandbox for
/// path checks and the shell classifier for command effects.
pub fn auto_classify(req: &PermissionRequest, sandbox: &Sandbox) -> AutoDecision {
    let tool = req.tool_name;
    if tool.starts_with("mcp__") {
        // The human configured this server; trust published tools.
        return AutoDecision::Allow;
    }
    match tool_kind(tool) {
        "read" => AutoDecision::Allow,
        "memory" | "question" | "vision" | "subagent" => AutoDecision::Allow,
        "web" => match tool {
            "web_search" | "web_fetch" => AutoDecision::Allow,
            _ => AutoDecision::Undetermined,
        },
        "edit" => {
            if sandbox.allows(req.target) {
                AutoDecision::Allow
            } else {
                AutoDecision::Deny("edits outside the sandbox are not auto-approved")
            }
        }
        "skill" => AutoDecision::Deny("install_skill is not auto-approved"),
        "bash" => auto_classify_bash(req, sandbox),
        _ => AutoDecision::Undetermined,
    }
}

fn auto_classify_bash(req: &PermissionRequest, sandbox: &Sandbox) -> AutoDecision {
    use crate::shell_command::{classify, CommandClass};
    let eff = classify(req.target);
    match eff.class {
        CommandClass::ReadOnly => AutoDecision::Allow,
        CommandClass::Write => {
            if eff.destructive {
                return AutoDecision::Deny("destructive command is not auto-approved");
            }
            // Writes must stay inside the sandbox.
            for p in &eff.paths {
                let expanded = p.strip_prefix('~').unwrap_or(p);
                let path = std::path::Path::new(expanded);
                if path.is_absolute() && !sandbox.allows(expanded) {
                    return AutoDecision::Deny("command writes outside the sandbox");
                }
            }
            AutoDecision::Allow
        }
        CommandClass::Network => {
            // Read-only network (curl GET, git fetch/pull) is fine; pushes,
            // POST bodies, and writes that land outside the sandbox are not.
            if eff.destructive {
                AutoDecision::Deny("network write/push is not auto-approved")
            } else if eff.writes {
                let outside = eff.paths.iter().any(|p| {
                    let expanded = p.strip_prefix('~').unwrap_or(p);
                    std::path::Path::new(expanded).is_absolute() && !sandbox.allows(expanded)
                });
                if outside {
                    AutoDecision::Deny("network command writes outside the sandbox")
                } else {
                    AutoDecision::Allow
                }
            } else if eff.raw.is_empty() {
                AutoDecision::Deny("network command is not auto-approved")
            } else {
                AutoDecision::Allow
            }
        }
        CommandClass::Package => {
            AutoDecision::Deny("package/tool installation is not auto-approved")
        }
        CommandClass::Interactive => {
            AutoDecision::Deny("interactive/long-running process is not auto-approved")
        }
        CommandClass::Dangerous => AutoDecision::Deny("dangerous command is not auto-approved"),
        CommandClass::Unknown => AutoDecision::Undetermined,
    }
}

#[cfg(test)]
mod test_auto {
    use super::*;
    use crate::config::SandboxMode;

    fn req<'a>(tool: &'a str, target: &'a str, ws: &'a str) -> PermissionRequest<'a> {
        PermissionRequest {
            tool_name: tool,
            target,
            input_text: String::new(),
            workspace: std::path::Path::new(ws),
        }
    }

    fn sb(ws: &str) -> Sandbox {
        Sandbox {
            mode: SandboxMode::Auto,
            workspace: std::path::PathBuf::from(ws),
            additional: vec![std::path::PathBuf::from("/tmp/shared")],
        }
    }

    #[test]
    fn sandbox_allows_workspace_and_additional() {
        let s = sb("/ws");
        assert!(s.allows("/ws/src/main.rs"));
        assert!(s.allows("/tmp/shared/x.txt"));
        assert!(!s.allows("/etc/passwd"));
    }

    #[test]
    fn sandbox_rejects_parentdir_escape() {
        // /ws/../../etc/passwd must NOT satisfy the lexical starts_with check.
        let s = sb("/ws");
        assert!(!s.allows("/ws/../../etc/passwd"));
        assert!(!s.allows("../etc/passwd"));
        assert!(s.allows("/ws/a/../b.txt")); // stays inside after normalization
        assert!(s.allows("/ws"));
        assert!(s.allows("/ws/src/x.rs"));
    }

    #[test]
    fn sandbox_none_allows_everything() {
        let s = Sandbox {
            mode: SandboxMode::None,
            workspace: "/ws".into(),
            additional: vec![],
        };
        assert!(s.allows("/etc/passwd"));
    }

    #[test]
    fn read_tools_allowed_in_auto() {
        let s = sb("/ws");
        assert_eq!(
            auto_classify(&req("read_file", "/ws/a.txt", "/ws"), &s),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("web_search", "x", "/ws"), &s),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("memory", "", "/ws"), &s),
            AutoDecision::Allow
        );
    }

    #[test]
    fn edits_inside_sandbox_allowed_outside_denied() {
        let s = sb("/ws");
        assert_eq!(
            auto_classify(&req("write_file", "/ws/a.txt", "/ws"), &s),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("delete_file", "/etc/passwd", "/ws"), &s),
            AutoDecision::Deny("edits outside the sandbox are not auto-approved")
        );
    }

    #[test]
    fn bash_readonly_allowed() {
        let s = sb("/ws");
        assert_eq!(
            auto_classify(&req("run_command", "git status", "/ws"), &s),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("run_command", "ls -la", "/ws"), &s),
            AutoDecision::Allow
        );
    }

    #[test]
    fn bash_writes_confined() {
        let s = sb("/ws");
        assert_eq!(
            auto_classify(&req("run_command", "cp a.txt b.txt", "/ws"), &s),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("run_command", "rm -rf /etc", "/ws"), &s),
            AutoDecision::Deny("command writes outside the sandbox")
        );
        assert_eq!(
            auto_classify(&req("run_command", "sudo apt update", "/ws"), &s),
            AutoDecision::Deny("dangerous command is not auto-approved")
        );
        assert_eq!(
            auto_classify(
                &req("run_command", "curl -s https://example.com", "/ws"),
                &s
            ),
            AutoDecision::Allow
        );
        assert_eq!(
            auto_classify(&req("run_command", "git push origin main", "/ws"), &s),
            AutoDecision::Deny("network write/push is not auto-approved")
        );
        assert_eq!(
            auto_classify(&req("run_command", "npm install", "/ws"), &s),
            AutoDecision::Deny("package/tool installation is not auto-approved")
        );
        assert_eq!(
            auto_classify(&req("run_command", "vim x", "/ws"), &s),
            AutoDecision::Deny("interactive/long-running process is not auto-approved")
        );
    }

    #[test]
    fn network_write_outside_sandbox_denied() {
        let s = sb("/ws");
        // curl -o /tmp/x writes outside the sandbox -> deny, not allow.
        let r = auto_classify(
            &req(
                "run_command",
                "curl -o /tmp/x https://example.com/data",
                "/ws",
            ),
            &s,
        );
        assert_eq!(
            r,
            AutoDecision::Deny("network command writes outside the sandbox")
        );
        // curl install-script to workspace is fine (read-only fetch).
        let r2 = auto_classify(
            &req("run_command", "curl -s https://example.com/data", "/ws"),
            &s,
        );
        assert_eq!(r2, AutoDecision::Allow);
    }

    #[test]
    fn unknown_undetermined() {
        let s = sb("/ws");
        assert_eq!(
            auto_classify(&req("run_command", "frobnicate --all", "/ws"), &s),
            AutoDecision::Undetermined
        );
    }
}
