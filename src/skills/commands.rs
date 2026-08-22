//! `/skills` command harness — faithful port of fx's
//! `core/skills/skill_commands.zig` + `builtins/skills.zig` install/create/
//! remove machinery. Two entry points: the parse/execute harness used by the
//! shell and CLI, and install helpers used by the `install_skill` tool.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::contract::{validate_managed_skill_name, SkillMetadataResult};
use super::{Catalog, Registry, Skill, managed_root, SKILL_FILE_BYTES_DEFAULT};

/// Parsed skills command (upstream `skill_commands.Command`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    Show(String),
    Install(InstallCommand),
    Create(String),
    Remove(String),
    Path,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    pub source: String,
    pub filter: Option<String>,
}

pub fn parse_command(rest: &str) -> Command {
    let trimmed = rest.trim();
    if trimmed.is_empty() || trimmed == "list" {
        return Command::List;
    }
    if let Some(name) = trimmed.strip_prefix("show ") {
        return Command::Show(name.trim().to_string());
    }
    if let Some(name) = trimmed.strip_prefix("add ") {
        return Command::Install(parse_install_command(name.trim()));
    }
    if let Some(name) = trimmed.strip_prefix("install ") {
        return Command::Install(parse_install_command(name.trim()));
    }
    if let Some(name) = trimmed.strip_prefix("create ") {
        return Command::Create(name.trim().to_string());
    }
    if let Some(name) = trimmed.strip_prefix("remove ") {
        return Command::Remove(name.trim().to_string());
    }
    if trimmed == "path" {
        return Command::Path;
    }
    Command::Usage
}

fn parse_install_command(args: &str) -> InstallCommand {
    let mut source = args.to_string();
    let mut filter: Option<String> = None;
    if let Some(idx) = args.find("--skill ") {
        source = args[..idx].trim().to_string();
        filter = Some(args[idx + "--skill ".len()..].trim().to_string());
    } else if let Some(idx) = args.find("--skill=") {
        source = args[..idx].trim().to_string();
        filter = Some(args[idx + "--skill=".len()..].trim().to_string());
    }
    InstallCommand { source, filter }
}

/// A command result for the shell/CLI (upstream `CommandResult`).
#[derive(Debug)]
pub enum CommandResult {
    Notice(String),
    Installed(Vec<String>),
}

impl CommandResult {
    pub fn render(&self) -> String {
        match self {
            CommandResult::Notice(text) => text.clone(),
            CommandResult::Installed(names) => {
                if names.is_empty() {
                    "No skills found (no SKILL.md files).".to_string()
                } else {
                    let mut out = format!("Installed {} skill(s) into fx.", names.len());
                    for name in names {
                        out.push_str(&format!("\n- {name}"));
                    }
                    out
                }
            }
        }
    }

    pub fn reload(&self) -> bool {
        matches!(self, CommandResult::Installed(_))
    }
}

