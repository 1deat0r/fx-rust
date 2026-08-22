//! Skills runtime — faithful port of fx's `core/skills/skill_runtime.zig`
//! surface used by discovery, the catalog, the `<available_skills>` system
//! prompt section, and resource reads. The contract parser lives in
//! `contract.rs`; the `/skills` command harness lives in `commands.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod commands;
pub mod contract;

pub use contract::{
    invalid_skill_name_cause, parse_skill_file, resolve_metadata, validate_managed_skill_name,
    BlockDescription, BlockDescriptionStyle, InvalidMetadataCause, MetadataStatus,
    ParsedSkillFile, RootPolicy, RootSpec, SkillMetadata, SkillMetadataResult, SkillSource,
    FX_ROOT_POLICY, MAX_DESCRIPTION_BYTES, MAX_FRONTMATTER_BYTES, MAX_NAME_BYTES,
};

/// Defaults for the two skill context limits (upstream context_limits.zig).
pub const SKILL_DESCRIPTION_BYTES_DEFAULT: usize = 1024;
pub const SKILL_CATALOG_BYTES_DEFAULT: usize = 16 * 1024;
/// Default read limit for a single skill file body (skill_file_bytes).
pub const SKILL_FILE_BYTES_DEFAULT: usize = 1024 * 1024;
/// Default per-chunk limit when a skill body is loaded in chunks.
pub const SKILL_CHUNK_BYTES_DEFAULT: usize = 20 * 1024;

/// A discovered skill. `managed_install` is true when the skill lives in the
/// fx managed install root (`~/.fx/skills`).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: SkillSource,
    pub managed_install: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticScope {
    Root,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDiagnosticCause {
    InvalidMetadata(InvalidMetadataCause),
    Unreadable,
    Oversized,
}

#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub source: SkillSource,
    pub scope: SkillDiagnosticScope,
    pub cause: SkillDiagnosticCause,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl Catalog {
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn find_by_path(&self, path: &Path) -> Option<&Skill> {
        self.skills.iter().find(|s| s.path == path)
    }

    pub fn is_managed(&self, name: &str) -> bool {
        self.find(name).map(|s| s.managed_install).unwrap_or(false)
    }
}

/// The managed install root is the fx home skills directory.
pub fn managed_root() -> PathBuf {
    crate::config::fx_home().join("skills")
}

/// Collect skill roots for a workspace in precedence order, mirroring
/// `appendWorkspaceRoots` (workspace ancestor scan) plus the managed root and
/// home compatibility roots. `/home`-relative: the ancestor scan stops at —
/// but does not include — the home directory itself.
pub fn collect_roots(workspace: &Path, home: Option<&Path>) -> Vec<(PathBuf, SkillSource, bool)> {
    let mut roots: Vec<(PathBuf, SkillSource, bool)> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let managed_root_path = managed_root();

    // Workspace ancestors (inclusive) up to but not including home.
    let mut current = Some(workspace.to_path_buf());
    while let Some(dir) = current {
        if let Some(home_root) = home {
            if dir == home_root {
                break;
            }
        }
        for spec in FX_ROOT_POLICY.workspace_roots {
            let path = dir.join(spec.path);
            if !seen.contains(&path) {
                let managed = path == managed_root_path;
                seen.push(path.clone());
                roots.push((path, spec.source, managed));
            }
        }
        current = dir.parent().map(|p| p.to_path_buf());
    }

    // Managed install root (global_fx).
    let managed = managed_root_path;
    if !seen.contains(&managed) {
        seen.push(managed.clone());
        roots.push((managed, SkillSource::GlobalFx, true));
    }

    // Home compatibility roots.
    if let Some(home_root) = home {
        for spec in FX_ROOT_POLICY.global_roots {
            let path = home_root.join(spec.path);
            if !seen.contains(&path) {
                seen.push(path.clone());
                roots.push((path, spec.source, false));
            }
        }
    }

    roots
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Discover skills across all roots in precedence order. The first root that
/// contains a given canonical path or skill name wins. Diagnostics capture
/// invalid/unreadable/oversized candidates (upstream appendSkillsFromDir).
pub fn discover(workspace: &Path) -> Catalog {
    let home = dirs::home_dir();
    let roots = collect_roots(workspace, home.as_deref());

    let mut catalog = Catalog::default();
    let mut seen_canonical: Vec<PathBuf> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();

    for (root, source, managed) in roots {
        if !root.is_dir() {
            continue;
        }
        // Root-level diagnostic: root exists but is unreadable.
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => {
                catalog.diagnostics.push(SkillDiagnostic {
                    path: root,
                    source,
                    scope: SkillDiagnosticScope::Root,
                    cause: SkillDiagnosticCause::Unreadable,
                });
                continue;
            }
        };
        let _canonical_root = canonical(&root);

        // The managed root may legitimately be missing; a bare root dir with
        // no candidates is fine. Upstream keeps diagnostics for root paths
        // that cannot be opened; we do the same for existing-but-unreadable.
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            let name = entry_name.to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }
            let candidate = root.join(&name);
            let canonical_candidate = canonical(&candidate);
            if seen_canonical.contains(&canonical_candidate) {
                continue;
            }
            let skill_md = candidate.join("SKILL.md");
            let (name, description) = match read_candidate_metadata(&skill_md) {
                Some(Ok(metadata)) => metadata,
                Some(Err(cause)) => {
                    seen_canonical.push(canonical_candidate);
                    catalog.diagnostics.push(SkillDiagnostic {
                        path: candidate,
                        source,
                        scope: SkillDiagnosticScope::Candidate,
                        cause,
                    });
                    continue;
                }
                None => continue,
            };
            seen_canonical.push(canonical_candidate.clone());
            if seen_names.contains(&name) {
                continue;
            }
            seen_names.push(name.clone());
            catalog.skills.push(Skill {
                name,
                description,
                path: canonical_candidate,
                source,
                managed_install: managed,
            });
            let _ = _canonical_root;
        }
    }

    catalog.skills.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// Read a candidate's metadata prefix, returning:
