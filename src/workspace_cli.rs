//! `fxrs workspace` — manage additional workspace directories (port of
//! upstream `fx workspace`). Additional directories are persisted per primary
//! workspace in `~/.fx/workspace_dirs.json` and merged into config resolution
//! when `FX_ADDITIONAL_DIRECTORIES` is unset (env wins, matching upstream).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceDirsStore {
    /// primary workspace (canonical path) -> additional directories
    #[serde(default)]
    pub workspaces: BTreeMap<String, Vec<String>>,
}

impl WorkspaceDirsStore {
    pub fn path() -> PathBuf {
        fx_home().join("workspace_dirs.json")
    }

    pub fn open() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn dirs_for(&self, workspace: &Path) -> Vec<PathBuf> {
        self.workspaces
            .get(&workspace_key(workspace))
            .map(|v| v.iter().map(PathBuf::from).collect())
            .unwrap_or_default()
    }

    pub fn add(&mut self, workspace: &Path, dir: &Path) -> Result<bool> {
        let dir = dir
            .canonicalize()
            .with_context(|| format!("resolving {}", dir.display()))?;
        if !dir.is_dir() {
            anyhow::bail!("not a directory: {}", dir.display());
        }
        let key = workspace_key(workspace);
        let entry = self.workspaces.entry(key).or_default();
        let text = dir.display().to_string();
        if entry.iter().any(|d| d == &text) {
            return Ok(false);
        }
        entry.push(text);
        Ok(true)
    }

    pub fn remove(&mut self, workspace: &Path, dir: &Path) -> bool {
        let key = workspace_key(workspace);
        let text = dir.display().to_string();
        let (changed, became_empty) = {
            let Some(entry) = self.workspaces.get_mut(&key) else {
                return false;
            };
            let before = entry.len();
            entry.retain(|d| d != &text);
            (entry.len() != before, entry.is_empty())
        };
        if became_empty {
            self.workspaces.remove(&key);
        }
        changed
    }

    pub fn clear(&mut self, workspace: &Path) -> bool {
        let key = workspace_key(workspace);
        self.workspaces.remove(&key).is_some()
    }

    pub fn all_dirs(&self) -> Vec<String> {
        self.workspaces
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
    }
}

fn workspace_key(workspace: &Path) -> String {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .display()
        .to_string()
}

/// Run the `fxrs workspace` CLI.
pub fn run_workspace(args: &[String], cwd: &Path) -> Result<i32> {
    let wants_json = args.iter().any(|a| a == "--json");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("list");
    let mut store = WorkspaceDirsStore::open()?;

    match sub {
        "list" | "ls" => {
            let primary = cwd;
            let additional = store.dirs_for(primary);
            if wants_json {
                let arr: Vec<_> = std::iter::once(primary.display().to_string())
                    .chain(additional.iter().map(|d| d.display().to_string()))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                println!("{}", primary.display());
                if additional.is_empty() {
                    println!("  (no additional directories)");
                } else {
                    for d in &additional {
                        println!("  {}", d.display());
                    }
                }
            }
        }
        "add" => {
            let dir = positional
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: fxrs workspace add <path>"))?;
            let added = store.add(cwd, Path::new(dir))?;
            store.save()?;
            println!(
                "{} {}",
                if added { "added" } else { "already present" },
                dir
            );
        }
        "remove" | "rm" => {
            let dir = positional
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: fxrs workspace remove <path>"))?;
            let removed = store.remove(cwd, Path::new(dir));
            store.save()?;
            println!("{} {}", if removed { "removed" } else { "not found" }, dir);
        }
        "clear" => {
            let cleared = store.clear(cwd);
            store.save()?;
            println!(
                "{}",
                if cleared {
                    "cleared additional directories"
                } else {
                    "nothing to clear"
                }
            );
        }
        other => {
            eprintln!("fxrs workspace: unknown subcommand `{other}` (list | add <path> | remove <path> | clear)");
            return Ok(2);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env;

    #[test]
    fn add_remove_roundtrip() {
        test_env::with(|| {
            let tmp = std::env::temp_dir().join(format!("fxrs-ws-test-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).unwrap();
            let mut store = WorkspaceDirsStore::default();
            let ws = Path::new("/tmp");
            assert!(store.add(ws, &tmp).unwrap());
            assert!(!store.add(ws, &tmp).unwrap()); // dedupe
            assert_eq!(store.dirs_for(ws), vec![tmp.clone()]);
            assert!(store.remove(ws, &tmp));
            assert!(store.dirs_for(ws).is_empty());
        });
    }

    #[test]
    fn add_missing_dir_fails() {
        test_env::with(|| {
            let mut store = WorkspaceDirsStore::default();
            let r = store.add(Path::new("/tmp"), Path::new("/definitely/not/here"));
            assert!(r.is_err());
        });
    }
}
