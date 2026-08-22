//! Gateway model catalog: fetch + parse the Vercel AI Gateway model catalog
//! (`GET {base}/coding-agent/v1/models`) and classify request failures.
//! Port of upstream `src/core/gateway/model_catalog.zig` and
//! `src/builtins/gateway.zig` (parse + failure classification + public
//! fallback on 401/403).

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

pub const DEFAULT_MODEL_CATALOG_BASE_URL: &str = "https://ai-gateway.vercel.sh";
pub const MODELS_PATH: &str = "/coding-agent/v1/models";
/// Team-scoped header for fx-login credentials on the gateway (upstream
/// `vercel_ai_gateway_team_header`).
pub const TEAM_HEADER: &str = "x-vercel-ai-gateway-team";
pub const BASE_URL_ENV: &str = "FX_GATEWAY_BASE_URL";
pub const E2E_MODELS_URL_ENV: &str = "FX_E2E_GATEWAY_MODELS_URL";

/// One model in the catalog (upstream `ModelCatalogEntry`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub model_type: String,
    pub released: i64,
    pub has_tool_use: bool,
    pub has_reasoning: bool,
    pub reasoning_efforts: Vec<String>,
    pub supports_fast_mode: bool,
    pub has_vision: bool,
    pub has_file_input: bool,
    pub has_web_search: bool,
    pub has_explicit_caching: bool,
    pub has_implicit_caching: bool,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
    pub web_search_price: Option<String>,
}

impl ModelCatalogEntry {
    /// Compact capability summary for tables: `tools,vision,reasoning,128k`.
    pub fn capability_flags(&self) -> String {
        let mut flags = String::new();
        if self.has_tool_use {
            flags.push_str("tools ");
        }
        if self.has_vision {
            flags.push_str("vision ");
        }
        if self.has_reasoning {
            flags.push_str("reasoning ");
        }
        if self.supports_fast_mode {
            flags.push_str("fast ");
        }
        if let Some(ctx) = self.context_window {
            let k = ctx / 1000;
            flags.push_str(&format!("{k}k"));
        }
        flags.trim_end().to_string()
    }
}

// ---------------------------------------------------------------------------
// Failure classification (upstream `Failure` / `failureForHttpStatus`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    Authentication,
    RateLimited,
    GatewayUnavailable,
    Cancellation,
    Transport,
    MalformedResponse,
    HttpStatus,
    ResourceExhausted,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub category: FailureCategory,
    pub http_status: Option<u16>,
    pub retryable: bool,
}

impl Failure {
    pub fn is_auth(&self) -> bool {
        self.category == FailureCategory::Authentication
    }

    pub fn allows_public_fallback(&self) -> bool {
        if self.category != FailureCategory::Authentication {
            return false;
        }
        matches!(self.http_status, Some(401) | Some(403))
    }

    pub fn describe(&self) -> String {
        let cat = match self.category {
            FailureCategory::Authentication => "authentication",
            FailureCategory::RateLimited => "rate_limited",
            FailureCategory::GatewayUnavailable => "gateway_unavailable",
            FailureCategory::Cancellation => "cancellation",
            FailureCategory::Transport => "transport",
            FailureCategory::MalformedResponse => "malformed_response",
            FailureCategory::HttpStatus => "http_status",
            FailureCategory::ResourceExhausted => "resource_exhausted",
            FailureCategory::Runtime => "runtime",
        };
        let mut out = cat.to_string();
        if let Some(status) = self.http_status {
            out.push_str(&format!(" (http {status})"));
        }
        if self.retryable {
            out.push_str(" retryable");
        }
        out
    }
}

pub fn failure_for_http_status(status: u16) -> Failure {
    if status == 401 || status == 403 {
        return Failure {
            category: FailureCategory::Authentication,
            http_status: Some(status),
            retryable: false,
        };
    }
    if status == 429 {
        return Failure {
            category: FailureCategory::RateLimited,
            http_status: Some(status),
            retryable: true,
        };
    }
    if (500..600).contains(&status) {
        return Failure {
            category: FailureCategory::GatewayUnavailable,
            http_status: Some(status),
            retryable: matches!(status, 500 | 502 | 503 | 504),
        };
    }
    Failure {
        category: FailureCategory::HttpStatus,
        http_status: Some(status),
        retryable: false,
    }
}

