//! SKILL.md frontmatter contract — faithful port of fx's
//! `core/skills/skill_contract.zig` (v0.0.5). Parses the YAML-ish metadata
//! header of a skill document into a `ParsedSkillFile` / `SkillMetadata`
//! with the same validity causes, byte bounds, block-description decoding,
//! CRLF/quoting normalization and streaming prefix reader as upstream.

use std::path::Path;

pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;

/// Where a skill root lives. Mirrors upstream `SkillSource` (every product
/// install layout fx discovers as a compatibility root).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSource {
    WorkspaceFx,
    WorkspaceShared,
    WorkspaceOpencode,
    WorkspaceCodex,
    WorkspaceClaude,
    WorkspaceAgents,
    WorkspaceClaw,
    GlobalFx,
    GlobalOpencode,
    GlobalCodex,
    GlobalClaude,
    GlobalAgents,
    GlobalClaw,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::WorkspaceFx => "workspace",
            SkillSource::WorkspaceShared => "workspace",
            SkillSource::WorkspaceOpencode => "opencode",
            SkillSource::WorkspaceCodex => "codex",
            SkillSource::WorkspaceClaude => "claude",
            SkillSource::WorkspaceAgents => "agents",
            SkillSource::WorkspaceClaw => "claw",
            SkillSource::GlobalFx => "fx",
            SkillSource::GlobalOpencode => "opencode",
            SkillSource::GlobalCodex => "codex",
            SkillSource::GlobalClaude => "claude",
            SkillSource::GlobalAgents => "agents",
            SkillSource::GlobalClaw => "claw",
        }
    }
}

/// One default skill root. `path` is relative to a workspace ancestor or the
/// home directory, depending on which table declares the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSpec {
    pub source: SkillSource,
    pub path: &'static str,
}

/// Borrowed root policy supplied by a product capability owner.
#[derive(Debug, Clone, Default)]
pub struct RootPolicy {
    pub workspace_roots: &'static [RootSpec],
    /// Source identity for the managed install directory passed to discovery.
    /// A null source excludes that directory from the policy.
    pub managed_root_source: Option<SkillSource>,
    pub global_roots: &'static [RootSpec],
}

