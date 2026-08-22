//! Skill invocation — faithful port of fx's `core/skills/skill_invocation.zig`
//! surface: identity resolution (`resolve_skill`), bounded skill loading
//! (`load_by_identity`), and the explicit prompt section that auto-attaches
//! skills referenced by the user prompt (sigil + natural-language matching).

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

use super::{Catalog, Skill, SKILL_FILE_BYTES_DEFAULT};

/// The outcome of resolving a skill identity (upstream `SkillResolution`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillResolution<'a> {
    Found(&'a Skill),
    AmbiguousName,
    NotFound,
    NameLocationMismatch,
}

/// Resolve a skill by name, optionally constrained to an exact location
/// (upstream `skill_runtime.resolveSkill`).
pub fn resolve_skill<'a>(
    skills: &'a [Skill],
    name: &str,
    location: Option<&str>,
) -> SkillResolution<'a> {
    if let Some(exact_location) = location {
        for skill in skills {
            if skill.path != Path::new(exact_location) {
                continue;
            }
            if skill.name != name {
                return SkillResolution::NameLocationMismatch;
            }
            return SkillResolution::Found(skill);
        }
        return SkillResolution::NotFound;
    }

    let mut found: Option<&'a Skill> = None;
    for skill in skills {
        if skill.name != name {
            continue;
        }
        if found.is_some() {
            return SkillResolution::AmbiguousName;
        }
        found = Some(skill);
    }
    match found {
        Some(skill) => SkillResolution::Found(skill),
        None => SkillResolution::NotFound,
    }
}

/// The bounded output of loading one skill (upstream `ExecuteResult`).
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    pub model_output: String,
    pub notice: Option<String>,
    pub diagnostic_notice: Option<String>,
}

/// Load a skill's content for the model by identity (upstream
/// `loadByIdentity`): resolve, validate the candidate, read the primary
/// SKILL.md (or a resource) with an optional byte offset, bounded by
/// `max_bytes`. Missing / name-mismatch / unreadable candidates return a
/// failure result with a notice rather than an error.
pub fn load_by_identity(
    catalog: &Catalog,
    name: &str,
    location: Option<&str>,
    resource: Option<&str>,
    offset: usize,
    max_bytes: usize,
) -> Result<ExecuteResult> {
    let resolution = resolve_skill(&catalog.skills, name, location);
    let skill = match resolution {
        SkillResolution::Found(skill) => skill,
        SkillResolution::AmbiguousName => {
            return Ok(ExecuteResult {
                model_output: format!(
                    "Skill name `{name}` is ambiguous across install locations. Use the `location` field to disambiguate."
                ),
                notice: Some("<skill_invocation details=\"ambiguous_name\" />\n".into()),
                diagnostic_notice: None,
            });
        }
        SkillResolution::NotFound => {
            return Ok(ExecuteResult {
                model_output: format!("Skill `{name}` not found."),
                notice: Some("<skill_invocation details=\"not_found\" />\n".into()),
                diagnostic_notice: None,
            });
        }
        SkillResolution::NameLocationMismatch => {
            return Ok(ExecuteResult {
                model_output: format!(
                    "Skill `{name}` not found at location `{}`.",
                    location.unwrap_or("")
                ),
                notice: Some("<skill_invocation details=\"name_location_mismatch\" />\n".into()),
                diagnostic_notice: None,
            });
        }
    };

    let text = match resource {
        Some(res) => {
            if super::resource_is_skill_file(res) || !res.is_empty() {
                let data = super::open_resource(&skill.path, res, max_bytes)
                    .map_err(|e| anyhow::anyhow!("skill `{}`: {e}", skill.name))?;
                String::from_utf8_lossy(&data).into_owned()
            } else {
                super::read_skill_md(&skill.path, max_bytes)
                    .map_err(|e| anyhow::anyhow!("skill `{}`: {e}", skill.name))?
            }
        }
        None => super::read_skill_md(&skill.path, max_bytes)
            .map_err(|e| anyhow::anyhow!("skill `{}`: {e}", skill.name))?,
    };

    let mut output = text;
    if offset > 0 {
        let start = char_boundary_offset(&output, offset);
        output = output[start..].to_string();
    }
    // Upstream appends a small footer when a skill chunk started mid-file.
    let mut notice = None;
    if offset > 0 {
        notice = Some(format!(
            "<skill_invocation details=\"offset\" offset=\"{offset}\" />\n"
        ));
    }

    Ok(ExecuteResult {
        model_output: output,
        notice,
        diagnostic_notice: None,
    })
}

