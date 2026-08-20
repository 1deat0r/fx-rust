//! Web tools: web_fetch (fetch + text extraction) and web_search
//! (DuckDuckGo lite HTML backend, no API key required).

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};

use super::{ToolContext, arg};

const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

fn valid_url(url: &str) -> Result<reqwest::Url> {
    let u = reqwest::Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
    match u.scheme() {
        "http" | "https" => Ok(u),
        other => bail!("unsupported url scheme `{other}` (only http/https)"),
    }
}

fn textify(html: &str) -> String {
    // Lightweight HTML-to-text: strip scripts/styles, tags, decode entities.
    let mut out = String::new();
    let mut skip_depth = 0usize;
    let mut in_tag = false;
    let mut tag = String::new();
    let mut last_was_blank = false;

    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if skip_depth > 0 {
            if c == '<' {
                // could open a nested script tag — approximate by scanning to '>'
                let mut rest = String::new();
                while let Some(&c2) = chars.peek() {
                    rest.push(c2);
                    chars.next();
                    if c2 == '>' {
                        break;
                    }
                }
                let lower = rest.to_lowercase();
                if lower.contains("</script") || lower.contains("</style") {
                    skip_depth -= 1;
                } else if lower.starts_with("<script") || lower.starts_with("<style") {
                    skip_depth += 1;
                }
            }
            if c == '>' && skip_depth > 0 && false {
                let _ = c;
            }
            continue;
        }
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let lower = tag.to_lowercase();
                if lower.starts_with("script") || lower.starts_with("style") {
                    skip_depth += 1;
                } else if matches!(lower.as_str(), "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "pre" | "section" | "td" | "blockquote") {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                }
            }
            _ if in_tag => {
                tag.push(if c == '\n' { ' ' } else { c });
            }
            _ => {
                // HTML entity decode (common ones)
                let push = match c {
                    '&' => {
                        let mut ent = String::new();
                        while let Some(&c2) = chars.peek() {
                            if c2 == ';' {
                                chars.next();
                                break;
                            }
                            ent.push(c2);
                            chars.next();
                            if ent.len() > 12 {
                                break;
                            }
                        }
                        match ent.as_str() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "#39" => "'",
                            "nbsp" => " ",
                            _ => {
                                out.push('&');
                                out.push_str(&ent);
                                if chars.peek().is_none() && !ent.is_empty() {
                                    // consumed the ';' already
                                    out.push(';');
                                    continue;
                                }
                                continue;
                            }
                        }
                        .to_string()
                    }
                    _ => c.to_string(),
                };
                if push == "\n" {
                    if last_was_blank {
                        continue;
                    }
                    last_was_blank = true;
                } else if !push.trim().is_empty() {
                    last_was_blank = false;
                }
                out.push_str(&push);
                // Collapse runs of whitespace to single spaces (except newlines we add).
            }
        }
    }
    let lines: Vec<String> = out
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

pub async fn web_fetch(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let url = arg(args, "url").ok_or_else(|| anyhow::anyhow!("missing `url`"))?;
    let u = valid_url(url)?;
    let client = Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("fxrs/0.1 (model tool) Rust")
        .build()?;
    let resp = client.get(u.clone()).send().await.with_context(|| format!("fetching {u}"))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.bytes().await.with_context(|| format!("reading body of {u}"))?;

    const MAX_BYTES: usize = 2 * 1024 * 1024;
    let body = if bytes.len() > MAX_BYTES {
        bytes[..MAX_BYTES].to_vec()
    } else {
        bytes.to_vec()
    };

    let is_html = content_type.contains("html") || content_type.is_empty() && guess_html(&body);
    let text = if is_html {
        textify(&String::from_utf8_lossy(&body))
    } else {
        String::from_utf8_lossy(&body).to_string()
    };

    Ok(json!({
        "url": u.to_string(),
        "status": status.as_u16(),
        "content_type": content_type,
        "content": ctx.truncate(&text),
    }))
}

fn guess_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<head")
}

pub async fn web_search(ctx: &ToolContext, args: &Value) -> Result<Value> {
    let query = arg(args, "query").ok_or_else(|| anyhow::anyhow!("missing `query`"))?;
    let client = Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("Mozilla/5.0 fxrs")
        .build()?;
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let resp = client.get(&url).send().await.context("DuckDuckGo request failed")?;
    if !resp.status().is_success() {
        bail!("DuckDuckGo returned {}", resp.status());
    }
    let html = resp.text().await?;
    let results = parse_ddg(&html);
    if results.is_empty() {
        return Ok(json!({ "query": query, "results": [], "note": "no results parsed (backend may be rate-limited)" }));
    }
    Ok(json!({ "query": query, "results": results }))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_ddg(html: &str) -> Vec<Value> {
    // Very small parser for DDG result blocks: <a class="result__a" href="...">title</a>
    // and sibling <a class="result__snippet">snippet</a>.
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(anchor_start) = rest.find("class=\"result__a\"") {
        let before = &rest[..anchor_start];
        let href_start = before.rfind("href=\"");
        let title_start = match rest.find('>') {
            Some(i) => i + 1,
            None => break,
        };
        let title_end = rest[title_start..].find("</a>").map(|i| title_start + i).unwrap_or(rest.len());
        let title = strip_tags(&rest[title_start..title_end]);
        rest = &rest[title_end..];

        let snippet = if let Some(snip) = rest.find("result__snippet") {
            let s = &rest[snip..];
            let t = match s.find('>') {
                Some(i) => i + 1,
                None => 0,
            };
            let e = s[t..].find("</a>").map(|i| t + i).unwrap_or(0);
            strip_tags(&s[t..t + e])
        } else {
            String::new()
        };

        let url = if let Some(h) = href_start {
            let mut u = String::new();
            for c in before[h + 6..].chars() {
                if c == '"' {
                    break;
                }
                u.push(c);
            }
            u
        } else {
            String::new()
        };

        out.push(json!({ "title": title, "url": url, "snippet": snippet }));
        if out.len() >= 10 {
            break;
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