/// - None when SKILL.md is missing (a plain directory is not a skill).
/// - Some(Err(cause)) for invalid/unreadable/oversized candidates.
/// - Some(Ok(metadata)) for a valid skill.
fn read_candidate_metadata(
    skill_md: &Path,
) -> Option<Result<(String, String), SkillDiagnosticCause>> {
    let data = std::fs::read(skill_md).ok()?;
    // An empty (or legacy, no-frontmatter) SKILL.md is still a valid skill:
    // discovery falls back to the directory name, matching upstream
    // `readMetadataPrefix` + `resolveMetadata`.
    if data.len() > SKILL_FILE_BYTES_DEFAULT {
        return Some(Err(SkillDiagnosticCause::Oversized));
    }
    let content: &[u8] = if data.len() > MAX_FRONTMATTER_BYTES + 1 {
        &data[..MAX_FRONTMATTER_BYTES + 1]
    } else {
        &data
    };
    let fallback = skill_md
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parsed = parse_skill_file(content);
    match resolve_metadata(parsed, &fallback) {
        SkillMetadataResult::Valid(metadata) => {
            Some(Ok((metadata.name_str(), metadata.resolved_description())))
        }
        SkillMetadataResult::Invalid(cause) => {
            Some(Err(SkillDiagnosticCause::InvalidMetadata(cause)))
        }
    }
}