// ---------------------------------------------------------------------------
// Catalog parsing (upstream `parseModelCatalogEntry` + compare)
// ---------------------------------------------------------------------------

/// Parse an OpenAI-style `{ "data": [...] }` catalog body, filtering to
/// language models, then sorting like upstream (tool-use first, tier,
/// provider, release date, id).
pub fn parse_catalog(json_text: &str) -> Result<Vec<ModelCatalogEntry>> {
    let parsed: Value = serde_json::from_str(json_text).context("parse catalog JSON")?;
    let data = parsed
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("catalog response missing `data` array"))?;
    let mut entries = Vec::new();
    for item in data {
        if let Some(entry) = parse_entry(item) {
            entries.push(entry);
        }
    }
    entries.sort_by(compare_entries);
    Ok(entries)
}

fn parse_entry(entry: &Value) -> Option<ModelCatalogEntry> {
    let obj = entry.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();
    let model_type = obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if !model_type.is_empty() && !model_type.eq_ignore_ascii_case("language") {
        return None;
    }
    let released = obj.get("released").and_then(|v| v.as_i64()).unwrap_or(0);
    let tags = obj.get("tags").and_then(|v| v.as_array());
    let has_tool_use = tag_has(tags, "tool-use");
    let reasoning_efforts = parse_reasoning_efforts(obj.get("reasoning_options"));
    let has_reasoning = tag_has(tags, "reasoning") || !reasoning_efforts.is_empty();
    let supports_fast_mode = supports_fast_mode(obj);
    let has_vision = tag_has(tags, "vision");
    let has_file_input = tag_has(tags, "file-input");
    let has_web_search = tag_has(tags, "web-search");
    let has_explicit_caching = tag_has(tags, "explicit-caching");
    let has_implicit_caching = tag_has(tags, "implicit-caching");
    let context_window = obj
        .get("context_window")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .and_then(|n| u32::try_from(n).ok());
    let max_tokens = obj
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .and_then(|n| u32::try_from(n).ok());
    let web_search_price = obj
        .get("pricing")
        .and_then(|p| p.get("web_search"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(ModelCatalogEntry {
        id,
        model_type,
        released,
        has_tool_use,
        has_reasoning,
        reasoning_efforts,
        supports_fast_mode,
        has_vision,
        has_file_input,
        has_web_search,
        has_explicit_caching,
        has_implicit_caching,
        context_window,
        max_tokens,
        web_search_price,
    })
}

fn tag_has(tags: Option<&Vec<Value>>, name: &str) -> bool {
    tags.map(|tags| tags.iter().any(|t| t.as_str() == Some(name)))
        .unwrap_or(false)
}

fn parse_reasoning_efforts(value: Option<&Value>) -> Vec<String> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(obj) = item.as_object() {
            if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
                if t == "toggle" {
                    continue;
                }
            }
            if let Some(label) = obj.get("label").and_then(|v| v.as_str()) {
                out.push(label.to_string());
            }
        }
    }
    out
}

fn supports_fast_mode(obj: &serde_json::Map<String, Value>) -> bool {
    if obj.get("fast_options").is_some() && has_toggle(obj.get("fast_options")) {
        return true;
    }
    if object_field(obj.get("pricing"), "fast").is_some() {
        return true;
    }
    let owned_by = obj.get("owned_by").and_then(|v| v.as_str()).unwrap_or("");
    owned_by.eq_ignore_ascii_case("openai")
        && object_field(obj.get("pricing"), "service_tiers").is_some()
}

fn object_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value.and_then(|v| v.as_object()).and_then(|o| o.get(key))
}

fn has_toggle(options: Option<&Value>) -> bool {
    options
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|o| {
                o.as_object()
                    .and_then(|o| o.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("toggle")
            })
        })
        .unwrap_or(false)
}

fn provider_rank(id: &str) -> u8 {
    const PROVIDERS: [(&str, u8); 8] = [
        ("anthropic/", 0),
        ("openai/", 1),
        ("google/", 2),
        ("xai/", 3),
        ("deepseek/", 4),
        ("meta/", 5),
        ("mistral/", 6),
        ("alibaba/", 7),
    ];
    for (prefix, rank) in PROVIDERS {
        if id.starts_with(prefix) {
            return rank;
        }
    }
    8
}