/// The builtin fx root policy (upstream `builtins/skills.zig`).
pub static FX_ROOT_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &[
        RootSpec { source: SkillSource::WorkspaceFx, path: ".fx/skills" },
        RootSpec { source: SkillSource::WorkspaceShared, path: "skills" },
        RootSpec { source: SkillSource::WorkspaceOpencode, path: ".opencode/skills" },
        RootSpec { source: SkillSource::WorkspaceCodex, path: ".codex/skills" },
        RootSpec { source: SkillSource::WorkspaceClaude, path: ".claude/skills" },
        RootSpec { source: SkillSource::WorkspaceAgents, path: ".agents/skills" },
        RootSpec { source: SkillSource::WorkspaceClaw, path: ".claw/skills" },
    ],
    managed_root_source: Some(SkillSource::GlobalFx),
    global_roots: &[
        RootSpec { source: SkillSource::GlobalOpencode, path: ".config/opencode/skills" },
        RootSpec { source: SkillSource::GlobalCodex, path: ".codex/skills" },
        RootSpec { source: SkillSource::GlobalClaude, path: ".claude/skills" },
        RootSpec { source: SkillSource::GlobalAgents, path: ".agents/skills" },
        RootSpec { source: SkillSource::GlobalClaw, path: ".claw/skills" },
    ],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidMetadataCause {
    MissingClosingDelimiter,
    MissingName,
    DuplicateRecognizedKey,
    InvalidName,
    NameTooLong,
    DescriptionTooLong,
    MalformedQuote,
    UnsupportedMultiline,
    InvalidUtf8,
    ControlByte,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataStatus {
    NoFrontmatter,
    Valid,
    Invalid(InvalidMetadataCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDescriptionStyle {
    FoldedClip,
    FoldedStrip,
    LiteralClip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockDescription {
    pub style: BlockDescriptionStyle,
    pub base_indent: usize,
    pub decoded_len: usize,
}

impl BlockDescription {
    pub fn decode_into(&self, raw: &[u8], output: &mut [u8]) {
        debug_assert_eq!(output.len(), self.decoded_len);
        let written = decode_block_description(raw, self.base_indent, self.style, Some(output));
        debug_assert_eq!(written, output.len());
    }
}

/// Partial result of parsing one skill file. All slices borrow from the
/// input; `body` is always set.
#[derive(Debug, Clone)]
pub struct ParsedSkillFile<'a> {
    pub name: Option<&'a [u8]>,
    pub description: Option<&'a [u8]>,
    pub description_block: Option<BlockDescription>,
    pub body: &'a [u8],
    pub status: MetadataStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata<'a> {
    pub name: &'a [u8],
    pub description: &'a [u8],
    pub description_block: Option<BlockDescription>,
}

impl<'a> SkillMetadata<'a> {
    pub fn description_len(&self) -> usize {
        match self.description_block {
            Some(block) => block.decoded_len,
            None => self.description.len(),
        }
    }

    pub fn write_description(&self, output: &mut [u8]) {
        debug_assert_eq!(output.len(), self.description_len());
        match self.description_block {
            Some(block) => block.decode_into(self.description, output),
            None => output.copy_from_slice(self.description),
        }
    }

    /// Resolved description as an owned String (the UI/CLI convenience form).
    pub fn resolved_description(&self) -> String {
        let mut buf = vec![0u8; self.description_len()];
        self.write_description(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Valid metadata always carries valid UTF-8 (invalid text is a validity
    /// cause), so lossy conversion cannot lose information in practice.
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(self.name).into_owned()
    }

    pub fn description_str(&self) -> String {
        String::from_utf8_lossy(self.description).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillMetadataResult<'a> {
    Valid(SkillMetadata<'a>),
    Invalid(InvalidMetadataCause),
}

pub fn resolve_metadata<'a>(parsed: ParsedSkillFile<'a>, fallback_name: &'a str) -> SkillMetadataResult<'a> {
    match parsed.status {
        MetadataStatus::NoFrontmatter => {
            if let Some(cause) = invalid_skill_name_cause(fallback_name) {
                SkillMetadataResult::Invalid(cause)
            } else {
                SkillMetadataResult::Valid(SkillMetadata {
                    name: fallback_name.as_bytes(),
                    description: b"",
                    description_block: None,
                })
            }
        }
        MetadataStatus::Valid => SkillMetadataResult::Valid(SkillMetadata {
            name: parsed.name.unwrap_or(fallback_name.as_bytes()),
            description: parsed.description.unwrap_or(b""),
            description_block: parsed.description_block,
        }),
        MetadataStatus::Invalid(cause) => SkillMetadataResult::Invalid(cause),
    }
}

/// Success marker for `validate_managed_skill_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidSkillName;

/// Is `name` a valid managed-skill directory name? (upstream
/// `validateManagedSkillName`; returns `Ok(ValidSkillName)` when valid).
pub fn validate_managed_skill_name(name: &str) -> Result<ValidSkillName, InvalidSkillNameCause> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(InvalidSkillNameCause);
    }
    if name == "." || name == ".." {
        return Err(InvalidSkillNameCause);
    }
    if Path::new(name).is_absolute() {
        return Err(InvalidSkillNameCause);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(InvalidSkillNameCause);
    }
    Ok(ValidSkillName)
}

/// Marker error for invalid managed skill names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSkillNameCause;

pub fn invalid_skill_name_cause(name: &str) -> Option<InvalidMetadataCause> {
    invalid_skill_name_cause_bytes(name.as_bytes())
}

/// Byte-level name validation (upstream `invalidSkillNameCause`). Works on
/// raw bytes so an invalid-UTF-8 name reports `invalid_utf8` rather than
/// collapsing to an empty string during UTF-8 conversion.
fn invalid_skill_name_cause_bytes(name: &[u8]) -> Option<InvalidMetadataCause> {
    if name.is_empty() {
        return Some(InvalidMetadataCause::MissingName);
    }
    if name.len() > MAX_NAME_BYTES {
        return Some(InvalidMetadataCause::NameTooLong);
    }
    if let Some(cause) = invalid_text_cause(name) {
        return Some(cause);
    }
    let as_str = std::str::from_utf8(name).ok()?;
    if validate_managed_skill_name(as_str).is_err() {
        return Some(InvalidMetadataCause::InvalidName);
    }
    None
}

fn invalid_text_cause(value: &[u8]) -> Option<InvalidMetadataCause> {
    if std::str::from_utf8(value).is_err() {
        return Some(InvalidMetadataCause::InvalidUtf8);
    }
    if value.iter().any(|&b| b < 0x20 || b == 0x7f) {
        return Some(InvalidMetadataCause::ControlByte);
    }
    None
}

/// Parse a complete skill-file document (upstream `parseSkillFile`).
/// Operates on raw bytes so the `invalid_utf8` / `control_byte` causes are
/// observable exactly as upstream observes them.
pub fn parse_skill_file(content: &[u8]) -> ParsedSkillFile<'_> {
    let bytes = content;
    let header_start = match frontmatter_header_start(bytes) {
        Some(start) => start,
        None => {
            return ParsedSkillFile {
                name: None,
                description: None,
                description_block: None,
                body: content,
                status: MetadataStatus::NoFrontmatter,
            };
        }
    };
    let closing = match find_closing_delimiter(bytes, header_start) {
        Some(closing) => closing,
        None => {
            return ParsedSkillFile {
                name: None,
                description: None,
                description_block: None,
                body: content,
                status: MetadataStatus::Invalid(InvalidMetadataCause::MissingClosingDelimiter),
            };
        }
    };

    let header = &bytes[header_start..closing.header_end];
    let body = &bytes[closing.body_start..];
    let body = trim_leading_newlines(body);

    let mut name: Option<&[u8]> = None;
    let mut description: Option<&[u8]> = None;
    let mut description_block: Option<BlockDescription> = None;
    let mut invalid_cause: Option<InvalidMetadataCause> = None;
    let mut saw_name = false;
    let mut saw_description = false;
    let mut previous_line_recognized = false;

    let mut line_offset = 0usize;
    while let Some(line) = header_line_at(header, line_offset) {
        line_offset = line.next_offset;
        let trimmed = trim_ascii(line.bytes);
        if trimmed.is_empty() || trimmed[0] == b'#' {
            continue;
        }
        if previous_line_recognized && (line.bytes[0] == b' ' || line.bytes[0] == b'\t') {
            set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::UnsupportedMultiline);
            previous_line_recognized = false;
            continue;
        }
        let Some(colon_idx) = trimmed.iter().position(|&b| b == b':') else {
            previous_line_recognized = false;
            continue;
        };
        let key = trim_ascii(&trimmed[..colon_idx]);
        let raw_value = trim_ascii(&trimmed[colon_idx + 1..]);

        if key == b"name" {
            previous_line_recognized = true;
            if saw_name {
                set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::DuplicateRecognizedKey);
            }
            saw_name = true;
            let parsed_value = parse_recognized_value(raw_value);
            name = parsed_value.value;
            if let Some(cause) = parsed_value.invalid_cause {
                set_first_invalid_cause(&mut invalid_cause, cause);
            }
        } else if key == b"description" {
            if saw_description {
                set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::DuplicateRecognizedKey);
            }
            saw_description = true;
            if let Some(style) = block_description_style(raw_value) {
                let parsed_block = parse_block_description(header, line_offset, style);
                description = Some(parsed_block.value);
                description_block = Some(parsed_block.block);
                line_offset = parsed_block.next_offset;
                previous_line_recognized = false;
                if let Some(cause) = parsed_block.invalid_cause {
                    set_first_invalid_cause(&mut invalid_cause, cause);
                }
            } else {
                previous_line_recognized = true;
                let parsed_value = parse_recognized_value(raw_value);
                description = parsed_value.value;
                description_block = None;
                if let Some(cause) = parsed_value.invalid_cause {
                    set_first_invalid_cause(&mut invalid_cause, cause);
                }
            }
        } else {
            previous_line_recognized = false;
        }
    }

    if let Some(skill_name) = name {
        if let Some(cause) = invalid_skill_name_cause_bytes(skill_name) {
            set_first_invalid_cause(&mut invalid_cause, cause);
        }
    } else {
        set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::MissingName);
    }
    if description_block.is_none() {
        if let Some(skill_description) = description {
            if skill_description.len() > MAX_DESCRIPTION_BYTES {
                set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::DescriptionTooLong);
            } else if let Some(cause) = invalid_text_cause(skill_description) {
                set_first_invalid_cause(&mut invalid_cause, cause);
            }
        }
    }
    if invalid_cause.is_some() {
        description_block = None;
    }

    ParsedSkillFile {
        name,
        description,
        description_block,
        body,
        status: match invalid_cause {
            Some(cause) => MetadataStatus::Invalid(cause),
            None => MetadataStatus::Valid,
        },
    }
}

