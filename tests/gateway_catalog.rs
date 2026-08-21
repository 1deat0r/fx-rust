//! E2E gateway model catalog test against a loopback fixture. Skips unless
//! FX_E2E_GATEWAY_MODELS_URL points at a running server (see
//! tests/fake_mcp_server.py or any server returning an OpenAI-style
//! `{ "data": [...] }` body).

use fxrs::gateway;

const SAMPLE: &str = r#"{
  "data": [
    {"id": "openai/gpt-5.4", "type": "language", "context_window": 400000,
     "max_tokens": 64000, "tags": ["tool-use", "vision", "reasoning"],
     "owned_by": "openai"},
    {"id": "anthropic/claude-sonnet-4-6", "type": "language",
     "context_window": 200000, "tags": ["tool-use"], "owned_by": "anthropic"},
    {"id": "not-language", "type": "embedding", "tags": []}
  ]
}"#;

#[test]
fn catalog_url_policy_and_parse() {
    assert_eq!(
        gateway::catalog_url(None),
        "https://ai-gateway.vercel.sh/coding-agent/v1/models"
    );
    let entries = gateway::parse_catalog(SAMPLE).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "anthropic/claude-sonnet-4-6");
    assert!(entries[1].has_vision);
}

#[test]
fn fetch_catalog_from_loopback_fixture() {
    let Ok(url) = std::env::var("FX_E2E_GATEWAY_MODELS_URL") else {
        eprintln!("FX_E2E_GATEWAY_MODELS_URL unset; skipping");
        return;
    };
    if !gateway::is_loopback_http_url(&url) {
        eprintln!("fixture url must be loopback http; skipping");
        return;
    }
    std::env::set_var("FX_E2E_GATEWAY_MODELS_URL", &url);
    match gateway::fetch_catalog(None, None) {
        gateway::CatalogResult::Loaded { entries, .. } => {
            assert!(!entries.is_empty(), "fixture should return models");
            assert!(
                entries.iter().all(|e| !e.id.is_empty()),
                "catalog ids must be non-empty"
            );
        }
        gateway::CatalogResult::Failed { failure, .. } => {
            panic!("fixture fetch failed: {}", failure.describe());
        }
    }
    std::env::remove_var("FX_E2E_GATEWAY_MODELS_URL");
}

#[test]
fn failure_classification_roundtrip() {
    let f = gateway::failure_for_http_status(401);
    assert!(f.allows_public_fallback());
    let f = gateway::failure_for_http_status(503);
    assert!(f.retryable);
}