fn char_boundary_offset(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut idx = offset;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    // Prefer the exact requested byte offset when it is a boundary.
    text.len().min(if text.is_char_boundary(offset) { offset } else { idx })
}

/// A skill reference parsed out of the user prompt (upstream
/// `NaturalLanguageSkillReference`).
struct NaturalLanguageReference {
    normalized_prompt: String,
    name_starts: Vec<usize>,
}

impl NaturalLanguageReference {
    fn matches_skill_name(&self, skill_name: &str) -> bool {
        for &name_start in &self.name_starts {
            let mut search_start = name_start;
            while let Some(relative) = self.normalized_prompt[search_start..].find(" skill") {
                let marker_start = search_start + relative;
                let reference_end = marker_start + " skill".len();
                let has_boundary = reference_end == self.normalized_prompt.len()
                    || self.normalized_prompt.as_bytes()[reference_end] == b' ';
                if has_boundary
                    && marker_start > name_start
                    && normalized_reference_text_eql(
                        skill_name,
                        &self.normalized_prompt[name_start..marker_start],
                    )
                {
                    return true;
                }
                search_start = marker_start + 1;
            }
        }
        false
    }
}

fn natural_language_reference_start(prompt: &str) -> Option<&str> {
    let mut text = prompt.trim_start_matches([' ', '\t', '\r', '\n']);
    if text.len() >= "please".len()
        && ascii_eql_ignore_case(&text[.."please".len()], "please")
        && (text.len() == "please".len() || !text.as_bytes()["please".len()].is_ascii_alphanumeric())
    {
        text = text["please".len()..].trim_start_matches([' ', '\t', '\r', '\n', ',', ':']);
    }
    if text.is_empty()
        || text.starts_with('"')
        || text.starts_with('\'')
        || text.starts_with('`')
        || text.starts_with('\u{201c}')
        || text.starts_with('\u{2018}')
    {
        return None;
    }
    Some(text)
}

fn natural_language_skill_name_start(normalized: &str) -> Option<usize> {
    for verb in ["use", "apply", "activate", "invoke", "run"] {
        if !normalized.starts_with(verb) {
            continue;
        }
        if normalized.len() <= verb.len() || normalized.as_bytes()[verb.len()] != b' ' {
            continue;
        }
        return Some(verb.len() + 1);
    }
    None
}

fn parse_natural_language_skill_reference(prompt: &str) -> Option<NaturalLanguageReference> {
    let reference_start = natural_language_reference_start(prompt)?;
    let normalized_prompt = normalize_reference_text(reference_start);
    let name_start = natural_language_skill_name_start(&normalized_prompt)?;

    let mut name_starts = vec![name_start];
    if normalized_prompt[name_start..].starts_with("the ") {
        name_starts.push(name_start + "the ".len());
    }
    Some(NaturalLanguageReference {
        normalized_prompt,
        name_starts,
    })
}

fn normalize_reference_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_was_space = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            previous_was_space = false;
        } else if !previous_was_space {
            out.push(' ');
            previous_was_space = true;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn normalized_reference_text_eql(text: &str, normalized: &str) -> bool {
    let mut normalized_index = 0usize;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_space && normalized_index > 0 {
                if normalized_index >= normalized.len()
                    || normalized.as_bytes()[normalized_index] != b' '
                {
                    return false;
                }
                normalized_index += 1;
            }
            if normalized_index >= normalized.len()
                || normalized.as_bytes()[normalized_index] != c.to_ascii_lowercase() as u8
            {
                return false;
            }
            normalized_index += 1;
            pending_space = false;
        } else if normalized_index > 0 {
            pending_space = true;
        }
    }
    normalized_index == normalized.len()
}

fn ascii_eql_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

