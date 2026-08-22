//! `fxrs capture` — capture a session transcript to a file under
//! `~/.fx/captures/`. Defaults to the most recent session in the current
//! workspace; `--stdout` prints instead of writing. Markdown format renders
//! the transcript; JSON dumps the raw session record.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::config::fx_home;
use crate::sessions::SessionStore;

fn captures_dir() -> PathBuf {
    fx_home().join("captures")
}

/// Render a session's messages as a markdown transcript.
pub fn render_markdown(sess: &crate::sessions::Session) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Session {}\n\n- workspace: `{}`\n- model: {}\n- created: {}\n- updated: {}\n- schema: v{}\n\n",
        sess.id,
        sess.workspace,
        sess.model,
        sess.created_ms,
        sess.updated_ms,
        sess.schema_version,
    ));
    for msg in &sess.messages {
        let role = msg.role.as_str();
        match role {
            "user" => {
                out.push_str("## user\n\n");
                for block in &msg.content {
                    if let crate::providers::ContentBlock::Text(t) = block {
                        out.push_str(t);
                        out.push('\n');
                    } else if let crate::providers::ContentBlock::Image { .. } = block {
                        out.push_str("_(image attached)_\n\n");
                    }
                }
                out.push('\n');
            }
            "assistant" => {
                out.push_str("## assistant\n\n");
                for block in &msg.content {
                    match block {
                        crate::providers::ContentBlock::Text(t) => {
                            out.push_str(t);
                            out.push('\n');
                        }
                        crate::providers::ContentBlock::ToolUse { name, input, .. } => {
                            out.push_str(&format!("```tool\n⎿ {name} {input}\n```\n"));
                        }
                        _ => {}
                    }
                }
                out.push('\n');
            }
            "tool" => {
                out.push_str("## tool\n\n");
                for block in &msg.content {
                    if let crate::providers::ContentBlock::Text(t) = block {
                        out.push_str("```\n");
                        out.push_str(&truncate(t, 4000));
                        out.push('\n');
                        out.push_str("```\n");
                    }
                }
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// Run `fxrs capture [id|last] [--json|--markdown] [--stdout]` from `cwd`.
pub fn run_capture(args: &[String], cwd: &std::path::Path) -> Result<i32> {
    let wants_stdout = args.iter().any(|a| a == "--stdout");
    let wants_json = args.iter().any(|a| a == "--json");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let id_arg = positional.first().map(|s| s.as_str());

    let store = SessionStore::new()?;
    let id = match id_arg {
        None | Some("last") => store
            .list(Some(cwd))
            .ok()
            .and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0).id) }),
        Some(i) => Some(i.to_string()),
    };
    let Some(id) = id else {
        eprintln!("fxrs capture: no sessions in this workspace");
        return Ok(1);
    };
    let sess = store.load_or_error(cwd, &id)?;

    let content = if wants_json {
        serde_json::to_string_pretty(&sess)?
    } else {
        render_markdown(&sess)
    };

    if wants_stdout {
        print!("{content}");
        return Ok(0);
    }

    let ext = if wants_json { "json" } else { "md" };
    std::fs::create_dir_all(captures_dir())?;
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let path = captures_dir().join(format!("{sanitized}-{ts}.{ext}"));
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    println!("{}", path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_message_roles() {
        use crate::providers::{ContentBlock, Message};
        let sess = crate::sessions::Session {
            schema_version: 2,
            id: "sess1".into(),
            workspace: "/tmp".into(),
            created_ms: 0,
            updated_ms: 0,
            model: "m".into(),
            mode: crate::permissions::PermissionMode::Ask,
            interactive: true,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: vec![ContentBlock::Text("hello".into())],
                },
                Message {
                    role: "assistant".into(),
                    content: vec![ContentBlock::Text("hi there".into())],
                },
            ],
            grants: Default::default(),
            usage: Default::default(),
        };
        let md = render_markdown(&sess);
        assert!(md.contains("# Session sess1"));
        assert!(md.contains("## user\n\nhello"));
        assert!(md.contains("## assistant\n\nhi there"));
    }

    #[test]
    fn truncate_respects_char_limit() {
        assert_eq!(truncate(&"x".repeat(100), 10), "x".repeat(10));
        assert_eq!(truncate("short", 100), "short");
    }
}