/// Execute a parsed command against a workspace (upstream `executeCommand`).
pub fn execute_command(workspace: &Path, command: &Command) -> Result<CommandResult> {
    match command {
        Command::List => Ok(CommandResult::Notice(super::render_catalog(
            &Registry::discover(workspace).catalog().clone(),
        ))),
        Command::Show(name) => {
            let registry = Registry::discover(workspace);
            if let Some(skill) = registry.find(name) {
                let body = super::read_skill_md(&skill.path, SKILL_FILE_BYTES_DEFAULT)
                    .map_err(|e| anyhow!("skill failed: {e}"))?;
                Ok(CommandResult::Notice(format!(
                    "# {}\n\n{}",
                    skill.name,
                    body
                )))
            } else {
                Ok(CommandResult::Notice(format!("Skill '{name}' not found.")))
            }
        }
        Command::Install(install) => {
            let registry = Registry::discover(workspace);
            let names = install_from_source(
                &managed_root(),
                workspace,
                &registry,
                &install.source,
                install.filter.as_deref(),
            )?;
            if names.is_empty() {
                if let Some(filter) = &install.filter {
                    return Ok(CommandResult::Notice(format!(
                        "Skill '{filter}' not found in the repository."
                    )));
                }
                return Ok(CommandResult::Notice(
                    "No skills found (no SKILL.md files).".to_string(),
                ));
            }
            Ok(CommandResult::Installed(names))
        }
        Command::Create(name) => {
            match create_skill_template(&managed_root(), name) {
                Ok(path) => Ok(CommandResult::Notice(format!("Created {}", path.display()))),
                Err(e) if e.downcast_ref::<InvalidSkillName>().is_some() => Ok(
                    CommandResult::Notice(
                        "Invalid skill name. Use a single directory name without '/' or '\\'."
                            .to_string(),
                    ),
                ),
                Err(e) => Err(e),
            }
        }
        Command::Remove(name) => {
            let registry = Registry::discover(workspace);
            let Some(skill) = registry.find(name) else {
                return Ok(CommandResult::Notice(format!("Skill '{name}' not found.")));
            };
            if !skill.managed_install {
                return Ok(CommandResult::Notice(format!(
                    "Skill '{name}' comes from {}, not the fx managed install root. Remove it from {}.",
                    skill.source.label(),
                    skill.path.display()
                )));
            }
            let dir_name = skill
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            remove_skill(&managed_root(), &dir_name)?;
            Ok(CommandResult::Notice(format!("Removed skill '{name}'.")))
        }
        Command::Path => Ok(CommandResult::Notice(format!(
            "fx workspace roots are auto-discovered from .fx/skills and skills/.\n\
             fx managed install root: {}\n\
             compatibility roots are auto-discovered from workspace and home (.opencode/.codex/.claude/.agents/.claw).",
            managed_root().display()
        ))),
        Command::Usage => Ok(CommandResult::Notice(
            "Usage: /skills [list|add|install|show|create|remove|path] [name|url|path]".to_string(),
        )),
    }
}

#[derive(Debug)]
pub struct InvalidSkillName;

impl std::fmt::Display for InvalidSkillName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid skill name")
    }
}
impl std::error::Error for InvalidSkillName {}

/// Create a SKILL.md template for a new managed skill.
pub fn create_skill_template(skills_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_managed_skill_name(name).map_err(|_| InvalidSkillName)?;
    std::fs::create_dir_all(skills_dir).context("create skills dir")?;
    let skill_dir = skills_dir.join(name);
    std::fs::create_dir_all(&skill_dir).context("create skill dir")?;
    let skill_file = skill_dir.join("SKILL.md");
    let template = format!(
        "---\nname: {name}\ndescription: Describe when this skill should activate\n---\n\n# {name}\n\nInstructions for this skill...\n"
    );
    std::fs::write(&skill_file, template).context("write SKILL.md")?;
    Ok(skill_file)
}

/// Remove a managed skill directory by name.
pub fn remove_skill(skills_dir: &Path, name: &str) -> Result<()> {
    validate_managed_skill_name(name).map_err(|_| InvalidSkillName)?;
    let target = skills_dir.join(name);
    if !target.exists() {
        bail!("skill `{name}` not found in {}", skills_dir.display());
    }
    std::fs::remove_dir_all(&target).with_context(|| format!("remove {}", target.display()))?;
    Ok(())
}

/// Install skills from a local directory or a GitHub reference into the
/// managed root. Returns the installed skill names. Mirrors upstream
/// `installFromSource` -> `installFromDirectory` (root SKILL.md + recursive
/// walk), with transactional copy (stage + rename), `.git` skipping, and
/// name/filter matching.
pub fn install_from_source(
    skills_dir: &Path,
    workspace: &Path,
    _registry: &Registry,
    source: &str,
    filter: Option<&str>,
) -> Result<Vec<String>> {
    let _ = workspace;
    let requested = normalize_install_request(source, filter)?;

    if let Some(local) = resolve_install_directory(&requested.source) {
        let installed = install_from_directory(skills_dir, &local, None, requested.filter.as_deref())?;
        if !installed.is_empty() {
            return Ok(installed);
        }
    }

    install_from_github(skills_dir, &requested.source, requested.filter.as_deref())
}

