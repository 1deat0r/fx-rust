//! Line diff engine — faithful port of fx's `core/output/diff.zig` core:
//! LCS-based `compute`, `countStats`, and `formatUnified` (2 context lines,
//! 6-line display cap with elisions, decimal gutter). Used by file-change
//! previews in the TUI and by `fxrs diff`.

/// Maximum LCS matrix cells before we fall back to a coarse all-remove /
/// all-add projection (guards pathological inputs the way upstream's preview
/// budget does; the canonical preview limit is 1M cells).
pub const MAX_LCS_CELLS: usize = 16_000_000;

/// Trailing-newline markers (upstream `trailing_newline_removed_marker` /
/// `trailing_newline_added_marker`).
pub const TRAILING_NEWLINE_REMOVED_MARKER: &str = "(trailing newline removed)";
pub const TRAILING_NEWLINE_ADDED_MARKER: &str = "(trailing newline added)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOp {
    Equal,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: LineOp,
    pub old_num: Option<u32>,
    pub new_num: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub additions: u32,
    pub deletions: u32,
}

pub const CONTEXT_LINES: usize = 2;
pub const MAX_DISPLAY_LINES: usize = 6;

/// Split text into lines (empty text -> no lines). A trailing newline does
/// not produce a final empty line (upstream `appendLines`).
fn append_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            lines.push(text[start..index].to_string());
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

/// When exactly one side has a trailing newline, append the marker line to
/// that side so the newline change is visible (upstream
/// `appendTrailingNewlineMarker`).
fn append_trailing_newline_marker(old_text: &str, new_text: &str) -> (Vec<String>, Vec<String>) {
    let mut old_lines = append_lines(old_text);
    let mut new_lines = append_lines(new_text);
    if old_text.is_empty() || new_text.is_empty() {
        return (old_lines, new_lines);
    }
    let old_has_trailing = old_text.ends_with('\n');
    let new_has_trailing = new_text.ends_with('\n');
    if old_has_trailing == new_has_trailing {
        return (old_lines, new_lines);
    }
    if old_has_trailing {
        old_lines.push(TRAILING_NEWLINE_REMOVED_MARKER.to_string());
    } else {
        new_lines.push(TRAILING_NEWLINE_ADDED_MARKER.to_string());
    }
    (old_lines, new_lines)
}

/// Trailing-newline marker for one side (upstream `trailingNewlineMarker`).
pub fn trailing_newline_marker(old_text: &str, new_text: &str, old_side: bool) -> Option<&'static str> {
    if old_text.is_empty() || new_text.is_empty() {
        return None;
    }
    let old_has_trailing = old_text.ends_with('\n');
    let new_has_trailing = new_text.ends_with('\n');
    if old_has_trailing == new_has_trailing {
        return None;
    }
    if old_side && old_has_trailing {
        return Some(TRAILING_NEWLINE_REMOVED_MARKER);
    }
    if !old_side && new_has_trailing {
        return Some(TRAILING_NEWLINE_ADDED_MARKER);
    }
    None
}

/// Compute the line diff (upstream `compute`): classic LCS dynamic program
/// with a `(old_len+1)*(new_len+1)` u32 table, then backtrack to emit
/// equal/add/remove lines with 1-based old/new numbers.
pub fn compute(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let (old_lines, new_lines) = append_trailing_newline_marker(old_text, new_text);
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let cells = (old_len + 1).saturating_mul(new_len + 1);
    if cells > MAX_LCS_CELLS {
        // Coarse projection: the whole old block removed, whole new block
        // added. Numbers stay correct even though grouping is coarse.
        let mut out = Vec::with_capacity(old_len + new_len);
        for (i, line) in old_lines.iter().enumerate() {
            out.push(DiffLine {
                op: LineOp::Remove,
                old_num: Some((i + 1) as u32),
                new_num: None,
                text: line.clone(),
            });
        }
        for (j, line) in new_lines.iter().enumerate() {
            out.push(DiffLine {
                op: LineOp::Add,
                old_num: None,
                new_num: Some((j + 1) as u32),
                text: line.clone(),
            });
        }
        return out;
    }

    let stride = new_len + 1;
    let mut table = vec![0u32; (old_len + 1) * stride];
    for old_index in 1..=old_len {
        for new_index in 1..=new_len {
            if old_lines[old_index - 1] == new_lines[new_index - 1] {
                table[old_index * stride + new_index] =
                    table[(old_index - 1) * stride + new_index - 1] + 1;
            } else {
                let left = table[old_index * stride + new_index - 1];
                let up = table[(old_index - 1) * stride + new_index];
                table[old_index * stride + new_index] = left.max(up);
            }
        }
    }

    let mut result = Vec::new();
    let mut old_cursor = old_len;
    let mut new_cursor = new_len;
    while old_cursor > 0 || new_cursor > 0 {
        if old_cursor > 0
            && new_cursor > 0
            && old_lines[old_cursor - 1] == new_lines[new_cursor - 1]
        {
            result.push(DiffLine {
                op: LineOp::Equal,
                old_num: Some(old_cursor as u32),
                new_num: Some(new_cursor as u32),
                text: old_lines[old_cursor - 1].clone(),
            });
            old_cursor -= 1;
            new_cursor -= 1;
        } else if new_cursor > 0
            && (old_cursor == 0
                || table[old_cursor * stride + new_cursor - 1]
                    >= table[(old_cursor - 1) * stride + new_cursor])
        {
            result.push(DiffLine {
                op: LineOp::Add,
                old_num: None,
                new_num: Some(new_cursor as u32),
                text: new_lines[new_cursor - 1].clone(),
            });
            new_cursor -= 1;
        } else {
            result.push(DiffLine {
                op: LineOp::Remove,
                old_num: Some(old_cursor as u32),
                new_num: None,
                text: old_lines[old_cursor - 1].clone(),
            });
            old_cursor -= 1;
        }
    }
    result.reverse();
    result
}