/// A bounded `<available_skills>` system-prompt section, faithful to
/// upstream `buildSkillsSystemPromptSectionWithLimits`. Returns the section
/// text and an optional notice about omitted/truncated entries.
pub fn build_prompt_section(
    skills: &[Skill],
    description_limit: usize,
    catalog_limit: usize,
) -> (String, Option<String>) {
    if skills.is_empty() {
        return (String::new(), None);
    }
    let header =
        "\n\nSkills provide specialized instructions and workflows for specific tasks.\n\
         Use the skill tool to load a skill when a task matches its description.\n\
         Do not assume a skill is loaded just because it is available. Load it first when it seems relevant.\n\
         <available_skills>\n";
    let footer = "</available_skills>\n";

    let mut entries: Vec<String> = Vec::with_capacity(skills.len());
    let mut observed_catalog_bytes = header.len() + footer.len();
    let mut truncation_notice: Option<String> = None;
    let effective_description_limit = description_limit;

    let total_active = skills.len();
    let _retained_count = total_active;

    // First pass: build entries, bounding each description.
    let desc_limit = effective_description_limit;
    for skill in skills {
        let mut desc = skill.description.clone();
        let observed = desc.len();
        if observed > desc_limit {
            truncation_notice = Some(format!(
                "<context_limit name=\"skill_description_bytes\" action=\"truncated\" observed_bytes=\"{observed}\" effective_bytes=\"{desc_limit}\" />"
            ));
            let mut truncated = String::from(&desc[..desc_limit]);
            truncated.push_str("[truncated]");
            desc = truncated;
        }
        let entry = format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
            xml_escape(&skill.name),
            xml_escape(&desc),
            xml_escape(&skill.path.display().to_string()),
        );
        observed_catalog_bytes += entry.len();
        entries.push(entry);
    }

    // Second pass: honor the catalog limit by omitting trailing entries.
    if observed_catalog_bytes > catalog_limit {
        let mut retained_bytes = header.len() + footer.len();
        let mut keep = 0usize;
        for entry in &entries {
            let would = retained_bytes + entry.len();
            if would > catalog_limit {
                break;
            }
            retained_bytes = would;
            keep += 1;
        }
        if keep < total_active {
            let omitted = total_active - keep;
            let notice = format!(
                "  <context_limit name=\"skill_catalog_bytes\" action=\"omitted\" omitted_count=\"{omitted}\" observed_bytes=\"{observed_catalog_bytes}\" effective_bytes=\"{catalog_limit}\" />\n"
            );
            entries.truncate(keep);
            let _ = keep;
            let mut s = String::from(header);
            for e in &entries {
                s.push_str(e);
            }
            s.push_str(&notice);
            s.push_str(footer);
            let _ = observed_catalog_bytes;
            let _ = desc_limit;
            let _ = effective_description_limit;
            return (s, truncation_notice);
        }
    }

    let mut s = String::from(header);
    for e in &entries {
        s.push_str(e);
    }
    s.push_str(footer);
    let _ = effective_description_limit;
    (s, truncation_notice)
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Human-readable catalog listing (the CLI `/skills list` text form).
pub fn render_catalog(catalog: &Catalog) -> String {
    if catalog.skills.is_empty() {
        return "no skills found".to_string();
    }
    let mut lines = vec![format!("{:<28} {:<12} {}", "NAME", "SOURCE", "DESCRIPTION")];
    for skill in &catalog.skills {
        let desc: String = skill.description.chars().take(56).collect();
        let src = if skill.managed_install {
            "fx (managed)"
        } else {
            skill.source.label()
        };
        lines.push(format!("{:<28} {:<12} {}", skill.name, src, desc));
    }
    for d in &catalog.diagnostics {
        match &d.cause {
            SkillDiagnosticCause::InvalidMetadata(cause) => lines.push(format!(
                "! {} (invalid metadata: {cause:?})",
                d.path.display()
            )),
            SkillDiagnosticCause::Unreadable => {
                lines.push(format!("! {} (unreadable)", d.path.display()))
            }
            SkillDiagnosticCause::Oversized => {
                lines.push(format!("! {} (oversized)", d.path.display()))
            }
        }
    }
    lines.join("\n")
}