struct NormalizedInstall {
    source: String,
    filter: Option<String>,
}

fn normalize_install_request(source: &str, explicit_filter: Option<&str>) -> Result<NormalizedInstall> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("invalid skill install source");
    }
    // npx/bunx forms: `npx skills add owner/repo --skill x` etc.
    let (command_source, command_filter) = parse_npx_install_source(trimmed)
        .map(|ps| (ps.0, ps.1))
        .unwrap_or((trimmed.to_string(), None));
    let (inline_source, inline_filter) = parse_inline_install_source(&command_source);
    let merged = merge_install_filters(command_filter, inline_filter, explicit_filter.map(str::to_string))?;
    Ok(NormalizedInstall {
        source: inline_source,
        filter: merged,
    })
}

fn parse_npx_install_source(input: &str) -> Option<(String, Option<String>)> {
    if !looks_like_skills_install_command(input) {
        return None;
    }
    let mut source: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut tokens = input.split_whitespace();
    let mut consumed: Vec<String> = Vec::new();
    while let Some(token) = tokens.next() {
        consumed.push(token.to_string());
        match token {
            "npx" | "bunx" | "skills" | "add" | "-g" | "-y" | "--yes" => continue,
            "--skill" => {
                filter = tokens.next().map(str::to_string);
                continue;
            }
            _ if token.starts_with("--skill=") => {
                filter = Some(token["--skill=".len()..].to_string());
                continue;
            }
            _ if token.starts_with('-') => continue,
            _ => {
                if source.is_none() {
                    source = Some(token.to_string());
                    consumed.pop();
                }
            }
        }
    }
    let _ = consumed;
    source.map(|s| (s, filter))
}

fn looks_like_skills_install_command(input: &str) -> bool {
    (input.starts_with("npx ") || input.starts_with("bunx "))
        && input.contains("skills add")
}

fn parse_inline_install_source(input: &str) -> (String, Option<String>) {
    if let Some(parsed) = parse_skills_dot_sh_source(input) {
        return parsed;
    }
    if let Some(parsed) = parse_repo_skill_source(input) {
        return parsed;
    }
    (input.to_string(), None)
}

fn parse_skills_dot_sh_source(input: &str) -> Option<(String, Option<String>)> {
    let marker = input.find("skills.sh/")?;
    let rest = &input[marker + "skills.sh/".len()..];
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let skill = parts.next();
    let source = format!("{owner}/{repo}");
    Some((source, skill.map(str::to_string)))
}

fn parse_repo_skill_source(input: &str) -> Option<(String, Option<String>)> {
    if !is_likely_repo_skill_source(input) {
        return None;
    }
    let at = input.rfind('@')?;
    Some((input[..at].to_string(), Some(input[at + 1..].to_string())))
}

fn is_likely_repo_skill_source(input: &str) -> bool {
    if input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with("git@")
        || input.starts_with('/')
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with("~/")
    {
        return false;
    }
    let Some(at) = input.rfind('@') else {
        return false;
    };
    let Some(slash) = input[..at].rfind('/') else {
        return false;
    };
    slash > 0 && at + 1 < input.len()
}

fn merge_install_filters(
    a: Option<String>,
    b: Option<String>,
    c: Option<String>,
) -> Result<Option<String>> {
    let mut merged: Option<String> = None;
    for value in [a, b, c].into_iter().flatten() {
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        if let Some(existing) = &merged {
            if existing != &value {
                bail!("conflicting skill install filters");
            }
        } else {
            merged = Some(value);
        }
    }
    Ok(merged)
}