/// Count added/removed lines (upstream `countStats`).
pub fn count_stats(diff: &[DiffLine]) -> Stats {
    let mut stats = Stats::default();
    for line in diff {
        match line.op {
            LineOp::Equal => {}
            LineOp::Add => stats.additions += 1,
            LineOp::Remove => stats.deletions += 1,
        }
    }
    stats
}

/// Number of decimal digits of `n` for gutter width (min 1).
fn decimal_digits(mut n: u32) -> usize {
    if n < 10 {
        return 1;
    }
    let mut digits = 0;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits.max(1)
}

/// Render the unified diff (upstream `formatUnified`): include changed lines
/// plus `context_lines` of surrounding context, cap at `max_display_lines`
/// with elision lines in the gutter.
pub fn format_unified(diff: &[DiffLine], path: Option<&str>) -> String {
    let mut include = vec![false; diff.len()];
    for (index, line) in diff.iter().enumerate() {
        if line.op == LineOp::Equal {
            continue;
        }
        let start = index.saturating_sub(CONTEXT_LINES);
        let end = (index + CONTEXT_LINES + 1).min(diff.len());
        include[start..end].fill(true);
    }

    let mut max_num: u32 = 1;
    for (index, line) in diff.iter().enumerate() {
        if !include[index] {
            continue;
        }
        let line_num = line.new_num.or(line.old_num).unwrap_or(0);
        if line_num > max_num {
            max_num = line_num;
        }
    }
    let gutter = decimal_digits(max_num);
    let width = gutter + 1;

    let mut out = String::new();
    if let Some(path) = path {
        out.push_str(&format!("\x1b[1m{diff} {path}\x1b[0m\n", diff = "diff --git"));
    }
    let mut prev_included = false;
    let mut emitted_any = false;
    let mut emitted_lines = 0usize;
    for (index, line) in diff.iter().enumerate() {
        if !include[index] {
            prev_included = false;
            continue;
        }
        if emitted_lines >= MAX_DISPLAY_LINES {
            out.push_str(&elision_line(width));
            break;
        }
        if !prev_included && emitted_any {
            out.push_str(&elision_line(width));
        }
        out.push_str(&render_diff_line(width, line));
        prev_included = true;
        emitted_any = true;
        emitted_lines += 1;
    }
    out
}

fn elision_line(width: usize) -> String {
    format!("{prefix:wid$}\n", prefix = "…", wid = width.saturating_add(2))
}