fn tier_rank(id: &str) -> u8 {
    let lower = id.to_ascii_lowercase();
    for needle in ["preview", "beta"] {
        if lower.contains(needle) {
            return 4;
        }
    }
    for needle in ["haiku", "mini", "lite"] {
        if lower.contains(needle) {
            return 3;
        }
    }
    if lower.contains("flash") {
        return 2;
    }
    for needle in ["opus", "sonnet", "gpt-5", "o1", "o3", "o4", "pro", "grok-4"] {
        if lower.contains(needle) {
            return 0;
        }
    }
    1
}

fn compare_entries(a: &ModelCatalogEntry, b: &ModelCatalogEntry) -> std::cmp::Ordering {
    if a.has_tool_use != b.has_tool_use {
        return a.has_tool_use.cmp(&b.has_tool_use).reverse();
    }
    let a_tier = tier_rank(&a.id);
    let b_tier = tier_rank(&b.id);
    if a_tier != b_tier {
        return a_tier.cmp(&b_tier);
    }
    let a_provider = provider_rank(&a.id);
    let b_provider = provider_rank(&b.id);
    if a_provider != b_provider {
        return a_provider.cmp(&b_provider);
    }
    if a.released != b.released {
        return b.released.cmp(&a.released);
    }
    a.id.cmp(&b.id)
}

// ---------------------------------------------------------------------------
// Fetch (upstream `fetchModelCatalogResponse` + `fetchWithPublicFallback`)
// ---------------------------------------------------------------------------

pub enum CatalogResult {
    Loaded {
        entries: Vec<ModelCatalogEntry>,
        anonymous_fallback_used: bool,
        fallback_failure: Option<Failure>,
    },
    Failed {
        failure: Failure,
        anonymous_fallback_used: bool,
    },
}

/// The catalog URL. Honors `FX_E2E_GATEWAY_MODELS_URL` and
/// `FX_GATEWAY_BASE_URL`, but only loopback http overrides are trusted.
pub fn catalog_url(base_url_override: Option<&str>) -> String {
    if let Ok(url) = std::env::var(E2E_MODELS_URL_ENV) {
        if is_loopback_http_url(&url) {
            return url;
        }
    }
    if let Some(base) = base_url_override {
        if is_loopback_http_url(base) {
            return format!("{base}{MODELS_PATH}");
        }
    }
    format!("{DEFAULT_MODEL_CATALOG_BASE_URL}{MODELS_PATH}")
}

pub fn is_loopback_http_url(url: &str) -> bool {
    url.starts_with("http://") && {
        let rest = url
            .strip_prefix("http://")
            .unwrap_or(url)
            .split(['/', '?'])
            .next()
            .unwrap_or("");
        let host = rest
            .trim_start_matches('[')
            .split_once(']')
            .map(|(h, _)| h)
            .unwrap_or_else(|| rest.split(':').next().unwrap_or(""));
        host == "localhost" || host == "127.0.0.1" || host == "::1"
    }
}

fn fetch_body(url: &str, api_key: Option<&str>, team: Option<&str>) -> Result<(u16, Vec<u8>)> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(30));
    if let Some(key) = api_key {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    if let Some(team) = team {
        req = req.set(TEAM_HEADER, team);
    }
    match req.call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader()
                .take(8 * 1024 * 1024)
                .read_to_end(&mut buf)
                .context("read catalog body")?;
            Ok((200, buf))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut buf = Vec::new();
            let _ = resp.into_reader().take(64 * 1024).read_to_end(&mut buf);
            Ok((code, buf))
        }
        Err(e) => Err(e).context("fetch model catalog"),
    }
}

