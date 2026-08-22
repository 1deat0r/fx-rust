//! Durable tool-result store — faithful port of upstream
//! `core/session/result_store.zig` and the `read_tool_result`
//! session tool (`src/tools/session/read_tool_result.zig`).
//!
//! Large tool outputs (over [`large_result_threshold_bytes`]) are written to
//! the per-user result dir (`~/.fx/results/tool_results/`) under a content-
//! derived handle and the model instead receives a bounded preview envelope
//! that advertises `read_tool_result` for byte-range and query reads. Small
//! outputs stay inline (capped).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

// Upstream names these snake_case constants; keep the names for 1:1 parity.
#[allow(non_upper_case_globals)]
pub const large_result_threshold_bytes: usize = 16 * 1024;
#[allow(non_upper_case_globals)]
pub const preview_bytes: usize = 4 * 1024;
#[allow(non_upper_case_globals)]
pub const read_default_bytes: usize = 8 * 1024;
#[allow(non_upper_case_globals)]
pub const read_max_bytes: usize = 64 * 1024;
/// Upstream cap on what one stored text file may contain.
const STORED_TEXT_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PreparedResult {
    pub model_output: String,
    pub output_bytes: usize,
    pub stored_output_bytes: usize,
    pub truncated: bool,
    pub output_handle: Option<String>,
    pub preview: Option<String>,
}

pub fn result_dir() -> PathBuf {
    crate::config::fx_home()
        .join("results")
        .join("tool_results")
}

/// Deterministic on-disk handle (upstream `makeHandle`):
/// `result-{safe_tool}-{call_hex8}-{content_hex8}.txt`.
pub fn make_handle(tool_call_id: &str, tool_name: &str, text: &[u8]) -> String {
    let mut content = Sha256::new();
    content.update(text);
    let content_digest = content.finalize();
    let content_hex: String = content_digest[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut call = Sha256::new();
    call.update(tool_call_id.as_bytes());
    let call_digest = call.finalize();
    let call_hex: String = call_digest[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "result-{}-{call_hex}-{content_hex}.txt",
        safe_handle_part(tool_name)
    )
}

fn safe_handle_part(tool_name: &str) -> String {
    let mut out = String::new();
    for b in tool_name.bytes() {
        if out.len() >= 48 {
            break;
        }
        let safe = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        out.push(if safe { b as char } else { '-' });
    }
    if out.is_empty() {
        "call".to_string()
    } else {
        out
    }
}

fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty() || handle.len() > 160 {
        anyhow::bail!("invalid handle");
    }
    if handle.contains("..") {
        anyhow::bail!("invalid handle");
    }
    for b in handle.bytes() {
        let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.';
        if !ok {
            anyhow::bail!("invalid handle");
        }
    }
    Ok(())
}

fn handle_path(dir: &Path, handle: &str) -> PathBuf {
    dir.join(handle)
}

/// Store one large result under its content-derivable handle and return the
/// model-facing preview envelope.
pub fn prepare(
    result_dir: Option<&Path>,
    tool_call_id: &str,
    tool_name: &str,
    output: &[u8],
    inline_cap: usize,
) -> PreparedResult {
    match result_dir {
        Some(dir) if output.len() > large_result_threshold_bytes => {
            let handle = make_handle(tool_call_id, tool_name, output);
            if let Err(_err) = store_at_handle(dir, &handle, output) {
                // Store failures degrade to the inline cap rather than
                // aborting the tool turn.
                let capped = capped_inline(tool_name, output, inline_cap);
                return PreparedResult {
                    model_output: capped.clone(),
                    output_bytes: output.len(),
                    stored_output_bytes: 0,
                    truncated: false,
                    output_handle: None,
                    preview: None,
                };
            }
            let preview = preview_text(output, preview_bytes);
            let model_output = format_stored_result_output(&handle, &preview, output.len());
            PreparedResult {
                model_output,
                output_bytes: output.len(),
                stored_output_bytes: output.len(),
                truncated: true,
                output_handle: Some(handle),
                preview: Some(preview),
            }
        }
        _ => {
            let capped = capped_inline(tool_name, output, inline_cap);
            let truncated = capped.len() < output.len();
            PreparedResult {
                model_output: capped,
                output_bytes: output.len(),
                stored_output_bytes: 0,
                truncated,
                output_handle: None,
                preview: None,
            }
        }
    }
}