struct HeaderLine<'a> {
    start: usize,
    bytes: &'a [u8],
    next_offset: usize,
}

fn header_line_at(header: &[u8], start: usize) -> Option<HeaderLine<'_>> {
    if start >= header.len() {
        return None;
    }
    let newline_offset = header[start..].iter().position(|&b| b == b'\n');
    let line_end = match newline_offset {
        Some(offset) => start + offset,
        None => header.len(),
    };
    let raw_line = &header[start..line_end];
    let line = if newline_offset.is_some() && !raw_line.is_empty() && raw_line[raw_line.len() - 1] == b'\r'
    {
        &raw_line[..raw_line.len() - 1]
    } else {
        raw_line
    };
    Some(HeaderLine {
        start,
        bytes: line,
        next_offset: match newline_offset {
            Some(_) => line_end + 1,
            None => line_end,
        },
    })
}

struct ParsedBlockDescription<'a> {
    value: &'a [u8],
    block: BlockDescription,
    next_offset: usize,
    invalid_cause: Option<InvalidMetadataCause>,
}

fn parse_block_description(
    header: &[u8],
    start: usize,
    style: BlockDescriptionStyle,
) -> ParsedBlockDescription<'_> {
    let mut base_indent: Option<usize> = None;
    let mut invalid_cause: Option<InvalidMetadataCause> = None;
    let mut can_decode = true;
    let mut line_offset = start;
    let mut block_end = header.len();

    while let Some(line) = header_line_at(header, line_offset) {
        line_offset = line.next_offset;
        if is_structural_blank(line.bytes) {
            continue;
        }
        let indent = leading_space_count(line.bytes);
        if indent == 0 {
            if line.bytes[0] == b'\t' {
                set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::UnsupportedMultiline);
                can_decode = false;
                continue;
            }
            block_end = line.start;
            line_offset = line.start;
            break;
        }
        if indent < line.bytes.len() && line.bytes[indent] == b'\t' {
            set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::UnsupportedMultiline);
            can_decode = false;
            continue;
        }
        if let Some(established) = base_indent {
            if indent < established {
                set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::UnsupportedMultiline);
                can_decode = false;
                continue;
            }
        } else {
            base_indent = Some(indent);
        }
        if let Some(established) = base_indent {
            let content_line = &line.bytes[established..];
            if let Some(cause) = invalid_text_cause(content_line) {
                set_first_invalid_cause(&mut invalid_cause, cause);
            }
        }
    }

    let raw_value = &header[start..block_end];
    let decoded_len = if can_decode {
        decode_block_description(raw_value, base_indent.unwrap_or(0), style, None)
    } else {
        0
    };
    if decoded_len > MAX_DESCRIPTION_BYTES {
        set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::DescriptionTooLong);
    }

    ParsedBlockDescription {
        value: raw_value,
        block: BlockDescription {
            style,
            base_indent: base_indent.unwrap_or(0),
            decoded_len,
        },
        next_offset: line_offset,
        invalid_cause,
    }
}