/// Fetch the catalog, retrying anonymously when the authenticated request is
/// rejected with 401/403 (upstream `fetchWithPublicFallback`).
pub fn fetch_catalog(api_key: Option<&str>, team: Option<&str>) -> CatalogResult {
    let base = std::env::var(BASE_URL_ENV).ok();
    let url = catalog_url(base.as_deref());
    let first = fetch_body(&url, api_key, team);
    match first {
        Ok((status, body)) => {
            if status == 200 {
                match parse_catalog(&String::from_utf8_lossy(&body)) {
                    Ok(entries) => CatalogResult::Loaded {
                        entries,
                        anonymous_fallback_used: false,
                        fallback_failure: None,
                    },
                    Err(_) => CatalogResult::Failed {
                        failure: Failure {
                            category: FailureCategory::MalformedResponse,
                            http_status: Some(200),
                            retryable: false,
                        },
                        anonymous_fallback_used: false,
                    },
                }
            } else {
                let failure = failure_for_http_status(status);
                if !failure.allows_public_fallback() {
                    return CatalogResult::Failed {
                        failure,
                        anonymous_fallback_used: false,
                    };
                }
                match fetch_body(&url, None, None) {
                    Ok((200, body)) => match parse_catalog(&String::from_utf8_lossy(&body)) {
                        Ok(entries) => CatalogResult::Loaded {
                            entries,
                            anonymous_fallback_used: true,
                            fallback_failure: Some(failure),
                        },
                        Err(_) => CatalogResult::Failed {
                            failure: Failure {
                                category: FailureCategory::MalformedResponse,
                                http_status: Some(200),
                                retryable: false,
                            },
                            anonymous_fallback_used: true,
                        },
                    },
                    Ok((fallback_status, _)) => CatalogResult::Failed {
                        failure: failure_for_http_status(fallback_status),
                        anonymous_fallback_used: true,
                    },
                    Err(_) => CatalogResult::Failed {
                        failure: Failure {
                            category: FailureCategory::Transport,
                            retryable: true,
                            http_status: None,
                        },
                        anonymous_fallback_used: true,
                    },
                }
            }
        }
        Err(_) => CatalogResult::Failed {
            failure: Failure {
                category: FailureCategory::Transport,
                retryable: true,
                http_status: None,
            },
            anonymous_fallback_used: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Lazy process-wide cache + capability lookup
// ---------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<CachedCatalog>>> =
    std::sync::OnceLock::new();

struct CachedCatalog {
    at: Instant,
    entries: Vec<ModelCatalogEntry>,
}

fn cache() -> &'static std::sync::Mutex<Option<CachedCatalog>> {
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Load the catalog into the process cache (used by `fxrs models`); returns
/// true when fresh entries are now available.
pub fn refresh_cache() -> bool {
    let (key, team) = crate::auth::load()
        .ok()
        .map(|store| crate::auth::resolve_key("gateway", &store))
        .unwrap_or((None, None));
    match fetch_catalog(key.as_deref(), team.as_deref()) {
        CatalogResult::Loaded { entries, .. } => {
            *cache().lock().unwrap() = Some(CachedCatalog {
                at: Instant::now(),
                entries,
            });
            true
        }
        CatalogResult::Failed { .. } => false,
    }
}

/// Best-effort capability lookup for a model id from the cache. Never
/// triggers a network fetch.
pub fn lookup_cached(model_id: &str) -> Option<ModelCatalogEntry> {
    let guard = cache().lock().ok()?;
    let cached = guard.as_ref()?;
    if cached.at.elapsed() > CACHE_TTL {
        return None;
    }
    cached.entries.iter().find(|e| e.id == model_id).cloned()
}

/// Derive context limits from the model catalog when available and the user
/// has no explicit override (upstream capability-driven context limits).
pub fn context_limits_for(
    model_id: &str,
    user: crate::context::ContextLimits,
) -> crate::context::ContextLimits {
    if std::env::var("FX_CONTEXT_LIMIT").is_ok() {
        return user;
    }
    let Some(entry) = lookup_cached(model_id) else {
        return user;
    };
    let Some(ctx) = entry.context_window else {
        return user;
    };
    let cap = ctx as usize;
    if cap < user.max_tokens {
        crate::context::ContextLimits {
            warn_at_tokens: cap * 8 / 10,
            max_tokens: cap,
        }
    } else {
        user
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Credits / balance (upstream `credits_path` + `fetchCredits`)
// ---------------------------------------------------------------------------

pub const CREDITS_PATH: &str = "/coding-agent/v1/credits";

/// Snapshot of the AI Gateway credit balance (upstream `CreditsSnapshot`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreditsSnapshot {
    pub balance: Option<String>,
    pub used: Option<String>,
    pub plan: Option<String>,
    pub raw_json: Option<String>,
    pub err_message: Option<String>,
}

impl CreditsSnapshot {
    pub fn is_error(&self) -> bool {
        self.err_message.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditsResult {
    Loaded(CreditsSnapshot),
    Failed { failure: Failure },
}

/// The credits URL. Honors `FX_E2E_GATEWAY_CREDITS_URL` and
/// `FX_GATEWAY_BASE_URL`, but only loopback http overrides are trusted
/// (mirrors `catalog_url`).
pub fn credits_url() -> String {
    if let Ok(url) = std::env::var("FX_E2E_GATEWAY_CREDITS_URL") {
        if is_loopback_http_url(&url) {
            return url;
        }
    }
    if let Ok(base) = std::env::var(BASE_URL_ENV) {
        if is_loopback_http_url(&base) {
            return format!("{base}{CREDITS_PATH}");
        }
    }
    format!("{DEFAULT_MODEL_CATALOG_BASE_URL}{CREDITS_PATH}")
}

/// Parse the credits JSON `{balance, used, plan}` (all strings; upstream
/// `creditsSnapshotFromJsonValue`).
pub fn parse_credits(json: &str) -> Result<CreditsSnapshot> {
    let value: Value = serde_json::from_str(json).context("parse credits json")?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("credits response must be an object"))?;
    let string_field = |name: &str| -> Option<String> {
        obj.get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    Ok(CreditsSnapshot {
        balance: string_field("balance"),
        used: string_field("used"),
        plan: string_field("plan"),
        raw_json: Some(json.to_string()),
        err_message: None,
    })
}

/// Fetch the gateway credits balance (`GET {base}/coding-agent/v1/credits`).
/// Unlike the model catalog there is no anonymous fallback: credits require a
/// credential, so auth/rate-limit failures surface directly.
pub fn fetch_credits(api_key: Option<&str>, team: Option<&str>) -> CreditsResult {
    let url = credits_url();
    let mut team_query = None;
    // A team-selecting credential (fx login) must name the team in the query
    // value: `/v1/credits` reads `teamId` and ignores the header (the reverse
    // of the inference endpoint).
    if let Some(team) = team {
        if valid_gateway_team(team) {
            team_query = Some(format!("?teamId={}", encode_team(team)));
        }
    }
    let final_url = match team_query {
        Some(q) => format!("{url}{q}"),
        None => url,
    };
    match fetch_body(&final_url, api_key, None) {
        Ok((200, body)) => match parse_credits(&String::from_utf8_lossy(&body)) {
            Ok(snapshot) => CreditsResult::Loaded(snapshot),
            Err(_) => CreditsResult::Failed {
                failure: Failure {
                    category: FailureCategory::MalformedResponse,
                    http_status: Some(200),
                    retryable: false,
                },
            },
        },
        Ok((status, _)) => CreditsResult::Failed {
            failure: failure_for_http_status(status),
        },
        Err(_) => CreditsResult::Failed {
            failure: Failure {
                category: FailureCategory::Transport,
                retryable: true,
                http_status: None,
            },
        },
    }
}

fn valid_gateway_team(team: &str) -> bool {
    // Upstream `validGatewayTeam`: url-safe team id (letters, digits, dash,
    // underscore); rejects anything that cannot round-trip in a query.
    !team.is_empty()
        && team.len() < 512
        && team
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ')
}

fn encode_team(team: &str) -> String {
    // Minimal query encoding for the safe set upstream allows.
    team.replace(' ', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "data": [
        {"id": "openai/gpt-5.4", "type": "language", "released": 1780000000,
         "context_window": 400000, "max_tokens": 64000,
         "tags": ["tool-use", "vision", "reasoning", "explicit-caching"],
         "reasoning_options": [{"label": "low"}, {"label": "high"}],
         "owned_by": "openai"},
        {"id": "anthropic/claude-sonnet-4-6", "type": "language", "released": 1770000000,
         "context_window": 200000, "max_tokens": 32000,
         "tags": ["tool-use", "vision"], "owned_by": "anthropic"},
        {"id": "deepseek/deepseek-v4-flash", "type": "language", "released": 1760000000,
         "context_window": 128000,
         "tags": ["tool-use"], "owned_by": "deepseek"},
        {"id": "image-maker", "type": "image", "released": 1750000000,
         "tags": []}
      ]
    }"#;

    #[test]
    fn parses_and_filters_catalog() {
        let entries = parse_catalog(SAMPLE).unwrap();
        // image-maker excluded (non-language), others sorted: tool-use first,
        // tier (sonnet/gpt-5 before flash), provider (anthropic, openai, deepseek).
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "anthropic/claude-sonnet-4-6");
        assert_eq!(entries[1].id, "openai/gpt-5.4");
        assert_eq!(entries[2].id, "deepseek/deepseek-v4-flash");
        let gpt = &entries[1];
        assert_eq!(gpt.context_window, Some(400000));
        assert!(gpt.has_vision && gpt.has_reasoning);
        assert_eq!(gpt.reasoning_efforts, vec!["low", "high"]);
    }

    #[test]
    fn failure_classes() {
        let f = failure_for_http_status(401);
        assert!(f.is_auth() && f.allows_public_fallback());
        let f = failure_for_http_status(429);
        assert_eq!(f.category, FailureCategory::RateLimited);
        assert!(f.retryable);
        let f = failure_for_http_status(503);
        assert_eq!(f.category, FailureCategory::GatewayUnavailable);
        assert!(f.retryable);
        let f = failure_for_http_status(404);
        assert_eq!(f.category, FailureCategory::HttpStatus);
        assert!(!f.retryable);
    }

    #[test]
    fn loopback_url_policy() {
        assert!(is_loopback_http_url(
            "http://127.0.0.1:43123/coding-agent/v1/models"
        ));
        assert!(is_loopback_http_url("http://localhost:1234/x"));
        assert!(!is_loopback_http_url(
            "https://ai-gateway.vercel.sh/coding-agent/v1/models"
        ));
        assert!(!is_loopback_http_url("http://evil.com/x"));
    }

    #[test]
    fn composed_catalog_url() {
        assert_eq!(
            catalog_url(None),
            "https://ai-gateway.vercel.sh/coding-agent/v1/models"
        );
        assert_eq!(
            catalog_url(Some("http://evil.com")),
            "https://ai-gateway.vercel.sh/coding-agent/v1/models"
        );
        assert_eq!(
            catalog_url(Some("http://127.0.0.1:9999")),
            "http://127.0.0.1:9999/coding-agent/v1/models"
        );
    }
    #[test]
    fn parses_credits_snapshot() {
        let snap = parse_credits(r#"{"balance":"123.45","used":"67","plan":"hobby"}"#).unwrap();
        assert_eq!(snap.balance.as_deref(), Some("123.45"));
        assert_eq!(snap.used.as_deref(), Some("67"));
        assert_eq!(snap.plan.as_deref(), Some("hobby"));
        assert!(!snap.is_error());

        let empty = parse_credits("{}").unwrap();
        assert_eq!(empty.balance, None);
        assert_eq!(empty.plan, None);

        assert!(parse_credits("[1,2]").is_err());
        assert!(parse_credits("not json").is_err());
    }

    #[test]
    fn credits_url_is_loopback_trusted() {
        let _g = crate::test_env::lock().lock().unwrap();
        std::env::remove_var("FX_E2E_GATEWAY_CREDITS_URL");
        std::env::set_var("FX_GATEWAY_BASE_URL", "https://gateway.example.com");
        let url = credits_url();
        // Non-loopback base is not trusted -> default gateway.
        assert!(
            url.starts_with("https://ai-gateway.vercel.sh"),
            "url: {url}"
        );
        std::env::set_var("FX_GATEWAY_BASE_URL", "http://127.0.0.1:8787");
        let url = credits_url();
        assert_eq!(url, "http://127.0.0.1:8787/coding-agent/v1/credits");
        std::env::set_var(
            "FX_E2E_GATEWAY_CREDITS_URL",
            "http://localhost:9999/credits",
        );
        let url = credits_url();
        assert_eq!(url, "http://localhost:9999/credits");
        std::env::remove_var("FX_E2E_GATEWAY_CREDITS_URL");
        std::env::remove_var("FX_GATEWAY_BASE_URL");
        drop(_g);
    }

    #[test]
    fn valid_teams_are_url_safe() {
        assert!(valid_gateway_team("team_acme-42"));
        assert!(!valid_gateway_team(""));
        assert!(!valid_gateway_team("a b/../../x"));
        assert_eq!(encode_team("team 1"), "team%201");
    }
}
