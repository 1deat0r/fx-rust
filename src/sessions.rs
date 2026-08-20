//! Session store: workspace-scoped JSON sessions under ~/.fx/sessions,
//! mirroring fx's session persistence + resume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;
use crate::providers::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    #[serde(default)]
    pub created_ms: u128,
    pub updated_ms: u128,
    pub model: String,
    pub mode: crate::permissions::PermissionMode,
    pub interactive: bool,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub grants: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub workspace: String,
    pub updated_ms: u128,
    pub model: String,
    pub messages: usize,
    pub last_text: String,
}

pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new() -> Result<Self> {
        let root = fx_home().join("sessions");
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_for(&self, workspace: &Path) -> PathBuf {
        // Sanitize workspace path into a directory name: absolute paths become
        // `ws-<hash of canonical path>` to keep it filesystem-safe.
        let canon = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
        let key = canon.to_string_lossy().to_string();
        let hash = simple_hash(&key);
        self.root.join(format!("ws-{hash}"))
    }

    pub fn create(&self, workspace: &Path, _interactive: bool) -> Result<(Vec<Message>, String, BTreeMap<String, String>)> {
        let id = new_session_id(workspace);
        let _ = self.dir_for(workspace); // ensure dir exists
        Ok((Vec::new(), id, BTreeMap::new()))
    }

    pub fn load(&self, workspace: &Path, id: &str) -> Result<Option<Session>> {
        let path = self.dir_for(workspace).join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let sess = serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(sess))
    }

    pub fn load_or_error(&self, workspace: &Path, id: &str) -> Result<Session> {
        self.load(workspace, id)?
            .ok_or_else(|| anyhow::anyhow!("no session `{id}` in workspace {}", workspace.display()))
    }

    pub fn save(&self, sess: &Session) -> Result<()> {
        let dir = self.dir_for(Path::new(&sess.workspace));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", sess.id));
        let data = serde_json::to_string_pretty(sess)?;
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn list(&self, workspace: Option<&Path>) -> Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        for dir in std::fs::read_dir(&self.root)? {
            let dir = dir?;
            if !dir.path().is_dir() {
                continue;
            }
            // Filter by workspace when requested.
            if let Some(ws) = workspace {
                let canon = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
                if dir.file_name().to_string_lossy() != format!("ws-{}", simple_hash(&canon.to_string_lossy())) {
                    continue;
                }
            }
            for entry in std::fs::read_dir(dir.path())? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(data) = std::fs::read_to_string(entry.path()) {
                    if let Ok(sess) = serde_json::from_str::<Session>(&data) {
                        let last_text = sess
                            .messages
                            .iter()
                            .rev()
                            .find_map(|m| m.last_text())
                            .unwrap_or_default()
                            .chars()
                            .take(80)
                            .collect();
                        out.push(SessionSummary {
                            id: sess.id,
                            workspace: sess.workspace,
                            updated_ms: sess.updated_ms,
                            model: sess.model,
                            messages: sess.messages.len(),
                            last_text,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    pub fn latest(&self, workspace: &Path) -> Result<Option<Session>> {
        let mut summaries = self.list(Some(workspace))?;
        if summaries.is_empty() {
            return Ok(None);
        }
        summaries.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        self.load(workspace, &summaries[0].id)
    }

    pub fn delete_all_for(&self, workspace: &Path) -> Result<usize> {
        let dir = self.dir_for(workspace);
        if !dir.exists() {
            return Ok(0);
        }
        let n = std::fs::read_dir(&dir)?.count();
        std::fs::remove_dir_all(&dir)?;
        Ok(n)
    }
}

fn simple_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn new_session_id(workspace: &Path) -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let canon = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let hash = simple_hash(&canon.to_string_lossy());
    format!("s{ms:x}-{hash:.8}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let store = SessionStore::new().unwrap();
        let (_, id, _) = store.create(Path::new("/tmp/fxrs-sess-test"), false).unwrap();
        let sess = Session {
            id: id.clone(),
            workspace: "/tmp/fxrs-sess-test".into(),
            created_ms: 1,
            updated_ms: 2,
            model: "m".into(),
            mode: crate::permissions::PermissionMode::Ask,
            interactive: false,
            messages: vec![Message::user("hi")],
            grants: Default::default(),
        };
        store.save(&sess).unwrap();
        let loaded = store.load_or_error(Path::new("/tmp/fxrs-sess-test"), &id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].plain_text(), Some("hi"));
    }
}