fn store_at_handle(dir: &Path, handle: &str, text: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = handle_path(dir, handle);
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Format used by the upstream prepare path + failure path.
pub fn format_stored_result_output(handle: &str, preview: &str, stored_bytes: usize) -> String {
    format!(
        "<tool_result_preview handle=\"{handle}\" stored_bytes=\"{stored_bytes}\">\n{preview}\n</tool_result_preview>\n<tool_result_handle>{handle}</tool_result_handle>\nFull redacted result is stored outside session JSON. Use read_tool_result with this handle to inspect a byte range or literal query."
    )
}

fn capped_inline(_tool_name: &str, text: &[u8], cap: usize) -> String {
    let s = String::from_utf8_lossy(text);
    if s.len() <= cap {
        s.into_owned()
    } else {
        let mut out = String::with_capacity(cap + 128);
        out.push_str(&s[..cap]);
        out.push_str(&format!(
            "\n[truncated {} bytes; use read_tool_result if this tool supports session handles]",
            text.len().saturating_sub(cap)
        ));
        out
    }
}

fn preview_text(text: &[u8], cap: usize) -> String {
    let s = String::from_utf8_lossy(text);
    if s.len() <= cap {
        s.into_owned()
    } else {
        s[..cap].to_string()
    }
}

fn read_stored(dir: &Path, handle: &str) -> Result<Vec<u8>> {
    validate_handle(handle)?;
    let path = handle_path(dir, handle);
    let bytes =
        std::fs::read(&path).with_context(|| format!("reading result {}", path.display()))?;
    if bytes.len() > STORED_TEXT_MAX_BYTES {
        anyhow::bail!("stored result exceeds size limit");
    }
    Ok(bytes)
}

/// Byte-range read (upstream `readByRange`). `start_byte` is 1-based;
/// offsets are clamped to UTF-8 boundaries; `byte_count` defaults to
/// [`read_default_bytes`] and is capped at [`read_max_bytes`].
pub fn read_by_range(
    dir: &Path,
    handle: &str,
    start_byte: usize,
    byte_count: usize,
) -> Result<String> {
    let text = read_stored(dir, handle)?;
    let start = if start_byte == 0 {
        0
    } else {
        start_byte.min(text.len() + 1) - 1
    };
    let requested = if byte_count == 0 {
        read_default_bytes
    } else {
        byte_count.min(read_max_bytes)
    };
    let end = text.len().min(start + requested);
    let safe_start = utf8_backward_boundary(&text, start);
    let safe_end = utf8_forward_boundary(&text, end);
    let body = String::from_utf8_lossy(&text[safe_start..safe_end]);
    Ok(format!(
        "<tool_result handle=\"{handle}\" start_byte=\"{}\" end_byte=\"{}\" total_bytes=\"{}\">\n{body}\n</tool_result>",
        safe_start + 1,
        safe_end,
        text.len()
    ))
}

/// Literal substring query (upstream `searchByQuery`): returns a bounded
/// context window around the first match (up to [`read_default_bytes`]).
pub fn search_by_query(dir: &Path, handle: &str, query: &str) -> Result<String> {
    let text = read_stored(dir, handle)?;
    let hay = String::from_utf8_lossy(&text);
    let Some(rel) = hay.find(query) else {
        anyhow::bail!("read_tool_result query {query:?} not found in stored result {handle}");
    };
    let around = read_default_bytes.saturating_sub(query.len());
    let half = around / 2;
    let start_rel = rel.saturating_sub(half);
    let end_rel = (rel + query.len() + half).min(hay.len());
    let body = &hay[start_rel..end_rel];
    Ok(format!(
        "<tool_result handle=\"{handle}\" query=\"{query}\" total_bytes=\"{}\">\n{body}\n</tool_result>",
        text.len()
    ))
}

fn utf8_forward_boundary(text: &[u8], mut idx: usize) -> usize {
    while idx < text.len() && (text[idx] & 0xC0) == 0x80 {
        idx += 1;
    }
    idx
}

fn utf8_backward_boundary(text: &[u8], mut idx: usize) -> usize {
    while idx > 0 && (text[idx - 1] & 0xC0) == 0x80 {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_results_stay_inline() {
        let dir = std::env::temp_dir().join(format!("fxrs-res-inline-{}", std::process::id()));
        let out = "small".repeat(100);
        let prepared = prepare(Some(&dir), "call-1", "read_file", out.as_bytes(), 16 * 1024);
        assert!(!prepared.truncated);
        assert!(prepared.output_handle.is_none());
        assert!(prepared.model_output.contains("small"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_results_store_and_preview() {
        let dir = std::env::temp_dir().join(format!("fxrs-res-large-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big: String = "x".repeat(large_result_threshold_bytes + 64);
        let prepared = prepare(Some(&dir), "call-1", "web_search", big.as_bytes(), 1024);
        assert!(prepared.truncated);
        let handle = prepared.output_handle.as_deref().unwrap();
        assert!(handle.starts_with("result-web_search-"));
        assert!(handle.ends_with(".txt"));
        assert!(prepared.model_output.contains("<tool_result_preview"));
        assert!(prepared.model_output.contains("Use read_tool_result"));
        // The stored file exists at the handle.
        assert!(handle_path(&dir, handle).exists());
        // Byte-range read reaches the tail.
        let ranged = read_by_range(&dir, handle, big.len() - 40, 80).unwrap();
        assert!(ranged.contains("<tool_result handle="));
        assert!(ranged.contains("x".repeat(40).as_str()));
        // Query read finds a needle planted in the middle.
        let haystack = format!(
            "{}{}{}",
            "a".repeat(large_result_threshold_bytes + 64),
            "NEEDLE-HERE",
            "b".repeat(large_result_threshold_bytes + 64)
        );
        let prepared2 = prepare(
            Some(&dir),
            "call-2",
            "web_search",
            haystack.as_bytes(),
            1024,
        );
        let handle2 = prepared2.output_handle.unwrap();
        let query = search_by_query(&dir, &handle2, "NEEDLE-HERE").unwrap();
        assert!(query.contains("NEEDLE-HERE"));
        assert!(query.contains("total_bytes="));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_missing_handle_fails_with_not_found() {
        let dir = std::env::temp_dir().join(format!("fxrs-res-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = read_by_range(&dir, "result-unknown-00000000-00000000.txt", 1, 64).unwrap_err();
        assert!(err.to_string().contains("reading result"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_handle_rejects_path_traversal() {
        assert!(validate_handle("../../etc/passwd").is_err());
        assert!(validate_handle("ok.txt").is_ok());
        assert!(validate_handle("").is_err());
    }

    #[test]
    fn handle_is_deterministic_and_tool_scoped() {
        let h1 = make_handle("call-1", "web_search", b"same content");
        let h2 = make_handle("call-1", "web_search", b"same content");
        assert_eq!(h1, h2);
        let h3 = make_handle("call-1", "read_file", b"same content");
        assert_ne!(h1, h3);
        let h4 = make_handle("call-1", "web_search", b"different");
        assert_ne!(h1, h4);
    }
}