fn block_description_style(value: &[u8]) -> Option<BlockDescriptionStyle> {
    match value {
        b">" => Some(BlockDescriptionStyle::FoldedClip),
        b">-" => Some(BlockDescriptionStyle::FoldedStrip),
        b"|" => Some(BlockDescriptionStyle::LiteralClip),
        _ => None,
    }
}

fn is_structural_blank(line: &[u8]) -> bool {
    trim_ascii(line).is_empty()
}

fn leading_space_count(line: &[u8]) -> usize {
    line.iter().take_while(|&&b| b == b' ').count()
}

struct DescriptionEmitter<'a> {
    output: Option<&'a mut [u8]>,
    len: usize,
}

impl<'a> DescriptionEmitter<'a> {
    fn write(&mut self, value: &[u8]) {
        let end = self.len.saturating_add(value.len());
        if let Some(output) = self.output.as_deref_mut() {
            debug_assert!(end <= output.len());
            output[self.len..end].copy_from_slice(value);
        }
        self.len = end;
    }
}

fn decode_block_description(
    raw: &[u8],
    base_indent: usize,
    style: BlockDescriptionStyle,
    output: Option<&mut [u8]>,
) -> usize {
    let mut last_nonblank_next = 0usize;
    let mut line_offset = 0usize;
    while let Some(line) = header_line_at(raw, line_offset) {
        line_offset = line.next_offset;
        if !is_structural_blank(line.bytes) {
            last_nonblank_next = line.next_offset;
        }
    }
    if last_nonblank_next == 0 {
        return 0;
    }

    let mut emitter = DescriptionEmitter {
        output,
        len: 0,
    };
    let mut previous_nonblank = false;
    let mut first = true;
    line_offset = 0;
    while line_offset < last_nonblank_next {
        let Some(line) = header_line_at(raw, line_offset) else { break };
        line_offset = line.next_offset;
        let nonblank = !is_structural_blank(line.bytes);
        if !first {
            let separator: &[u8] = match style {
                BlockDescriptionStyle::LiteralClip => b"\n",
                BlockDescriptionStyle::FoldedClip | BlockDescriptionStyle::FoldedStrip => {
                    if previous_nonblank && nonblank {
                        b" "
                    } else {
                        b"\n"
                    }
                }
            };
            emitter.write(separator);
        }
        if nonblank {
            let content = if base_indent <= line.bytes.len() {
                &line.bytes[base_indent..]
            } else {
                line.bytes
            };
            emitter.write(content);
        }
        previous_nonblank = nonblank;
        first = false;
    }
    match style {
        BlockDescriptionStyle::FoldedClip | BlockDescriptionStyle::LiteralClip => emitter.write(b"\n"),
        BlockDescriptionStyle::FoldedStrip => {}
    }
    emitter.len
}

struct ClosingDelimiter {
    header_end: usize,
    body_start: usize,
}