fn resolve_install_directory(source: &str) -> Option<PathBuf> {
    let path = if Path::new(source).is_absolute() {
        PathBuf::from(source)
    } else {
        std::env::current_dir().ok()?.join(source)
    };
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

fn clone_url_for_source(url: &str) -> String {
    if url.starts_with("http") || url.starts_with("git@") {
        url.to_string()
    } else {
        format!("https://github.com/{url}.git")
    }
}

fn extract_repo_name(url: &str) -> String {
    let mut name = url.rsplit('/').next().unwrap_or(url).to_string();
    if let Some(stripped) = name.strip_suffix(".git") {
        name = stripped.to_string();
    }
    name
}

fn install_from_github(
    skills_dir: &Path,
    url: &str,
    filter: Option<&str>,
) -> Result<Vec<String>> {
    std::fs::create_dir_all(skills_dir).context("create skills dir")?;
    let tmp_dir = std::env::temp_dir().join(format!(
        "fx-skill-install-{}",
        crate::util::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let git_url = clone_url_for_source(url);
    let repo_name = extract_repo_name(&git_url);
    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", &git_url])
        .arg(&tmp_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("spawn git clone")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        bail!("skill install failed: git clone of {url} failed");
    }
    let result = install_from_directory(skills_dir, &tmp_dir, Some(&repo_name), filter);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    result
}

/// Walk `source_dir` for SKILL.md files (root file + recursive), validate
/// metadata, apply the filter, and copy valid skills into `skills_dir`
/// transactionally (stage + atomic rename, skipping `.git` entries).
fn install_from_directory(
    skills_dir: &Path,
    source_dir: &Path,
    fallback_name: Option<&str>,
    filter: Option<&str>,
) -> Result<Vec<String>> {
    if !source_dir.is_dir() {
        bail!("source is not a directory: {}", source_dir.display());
    }
    std::fs::create_dir_all(skills_dir).context("create skills dir")?;

    let mut installed: Vec<String> = Vec::new();
    let mut skip_parents: Vec<PathBuf> = Vec::new();

    // Root SKILL.md (a single-skill repo / directory).
    let root_md = source_dir.join("SKILL.md");
    if root_md.is_file() {
        let root_fallback = fallback_name
            .map(str::to_string)
            .unwrap_or_else(|| {
                source_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "skill".to_string())
            });
        if let Some(name) = install_candidate_skill(
            skills_dir,
            source_dir,
            source_dir,
            root_fallback,
            filter,
        )? {
            installed.push(name);
            // If the repo root is a single skill, do not also walk children.
            skip_parents.push(source_dir.to_path_buf());
        }
    }

    // Iterative post-order walk (directories on a stack), so SKILL.md files
    // at any depth are discovered without recursion.
    let mut stack: Vec<PathBuf> = vec![source_dir.to_path_buf()];
    let mut walked = Vec::new();
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut root_candidates: Vec<PathBuf> = Vec::new();
        for entry in rd.flatten() {
            let entry_path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') {
                continue;
            }
            if entry_path.is_dir() {
                if skip_parents.contains(&entry_path) {
                    continue;
                }
                dirs.push(entry_path);
            } else if file_name == "SKILL.md" {
                root_candidates.push(entry_path);
            }
        }
        // Process SKILL.md candidates in this directory before descending.
        for skill_md in root_candidates {
            let parent = skill_md.parent().unwrap_or(&dir);
            let dir_name = parent
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if dir_name.starts_with('.') || validate_managed_skill_name(&dir_name).is_err() {
                continue;
            }
            if skip_parents.contains(&parent.to_path_buf()) {
                continue;
            }
            if let Some(name) = install_candidate_skill(
                skills_dir,
                parent,
                source_dir,
                dir_name.clone(),
                filter,
            )? {
                walked.push(name);
                skip_parents.push(parent.to_path_buf());
            }
        }
        stack.extend(dirs);
    }
    installed.extend(walked);

    Ok(installed)
}