/// Summary counts for JSON output.
pub fn catalog_summary(catalog: &Catalog) -> serde_json::Value {
    serde_json::json!({
        "skills": catalog.skills.iter().map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "path": s.path.display().to_string(),
                "source": s.source.label(),
                "managed_install": s.managed_install,
            })
        }).collect::<Vec<_>>(),
        "diagnostics": catalog.diagnostics.iter().map(|d| {
            serde_json::json!({
                "path": d.path.display().to_string(),
                "source": d.source.label(),
                "scope": match d.scope { SkillDiagnosticScope::Root => "root", SkillDiagnosticScope::Candidate => "candidate" },
                "cause": format!("{:?}", d.cause),
            })
        }).collect::<Vec<_>>(),
    })
}

/// Open a resource inside a skill directory (upstream `openResource`):
/// relative paths only, no "." / ".." segments, symlinks not followed.
/// Returns the (bounded) file bytes.
pub fn open_resource(skill_dir: &Path, resource: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    use anyhow::{bail, Context};
    let trimmed = resource.trim_matches([' ', '\t', '\r', '\n']);
    if trimmed.is_empty() || Path::new(trimmed).is_absolute() {
        bail!("invalid skill resource path");
    }
    let segments: Vec<&str> = trimmed.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        bail!("invalid skill resource path");
    }
    for seg in &segments {
        if *seg == "." || *seg == ".." {
            bail!("invalid skill resource path");
        }
    }
    let mut target = skill_dir.to_path_buf();
    for seg in &segments {
        target.push(seg);
    }
    // Reject anything that escapes the skill directory (belt and braces).
    if !target.starts_with(skill_dir) {
        bail!("invalid skill resource path");
    }
    let data = std::fs::read(&target).with_context(|| format!("read {}", target.display()))?;
    if data.len() > max_bytes {
        Ok(data[..max_bytes].to_vec())
    } else {
        Ok(data)
    }
}

/// Whether a resource reference is exactly the primary SKILL.md file
/// (upstream `resourceIsSkillFile`).
pub fn resource_is_skill_file(resource: &str) -> bool {
    let trimmed = resource.trim_matches([' ', '\t', '\r', '\n']);
    if trimmed.is_empty() || Path::new(trimmed).is_absolute() {
        return false;
    }
    let segments: Vec<&str> = trimmed.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    segments.len() == 1 && segments[0] == "SKILL.md"
}