fn render_diff_line(width: usize, line: &DiffLine) -> String {
    let line_num = line.new_num.or(line.old_num).unwrap_or(0);
    let (sign, fg): (&str, &str) = match line.op {
        LineOp::Add => ("+", "\x1b[38;5;252m"),
        LineOp::Remove => ("-", "\x1b[38;5;252m"),
        LineOp::Equal => (" ", "\x1b[38;5;245m"),
    };
    format!(
        "{fg}{sign}{num:>width$} {text}\x1b[0m\n",
        sign = sign,
        num = line_num,
        width = width.saturating_sub(1),
        text = line.text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_produce_only_equal_ops() {
        let diff = compute("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(diff.len(), 3);
        for line in &diff {
            assert_eq!(line.op, LineOp::Equal, "{line:?}");
        }
        assert_eq!(count_stats(&diff), Stats::default());
    }

    #[test]
    fn pure_insert_in_the_middle() {
        let diff = compute("a\nb\n", "a\nX\nb\n");
        assert_eq!(diff.len(), 3);
        assert_eq!(diff[0].op, LineOp::Equal);
        assert_eq!(diff[1].op, LineOp::Add);
        assert_eq!(diff[1].text, "X");
        assert_eq!(diff[1].old_num, None);
        assert_eq!(diff[1].new_num, Some(2));
        assert_eq!(diff[2].op, LineOp::Equal);
        assert_eq!(count_stats(&diff), Stats { additions: 1, deletions: 0 });
    }

    #[test]
    fn pure_delete() {
        let diff = compute("a\nb\nc\n", "a\nc\n");
        assert_eq!(diff.len(), 3);
        assert_eq!(diff[1].op, LineOp::Remove);
        assert_eq!(diff[1].text, "b");
        assert_eq!(diff[1].old_num, Some(2));
        assert_eq!(diff[1].new_num, None);
        assert_eq!(count_stats(&diff), Stats { additions: 0, deletions: 1 });
    }

    #[test]
    fn lines_have_correct_1_based_numbers() {
        let diff = compute("one\ntwo\nthree\n", "one\ntwo\nthree\nfour\n");
        let adds = diff.iter().filter(|l| l.op == LineOp::Add).collect::<Vec<_>>();
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].text, "four");
        assert_eq!(adds[0].new_num, Some(4));
    }

    #[test]
    fn empty_text_handling() {
        let diff = compute("", "x\ny\n");
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().all(|l| l.op == LineOp::Add));
        let diff = compute("x\ny\n", "");
        assert!(diff.iter().all(|l| l.op == LineOp::Remove));
        assert_eq!(compute("", ""), vec![]);
    }

    #[test]
    fn trailing_newline_marker_is_noted() {
        // exactly one side has a trailing newline -> marker on that side
        let diff = compute("a\n", "a");
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].op, LineOp::Equal);
        assert_eq!(diff[1].op, LineOp::Remove);
        assert_eq!(diff[1].text, TRAILING_NEWLINE_REMOVED_MARKER);
        let diff = compute("a", "a\n");
        assert_eq!(diff[1].op, LineOp::Add);
        assert_eq!(diff[1].text, TRAILING_NEWLINE_ADDED_MARKER);
        // both/no trailing newline -> no marker
        let diff = compute("a\n", "a\n");
        assert!(!diff.iter().any(|l| l.text.contains("trailing newline")));
        let diff = compute("a", "a");
        assert_eq!(diff.len(), 1);
    }

    #[test]
    fn unified_format_includes_context_and_elides() {
        let old_text = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let new_text = (1..=30)
            .map(|n| if n == 15 { format!("CHANGED {n}") } else { format!("line {n}") })
            .collect::<Vec<_>>()
            .join("\n");
        let diff = compute(&old_text, &new_text);
        let rendered = format_unified(&diff, Some("file.txt"));
        assert!(rendered.contains("CHANGED 15"), "rendered: {rendered}");
        assert!(rendered.contains("line 13"), "context before");
        assert!(rendered.contains("line 17"), "context after");
        assert!(!rendered.contains("\u{2026}"), "single small hunk fits: {rendered}");
    }

    #[test]
    fn unified_format_caps_long_hunks_with_elision() {
        let old_text = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let new_text = (1..=30)
            .map(|n| if (15..=25).contains(&n) { format!("CHANGED {n}") } else { format!("line {n}") })
            .collect::<Vec<_>>()
            .join("\n");
        let diff = compute(&old_text, &new_text);
        let rendered = format_unified(&diff, Some("file.txt"));
        // The 6-line display cap shows the head of the hunk (context +
        // removals) then an elision; the add lines beyond the cap are omitted.
        assert!(rendered.contains("line 14"), "context: {rendered}");
        assert!(rendered.contains("\u{2026}"), "elision cap reached: {rendered}");
        assert!(!rendered.contains("CHANGED 15"), "beyond cap is elided: {rendered}");
        let count = rendered.lines().count();
        assert!(count <= MAX_DISPLAY_LINES + 2, "capped with elisions: {rendered}");
    }

    #[test]
    fn stats_count_adds_and_deletes() {
        let diff = compute("a\nb\nc\n", "a\nx\nc\nd\n");
        let stats = count_stats(&diff);
        assert_eq!(stats.additions, 2);
        assert_eq!(stats.deletions, 1);
    }
}