fn install_candidate_skill(
    skills_dir: &Path,
    candidate_dir: &Path,
    source_root: &Path,
    fallback_name: String,
    filter: Option<&str>,
) -> Result<Option<String>> {
    let skill_md = candidate_dir.join("SKILL.md");
    let data = std::fs::read(&skill_md).context("read SKILL.md")?;
    if data.len() > SKILL_FILE_BYTES_DEFAULT {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(&data) else {
        return Ok(None);
    };
    let parsed = super::contract::parse_skill_file(text.as_bytes());
    let metadata = match super::contract::resolve_metadata(parsed, &fallback_name) {
        SkillMetadataResult::Valid(metadata) => metadata,
        SkillMetadataResult::Invalid(_) => return Ok(None),
    };
    let name = metadata.name_str();
    let _ = metadata;
    if let Some(requested) = filter {
        if fallback_name != requested && name != requested {
            return Ok(None);
        }
    }
    if validate_managed_skill_name(&fallback_name).is_err() {
        // A directory name that can't be a managed destination is skipped
        // even if its metadata names it validly (upstream behavior).
        return Ok(None);
    }

    copy_skill_dir(source_root, candidate_dir, skills_dir, &fallback_name)?;
    Ok(Some(name))
}

/// Transactional copy: stage into a temp dir, then swap into place.
fn copy_skill_dir(source_root: &Path, src_dir: &Path, skills_dir: &Path, skill_name: &str) -> Result<()> {
    validate_managed_skill_name(skill_name).map_err(|_| InvalidSkillName)?;
    std::fs::create_dir_all(skills_dir).context("create skills dir")?;

    let dest_dir = skills_dir.join(skill_name);
    let token: String = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{n:x}")
    };
    let transaction_root = skills_dir.join(format!(".skill-install-{token}"));
    let staged = transaction_root.join("staged");
    let backup = transaction_root.join("backup");

    let result = (|| -> Result<()> {
        copy_dir_recursive(src_dir, &staged)?;
        // Atomic swap: move existing aside, move staged into place.
        if dest_dir.exists() {
            std::fs::rename(&dest_dir, &backup)
                .with_context(|| format!("backup {}", dest_dir.display()))?;
        }
        match std::fs::rename(&staged, &dest_dir) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&backup);
                Ok(())
            }
            Err(e) => {
                if backup.exists() {
                    let _ = std::fs::rename(&backup, &dest_dir);
                }
                let _ = std::fs::remove_dir_all(&transaction_root);
                Err(e).with_context(|| format!("commit skill install for {skill_name}"))
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&transaction_root);
    let _ = source_root;
    result
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).context("create dest dir")?;
    let rd = std::fs::read_dir(src).context("read source dir")?;
    for entry in rd.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name == ".git" || file_name.starts_with(".git") {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&file_name);
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Convenience used by the `install_skill` tool: install from a local
/// directory, returning installed names.
pub fn install_from_local_dir(
    skills_dir: &Path,
    source: &Path,
    filter: Option<&str>,
) -> Result<Vec<String>> {
    let fallback = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".into());
    install_from_directory(skills_dir, source, Some(&fallback), filter)
}

/// Does this input look like `npx skills add ...` / `bunx skills add ...`?
pub fn looks_like_install_command(input: &str) -> bool {
    looks_like_skills_install_command(input.trim())
}

