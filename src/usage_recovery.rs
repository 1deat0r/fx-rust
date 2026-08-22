//! Usage recovery — faithful port of the durable marker registry + bounded
//! collector from upstream `core/session/usage_recovery.zig` and the
//! usage-recovery sliver of `core/session/session_store.zig`.
//!
//! The problem: usage is checkpointed in the per-session file, but the
//! durable profile ledger (`~/.fx/usage.jsonl`) is written separately. If
//! the process dies in the window between the two, the session file carries
//! tokens the ledger never saw. Upstream protects that window with a
//! **recovery marker registry** (`~/.fx/usage_recovery/<session-id>` holding
//! `v1 <protected_updated_at_ms>\n`): written on session checkpoint while
//! profile usage is not yet proven durable, cleared once it is.
//!
//! `collect_from_home_conservative` reads the registry and reloads each
//! marked session to gather the unresolved publication state (facts,
//! incidents, pending hints) bounded by per-kind caps, mirroring upstream's
//! `OwnedRecovery`. In this port the session file carries only summary
//! counters (not per-generation facts), so an un-settled marked session is
//! represented as an [`Incident`] with `Completeness::Incomplete`, and a
//! marker whose session no longer exists (or cannot be read) flips
//! `unknown_pending`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;

const USAGE_RECOVERY_DIR: &str = "usage_recovery";
const MARKER_PREFIX: &str = "v1 ";
/// `v1 ` + a 20-digit i64 + newline (upstream bound).
const MAX_MARKER_BYTES: usize = MARKER_PREFIX.len() + 20 + 1;
const MAX_RECOVERY_SESSIONS: usize = 512;
#[allow(dead_code)]
const MAX_RECOVERY_FACTS: usize = 4096;
const MAX_RECOVERY_INCIDENTS: usize = 4096;
const MAX_RECOVERY_PENDING: usize = 4096;

/// One marked session in the registry (upstream `UsageRecoverySession`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecoverySession {
    pub id: String,
    /// `updated_at_ms` of the session checkpoint that produced the marker.
    pub protected_updated_at_ms: i64,
}

/// Completeness of a saved billing window (upstream `usage_report.Completeness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Pending,
    Incomplete,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    pub occurred_at_ms: i64,
    pub completeness: Completeness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMarker {
    pub id: String,
    pub observed_at_ms: i64,
}

/// Port of upstream `usage_report.GenerationFact`. This port's session files
/// carry summary counters, so the collector does not currently emit facts;
/// the shape is kept for parity with the upstream data contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationFact {
    pub id: String,
    pub created_at_ms: i64,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub billable_web_search_calls: u64,
    pub total_cost: f64,
}

/// Bounded recovery collection (upstream `OwnedRecovery`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnedRecovery {
    pub facts: Vec<GenerationFact>,
    pub incidents: Vec<Incident>,
    pub pending: Vec<PendingMarker>,
    pub unknown_pending: bool,
}

fn recovery_dir() -> PathBuf {
    fx_home().join(USAGE_RECOVERY_DIR)
}

fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Read + validate one marker file. Returns the protected timestamp.
/// `Ok(None)` = marker missing. Errors model upstream's
/// `InvalidUsageRecoveryIndex` for corrupt/lax files.
fn read_marker(dir: &Path, session_id: &str) -> Result<Option<i64>> {
    let path = dir.join(session_id);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("usage recovery marker stat failed: {e}")),
    };
    if !meta.is_file() || meta.len() == 0 || meta.len() as usize > MAX_MARKER_BYTES {
        return Err(anyhow::anyhow!(
            "usage recovery marker `{session_id}` has invalid metadata"
        ));
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("reading usage recovery marker `{session_id}`"))?;
    let text = String::from_utf8_lossy(&data);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let Some(rest) = text.strip_prefix(MARKER_PREFIX) else {
        return Err(anyhow::anyhow!(
            "usage recovery marker `{session_id}` has invalid prefix"
        ));
    };
    if rest.is_empty() {
        return Err(anyhow::anyhow!(
            "usage recovery marker `{session_id}` is empty"
        ));
    }
    let ts: i64 = rest
        .trim()
        .parse()
        .with_context(|| format!("usage recovery marker `{session_id}` timestamp"))?;
    if ts < 0 {
        return Err(anyhow::anyhow!(
            "usage recovery marker `{session_id}` has negative timestamp"
        ));
    }
    Ok(Some(ts))
}