/// Load the full text of a skill's SKILL.md (bounded by max_bytes).
pub fn read_skill_md(skill_dir: &Path, max_bytes: usize) -> anyhow::Result<String> {
    let data = open_resource(skill_dir, "SKILL.md", max_bytes)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Registry helper: map skill name -> Skill used by commands and tools.
pub struct Registry {
    catalog: Catalog,
    by_name: BTreeMap<String, Skill>,
}

impl Registry {
    pub fn discover(workspace: &Path) -> Registry {
        let catalog = discover(workspace);
        let mut by_name = BTreeMap::new();
        for skill in &catalog.skills {
            by_name.entry(skill.name.clone()).or_insert_with(|| skill.clone());
        }
        Registry { catalog, by_name }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fxrs-skills-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_workspace_managed_and_compat_roots() {
        let root = temp_dir("discover");
        write(&root.join(".fx/skills/a/SKILL.md"), "---\nname: a-skill\ndescription: The A skill.\n---\nBody\n");
        write(&root.join(".fx/skills/b/SKILL.md"), "# Legacy\n");
        write(&root.join("skills/shared-one/SKILL.md"), "---\nname: shared-one\ndescription: Shared skill\n---\nBody\n");
        // compatibility root
        write(&root.join(".claude/skills/claude-one/SKILL.md"), "---\nname: claude-one\ndescription: For claude\n---\nBody\n");

        let catalog = discover(&root);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a-skill"), "{names:?}");
        assert!(names.contains(&"b"), "{names:?}"); // legacy fallback name
        assert!(names.contains(&"shared-one"), "{names:?}");
        assert!(names.contains(&"claude-one"), "{names:?}");
        let a = catalog.find("a-skill").unwrap();
        assert_eq!(a.description, "The A skill.");
        assert!(!a.managed_install, "workspace .fx/skills is a workspace root, not the managed install root");

        // cleanup
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_reports_invalid_metadata_diagnostics() {
        let root = temp_dir("diag");
        write(&root.join(".fx/skills/bad/SKILL.md"), "---\nname: bad\n  continued\ndescription: x\n---\n");
        write(&root.join(".fx/skills/good/SKILL.md"), "---\nname: good\ndescription: ok\n---\n");
        let catalog = discover(&root);
        assert!(catalog.find("good").is_some());
        assert!(catalog.find("bad").is_none());
        assert!(
            catalog.diagnostics.iter().any(|d| {
                d.path.ends_with("bad")
                    && matches!(
                        &d.cause,
                        SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::UnsupportedMultiline)
                    )
            }),
            "diagnostics: {:?}",
            catalog.diagnostics
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn precedence_first_root_wins() {
        let root = temp_dir("precedence");
        write(&root.join(".fx/skills/dup/SKILL.md"), "---\nname: dup\ndescription: workspace version\n---\n");
        write(&root.join("skills/dup/SKILL.md"), "---\nname: dup\ndescription: shared version\n---\n");
        let catalog = discover(&root);
        let skills: Vec<&Skill> = catalog.skills.iter().filter(|s| s.name == "dup").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "workspace version");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prompt_section_renders_available_skills_and_can_omit() {
        let skills = vec![
            Skill {
                name: "alpha".into(),
                description: "first skill".into(),
                path: "/tmp/x/alpha".into(),
                source: SkillSource::WorkspaceFx,
                managed_install: false,
            },
            Skill {
                name: "beta".into(),
                description: "second skill".into(),
                path: "/tmp/x/beta".into(),
                source: SkillSource::GlobalFx,
                managed_install: true,
            },
        ];
        let (section, _notice) = build_prompt_section(&skills, 1024, 16 * 1024);
        assert!(section.contains("<available_skills>"));
        assert!(section.contains("<name>alpha</name>"));
        assert!(section.contains("<description>first skill</description>"));
        assert!(section.contains("<location>/tmp/x/beta</location>"));

        // Tiny catalog limit forces omission.
        let (section, _n) = build_prompt_section(&skills, 1024, 100);
        assert!(section.contains("context_limit name=\"skill_catalog_bytes\" action=\"omitted\""));
    }

    #[test]
    fn resource_paths_are_confined() {
        let root = temp_dir("resource");
        write(&root.join("SKILL.md"), "---\nname: x\ndescription: d\n---\nBody content\n");
        write(&root.join("notes.txt"), "secret");
        write(&root.join("..").join("evil.txt"), "evil");
        let skill_dir = root.clone();

        let body = open_resource(&skill_dir, "SKILL.md", 1024).unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Body content"));
        assert!(open_resource(&skill_dir, "notes.txt", 1024).is_ok());
        assert!(open_resource(&skill_dir, "../evil.txt", 1024).is_err());
        assert!(open_resource(&skill_dir, "/abs/path", 1024).is_err());
        assert!(resource_is_skill_file("SKILL.md"));
        assert!(!resource_is_skill_file("SKILL.md/../x"));
        assert!(!resource_is_skill_file("/SKILL.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_skill_md_is_legacy_skill_with_dir_name() {
        let root = temp_dir("legacy");
        write(&root.join(".fx/skills/legacy/SKILL.md"), "");
        let catalog = discover(&root);
        let skill = catalog.find("legacy").expect("legacy skill"); 
        assert_eq!(skill.description, "");
        assert!(catalog.find("legacy").is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn managed_root_is_fx_home_skills() {
        let m = managed_root();
        assert!(m.ends_with("skills"));
    }
}
