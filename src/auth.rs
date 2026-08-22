//! Credentials store for model providers and MCP servers, mirroring
//! upstream fx's `credentials.jsonl` + `~/.fx/auth/` layout (file backend;
//! OS keychain integration is a later parity item).
//!
//! File: `~/.fx/auth.json` — mode 0600, schema version 1:
//!   { "version": 1, "providers": { "<provider>": { "api_key": "..." } } }
//!
//! Providers with no stored key fall back to their conventional env var, so
//! `fxrs auth add anthropic` complements (never conflicts with)
//! `ANTHROPIC_API_KEY`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u64 = 1;
const MAX_STORE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthStore {
    #[serde(default = "default_version")]
    pub version: u64,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCredential>,
}

impl Default for AuthStore {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderCredential {
    pub api_key: Option<String>,
    /// Optional base URL override (for self-hosted gateways).
    pub base_url: Option<String>,
    /// When the entry was written (RFC3339).
    pub created_at: Option<String>,
}

fn default_version() -> u64 {
    SCHEMA_VERSION
}

pub fn auth_path() -> PathBuf {
    crate::config::fx_home().join("auth.json")
}

/// Load the auth store (missing file is an empty store).
pub fn load() -> Result<AuthStore> {
    let path = auth_path();
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let data = std::fs::read_to_string(&path).context("read auth store")?;
    if data.len() as u64 > MAX_STORE_BYTES {
        bail!("auth store exceeds size limit");
    }
    let store: AuthStore = serde_json::from_str(&data).context("parse auth store")?;
    Ok(store)
}

/// Persist the store with 0600 permissions on a fresh file.
pub fn save(store: &AuthStore) -> Result<()> {
    let path = auth_path();
    save_to(&path, store)
}

fn save_to(path: &Path, store: &AuthStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create auth store directory")?;
    }
    let data = serde_json::to_string_pretty(store)?;
    // Write-then-rename avoids a window where the store is partially written.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).context("write auth store")?;
    set_private_permissions(&tmp)?;
    std::fs::rename(&tmp, path).context("commit auth store")?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Canonical provider name; validates against the known set.
pub fn canonical_provider(name: &str) -> Result<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "gateway" | "ai-gateway" | "ai_gateway" | "vercel" => Ok("gateway"),
        "anthropic" | "claude" => Ok("anthropic"),
        "openai" | "local" | "openai-compatible" | "openai_compatible" => Ok("openai"),
        other => bail!("unknown provider `{other}` (expected gateway, anthropic, or openai)"),
    }
}

/// Resolve an API key for a provider: env var first, then the store.
/// Returns (api_key, base_url).
pub fn resolve_key(provider: &str, store: &AuthStore) -> (Option<String>, Option<String>) {
    let (env_key, env_base): (Option<String>, Option<String>) = match provider {
        "gateway" => (
            std::env::var("AI_GATEWAY_API_KEY")
                .ok()
                .or_else(|| std::env::var("FX_GATEWAY_API_KEY").ok()),
            std::env::var("FX_GATEWAY_BASE_URL")
                .ok()
                .or_else(|| std::env::var("AI_GATEWAY_BASE_URL").ok()),
        ),
        "anthropic" => (
            std::env::var("ANTHROPIC_API_KEY").ok(),
            std::env::var("ANTHROPIC_BASE_URL").ok(),
        ),
        "openai" => (
            std::env::var("AI_API_KEY").ok(),
            std::env::var("AI_BASE_URL").ok(),
        ),
        _ => (None, None),
    };
    if env_key.is_some() {
        return (env_key, env_base);
    }
    let stored = store
        .providers
        .get(provider)
        .and_then(|c| c.api_key.clone());
    (
        stored,
        env_base.or_else(|| {
            store
                .providers
                .get(provider)
                .and_then(|c| c.base_url.clone())
        }),
    )
}

/// Store an API key for a provider, keeping other entries intact.
pub fn set_key(provider: &str, api_key: &str, base_url: Option<&str>) -> Result<()> {
    let provider = canonical_provider(provider)?;
    let mut store = load()?;
    let entry = store.providers.entry(provider.to_string()).or_default();
    if !api_key.trim().is_empty() {
        entry.api_key = Some(api_key.trim().to_string());
    }
    if let Some(base) = base_url {
        if !base.trim().is_empty() {
            entry.base_url = Some(base.trim().to_string());
        }
    }
    entry.created_at = Some(crate::util::today_with_time());
    save(&store)
}

/// Remove a provider credential (no-op if absent).
pub fn remove_key(provider: &str) -> Result<bool> {
    let provider = canonical_provider(provider)?;
    let mut store = load()?;
    let removed = store.providers.remove(provider).is_some();
    if removed {
        save(&store)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_providers() {
        assert_eq!(canonical_provider("Anthropic").unwrap(), "anthropic");
        assert_eq!(canonical_provider("AI_GATEWAY").unwrap(), "gateway");
        assert_eq!(canonical_provider("local").unwrap(), "openai");
        assert!(canonical_provider("nonsense").is_err());
    }

    #[test]
    fn resolve_prefers_env_over_store() {
        let mut store = AuthStore::default();
        store.providers.insert(
            "anthropic".into(),
            ProviderCredential {
                api_key: Some("stored".into()),
                ..Default::default()
            },
        );
        let _g = crate::test_env::lock().lock().unwrap();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "env") };
        let (key, _) = resolve_key("anthropic", &store);
        assert_eq!(key.as_deref(), Some("env"));
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        drop(_g);
        let (key, _) = resolve_key("anthropic", &store);
        assert_eq!(key.as_deref(), Some("stored"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("fxrs-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("auth.json");
        let mut store = AuthStore::default();
        store.providers.insert(
            "gateway".into(),
            ProviderCredential {
                api_key: Some("k".into()),
                base_url: Some("https://example.com".into()),
                ..Default::default()
            },
        );
        save_to(&path, &store).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "store must be private");
        }
        let loaded: AuthStore = serde_json::from_str(&data).unwrap();
        assert_eq!(loaded.version, SCHEMA_VERSION);
        assert_eq!(loaded.providers["gateway"].api_key.as_deref(), Some("k"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
