//! GitHub integration — faithful port of upstream `core/github/*`:
//! `git_context.zig` (prepared git snapshot), `github_publish.zig`
//! (draft parsing + `gh` CLI publish), `github_workflows.zig` (PR/issue
//! draft prompts), plus the fx.sh feedback URL from `core/feedback/runtime.zig`.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

/// The canonical feedback endpoint (upstream `core/feedback/runtime.zig`).
#[allow(non_upper_case_globals)]
pub const feedback_url: &str = "https://fx.sh/feedback";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workflow {
    PullRequest,
    Issue,
}

/// Prepared git snapshot (upstream `git_context.Snapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub in_git_repo: bool,
    pub text: String,
}

pub fn is_git_repository(workspace: &Path) -> bool {
    matches!(run_git(workspace, &["rev-parse", "--is-inside-work-tree"]), Some(t) if t == "true")
}

/// Build the prepared context snapshot exactly like upstream: branch, status,
/// recent commits, staged + unstaged diff stats.
pub fn git_snapshot(workspace: &Path) -> Snapshot {
    let in_git_repo = is_git_repository(workspace);
    let branch = run_git(workspace, &["branch", "--show-current"]);
    let status = run_git(workspace, &["status", "--short", "--branch"]);
    let log = run_git(workspace, &["log", "--oneline", "-5"]);
    let staged = run_git(workspace, &["diff", "--stat", "--cached"]);
    let unstaged = run_git(workspace, &["diff", "--stat"]);
    let text = format_snapshot(&branch, &status, &log, &staged, &unstaged);
    Snapshot { in_git_repo, text }
}

fn format_snapshot(
    branch: &Option<String>,
    status: &Option<String>,
    log: &Option<String>,
    staged: &Option<String>,
    unstaged: &Option<String>,
) -> String {
    let mut out = String::new();
    out.push_str("Git snapshot\n");
    out.push_str(&format!(
        "Branch: {}\n",
        branch.as_deref().unwrap_or("unavailable")
    ));
    out.push('\n');
    out.push_str("Status:\n");
    push_body(&mut out, status, "unavailable");
    out.push('\n');
    out.push_str("Recent commits:\n");
    push_body(&mut out, log, "unavailable");
    out.push('\n');
    out.push_str("Staged diff stat:\n");
    push_body(&mut out, staged, "none");
    out.push('\n');
    out.push_str("Unstaged diff stat:\n");
    push_body(&mut out, unstaged, "none");
    out
}

fn push_body(out: &mut String, body: &Option<String>, fallback: &str) {
    match body {
        Some(text) => {
            out.push_str(text);
            out.push('\n');
        }
        None => out.push_str(fallback),
    }
}

fn run_git(workspace: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .args(args)
        .current_dir(workspace);
    cmd.output().ok().and_then(|out| {
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    })
}

/// Draft = first non-empty line as title, remainder as body (upstream
/// `parseDraft`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

pub fn parse_draft(text: &str) -> Result<Draft> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("invalid github draft: empty");
    }
    match trimmed.find('\n') {
        None => Ok(Draft {
            title: trimmed.to_string(),
            body: String::new(),
        }),
        Some(first_break) => {
            let title = trimmed[..first_break].trim();
            if title.is_empty() {
                bail!("invalid github draft: empty title");
            }
            let body = trimmed[first_break + 1..].trim_start().to_string();
            Ok(Draft {
                title: title.to_string(),
                body,
            })
        }
    }
}

/// Publish result (upstream `PublishResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub ok: bool,
    pub text: String,
}

/// Publish via the `gh` CLI exactly like upstream
/// (`gh pr create --title "$title" --body "$body"`).
pub fn publish(workflow: &Workflow, draft: &Draft) -> PublishResult {
    let mut cmd = Command::new("gh");
    cmd.arg(match workflow {
        Workflow::PullRequest => "pr",
        Workflow::Issue => "issue",
    });
    cmd.arg("create")
        .arg("--title")
        .arg(&draft.title)
        .arg("--body")
        .arg(&draft.body);
    match cmd.output() {
        Err(_) => PublishResult {
            ok: false,
            text: "gh CLI not found in PATH".to_string(),
        },
        Ok(out) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return PublishResult {
                    ok: false,
                    text: if stderr.is_empty() {
                        "gh command failed".to_string()
                    } else {
                        stderr
                    },
                };
            }
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            PublishResult {
                ok: true,
                text: if stdout.is_empty() {
                    "created successfully".to_string()
                } else {
                    stdout
                },
            }
        }
    }
}