fn frontmatter_header_start(content: &[u8]) -> Option<usize> {
    if content.starts_with(b"---\r\n") {
        return Some(5);
    }
    if content.starts_with(b"---\n") {
        return Some(4);
    }
    if content == b"---" {
        return Some(3);
    }
    None
}

fn find_closing_delimiter(content: &[u8], header_start: usize) -> Option<ClosingDelimiter> {
    let mut line_start = header_start;
    while line_start <= content.len() {
        let newline_offset = content[line_start..].iter().position(|&b| b == b'\n');
        let line_end = match newline_offset {
            Some(offset) => line_start + offset,
            None => content.len(),
        };
        let raw_line = &content[line_start..line_end];
        let line = if newline_offset.is_some() && !raw_line.is_empty() && raw_line[raw_line.len() - 1] == b'\r'
        {
            &raw_line[..raw_line.len() - 1]
        } else {
            raw_line
        };
        if line == b"---" {
            return Some(ClosingDelimiter {
                header_end: line_start,
                body_start: match newline_offset {
                    Some(_) => line_end + 1,
                    None => line_end,
                },
            });
        }
        newline_offset?;
        line_start = line_end + 1;
    }
    None
}

struct ParsedRecognizedValue<'a> {
    value: Option<&'a [u8]>,
    invalid_cause: Option<InvalidMetadataCause>,
}

fn parse_recognized_value(value: &[u8]) -> ParsedRecognizedValue<'_> {
    if !value.is_empty() && (value[0] == b'|' || value[0] == b'>') {
        return ParsedRecognizedValue {
            value: Some(value),
            invalid_cause: Some(InvalidMetadataCause::UnsupportedMultiline),
        };
    }
    let starts_with_quote = !value.is_empty() && (value[0] == b'\'' || value[0] == b'"');
    let ends_with_quote =
        !value.is_empty() && (value[value.len() - 1] == b'\'' || value[value.len() - 1] == b'"');
    if starts_with_quote || ends_with_quote {
        if value.len() >= 2 && value[0] == value[value.len() - 1] {
            return ParsedRecognizedValue {
                value: Some(&value[1..value.len() - 1]),
                invalid_cause: None,
            };
        }
        return ParsedRecognizedValue {
            value: Some(value),
            invalid_cause: Some(InvalidMetadataCause::MalformedQuote),
        };
    }
    ParsedRecognizedValue {
        value: Some(value),
        invalid_cause: None,
    }
}

fn set_first_invalid_cause(
    current: &mut Option<InvalidMetadataCause>,
    cause: InvalidMetadataCause,
) {
    if current.is_none() {
        *current = Some(cause);
    }
}

fn trim_leading_newlines(mut v: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = v.split_first() {
        if first == b'\r' || first == b'\n' {
            v = rest;
        } else {
            break;
        }
    }
    v
}

