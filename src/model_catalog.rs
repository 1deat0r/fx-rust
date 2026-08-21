//! Gateway model catalog: a bounded prompt section describing the MCP server
//! availability visible to the model this turn, plus `fxrs models` output.
//! Port of upstream `src/core/mcp/model_catalog.zig` (header/footer, entry
//! format, truncation policy).

use crate::mcp::{McpAvailability, McpDiscovery, McpServerState};

const MAX_PROMPT_BYTES: usize = 4 * 1024;
const HEADER: &str =
    "Configured MCP servers visible to this model turn are listed below.\n\
     Use mcp_search_tools with the server alias and requested use case. Then use mcp_select_tool with one exact result. Do not guess tool names.\n\
     <mcp_servers>\n";
const FOOTER: &str = "</mcp_servers>\n";
const EMPTY_ENTRY: &str = "  <none />\n";

/// One row of the catalog.
#[derive(Debug, Clone)]
pub struct ServerSummary {
    pub name: String,
    pub availability: McpAvailability,
    pub tool_count: Option<usize>,
}

impl ServerSummary {
    fn render(&self) -> String {
        let mut out = format!(
            "  <server name=\"{}\" state=\"{}\"",
            escape(&self.name),
            self.availability.as_str()
        );
        if let Some(count) = self.tool_count {
            out.push_str(&format!(" tools=\"{count}\""));
        }
        out.push_str(" />\n");
        out
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Classify availability from discovery states (thin wrapper kept for parity
/// with upstream's `classifyAvailability` signature shape).
pub fn summarize(discovery: &McpDiscovery) -> Vec<ServerSummary> {
    discovery
        .states
        .iter()
        .map(|s| ServerSummary {
            name: s.name.clone(),
            availability: s.availability,
            tool_count: (s.availability == McpAvailability::Ready).then_some(s.tool_count),
        })
        .collect()
}

/// Build the bounded `<mcp_servers>` prompt section. Mirrors upstream
/// `renderWithLimit`: entries sorted by name, truncated (with a marker) when
/// the fixed byte budget would be exceeded.
pub fn render_prompt_section(discovery: &McpDiscovery) -> String {
    let empty = discovery.states.is_empty();
    let snap = summarize(discovery);
    let mut entries: Vec<ServerSummary> = snap.clone();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut parts: Vec<String> = Vec::new();
    parts.push(HEADER.to_string());
    if empty {
        parts.push(EMPTY_ENTRY.to_string());
        parts.push(FOOTER.to_string());
        if total_len(&parts) <= MAX_PROMPT_BYTES {
            return parts.concat();
        }
        return String::new();
    }
    for e in &entries {
        parts.push(e.render());
    }
    parts.push(FOOTER.to_string());
    if total_len(&parts) <= MAX_PROMPT_BYTES {
        return parts.concat();
    }

    // Truncate from the end, retaining the marker.
    let mut best: Option<Vec<String>> = None;
    for candidate in (0..=entries.len()).rev() {
        let mut cand: Vec<String> = vec![HEADER.to_string()];
        for e in &entries[..candidate] {
            cand.push(e.render());
        }
        let omitted = entries.len() - candidate;
        if omitted > 0 {
            cand.push(format!(
                "  <catalog_truncated omitted_count=\"{omitted}\" />\n"
            ));
        }
        cand.push(FOOTER.to_string());
        if total_len(&cand) <= MAX_PROMPT_BYTES {
            best = Some(cand);
            break;
        }
    }
    best.map(|v| v.concat()).unwrap_or_default()
}

fn total_len(parts: &[String]) -> usize {
    parts.iter().map(|p| p.len()).sum()
}

/// Human-readable table for `fxrs models` / `fxrs mcp`.
pub fn render_models_table(states: &[McpServerState]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "{:<24} {:<10} {:<7} {}",
        "SERVER", "TRANSPORT", "STATE", "TOOLS/ERROR"
    ));
    let mut sorted: Vec<&McpServerState> = states.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for s in sorted {
        let detail = s.error.clone().unwrap_or_else(|| s.tool_count.to_string());
        lines.push(format!(
            "{:<24} {:<10} {:<7} {}",
            s.name,
            s.transport,
            s.availability.as_str(),
            detail
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpTransport;
    use crate::mcp::{McpAvailability as A, McpServerState};

    fn state(name: &str, avail: A, count: usize) -> McpServerState {
        McpServerState {
            name: name.to_string(),
            transport: McpTransport::Http.as_str(),
            enabled: true,
            availability: avail,
            tool_count: count,
            error: None,
        }
    }

    #[test]
    fn renders_entries_and_footer() {
        let discovery = McpDiscovery {
            states: vec![state("fetch", A::Ready, 3), state("alpha", A::Failed, 0)],
            tools: Vec::new(),
        };
        let section = render_prompt_section(&discovery);
        assert!(section.starts_with("Configured MCP servers"));
        assert!(section.contains("<server name=\"alpha\" state=\"failed\" />"));
        assert!(section.contains("<server name=\"fetch\" state=\"ready\" tools=\"3\" />"));
        assert!(section.ends_with("</mcp_servers>\n"));
    }

    #[test]
    fn empty_servers_render_none_entry() {
        let discovery = McpDiscovery {
            states: vec![],
            tools: Vec::new(),
        };
        let section = render_prompt_section(&discovery);
        assert!(section.contains("<none />"));
    }

    #[test]
    fn escapes_names() {
        let discovery = McpDiscovery {
            states: vec![state("a<b>\"c\"", A::Ready, 1)],
            tools: Vec::new(),
        };
        let section = render_prompt_section(&discovery);
        assert!(section.contains("a&lt;b&gt;&quot;c&quot;"));
    }

    #[test]
    fn truncates_large_catalogs() {
        let mut states = Vec::new();
        for i in 0..200 {
            states.push(state(&format!("server-{i:03}"), A::Ready, 1));
        }
        let discovery = McpDiscovery {
            states,
            tools: Vec::new(),
        };
        let section = render_prompt_section(&discovery);
        assert!(section.len() <= MAX_PROMPT_BYTES);
        assert!(section.contains("catalog_truncated"));
        assert!(section.ends_with("</mcp_servers>\n"));
    }

    #[test]
    fn table_lists_errors() {
        let s = McpServerState {
            name: "x".into(),
            transport: "http",
            enabled: true,
            availability: A::Failed,
            tool_count: 0,
            error: Some("boom".into()),
        };
        let table = render_models_table(&[s]);
        assert!(table.contains("boom"));
    }
}
