//! Session store: workspace-scoped JSON sessions under ~/.fx/sessions,
//! mirroring fx's session persistence + resume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;
use crate::providers::Message;

/// Current session file schema. Bump when the on-disk shape changes;
/// `migrate_versions` converts older files forward on save.
pub const SCHEMA_VERSION: u32 = 2;

/// Turn/usage numbers attached to a session (sidecar-shaped subset of fx's
/// session_usage_sidecar — the single usage.jsonl remains the global store).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub steps: usize,
    #[serde(default)]
    pub tool_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub schema_version: u32,
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
    #[serde(default)]
    pub usage: SessionUsage,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub workspace: String,
    /// Directory name (`~/project`) shown in listings.
    pub workspace_name: String,
    pub created_ms: u128,
    pub updated_ms: u128,
    pub duration_ms: u128,
    pub model: String,
    pub messages: usize,
    pub role_counts: RoleCounts,
    pub last_text: String,
    pub tokens: u64,
    pub tool_calls: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleCounts {
    pub user: usize,
    pub assistant: usize,
    pub tool: usize,
}

#[derive(Clone)]
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

    pub(crate) fn dir_for(&self, workspace: &Path) -> PathBuf {
        // Sanitize workspace path into a directory name: absolute paths become
        // `ws-<hash of canonical path>` to keep it filesystem-safe.
        let canon = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let key = canon.to_string_lossy().to_string();
        let hash = simple_hash(&key);
        self.root.join(format!("ws-{hash}"))
    }

    pub fn create(
        &self,
        workspace: &Path,
        _interactive: bool,
    ) -> Result<(Vec<Message>, String, BTreeMap<String, String>)> {
        let id = new_session_id(workspace);
        let _ = self.dir_for(workspace); // ensure dir exists
        Ok((Vec::new(), id, BTreeMap::new()))
    }

    pub fn load(&self, workspace: &Path, id: &str) -> Result<Option<Session>> {
        let path = self.dir_for(workspace).join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let sess =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(sess))
    }

    pub fn load_or_error(&self, workspace: &Path, id: &str) -> Result<Session> {
        self.load(workspace, id)?.ok_or_else(|| {
            anyhow::anyhow!("no session `{id}` in workspace {}", workspace.display())
        })
    }

    /// Find a session by id across all workspace dirs (used by usage
    /// recovery, where the marker registry keys on session id only).
    pub fn load_by_id(&self, id: &str) -> Result<Option<Session>> {
        let mut found = None;
        for dir in std::fs::read_dir(&self.root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let path = dir.path().join(format!("{id}.json"));
            if path.exists() {
                let data = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let sess = serde_json::from_str(&data)
                    .with_context(|| format!("parsing {}", path.display()))?;
                found = Some(sess);
                break;
            }
        }
        Ok(found)
    }

    pub fn save(&self, sess: &Session) -> Result<()> {
        let mut sess = sess.clone();
        self.migrate(&mut sess);
        let dir = self.dir_for(Path::new(&sess.workspace));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", sess.id));
        let data = serde_json::to_string_pretty(&sess)?;
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        self.write_latest_pointer(Path::new(&sess.workspace), &sess.id, sess.updated_ms);
        Ok(())
    }

    /// Latest-session pointer file name inside a workspace dir.
    const LATEST: &'static str = "latest.json";

    fn latest_pointer(&self, workspace: &Path) -> Option<String> {
        let p = self.dir_for(workspace).join(Self::LATEST);
        let data = std::fs::read_to_string(p).ok()?;
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
    }

    fn write_latest_pointer(&self, workspace: &Path, id: &str, updated_ms: u128) {
        let dir = self.dir_for(workspace);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[fxrs] cannot create {}: {e:#}", dir.display());
            return;
        }
        let p = dir.join(Self::LATEST);
        let _ = std::fs::write(
            &p,
            serde_json::json!({ "id": id, "updated_ms": updated_ms }).to_string(),
        );
    }

    /// Migrate a session forward to the current schema version in place.
    /// v1 (no schema field) -> v2 adds `schema_version` + empty `usage`.
    pub fn migrate(&self, sess: &mut Session) -> bool {
        if sess.schema_version >= SCHEMA_VERSION {
            return false;
        }
        match sess.schema_version {
            0 | 1 => {
                // v1 files simply lacked the fields; serde defaults already
                // filled them. Normalize + mark current.
                sess.usage = std::mem::take(&mut sess.usage);
                sess.schema_version = SCHEMA_VERSION;
                true
            }
            other => {
                eprintln!(
                    "[fxrs] session {}: unknown schema v{other}; leaving as-is",
                    sess.id
                );
                false
            }
        }
    }

    /// Rewrite every session older than the current schema (used by doctor).
    pub fn migrate_all(&self) -> Result<usize> {
        let mut n = 0;
        for dir in std::fs::read_dir(&self.root)? {
            let dir = dir?;
            if !dir.path().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(dir.path())? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if entry.file_name().to_string_lossy() == Self::LATEST {
                    continue;
                }
                if let Ok(data) = std::fs::read_to_string(entry.path()) {
                    if let Ok(mut sess) = serde_json::from_str::<Session>(&data) {
                        if self.migrate(&mut sess) {
                            let _ = self.save(&sess);
                            n += 1;
                        }
                    }
                }
            }
        }
        Ok(n)
    }

    /// Recover a session file into a fresh session copy (upstream `session
    /// recover`): valid sessions are migrated into a copy under a new id;
    /// corrupt files are salvaged by walking to the largest balanced JSON
    /// prefix that still parses, then built from surviving fields. Returns
    /// the new session id, or None when the source is missing or
    /// unrecoverable.
    pub fn recover(&self, workspace: &Path, id: &str) -> Result<Option<String>> {
        let path = self.dir_for(workspace).join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let text = String::from_utf8_lossy(&data);
        let sess: Option<Session> = serde_json::from_str(&text).ok().or_else(|| {
            salvage_session(&text)
                .or_else(|| salvage_partial_session(&text, workspace))
        });
        let Some(mut sess) = sess else {
            return Ok(None);
        };
        self.migrate(&mut sess);
        // Recovery ids must never collide with the source or each other: a
        // timestamp-based id can repeat within the same millisecond for the
        // same workspace, which would overwrite the source file.
        let base = new_session_id(workspace);
        let mut n = 0u32;
        let new_id = loop {
            let candidate = if n == 0 {
                format!("{base}-rec")
            } else {
                format!("{base}-rec{n}")
            };
            let path = self.dir_for(workspace).join(format!("{candidate}.json"));
            if !path.exists() && candidate != id {
                break candidate;
            }
            n += 1;
        };
        sess.id = new_id.clone();
        sess.created_ms = crate::util::now_ms();
        self.save(&sess)?;
        Ok(Some(new_id))
    }

    /// Delete one session file. Returns true when a file was removed.
    pub fn delete(&self, workspace: &Path, id: &str) -> Result<bool> {
        let path = self.dir_for(workspace).join(format!("{id}.json"));
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path)?;
        if self.latest_pointer(workspace).as_deref() == Some(id) {
            let _ = std::fs::remove_file(self.dir_for(workspace).join(Self::LATEST));
        }
        Ok(true)
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
                if dir.file_name().to_string_lossy()
                    != format!("ws-{}", simple_hash(&canon.to_string_lossy()))
                {
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
                        let mut roles = RoleCounts::default();
                        for m in &sess.messages {
                            match m.role_str() {
                                "user" => roles.user += 1,
                                "assistant" => roles.assistant += 1,
                                "tool" => roles.tool += 1,
                                _ => {}
                            }
                        }
                        out.push(SessionSummary {
                            id: sess.id,
                            workspace: sess.workspace.clone(),
                            workspace_name: workspace_name(&sess.workspace),
                            created_ms: sess.created_ms,
                            updated_ms: sess.updated_ms,
                            duration_ms: sess.updated_ms.saturating_sub(sess.created_ms),
                            model: sess.model,
                            messages: sess.messages.len(),
                            role_counts: roles,
                            last_text,
                            tokens: sess.usage.total_tokens,
                            tool_calls: sess.usage.tool_calls,
                        });
                    }
                }
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        Ok(out)
    }

    pub fn latest(&self, workspace: &Path) -> Result<Option<Session>> {
        // Fast path: the pointer file. Slow path: scan + sort.
        if let Some(id) = self.latest_pointer(workspace) {
            if let Some(sess) = self.load(workspace, &id)? {
                return Ok(Some(sess));
            }
        }
        let mut summaries = self.list(Some(workspace))?;
        if summaries.is_empty() {
            return Ok(None);
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
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

pub fn workspace_name(workspace: &str) -> String {
    let trimmed = workspace.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(workspace).to_string()
}

fn simple_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Best-effort JSON salvage: walk to the largest balanced-brace prefix and
/// parse it as a `Session`. Returns the full parse if one is found.
fn salvage_session(text: &str) -> Option<Session> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escape = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let prefix = &text[start..=i];
                    if let Ok(s) = serde_json::from_str::<Session>(prefix) {
                        return Some(s);
                    }
                    break;
                }
            }
            _ => {}
        }
    }
    None
}

