//! ask_user_question: prompt the human for input mid-task (interactive only).

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

use super::{arg, ToolContext};

pub fn ask_user_question(ctx: &ToolContext, args: &Value) -> Result<Value> {
    if !ctx.interactive {
        bail!("ask_user_question is only available in interactive mode; proceed using your best judgment");
    }
    let question = arg(args, "question").ok_or_else(|| anyhow::anyhow!("missing `question`"))?;
    let options = args
        .get("options")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut stdout = std::io::stdout();
    let stdin = std::io::stdin();

    loop {
        // Use stdout for the question so it lands in the transcript stream.
        writeln!(stdout, "\n\x1b[36mƒ {question}\x1b[0m")?;
        if !options.is_empty() {
            for opt in options.iter() {
                writeln!(stdout, "   \x1b[90m[{{i+1}}]\x1b[0m {opt}")?;
            }
            writeln!(stdout, "   \x1b[90m[number or free text]\x1b[0m")?;
        }
        stdout.flush()?;
        eprint!("\x1b[36manswer>\x1b[0m ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            bail!("stdin closed while waiting for an answer");
        }
        let answer = line.trim().to_string();
        if answer.is_empty() {
            continue;
        }
        if let Ok(n) = answer.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return Ok(json!({
                    "answer_index": n,
                    "answer": options[n - 1],
                    "question": question,
                }));
            }
        }
        return Ok(json!({ "answer": answer, "question": question }));
    }
}
