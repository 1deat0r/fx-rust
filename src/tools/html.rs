//! HTML → Markdown conversion (fx's `tools/web/html_to_markdown.zig`): a
//! self-contained state-machine converter producing model-friendly markdown
//! from arbitrary web HTML. No DOM, no dependencies — deliberately careful
//! about script/style/comment removal and entity decoding.

pub fn to_markdown(html: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut skip_depth = 0usize;
    let mut in_pre = false;
    let mut pending_blank = false;
    let mut pending_link_href: Option<String> = None;
    let chars: Vec<char> = html.chars().collect();
    let n = chars.len();

    while i < n {
        let c = chars[i];
        // Comments.
        if c == '<' && i + 3 < n && chars[i + 1] == '!' && chars[i + 2] == '-' && chars[i + 3] == '-' {
            // skip to -->
            let mut j = i + 4;
            while j + 2 < n && !(chars[j] == '-' && chars[j + 1] == '-' && chars[j + 2] == '>') {
                j += 1;
            }
            i = (j + 3).min(n);
            continue;
        }
        if c == '<' {
            // Read the tag.
            let mut j = i + 1;
            let mut tag = String::new();
            while j < n && chars[j] != '>' {
                tag.push(chars[j]);
                j += 1;
                if tag.len() > 256 {
                    break;
                }
            }
            i = (j + 1).min(n);
            let raw = tag.trim();
            let lower = raw.to_lowercase();
            let is_close = lower.starts_with('/');
            let name: String = lower
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();

            // Enter/exit script/style blocks.
            if !is_close && matches!(name.as_str(), "script" | "style" | "template" | "svg" | "head" | "noscript") {
                skip_depth += 1;
                continue;
            }
            if skip_depth > 0 {
                if is_close && matches!(name.as_str(), "script" | "style" | "template" | "svg" | "head" | "noscript") {
                    skip_depth -= 1;
                }
                continue;
            }

            if is_close {
                match name.as_str() {
                    "p" | "div" | "li" | "tr" | "section" | "blockquote" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        push_newline(&mut out, &mut pending_blank);
                    }
                    "pre" => {
                        in_pre = false;
                        out.push_str("\n```");
                        push_newline(&mut out, &mut pending_blank);
                    }
                    "b" | "strong" => out.push_str("**"),
                    "i" | "em" => out.push_str("*"),
                    "code" if !in_pre => out.push('`'),
                    "a" => {
                        out.push(']');
                        if let Some(href) = pending_link_href.take() {
                            if !href.is_empty() {
                                out.push_str(&format!("({href})"));
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            match name.as_str() {
                "br" => push_newline(&mut out, &mut pending_blank),
                "p" | "div" | "tr" | "section" | "article" | "blockquote" | "ul" | "ol" | "table" | "hr" => {
                    push_newline(&mut out, &mut pending_blank);
                }
                "h1" => { push_newline(&mut out, &mut pending_blank); out.push_str("# "); }
                "h2" => { push_newline(&mut out, &mut pending_blank); out.push_str("## "); }
                "h3" => { push_newline(&mut out, &mut pending_blank); out.push_str("### "); }
                "h4" => { push_newline(&mut out, &mut pending_blank); out.push_str("#### "); }
                "h5" | "h6" => { push_newline(&mut out, &mut pending_blank); }
                "li" => { push_newline(&mut out, &mut pending_blank); out.push_str("- "); }
                "pre" => { in_pre = true; push_newline(&mut out, &mut pending_blank); out.push_str("```\n"); }
                "code" if !in_pre => out.push('`'),
                "b" | "strong" => out.push_str("**"),
                "i" | "em" => out.push_str("*"),
                "a" => {
                    // [text](href) — href remembered until the closing tag.
                    pending_link_href = attr(&lower, "href");
                    out.push('[');
                }
                "img" => {
                    let src = attr(&lower, "src").unwrap_or_default();
                    let alt = attr(&lower, "alt").unwrap_or_default();
                    out.push_str(&format!("![{alt}]({src})"));
                }
                "input" => out.push_str("[ ] "),
                _ => {}
            }
            continue;
        }

        // Skip everything while inside a script/style/template block.
        if skip_depth > 0 {
            i += 1;
            continue;
        }

        // Entity decode + whitespace handling outside tags.
        if c == '&' {
            if let Some((decoded, consumed)) = decode_entity(&chars[i..], n - i) {
                push_text(&mut out, &decoded, &mut pending_blank, in_pre);
                i += consumed;
                continue;
            }
        }
        let s = c.to_string();
        push_text(&mut out, &s, &mut pending_blank, in_pre);
        i += 1;
    }
    // Trim excessive trailing newlines.
    let trimmed = out.trim_end();
    let mut clean = String::from(trimmed);
    // Collapse 3+ blank lines to one.
    clean.push('\n');
    let collapsed = collapse_blank_lines(&clean);
    collapsed
}

fn push_newline(out: &mut String, pending_blank: &mut bool) {
    if out.ends_with('\n') {
        return;
    }
    if out.is_empty() {
        return;
    }
    out.push('\n');
    *pending_blank = false;
}

fn push_text(out: &mut String, s: &str, pending_blank: &mut bool, in_pre: bool) {
    if in_pre {
        out.push_str(s);
        return;
    }
    if s == "\n" {
        return; // raw newlines inside tags are collapsed by block handlers
    }
    if s.trim().is_empty() {
        if *pending_blank {
            return;
        }
        if out.ends_with(' ') || out.is_empty() || out.ends_with('\n') {
            return;
        }
        out.push(' ');
        *pending_blank = true;
        return;
    }
    *pending_blank = false;
    out.push_str(s);
}

fn decode_entity(chars: &[char], _max: usize) -> Option<(String, usize)> {
    // chars[0] == '&'
    let mut ent = String::new();
    let mut k = 1;
    while k < chars.len() && k <= 16 {
        let c = chars[k];
        if c == ';' {
            let decoded = match ent.as_str() {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "#39" | "#x27" => "'",
                "nbsp" => " ",
                "copy" => "©",
                "reg" => "®",
                "hellip" => "…",
                "mdash" => "—",
                "ndash" => "–",
                "lsquo" => "‘",
                "rsquo" => "’",
                "ldquo" => "“",
                "rdquo" => "”",
                "apos" => "'",
                _ => {
                    // numeric: &#123; or &#x1F;
                    let num = match ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()),
                    };
                    match num.and_then(char::from_u32) {
                        Some(ch) => return Some((format!("{ch}"), k + 1)),
                        None => return Some((format!("&{ent};"), k + 1)),
                    }
                }
            };
            return Some((decoded.to_string(), k + 1));
        }
        ent.push(c);
        k += 1;
    }
    // Not an entity; return '&' unchanged.
    Some(("&".to_string(), 1))
}

fn attr(lower: &str, key: &str) -> Option<String> {
    // Find key= in the raw tag (lowercased). Handle quoted values.
    let pairs = lower.split_whitespace().collect::<Vec<_>>();
    let mut want_val = false;
    for tok in pairs {
        if want_val {
            return Some(tok.trim_matches(['"', '\'']).to_string());
        }
        if let Some(rest) = tok.strip_prefix(key) {
            if rest.starts_with('=') {
                let v = &rest[1..];
                let v = v.trim_matches(['"', '\'']);
                if v.is_empty() {
                    want_val = true;
                } else {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blanks = 0;
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_and_tags() {
        let md = to_markdown("<html><head><style>p{color:red}</style></head><body><script>alert(1)</script><p>Hello <b>world</b></p></body></html>");
        assert!(md.contains("Hello **world**"));
        assert!(!md.contains("alert"));
        assert!(!md.contains("color:red"));
    }

    #[test]
    fn headings_and_lists() {
        let md = to_markdown("<h1>Title</h1><ul><li>one</li><li>two</li></ul>");
        assert!(md.starts_with("# Title"));
        assert!(md.contains("- one"));
        assert!(md.contains("- two"));
    }

    #[test]
    fn links_and_images() {
        let md = to_markdown(r#"<a href="https://ex.com">click</a><img src="i.png" alt="pic">"#);
        assert!(md.contains("[click]"));
        assert!(md.contains("!["), "{md}");
    }

    #[test]
    fn code_blocks_preserved() {
        let md = to_markdown("<pre><code>fn main() {}\n</code></pre>");
        assert!(md.contains("```"));
        assert!(md.contains("fn main() {}"));
    }

    #[test]
    fn entities_decoded() {
        let md = to_markdown("a &amp; b &lt; c &#65;");
        assert!(md.contains("a & b < c A"), "{md}");
    }

    #[test]
    fn comments_removed() {
        let md = to_markdown("<!-- secret --><p>visible</p>");
        assert!(!md.contains("secret"));
        assert!(md.contains("visible"));
    }
}