/// Salvage a truncated session from whatever fields survived in the largest
/// parseable JSON prefix: every missing field falls back to defaults. Field
/// candidates are (a) every top-level balanced `{...}`, and (b) every
/// top-level field boundary closed with `}` (a truncated object whose
/// closing brace was lost).
fn salvage_partial_session(text: &str, workspace: &Path) -> Option<Session> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escape = false;
    // Byte offsets whose `[start..end]` slice is a parseable JSON object.
    let mut ends: Vec<usize> = Vec::new();
    for (i, &b) in bytes[start..].iter().enumerate() {
        let i = start + i;
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    ends.push(i + 1);
                }
            }
            b',' if depth == 1 => {
                // A complete top-level field followed by a truncated object:
                // close the object here and try to parse.
                ends.push(i);
            }
            _ => {}
        }
    }
    // Try every candidate from longest to shortest; the longest parseable
    // slice wins. Also try the literal full prefix in case it is complete.
    ends.push(bytes.len());
    ends.sort_unstable();
    ends.dedup();
    let mut value: Option<serde_json::Value> = None;
    for &end in ends.iter().rev() {
        if end <= start {
            continue;
        }
        let slice = &text[start..end];
        let candidate = if slice.trim_end().ends_with('}') || slice.trim_end().ends_with("]]") {
            slice.to_string()
        } else {
            format!("{slice}}}")
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if let Some(obj) = v.as_object() {
                if obj.contains_key("model") || obj.contains_key("id") || obj.contains_key("workspace") {
                    value = Some(v);
                    break;
                }
            }
        }
    }
    let value = value?;
    // We already know the object branch; unwrap the borrow.
    let value_owned = value;
    let obj = value_owned.as_object()?.clone();
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let mode = obj
        .get("mode")
        .and_then(|v| v.as_str())
        .and_then(|m| crate::permissions::PermissionMode::parse(m).ok())
        .unwrap_or_default();
    let interactive = obj.get("interactive").and_then(|v| v.as_bool()).unwrap_or(false);
    let updated_ms = obj
        .get("updated_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u128;
    let messages: Vec<Message> = obj
        .get("messages")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    Some(Session {
        schema_version: 0,
        id: String::new(),
        workspace: workspace.display().to_string(),
        created_ms: 0,
        updated_ms,
        model,
        mode,
        interactive,
        messages,
        grants: Default::default(),
        usage: Default::default(),
    })
}