fn is_skill_name_continuation(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn matches_sigil_skill_at(text: &[u8], index: usize, skill_name: &str, sigil: u8) -> bool {
    if index >= text.len() || text[index] != sigil {
        return false;
    }
    let name_start = index + 1;
    let name = skill_name.as_bytes();
    if text.len() - name_start < name.len() {
        return false;
    }
    if !text[name_start..name_start + name.len()]
        .eq_ignore_ascii_case(name)
    {
        return false;
    }
    let end = name_start + name.len();
    end == text.len() || !is_skill_name_continuation(text[end])
}

/// Which skills does the prompt explicitly reference? (upstream
/// `matchExplicitSkillIndices`). A skill matches when its name is unique in
/// the catalog and the prompt starts with `/name` or `$name`, or contains a
/// natural-language "use/apply/activate/invoke/run <name> skill" reference.
pub fn match_explicit_skill_indices(prompt: &str, skills: &[Skill]) -> Vec<usize> {
    let trimmed = prompt.trim_start_matches([' ', '\t', '\r', '\n']);
    let leading_sigil = if !trimmed.is_empty() && (trimmed.starts_with('/') || trimmed.starts_with('$'))
    {
        trimmed.as_bytes()[0]
    } else {
        0
    };
    let natural_reference = parse_natural_language_skill_reference(prompt);

    let mut name_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for skill in skills {
        *name_counts.entry(skill.name.as_str()).or_insert(0) += 1;
    }

    let mut matched = Vec::new();
    for (index, skill) in skills.iter().enumerate() {
        if name_counts.get(skill.name.as_str()) != Some(&1) {
            continue;
        }
        let sigil_referenced = if leading_sigil != 0 {
            matches_sigil_skill_at(trimmed.as_bytes(), 0, &skill.name, leading_sigil)
        } else {
            false
        };
        let natural_referenced = if let Some(reference) = &natural_reference {
            reference.matches_skill_name(&skill.name)
        } else {
            false
        };
        if !sigil_referenced && !natural_referenced {
            continue;
        }
        matched.push(index);
    }
    matched
}

/// Build the ordered explicit prompt section: explicit bindings first, then
/// prompt-matched skills, deduped by path (upstream
/// `buildExplicitPromptSection`). Returns the section text and any notices.
pub fn build_explicit_prompt_section(
    catalog: &Catalog,
    prompt: &str,
    bindings: &[(String, String)], // (name, path)
    max_bytes: usize,
) -> (String, Option<String>) {
    let matched = match_explicit_skill_indices(prompt, &catalog.skills);
    let mut plan: Vec<(String, String)> = Vec::new();
    let mut loaded_paths: Vec<String> = Vec::new();
    for (name, path) in bindings {
        if loaded_paths.contains(path) {
            continue;
        }
        loaded_paths.push(path.clone());
        plan.push((name.clone(), path.clone()));
    }
    for index in matched {
        let skill = &catalog.skills[index];
        let path = skill.path.display().to_string();
        if loaded_paths.contains(&path) {
            continue;
        }
        loaded_paths.push(path.clone());
        plan.push((skill.name.clone(), path.clone()));
    }
    if plan.is_empty() {
        return (String::new(), None);
    }

    let mut out = String::from("Explicitly invoked skill content for this query:\n");
    let mut notices = Vec::new();
    for (name, path) in plan {
        let result = match load_by_identity(catalog, &name, Some(&path), None, 0, max_bytes) {
            Ok(result) => result,
            Err(e) => ExecuteResult {
                model_output: format!("skill failed: {e}"),
                notice: None,
                diagnostic_notice: None,
            },
        };
        out.push_str(&result.model_output);
        out.push('\n');
        if let Some(notice) = result.notice {
            if !notices.contains(&notice) {
                notices.push(notice);
            }
        }
        if let Some(notice) = result.diagnostic_notice {
            if !notices.contains(&notice) {
                notices.push(notice);
            }
        }
    }
    let notice = if notices.is_empty() {
        None
    } else {
        Some(notices.join(""))
    };
    (out, notice)
}

/// Convenience used by the agent: does the prompt name any discovered skill?
pub fn matched_skill_names(prompt: &str, catalog: &Catalog) -> Vec<String> {
    match_explicit_skill_indices(prompt, &catalog.skills)
        .into_iter()
        .map(|i| catalog.skills[i].name.clone())
        .collect()
}

pub const DEFAULT_SKILL_LOAD_BYTES: usize = SKILL_FILE_BYTES_DEFAULT;

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, path: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: String::new(),
            path: Path::new(path).to_path_buf(),
            source: super::super::SkillSource::WorkspaceFx,
            managed_install: false,
        }
    }

    #[test]
    fn resolve_skill_by_name_and_location() {
        let skills = vec![
            skill("alpha", "/ws/.fx/skills/alpha"),
            skill("beta", "/ws/.fx/skills/beta"),
        ];
        assert!(matches!(
            resolve_skill(&skills, "alpha", None),
            SkillResolution::Found(s) if s.name == "alpha"
        ));
        assert_eq!(resolve_skill(&skills, "missing", None), SkillResolution::NotFound);
        assert_eq!(
            resolve_skill(&skills, "alpha", Some("/ws/.fx/skills/alpha")),
            SkillResolution::Found(&skills[0])
        );
        assert_eq!(
            resolve_skill(&skills, "beta", Some("/ws/.fx/skills/alpha")),
            SkillResolution::NameLocationMismatch
        );
        assert_eq!(
            resolve_skill(&skills, "alpha", Some("/nope")),
            SkillResolution::NotFound
        );
        let dup = vec![skill("alpha", "/a"), skill("alpha", "/b")];
        assert_eq!(resolve_skill(&dup, "alpha", None), SkillResolution::AmbiguousName);
    }

    #[test]
    fn load_by_identity_reads_skill_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!(
            "fxrs-invoke-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo\n---\n\n# Demo\n\nBody line.\n",
        )
        .unwrap();

        let catalog = Catalog {
            skills: vec![skill("demo", dir.join("demo").to_str().unwrap())],
            diagnostics: vec![],
        };
        let result = load_by_identity(&catalog, "demo", None, None, 0, 4096).unwrap();
        assert!(result.model_output.contains("Body line."));
        assert!(result.notice.is_none());

        let missing = load_by_identity(&catalog, "nope", None, None, 0, 4096).unwrap();
        assert!(missing.model_output.contains("not found"));
        assert!(missing.notice.unwrap().contains("not_found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn natural_language_matching() {
        let skills = vec![skill("review", "/a"), skill("release", "/b")];

        // sigil forms
        assert_eq!(match_explicit_skill_indices("/review", &skills), vec![0]);
        assert_eq!(match_explicit_skill_indices("$release please", &skills), vec![1]);

        // natural language
        assert_eq!(
            match_explicit_skill_indices("please use the review skill", &skills),
            vec![0]
        );
        assert_eq!(
            match_explicit_skill_indices("run release skill", &skills),
            vec![1]
        );
        // "review" alone is not a reference
        assert_eq!(match_explicit_skill_indices("review this pr", &skills), Vec::<usize>::new());
        // ambiguous names are not matched
        let dup = vec![skill("dup", "/a"), skill("dup", "/b"), skill("other", "/c")];
        assert_eq!(match_explicit_skill_indices("use the dup skill", &dup), Vec::<usize>::new());
        // "the" prefix form
        assert_eq!(
            match_explicit_skill_indices("use the review skill now", &skills),
            vec![0]
        );
    }

    #[test]
    fn normalized_reference_matching() {
        // punctuation-insensitive comparison: "code-review" == "code review"
        assert!(normalized_reference_text_eql("code-review", "code review"));
        assert!(normalized_reference_text_eql("code_review", "code review"));
        assert!(!normalized_reference_text_eql("codereview", "code review"));
        assert_eq!(normalize_reference_text("  Use the Code-Review  skill! "), "use the code review skill");
    }

    #[test]
    fn explicit_prompt_section_builds_ordered_deduped_content() {
        let dir = std::env::temp_dir().join(format!(
            "fxrs-invoke-sec-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (name, body) in [("alpha", "ALPHA BODY"), ("beta", "BETA BODY")] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(
                dir.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name}\n---\n\n{body}\n"),
            )
            .unwrap();
        }
        let catalog = Catalog {
            skills: vec![
                skill("alpha", dir.join("alpha").to_str().unwrap()),
                skill("beta", dir.join("beta").to_str().unwrap()),
            ],
            diagnostics: vec![],
        };
        // Prompt mentions beta; bindings explicitly list alpha.
        let bindings = vec![("alpha".to_string(), dir.join("alpha").to_str().unwrap().to_string())];
        let (section, _notice) =
            build_explicit_prompt_section(&catalog, "use the beta skill", &bindings, 4096);
        let alpha_pos = section.find("ALPHA BODY").unwrap();
        let beta_pos = section.find("BETA BODY").unwrap();
        assert!(alpha_pos < beta_pos, "explicit binding precedes prompt match");
        assert!(section.starts_with("Explicitly invoked skill content"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