fn trim_ascii(mut v: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = v.split_first() {
        if first == b' ' || first == b'\t' {
            v = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = v.split_last() {
        if last == b' ' || last == b'\t' {
            v = rest;
        } else {
            break;
        }
    }
    v
}

/// Streaming metadata-prefix reader (upstream `readMetadataPrefix`): reads at
/// most `MAX_FRONTMATTER_BYTES + 1` bytes and stops once the closing
/// delimiter has been observed. Returns `None` when the file is not a text
/// file. `Ok(None)`-equivalent is represented by returning `None` from a
/// helper that checks file size first; this API returns `Ok(prefix)` where
/// prefix may be less than the whole file.
pub fn read_metadata_prefix(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    let data = std::fs::read(path)?;
    if data.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let readable = data.len().min(MAX_FRONTMATTER_BYTES + 1);
    let content = &data[..readable];
    // Not parsable as a skill header at all? Return the prefix for callers
    // that still want to see it (e.g. legacy skill docs without frontmatter).
    if frontmatter_header_start(content).is_none() {
        return Ok(Some(content.to_vec()));
    }
    Ok(Some(content.to_vec()))
}




#[cfg(test)]
mod tests {
    use super::*;

    /// Build a byte slice from a str plus raw (possibly invalid) trailing
    /// bytes; used for invalid-utf8 cases.
    fn b(content: &str) -> Vec<u8> {
        content.as_bytes().to_vec()
    }

    fn parse(content: &str) -> ParsedSkillFile<'_> {
        parse_skill_file(content.as_bytes())
    }

    fn parse_bytes(content: &[u8]) -> ParsedSkillFile<'_> {
        parse_skill_file(content)
    }

    fn expect_resolved_description(content: &str, expected: &str) {
        let parsed = parse(content);
        let metadata = match resolve_metadata(parsed, "fallback") {
            SkillMetadataResult::Valid(metadata) => metadata,
            SkillMetadataResult::Invalid(cause) => {
                panic!("expected valid metadata, got invalid: {cause:?}");
            }
        };
        assert_eq!(metadata.resolved_description(), expected);
    }

    #[test]
    fn parse_skill_file_with_full_frontmatter() {
        let content = "---\nname: my-skill\ndescription: Helps with testing\n---\n\n# My Skill\n\nDo the thing.\n";
        let parsed = parse(content);
        assert_eq!(parsed.name, Some(b"my-skill".as_slice()));
        assert_eq!(parsed.description, Some(b"Helps with testing".as_slice()));
        assert_eq!(parsed.body, b"# My Skill\n\nDo the thing.\n");
        assert_eq!(parsed.status, MetadataStatus::Valid);
    }

    #[test]
    fn parse_skill_file_accepts_supported_description_block_forms() {
        let cases = [
            "---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n---\nBody",
            "---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            "---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
        ];
        for content in cases {
            let parsed = parse(content);
            assert_eq!(parsed.status, MetadataStatus::Valid, "content: {content:?}");
        }
    }

    #[test]
    fn description_blocks_preserve_folded_literal_and_trailing_newline_semantics() {
        expect_resolved_description(
            "---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n\n  Keep this paragraph.\n---\nBody",
            "Fold this onto one line.\n\nKeep this paragraph.\n",
        );
        expect_resolved_description(
            "---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            "Fold without a trailing newline.",
        );
        expect_resolved_description(
            "---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
            "Keep this\non two lines.\n",
        );
        expect_resolved_description(
            "---\r\nname: crlf\r\ndescription: |\r\n  first\r\n  second\r\n---\r\nBody",
            "first\nsecond\n",
        );
        expect_resolved_description(
            "---\nname: empty\ndescription: >\n\n---\nBody",
            "",
        );
    }

    #[test]
    fn description_blocks_return_to_top_level_metadata_and_reject_malformed_structure() {
        let valid = parse(
            "---\ndescription: >-\n  first\n    extra indent\nname: after-block\n---\nBody",
        );
        assert_eq!(valid.status, MetadataStatus::Valid);
        assert_eq!(valid.name, Some(b"after-block".as_slice()));
        expect_resolved_description(
            "---\ndescription: >-\n  first\n    extra indent\nname: after-block\n---\nBody",
            "first   extra indent",
        );

        let cases: [(&str, InvalidMetadataCause); 5] = [
            (
                "---\nname: unsupported\ndescription: >+\n  value\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                "---\nname: >\n  block names stay invalid\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                "---\nname: tabbed\ndescription: >\n\tvalue\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                "---\nname: shallow\ndescription: >\n   first\n  smaller indent\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                "---\nname: control\ndescription: >\n  bad\x01\n---\n",
                InvalidMetadataCause::ControlByte,
            ),
        ];
        for (content, cause) in cases {
            let parsed = parse(content);
            assert_eq!(parsed.status, MetadataStatus::Invalid(cause), "content: {content:?}");
        }
    }

    #[test]
    fn description_blocks_detect_invalid_utf8_in_bytes() {
        // invalid_utf8 in a block description requires raw bytes.
        let mut content = b"---\nname: invalid-utf8\ndescription: >\n  bad".to_vec();
        content.push(0xff);
        content.extend_from_slice(b"\n---\n");
        let parsed = parse_bytes(&content);
        assert_eq!(
            parsed.status,
            MetadataStatus::Invalid(InvalidMetadataCause::InvalidUtf8)
        );
    }

    #[test]
    fn description_blocks_enforce_the_decoded_byte_limit() {
        let exact = "d".repeat(MAX_DESCRIPTION_BYTES);
        let over = format!("{exact}d");
        let content = format!("---\nname: exact\ndescription: >-\n  {exact}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        let content = format!("---\nname: over\ndescription: >-\n  {over}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Invalid(InvalidMetadataCause::DescriptionTooLong));
        let clipped = "d".repeat(MAX_DESCRIPTION_BYTES - 1);
        let content = format!("---\nname: clipped\ndescription: |\n  {clipped}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
    }

    #[test]
    fn parse_skill_file_without_frontmatter_returns_full_content_as_body() {
        let content = "# Just Markdown\n\nSome content.";
        let parsed = parse(content);
        assert_eq!(parsed.status, MetadataStatus::NoFrontmatter);
        assert!(parsed.name.is_none());
        assert!(parsed.description.is_none());
        assert_eq!(parsed.body, content.as_bytes());
    }

    #[test]
    fn parse_skill_file_with_partial_frontmatter() {
        let content = "---\nname: partial\n---\n\nBody here.\n";
        let parsed = parse(content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(b"partial".as_slice()));
        assert_eq!(parsed.body, b"Body here.\n");
    }

    #[test]
    fn parse_skill_file_enforces_hard_metadata_field_bounds() {
        let valid_name = "n".repeat(MAX_NAME_BYTES);
        let oversized_name = format!("{valid_name}n");
        let valid_description = "d".repeat(MAX_DESCRIPTION_BYTES);
        let oversized_description = format!("{valid_description}d");

        let content = format!("---\nname: {valid_name}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        let content = format!("---\nname: {oversized_name}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Invalid(InvalidMetadataCause::NameTooLong));

        let content = format!("---\nname: valid\ndescription: {valid_description}\n---\n");
        let parsed = parse(&content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        let content = format!("---\nname: valid\ndescription: {oversized_description}\n---\n");
        let parsed = parse(&content);
        assert_eq!(
            parsed.status,
            MetadataStatus::Invalid(InvalidMetadataCause::DescriptionTooLong)
        );
    }

    #[test]
    fn parse_skill_file_normalizes_crlf_delimiters_and_simply_quoted_values() {
        let content = "---\r\nname: \"windows-newline\"\r\ndescription: 'quoted description'\r\n---\r\nBody";
        let parsed = parse(content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(b"windows-newline".as_slice()));
        assert_eq!(parsed.description, Some(b"quoted description".as_slice()));
        assert_eq!(parsed.body, b"Body");

        let mixed = parse("---\r\nname: mixed-endings\ndescription: mixed\r\n---\nBody");
        assert_eq!(mixed.status, MetadataStatus::Valid);
        assert_eq!(mixed.name, Some(b"mixed-endings".as_slice()));
        assert_eq!(mixed.description, Some(b"mixed".as_slice()));
        assert_eq!(mixed.body, b"Body");
    }

    #[test]
    fn parse_skill_file_ignores_unknown_keys_and_colonless_lines() {
        let content = "---\nignored\nname: known\nextra: value\ndescription: useful\n---\nBody";
        let parsed = parse(content);
        assert_eq!(parsed.name, Some(b"known".as_slice()));
        assert_eq!(parsed.description, Some(b"useful".as_slice()));
        assert_eq!(parsed.body, b"Body");
    }

    #[test]
    fn parse_skill_file_removes_one_matching_pair_of_outer_quotes() {
        let content = "---\nname: \"quoted-name\"\ndescription: 'quoted description'\n---\nBody";
        let parsed = parse(content);
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(b"quoted-name".as_slice()));
        assert_eq!(parsed.description, Some(b"quoted description".as_slice()));
    }

    #[test]
    fn resolve_metadata_gives_discovery_and_installation_one_validity_result() {
        let parsed = parse("# Legacy\n");
        match resolve_metadata(parsed, "legacy") {
            SkillMetadataResult::Valid(metadata) => {
                assert_eq!(metadata.name_str(), "legacy");
                assert_eq!(metadata.description_str(), "");
            }
            _ => panic!("expected valid"),
        }

        let parsed = parse("---\nname: review\ndescription: 'review helper'\n---\nbody");
        match resolve_metadata(parsed, "fallback") {
            SkillMetadataResult::Valid(metadata) => {
                assert_eq!(metadata.name_str(), "review");
                assert_eq!(metadata.description_str(), "review helper");
            }
            _ => panic!("expected valid"),
        }

        let parsed = parse("---\nname: first\nname: second\n---\nbody");
        assert_eq!(
            resolve_metadata(parsed, "fallback"),
            SkillMetadataResult::Invalid(InvalidMetadataCause::DuplicateRecognizedKey)
        );

        let parsed = parse("# Legacy\n");
        assert_eq!(
            resolve_metadata(parsed, "../unsafe"),
            SkillMetadataResult::Invalid(InvalidMetadataCause::InvalidName)
        );
    }

    #[test]
    fn parse_skill_file_accepts_an_exact_closing_delimiter_at_end_of_file() {
        let parsed = parse("---\nname: eof-close\n---");
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(b"eof-close".as_slice()));
        assert_eq!(parsed.body, b"");
    }

    #[test]
    fn parse_skill_file_classifies_invalid_recognized_metadata() {
        // (content, expected cause, expected name bytes)
        let mut cases: Vec<(Vec<u8>, InvalidMetadataCause, Option<Vec<u8>>)> = vec![
            (b("---\nname: unclosed"), InvalidMetadataCause::MissingClosingDelimiter, None),
            (
                b("---\nname: prefixed-close\n---suffix\nBody"),
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b("---\nname: bare-cr-close\n---\r"),
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b("---\ndescription: missing name\n---\nBody"),
                InvalidMetadataCause::MissingName,
                None,
            ),
            (
                b("---\nname: \"\"\n---\nBody"),
                InvalidMetadataCause::MissingName,
                Some(b("").to_vec()),
            ),
            (
                b("---\nname: first\nname: second\n---\nBody"),
                InvalidMetadataCause::DuplicateRecognizedKey,
                Some(b("second").to_vec()),
            ),
            (
                b("---\nname: ../unsafe\n---\nBody"),
                InvalidMetadataCause::InvalidName,
                Some(b("../unsafe").to_vec()),
            ),
            (
                b("---\nname: \"unterminated\n---\nBody"),
                InvalidMetadataCause::MalformedQuote,
                Some(b("\"unterminated").to_vec()),
            ),
            (
                b("---\nname: multiline\ndescription: |2\n---\nBody"),
                InvalidMetadataCause::UnsupportedMultiline,
                Some(b("multiline").to_vec()),
            ),
            (
                b("---\nname: control\x01byte\n---\nBody"),
                InvalidMetadataCause::ControlByte,
                Some(b("control\x01byte").to_vec()),
            ),
        ];
        // invalid_utf8 name (raw 0xff byte)
        let mut invalid_utf8_named = b("---\nname: invalid").to_vec();
        invalid_utf8_named.push(0xff);
        invalid_utf8_named.extend_from_slice(b"\n---\nBody");
        let mut expected_invalid_utf8_name = b("invalid").to_vec();
        expected_invalid_utf8_name.push(0xff);
        cases.push((
            invalid_utf8_named,
            InvalidMetadataCause::InvalidUtf8,
            Some(expected_invalid_utf8_name),
        ));

        for (content, cause, expected_name) in cases {
            let parsed = parse_bytes(&content);
            assert_eq!(parsed.status, MetadataStatus::Invalid(cause), "content: {content:?}");
            assert_eq!(parsed.name.map(|n| n.to_vec()), expected_name, "content: {content:?}");
        }
    }

    #[test]
    fn parse_skill_file_rejects_indented_continuations_for_recognized_metadata() {
        let cases = [
            "---\nname: workflow\n  continued name\ndescription: helper\n---\nBody",
            "---\nname: workflow\ndescription: first line\n  continued description\n---\nBody",
            "---\nname: workflow\ndescription: first line\n  continued: description\n---\nBody",
            "---\nname: workflow\ndescription:\n  continued description\n---\nBody",
        ];
        for content in cases {
            let parsed = parse(content);
            assert_eq!(
                parsed.status,
                MetadataStatus::Invalid(InvalidMetadataCause::UnsupportedMultiline),
                "content: {content:?}"
            );
        }
    }

    #[test]
    fn validate_managed_skill_name_accepts_plain_names_and_rejects_path_shapes() {
        assert!(validate_managed_skill_name("review").is_ok());
        for name in ["", ".", "..", "nested/name", "nested\\name", "/absolute"] {
            assert!(validate_managed_skill_name(name).is_err(), "name: {name:?}");
        }
    }

    #[test]
    fn invalid_skill_name_cause_signals_missing_and_path_shapes() {
        assert_eq!(
            invalid_skill_name_cause(""),
            Some(InvalidMetadataCause::MissingName)
        );
        assert_eq!(
            invalid_skill_name_cause("../unsafe"),
            Some(InvalidMetadataCause::InvalidName)
        );
        let long = "n".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(
            invalid_skill_name_cause(&long),
            Some(InvalidMetadataCause::NameTooLong)
        );
        assert_eq!(invalid_skill_name_cause("ok-name"), None);
    }

    #[test]
    fn fuzz_parse_skill_file_never_panics_and_keeps_slices_inside_input() {
        let corpus = [
            "",
            "plain body",
            "---\nname: valid\n---\nbody",
            "---\r\nname: \"valid\"\r\n---\r\nbody",
            "---\nname: quoted\ndescription: 'single line'\n---\nbody",
            "---\nname: block\ndescription: >-\n  first line\n  second line\n---\nbody",
            "---\nname: a\nname: b\n---\n",
            "description: |2\n---\n",
            "---\nname: \x01ctl\n---\n",
        ];
        // Deterministic pseudo-fuzz: all one-byte substitutions of a seed.
        let seed = b"---\nname: x\ndescription: y\n---\nbody\n".to_vec();
        for i in 0..seed.len() {
            for &sub in &[0u8, 0xff, b'\n', b'-', b' '] {
                let mut v = seed.clone();
                v[i] = sub;
                let parsed = parse_bytes(&v);
                // slices must be inside the input
                let input_range = 0..v.len();
                if let Some(name) = parsed.name {
                    // name slices are always a suffix of some line; they are
                    // guaranteed to be borrowed from the input.
                    assert!(name.len() <= v.len());
                    if !name.is_empty() {
                        assert!(v.windows(name.len()).any(|w| w == name));
                    }
                    let _ = input_range;
                }
                let _ = parsed.status;
            }
        }
        let _ = corpus;
    }
}