fn create_recovery_dir() -> Result<PathBuf> {
    let dir = recovery_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Validate + return the registry dir if present (upstream
/// `openUsageRecoveryDir`). `Ok(None)` when there is no registry yet.
fn open_recovery_dir() -> Result<Option<PathBuf>> {
    let dir = recovery_dir();
    if !dir.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(&dir)?;
    if !meta.is_dir() {
        return Err(anyhow::anyhow!(
            "usage recovery path exists but is not a directory: {}",
            dir.display()
        ));
    }
    Ok(Some(dir))
}

/// Write the `v1 <protected_updated_at_ms>\n` marker for a session
/// (upstream `markUsageRecoveryPending`).
pub fn mark_pending(session_id: &str, protected_updated_at_ms: i64) -> Result<()> {
    if !is_valid_session_id(session_id) {
        return Err(anyhow::anyhow!("invalid session id `{session_id}`"));
    }
    if protected_updated_at_ms < 0 {
        return Err(anyhow::anyhow!("invalid protected timestamp"));
    }
    let dir = create_recovery_dir()?;
    let body = format!("{MARKER_PREFIX}{protected_updated_at_ms}\n");
    let path = dir.join(session_id);
    // Durable replace: write temp + rename so readers never see a partial file.
    let tmp = dir.join(format!("{session_id}.tmp"));
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming onto {}", path.display()))?;
    Ok(())
}

/// Delete a session's marker (upstream `clearUsageRecoveryPending`).
/// Missing markers are a no-op.
pub fn clear_pending(session_id: &str) -> Result<()> {
    let Some(dir) = open_recovery_dir()? else {
        return Ok(());
    };
    let path = dir.join(session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Whether the ledger already covers the session's claimed usage counters —
/// the durable-recovery predicate for this port (upstream
/// `session_usage.needsProfileRecovery` analog).
pub fn needs_recovery(session: &crate::sessions::Session, ledger_tokens: u64) -> bool {
    session.usage.total_tokens > ledger_tokens
}

/// The bounded durable recovery marker set, sorted by session id
/// (upstream `listUsageRecoverySessions`).
pub fn list_recovery_sessions() -> Result<Vec<UsageRecoverySession>> {
    let Some(dir) = open_recovery_dir()? else {
        return Ok(Vec::new());
    };
    let mut marked = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(anyhow::anyhow!(
                "usage recovery registry contains a non-file entry"
            ));
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_valid_session_id(&name) {
            return Err(anyhow::anyhow!(
                "usage recovery registry contains invalid session id `{name}`"
            ));
        }
        let protected_updated_at_ms = read_marker(&dir, &name)?
            .ok_or_else(|| anyhow::anyhow!("usage recovery marker disappeared for `{name}`"))?;
        marked.push(UsageRecoverySession {
            id: name,
            protected_updated_at_ms,
        });
        if marked.len() > MAX_RECOVERY_SESSIONS {
            return Err(anyhow::anyhow!(
                "usage recovery registry exceeds {MAX_RECOVERY_SESSIONS} sessions"
            ));
        }
    }
    marked.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(marked)
}

/// Collect unresolved publication state through the bounded recovery
/// registry. Only marked sessions are loaded; historical session
/// directories are never scanned (upstream `collectFromHome`).
pub fn collect_from_home_conservative() -> OwnedRecovery {
    collect_from_home().unwrap_or_else(|err| {
        eprintln!("[fxrs] usage recovery incomplete: {err:#}");
        OwnedRecovery {
            unknown_pending: true,
            ..OwnedRecovery::default()
        }
    })
}

fn collect_from_home() -> Result<OwnedRecovery> {
    let marked = list_recovery_sessions()?;
    let mut recovery = OwnedRecovery::default();

    let ledger = crate::usage::UsageStore::new();
    let mut ledger_totals: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for rec in ledger.read_all() {
        *ledger_totals.entry(rec.session_id).or_insert(0) += rec.total_tokens;
    }

    let store = crate::sessions::SessionStore::new()?;

    for marked_session in marked {
        let session = match store.load_by_id(&marked_session.id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                recovery.unknown_pending = true;
                continue;
            }
            Err(_) => {
                recovery.unknown_pending = true;
                continue;
            }
        };
        let ledger_tokens = ledger_totals.get(&marked_session.id).copied().unwrap_or(0);
        if !needs_recovery(&session, ledger_tokens) {
            // Settled. Drop the marker only if the protected checkpoint has
            // been superseded by a later durable save; otherwise something
            // else is still unresolved.
            if marked_session.protected_updated_at_ms >= 0
                && session.updated_ms as i64 >= marked_session.protected_updated_at_ms
            {
                continue;
            }
            recovery.unknown_pending = true;
            continue;
        }
        if (session.updated_ms as i64) < marked_session.protected_updated_at_ms {
            recovery.unknown_pending = true;
        }
        if recovery.pending.len() < MAX_RECOVERY_PENDING {
            recovery.pending.push(PendingMarker {
                id: marked_session.id.clone(),
                observed_at_ms: session.updated_ms as i64,
            });
        } else {
            recovery.unknown_pending = true;
        }
        if session.usage.total_tokens > ledger_tokens
            && recovery.incidents.len() < MAX_RECOVERY_INCIDENTS
        {
            recovery.incidents.push(Incident {
                occurred_at_ms: session.updated_ms as i64,
                completeness: Completeness::Incomplete,
            });
        } else if session.usage.total_tokens > ledger_tokens {
            recovery.unknown_pending = true;
        }
    }
    Ok(recovery)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{Session, SessionUsage};
    use crate::test_env::with;

    fn tmp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("fxrs-ur-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("FX_HOME", &home);
        home
    }

    fn fake_session(id: &str, tokens: u64, updated_ms: u128) -> Session {
        Session {
            schema_version: crate::sessions::SCHEMA_VERSION,
            id: id.into(),
            workspace: "/tmp/ws".into(),
            created_ms: 1,
            updated_ms,
            model: "m".into(),
            mode: crate::permissions::PermissionMode::Ask,
            interactive: false,
            messages: Vec::new(),
            grants: Default::default(),
            usage: SessionUsage {
                total_tokens: tokens,
                ..Default::default()
            },
        }
    }

    #[test]
    fn marker_roundtrip_and_clear() {
        with(|| {
            let home = tmp_home("rt");
            mark_pending("sess-abc", 1234).unwrap();
            let marked = list_recovery_sessions().unwrap();
            assert_eq!(marked.len(), 1);
            assert_eq!(marked[0].id, "sess-abc");
            assert_eq!(marked[0].protected_updated_at_ms, 1234);
            // File format: `v1 1234\n` (upstream marker contract).
            let body = std::fs::read_to_string(home.join("usage_recovery/sess-abc")).unwrap();
            assert_eq!(body, "v1 1234\n");
            clear_pending("sess-abc").unwrap();
            assert!(list_recovery_sessions().unwrap().is_empty());
            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn marker_validation_rejects_bad_files() {
        with(|| {
            let home = tmp_home("bad");
            let dir = home.join("usage_recovery");
            std::fs::create_dir_all(&dir).unwrap();
            // Wrong prefix.
            std::fs::write(dir.join("s1"), "x2 5\n").unwrap();
            assert!(read_marker(&dir, "s1").is_err());
            // Negative timestamp.
            std::fs::write(dir.join("s1"), "v1 -1\n").unwrap();
            assert!(read_marker(&dir, "s1").is_err());
            // Non-numeric.
            std::fs::write(dir.join("s2"), "v1 abc\n").unwrap();
            assert!(read_marker(&dir, "s2").is_err());
            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn list_sorts_by_session_id() {
        with(|| {
            let home = tmp_home("sort");
            mark_pending("z-sess", 1).unwrap();
            mark_pending("a-sess", 2).unwrap();
            mark_pending("m-sess", 3).unwrap();
            let marked = list_recovery_sessions().unwrap();
            let ids: Vec<&str> = marked.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["a-sess", "m-sess", "z-sess"]);
            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn needs_recovery_compares_session_claims_against_ledger() {
        assert!(needs_recovery(&fake_session("s", 10, 1), 5));
        assert!(!needs_recovery(&fake_session("s", 10, 1), 10));
        assert!(!needs_recovery(&fake_session("s", 0, 1), 0));
    }

    #[test]
    fn collect_surfaces_unsettled_marked_sessions_and_drops_settled_ones() {
        with(|| {
            let home = tmp_home("collect");
            // An unsettled session: marker + session file with tokens the
            // ledger never recorded.
            let store = crate::sessions::SessionStore::new().unwrap();
            let sess = fake_session("unsettled-1", 42, 1000);
            store.save(&sess).unwrap();
            mark_pending("unsettled-1", 1000).unwrap();
            // A settled session: session file with tokens covered by ledger.
            let sess2 = fake_session("settled-1", 5, 2000);
            store.save(&sess2).unwrap();
            mark_pending("settled-1", 2000).unwrap();
            let ledger = crate::usage::UsageStore::new();
            ledger
                .record(&crate::usage::UsageRecord {
                    ts_ms: 999,
                    workspace: "/tmp/ws".into(),
                    session_id: "settled-1".into(),
                    model: "m".into(),
                    input_tokens: 5,
                    output_tokens: 0,
                    total_tokens: 5,
                    cost_usd: 0.0,
                    steps: 1,
                    tool_calls: 0,
                    interactive: false,
                })
                .unwrap();

            let recovery = collect_from_home().unwrap();
            assert!(!recovery.unknown_pending);
            assert_eq!(recovery.pending.len(), 1);
            assert_eq!(recovery.pending[0].id, "unsettled-1");
            assert_eq!(recovery.incidents.len(), 1);
            assert_eq!(recovery.incidents[0].completeness, Completeness::Incomplete);
            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn collect_treats_missing_session_as_unknown_pending() {
        with(|| {
            let home = tmp_home("missing");
            mark_pending("ghost-session", 100).unwrap();
            let recovery = collect_from_home().unwrap();
            assert!(recovery.unknown_pending);
            assert!(recovery.pending.is_empty());
            let _ = std::fs::remove_dir_all(&home);
        });
    }
}