/// Draft-prompt contract (upstream `github_workflows.buildPrompt*`).
pub fn build_prompt(
    workflow: &Workflow,
    language: &str,
    context: &str,
    workspace: &Path,
) -> Result<String> {
    let snapshot = git_snapshot(workspace);
    build_prompt_from_snapshot(workflow, language, context, &snapshot)
}

pub fn build_prompt_from_snapshot(
    workflow: &Workflow,
    _language: &str,
    context: &str,
    snapshot: &Snapshot,
) -> Result<String> {
    let trimmed = context.trim();
    if matches!(workflow, Workflow::PullRequest) && !snapshot.in_git_repo {
        bail!("not a git repository");
    }
    Ok(match workflow {
        Workflow::PullRequest => {
            let base = "Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session. ";
            let middle = "Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n";
            let tail = "\n\nIf you need more context, read relevant files. Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. Do not create the PR with gh or publish anything unless I explicitly ask you to.";
            if trimmed.is_empty() {
                format!("{base}{middle}{}{tail}", snapshot.text)
            } else {
                format!(
                    "{base}Additional context: {trimmed}. {middle}{}{tail}",
                    snapshot.text
                )
            }
        }
        Workflow::Issue => {
            let base = "Draft a GitHub issue from the current context. Reply in the same natural language as the current session. ";
            let middle = "Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n";
            let tail = "\n\nIf you need more context, inspect relevant files, errors, or logs. Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.";
            if trimmed.is_empty() {
                format!("{base}{middle}{}{tail}", snapshot.text)
            } else {
                format!(
                    "{base}Additional context: {trimmed}. {middle}{}{tail}",
                    snapshot.text
                )
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_draft_splits_title_and_body() {
        let draft = parse_draft("  Fix the widget  \n## Summary\n\nFixed.").unwrap();
        assert_eq!(draft.title, "Fix the widget");
        assert_eq!(draft.body, "## Summary\n\nFixed.");
    }

    #[test]
    fn parse_draft_single_line_has_empty_body() {
        let draft = parse_draft("Just a title").unwrap();
        assert_eq!(draft.title, "Just a title");
        assert_eq!(draft.body, "");
    }

    #[test]
    fn parse_draft_rejects_empty() {
        assert!(parse_draft("   \n  ").is_err());
        // Upstream trims leading whitespace before scanning, so an input that
        // only has internal whitespace collapses to a single-line title.
        let draft = parse_draft("\n  \nbody").unwrap();
        assert_eq!(draft.title, "body");
        assert_eq!(draft.body, "");
    }

    #[test]
    fn pr_prompt_requires_git_repo() {
        let snap = Snapshot {
            in_git_repo: false,
            text: "Git snapshot\nBranch: unavailable\n".into(),
        };
        assert!(build_prompt_from_snapshot(&Workflow::PullRequest, "en", "", &snap).is_err());
        // Issue prompts work outside git.
        let issue = build_prompt_from_snapshot(&Workflow::Issue, "en", "", &snap).unwrap();
        assert!(issue.contains("Draft a GitHub issue from the current context."));
        assert!(issue.contains("## Steps to Reproduce"));
        assert!(issue.contains("## Expected"));
        assert!(issue.contains("## Actual"));
    }

    #[test]
    fn pr_prompt_preserves_context_and_sections() {
        let snap = Snapshot {
            in_git_repo: true,
            text: "Git snapshot\nBranch: feature\n".into(),
        };
        let prompt =
            build_prompt_from_snapshot(&Workflow::PullRequest, "en", " ready for review \n", &snap)
                .unwrap();
        assert!(prompt.contains("Additional context: ready for review."));
        assert!(prompt.contains("Git snapshot\nBranch: feature\n"));
        assert!(prompt.contains("## Summary"));
        assert!(prompt.contains("## Testing"));
        assert!(!prompt.contains("## Steps to Reproduce"));
        assert!(prompt.contains(
            "Do not create the PR with gh or publish anything unless I explicitly ask you to."
        ));
    }

    #[test]
    fn issue_prompt_omits_empty_context_clause() {
        let snap = Snapshot {
            in_git_repo: false,
            text: "Git snapshot\nBranch: unavailable\n".into(),
        };
        let prompt = build_prompt_from_snapshot(&Workflow::Issue, "en", " \t\r\n", &snap).unwrap();
        assert!(prompt.contains("Draft a GitHub issue from the current context."));
        assert!(!prompt.contains("Additional context:"));
        assert!(prompt.contains(
            "Do not create the issue with gh or publish anything unless I explicitly ask you to."
        ));
    }

    #[test]
    fn feedback_url_is_canonical() {
        assert_eq!(feedback_url, "https://fx.sh/feedback");
    }
}