fn new_session_id(workspace: &Path) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let hash = simple_hash(&canon.to_string_lossy());
    format!("s{ms:x}-{hash:.8}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sess(store: &SessionStore, ws: &str, n: u32) -> Session {
        let (_, id, _) = store.create(Path::new(ws), false).unwrap();
        Session {
            schema_version: SCHEMA_VERSION,
            id,
            workspace: ws.into(),
            created_ms: n as u128,
            updated_ms: n as u128,
            model: "m".into(),
            mode: crate::permissions::PermissionMode::Ask,
            interactive: false,
            messages: vec![Message::user(format!("hello {n}"))],
            grants: Default::default(),
            usage: SessionUsage {
                total_tokens: 100,
                tool_calls: 2,
                input_tokens: 40,
                output_tokens: 60,
                cost_usd: 0.0,
                steps: 1,
            },
        }
    }

    #[test]
    fn save_writes_schema_and_latest_pointer() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-v2");
        let sess = make_sess(&store, "/tmp/fxrs-sess-v2", 7);
        store.save(&sess).unwrap();
        let loaded = store.load_or_error(ws, &sess.id).unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.usage.total_tokens, 100);
        assert_eq!(loaded.usage.tool_calls, 2);
        // Latest pointer resolves without scanning.
        let latest = store.latest(ws).unwrap().unwrap();
        assert_eq!(latest.id, sess.id);
        // Sanity: pointer file exists.
        assert!(store.dir_for(ws).join("latest.json").exists());
        store.delete_all_for(ws).unwrap();
    }

    #[test]
    fn old_v1_file_migrates_on_save() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-migrate");
        // Write a v1-shaped file by hand (no schema_version on disk).
        let dir = store.dir_for(ws);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sess_v1 = make_sess(&store, "/tmp/fxrs-sess-migrate", 3);
        sess_v1.schema_version = 1;
        let path = dir.join(format!("{}.json", sess_v1.id));
        std::fs::write(&path, serde_json::to_string_pretty(&sess_v1).unwrap()).unwrap();
        // Re-read as an old file: strip schema via manual removal is fiddly;
        // simulate by writing a file that lacks schema_version+usage.
        let v1_json = r#"{
  "id": "MIGRATE1",
  "workspace": "/tmp/fxrs-sess-migrate",
  "created_ms": 1,
  "updated_ms": 1,
  "model": "m",
  "mode": "ask",
  "interactive": false,
  "messages": []
}"#;
        std::fs::write(dir.join("MIGRATE1.json"), v1_json).unwrap();
        // Load works (serde defaults), migrate bumps to current on save.
        let mut loaded = store.load_or_error(ws, "MIGRATE1").unwrap();
        assert_eq!(loaded.schema_version, 0); // absent field -> 0
        assert!(store.migrate(&mut loaded));
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        store.save(&loaded).unwrap();
        let reloaded = store.load_or_error(ws, "MIGRATE1").unwrap();
        assert_eq!(reloaded.schema_version, SCHEMA_VERSION);
        assert!(store.latest(ws).unwrap().is_some());
        store.delete_all_for(ws).unwrap();
    }

    #[test]
    fn delete_removes_file_and_pointer() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-del");
        let sess = make_sess(&store, "/tmp/fxrs-sess-del", 9);
        store.save(&sess).unwrap();
        assert!(store.delete(ws, &sess.id).unwrap());
        assert!(store.load(ws, &sess.id).unwrap().is_none());
        assert!(store.latest(ws).unwrap().is_none());
        store.delete_all_for(ws).unwrap();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let store = SessionStore::new().unwrap();
        let (_, id, _) = store
            .create(Path::new("/tmp/fxrs-sess-test"), false)
            .unwrap();
        let sess = Session {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            workspace: "/tmp/fxrs-sess-test".into(),
            created_ms: 1,
            updated_ms: 2,
            model: "m".into(),
            mode: crate::permissions::PermissionMode::Ask,
            interactive: false,
            messages: vec![Message::user("hi")],
            grants: Default::default(),
            usage: Default::default(),
        };
        store.save(&sess).unwrap();
        let loaded = store
            .load_or_error(Path::new("/tmp/fxrs-sess-test"), &id)
            .unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].plain_text(), Some("hi"));
    }

    #[test]
    fn recover_copies_valid_session_to_new_id() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-recover");
        let sess = make_sess(&store, "/tmp/fxrs-sess-recover", 11);
        store.save(&sess).unwrap();

        let new_id = store.recover(ws, &sess.id).unwrap().expect("recovered");
        assert_ne!(new_id, sess.id);
        let recovered = store.load_or_error(ws, &new_id).unwrap();
        assert_eq!(recovered.model, sess.model);
        assert_eq!(recovered.messages.len(), sess.messages.len());
        assert!(store.load(ws, &sess.id).unwrap().is_some(), "source kept");
        store.delete_all_for(ws).unwrap();
    }

    #[test]
    fn recover_salvages_truncated_json() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-salvage");
        let sess = make_sess(&store, "/tmp/fxrs-sess-salvage", 5);
        let path = store.dir_for(ws).join(format!("{}.json", sess.id));
        std::fs::create_dir_all(store.dir_for(ws)).unwrap();
        // Write a truncated JSON file: cut mid-messages array.
        let full = serde_json::to_string_pretty(&sess).unwrap();
        let cut = full.find("\"messages\"").unwrap();
        std::fs::write(&path, &full[..cut]).unwrap();

        let new_id = store.recover(ws, &sess.id).unwrap().expect("salvaged");
        let recovered = store.load_or_error(ws, &new_id).unwrap();
        assert_eq!(recovered.model, "m");
        // The truncated file had dropped messages; recovered still valid.
        assert_eq!(recovered.schema_version, SCHEMA_VERSION);
        store.delete_all_for(ws).unwrap();
    }

    #[test]
    fn recover_missing_or_garbage_returns_none() {
        let store = SessionStore::new().unwrap();
        let ws = Path::new("/tmp/fxrs-sess-norecover");
        assert!(store.recover(ws, "missing-id").unwrap().is_none());

        let dir = store.dir_for(ws);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("garbage.json"), "{{{{ not json at all").unwrap();
        assert!(store.recover(ws, "garbage").unwrap().is_none());
        store.delete_all_for(ws).unwrap();
    }

}