/// Find one skill in a catalog by name.
pub fn find_in_catalog<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a Skill> {
    catalog.find(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fxrs-skills-cmd-{tag}-{}-{}",
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
    fn parses_commands() {
        assert_eq!(parse_command(""), Command::List);
        assert_eq!(parse_command("list"), Command::List);
        assert_eq!(parse_command("show foo"), Command::Show("foo".into()));
        assert_eq!(
            parse_command("add owner/repo --skill x"),
            Command::Install(InstallCommand {
                source: "owner/repo".into(),
                filter: Some("x".into())
            })
        );
        assert_eq!(
            parse_command("install owner/repo --skill=x"),
            Command::Install(InstallCommand {
                source: "owner/repo".into(),
                filter: Some("x".into())
            })
        );
        assert_eq!(parse_command("create my-skill"), Command::Create("my-skill".into()));
        assert_eq!(parse_command("remove my-skill"), Command::Remove("my-skill".into()));
        assert_eq!(parse_command("path"), Command::Path);
        assert_eq!(parse_command("wibble"), Command::Usage);
    }

    #[test]
    fn creates_and_removes_managed_skill() {
        let dir = temp_dir("crud");
        let skills_dir = dir.join("skills");

        let created = create_skill_template(&skills_dir, "review").unwrap();
        assert!(created.ends_with("review/SKILL.md"));
        let text = std::fs::read_to_string(&created).unwrap();
        assert!(text.contains("name: review"));

        // Invalid names rejected.
        assert!(create_skill_template(&skills_dir, "../evil").is_err());
        assert!(create_skill_template(&skills_dir, "a/b").is_err());

        remove_skill(&skills_dir, "review").unwrap();
        assert!(!skills_dir.join("review").exists());
        assert!(remove_skill(&skills_dir, "missing").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_from_local_directory_with_filter() {
        let dir = temp_dir("inst");
        let skills_dir = dir.join("skills");
        let src = dir.join("repo");
        write(&src.join("alpha/SKILL.md"), "---\nname: alpha\ndescription: Alpha skill\n---\nBody\n");
        write(&src.join("beta/SKILL.md"), "---\nname: beta\ndescription: Beta skill\n---\nBody\n");
        write(&src.join("README.md"), "not a skill\n");

        let names = install_from_local_dir(&skills_dir, &src, None).unwrap();
        assert!(names.contains(&"alpha".to_string()), "{names:?}");
        assert!(names.contains(&"beta".to_string()), "{names:?}");
        assert!(skills_dir.join("alpha/SKILL.md").is_file());
        assert!(skills_dir.join("beta/SKILL.md").is_file());

        // Filtered install to a fresh dir.
        let skills_dir2 = dir.join("skills2");
        let names = install_from_local_dir(&skills_dir2, &src, Some("beta")).unwrap();
        assert_eq!(names, vec!["beta".to_string()]);
        assert!(skills_dir2.join("beta/SKILL.md").is_file());
        assert!(!skills_dir2.join("alpha").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_single_skill_root() {
        let dir = temp_dir("single");
        let skills_dir = dir.join("skills");
        let src = dir.join("my-skill-repo");
        write(&src.join("SKILL.md"), "---\nname: my-skill\ndescription: A single skill\n---\nBody\n");
        write(&src.join("helper.sh"), "#!/bin/sh\n");

        let names = install_from_local_dir(&skills_dir, &src, None).unwrap();
        assert_eq!(names, vec!["my-skill".to_string()]);
        // The repo root is installed under its directory name (upstream
        // fallback_name), and discovery reports the metadata name.
        assert!(skills_dir.join("my-skill-repo/SKILL.md").is_file());
        assert!(skills_dir.join("my-skill-repo/helper.sh").is_file());
        let text = std::fs::read_to_string(skills_dir.join("my-skill-repo/SKILL.md")).unwrap();
        assert!(text.contains("name: my-skill"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_sources_parse() {
        assert_eq!(
            normalize_install_request("owner/repo", None).unwrap().source,
            "owner/repo"
        );
        let n = normalize_install_request("npx skills add owner/repo --skill web", None).unwrap();
        assert_eq!(n.source, "owner/repo");
        assert_eq!(n.filter.as_deref(), Some("web"));

        let n = normalize_install_request("skills.sh/owner/repo/web", None).unwrap();
        assert_eq!(n.source, "owner/repo");
        assert_eq!(n.filter.as_deref(), Some("web"));

        let n = normalize_install_request("owner/repo@web", Some("web")).unwrap();
        assert_eq!(n.source, "owner/repo");
        assert_eq!(n.filter.as_deref(), Some("web"));
    }

    #[test]
    fn conflicting_filters_error() {
        let r = normalize_install_request("owner/repo@web", Some("other"));
        assert!(r.is_err());
    }

    #[test]
    fn invalid_metadata_candidate_is_skipped() {
        let dir = temp_dir("skip");
        let skills_dir = dir.join("skills");
        let src = dir.join("repo");
        write(&src.join("bad/SKILL.md"), "---\nname: bad\n  continued\n---\n");
        write(&src.join("good/SKILL.md"), "---\nname: good\ndescription: ok\n---\n");
        let names = install_from_local_dir(&skills_dir, &src, None).unwrap();
        assert_eq!(names, vec!["good".to_string()]);
        assert!(!skills_dir.join("bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
