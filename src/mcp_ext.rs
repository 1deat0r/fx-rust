//! MCP protocol extensions — ports of the advanced `core/mcp/*` surfaces
//! that sit on top of the transports in `mcp.rs` / `mcp_transport.rs`:
//!
//! - **MRTR** (`multi-round-trip requests`): bounded parsing/validation of
//!   stateless client-input envelopes (`_meta.mrtr.inputRequired`) carried
//!   by modern MCP operation results, plus response building for the three
//!   request kinds (sampling/createMessage, roots/list, elicitation/create).
//! - **Elicitation**: bounded parsing of `elicitation/create` request bodies
//!   and a plain-text rendering used to surface the question to a human.
//! - **Tool subscription**: per-server list-changed subscription state and
//!   notification-name mapping (tools/resources/prompts).
//! - **OAuth DCR (minimal)**: builds the Dynamic Client Registration POST
//!   body and parses the registration response (client_id), persisting it to
//!   the auth store as a first-class remote-MCP credential.

use serde_json::{json, Value};

// ------------------------------------------------------------------ limits

/// Bounds for MRTR envelopes (upstream `mrtr.Limits`).
#[derive(Debug, Clone, Copy)]
pub struct MrtrLimits {
    pub max_requests: usize,
    pub max_name_bytes: usize,
    pub max_json_bytes: usize,
    pub max_string_bytes: usize,
    pub max_collection_items: usize,
    pub max_depth: usize,
}

impl Default for MrtrLimits {
    fn default() -> Self {
        MrtrLimits {
            max_requests: 32,
            max_name_bytes: 256,
            max_json_bytes: 128 * 1024,
            max_string_bytes: 64 * 1024,
            max_collection_items: 256,
            max_depth: 32,
        }
    }
}

/// Elicitation bounds (upstream `elicitation.Limits`, subset).
#[derive(Debug, Clone, Copy)]
pub struct ElicitationLimits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_message_bytes: usize,
    pub max_schema_bytes: usize,
    pub max_fields: usize,
    pub max_options: usize,
    pub max_name_bytes: usize,
    pub max_label_bytes: usize,
    pub max_string_bytes: usize,
}

impl Default for ElicitationLimits {
    fn default() -> Self {
        ElicitationLimits {
            max_request_bytes: 128 * 1024,
            max_response_bytes: 128 * 1024,
            max_message_bytes: 8 * 1024,
            max_schema_bytes: 128 * 1024,
            max_fields: 64,
            max_options: 64,
            max_name_bytes: 256,
            max_label_bytes: 1024,
            max_string_bytes: 64 * 1024,
        }
    }
}

// --------------------------------------------------------------- elicitation

/// One parsed `elicitation/create` request (upstream `elicitation.Request`,
/// bounded surface used for rendering to a human).
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationRequest {
    pub mode: String, // "form" | "url"
    /// Human-readable instruction (may be empty).
    pub instruction: String,
    /// Title/copy shown with the form.
    pub title: String,
    /// URL for mode=url.
    pub url: Option<String>,
    /// Field names for mode=form (the model fills these).
    pub field_names: Vec<String>,
    /// Option labels for a select field (mode=form), if any.
    pub options: Vec<String>,
    pub secret: bool,
}

impl ElicitationRequest {
    /// Parse a request body with default limits. Returns `None` on invalid,
    /// oversized, or unexpected shapes (the caller then reports the raw text).
    pub fn parse(value: &Value) -> Option<ElicitationRequest> {
        Self::parse_limited(value, ElicitationLimits::default())
    }

    pub fn parse_limited(value: &Value, limits: ElicitationLimits) -> Option<ElicitationRequest> {
        if !value.is_object() {
            return None;
        }
        let text = serde_json::to_string(value).ok()?;
        if text.len() > limits.max_request_bytes {
            return None;
        }
        let mode = value.get("mode")?.as_str()?.to_string();
        if mode != "form" && mode != "url" {
            return None;
        }
        let instruction = value
            .get("instruction")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(limits.max_message_bytes)
            .collect();
        let title = value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(limits.max_label_bytes)
            .collect();
        let url = value
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(limits.max_string_bytes).collect());
        if mode == "url" && url.is_none() {
            return None;
        }
        let mut field_names: Vec<String> = Vec::new();
        let mut options: Vec<String> = Vec::new();
        if let Some(schema) = value.get("schema").and_then(|v| v.as_object()) {
            if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
                for (k, v) in properties.iter().take(limits.max_fields) {
                    field_names.push(k.chars().take(limits.max_name_bytes).collect());
                    if let Some(enums) = v.get("enum").and_then(|e| e.as_array()) {
                        for e in enums.iter().take(limits.max_options) {
                            if let Some(s) = e.as_str() {
                                options.push(s.chars().take(limits.max_string_bytes).collect());
                            }
                        }
                    }
                }
            }
        }
        let secret = value
            .get("secret")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Some(ElicitationRequest {
            mode,
            instruction,
            title,
            url,
            field_names,
            options,
            secret,
        })
    }

    /// Render a plain-text question/hint for a human or the model.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.title.is_empty() {
            out.push_str(&self.title);
            out.push('\n');
        }
        if !self.instruction.is_empty() {
            out.push_str(&self.instruction);
            out.push('\n');
        }
        match self.mode.as_str() {
            "url" => {
                out.push_str("Please open this URL to continue:");
                out.push('\n');
                out.push_str(self.url.as_deref().unwrap_or(""));
            }
            _ => {
                if !self.field_names.is_empty() {
                    out.push_str("Provide values for:");
                    out.push('\n');
                    for (i, f) in self.field_names.iter().enumerate() {
                        out.push_str(&format!("  [{}] {f}\n", i + 1));
                    }
                }
                if !self.options.is_empty() {
                    out.push_str("Choose from:");
                    out.push('\n');
                    for (i, o) in self.options.iter().enumerate() {
                        out.push_str(&format!("  [{}] {o}\n", i + 1));
                    }
                }
                if self.secret {
                    out.push_str("(secret values will be masked)\n");
                }
            }
        }
        out.trim_end().to_string()
    }
}

// ------------------------------------------------------------------ mrtr

/// Kind of a client-input request embedded in an MRTR envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MrtrRequestKind {
    SamplingCreateMessage,
    RootsList,
    ElicitationCreate,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct MrtrRequest<'a> {
    pub key: &'a str,
    pub method: String,
    pub params: Value,
    pub kind: MrtrRequestKind,
}

/// A parsed MRTR envelope: `_meta.mrtr` with either `inputRequired` requests
/// (needing responses) or `responses` (answers to our own requests).
#[derive(Debug, Clone)]
pub struct MrtrEnvelope {
    pub input_required: Vec<Value>,
    pub responses: Vec<Value>,
}

/// Parse `_meta.mrtr` from an operation result / request object.
pub fn parse_mrtr(value: &Value) -> Option<MrtrEnvelope> {
    parse_mrtr_limited(value, MrtrLimits::default())
}

pub fn parse_mrtr_limited(value: &Value, limits: MrtrLimits) -> Option<MrtrEnvelope> {
    if !value.is_object() {
        return None;
    }
    let meta = value.get("_meta")?;
    if !meta.is_object() {
        return None;
    }
    let body = meta.get("mrtr")?;
    if !body.is_object() {
        return None;
    }
    let text = serde_json::to_string(body).ok()?;
    if text.len() > limits.max_json_bytes {
        return None;
    }
    let input_required =
        parse_bounded_array(body.get("inputRequired"), limits)?.unwrap_or_default();
    let responses = parse_bounded_array(body.get("responses"), limits)?.unwrap_or_default();
    if input_required.is_empty() && responses.is_empty() {
        return None;
    }
    Some(MrtrEnvelope {
        input_required,
        responses,
    })
}

fn parse_bounded_array(v: Option<&Value>, limits: MrtrLimits) -> Option<Option<Vec<Value>>> {
    // Missing or explicit null means "no requests of this side" — not an
    // error (an envelope may carry only inputRequired or only responses).
    match v {
        None | Some(Value::Null) => Some(None),
        Some(other) => {
            let arr = other.as_array()?;
            if arr.len() > limits.max_requests {
                return None;
            }
            Some(Some(arr.clone()))
        }
    }
}

pub fn parse_mrtr_request(value: &Value, limits: MrtrLimits) -> Option<MrtrRequest<'_>> {
    let key = value.get("key")?.as_str()?;
    let method = value.get("method")?.as_str()?.to_string();
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
    if key.len() > limits.max_name_bytes {
        return None;
    }
    let kind = match method.as_str() {
        "sampling/createMessage" => MrtrRequestKind::SamplingCreateMessage,
        "roots/list" => MrtrRequestKind::RootsList,
        "elicitation/create" => MrtrRequestKind::ElicitationCreate,
        _ => MrtrRequestKind::Unknown,
    };
    Some(MrtrRequest {
        key,
        method,
        params,
        kind,
    })
}

/// Build MRTR responses for a set of input-required requests.
///
/// - `sampling/createMessage` → declined (model sampling is not available to
///   third-party servers in fxrs; the server is told the sample was refused).
/// - `roots/list` → the workspace root(s) as `file://` URLs.
/// - `elicitation/create` → when `answer` is provided, echoes it; otherwise
///   the request is surfaced in the output text (the caller may choose to
///   prompt a human and then re-run with `answer`).
/// - Unknown methods → skipped (no fabricated answers).
pub fn build_mrtr_responses(
    input_required: &[Value],
    workspace_roots: &[String],
    limits: MrtrLimits,
    answer: &mut dyn FnMut(&MrtrRequest) -> Option<String>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for item in input_required.iter().take(limits.max_requests) {
        let Some(req) = parse_mrtr_request(item, limits) else {
            continue;
        };
        let response = match req.kind {
            MrtrRequestKind::RootsList => json!({
                "key": req.key,
                "result": { "roots": workspace_roots.iter().map(|r| json!({"uri": r})).collect::<Vec<_>>() }
            }),
            MrtrRequestKind::SamplingCreateMessage => json!({
                "key": req.key,
                "result": { "role": "assistant", "content": { "type": "text", "text": "sampling declined by fxrs" }, "modelPreferences": {"priority": 0.0} },
                "isError": false
            }),
            MrtrRequestKind::ElicitationCreate => {
                if let Some(a) = answer(&req) {
                    json!({ "key": req.key, "result": { "content": [ { "type": "text", "text": a } ] } })
                } else {
                    // No live answer source: carry the question text so the
                    // caller can show it to the agent/human and retry.
                    let req_text = ElicitationRequest::parse(&req.params)
                        .map(|e| e.render())
                        .unwrap_or_else(|| {
                            serde_json::to_string_pretty(&req.params).unwrap_or_default()
                        });
                    json!({
                        "key": req.key,
                        "needs_answer": true,
                        "prompt": req_text,
                    })
                }
            }
            MrtrRequestKind::Unknown => continue,
        };
        out.push(response);
    }
    out
}

/// Fold MRTR responses into the next request's `_meta.mrtr.responses`.
pub fn with_responses(request: &mut Value, responses: &[Value]) {
    if responses.is_empty() {
        return;
    }
    let meta = request.get_mut("_meta").and_then(|m| m.as_object_mut());
    match meta {
        Some(m) => {
            if let Some(mrtr) = m.entry("mrtr").or_insert_with(|| json!({})).as_object_mut() {
                mrtr.insert("responses".into(), Value::Array(responses.to_vec()));
            }
        }
        None => {
            request["_meta"] = json!({ "mrtr": { "responses": responses } });
        }
    }
}

// ------------------------------------------------------------- subscription

/// Subscription filters for list-changed notifications on one server
/// (upstream `tool_subscription.Filters`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionFilters {
    pub tools_list_changed: bool,
    pub resources_list_changed: bool,
    pub prompts_list_changed: bool,
}

/// The notification method a server sends for each subscription type.
pub fn notification_for(filter: SubscriptionFilters) -> Option<&'static str> {
    if filter.tools_list_changed {
        Some("notifications/tools/list_changed")
    } else if filter.resources_list_changed {
        Some("notifications/resources/list_changed")
    } else if filter.prompts_list_changed {
        Some("notifications/prompts/list_changed")
    } else {
        None
    }
}

/// Classify an inbound notification: does it match a subscription?
pub fn matches_subscription(method: &str, filters: SubscriptionFilters) -> bool {
    (filters.tools_list_changed && method == "notifications/tools/list_changed")
        || (filters.resources_list_changed && method == "notifications/resources/list_changed")
        || (filters.prompts_list_changed && method == "notifications/prompts/list_changed")
}

const SUBSCRIBE_METHODS: &[&str] = &[
    "tools/list_changed",
    "resources/list_changed",
    "prompts/list_changed",
];

/// Build the `tools/subscribe`, `resources/subscribe`, `prompts/subscribe`
/// params for a full subscription (upstream subscribes per server).
pub fn subscribe_params() -> impl Iterator<Item = &'static str> {
    SUBSCRIBE_METHODS.iter().copied()
}

// --------------------------------------------------------------- oauth dcr

/// Build the Dynamic Client Registration POST body for a remote MCP server
/// (upstream `mcp_auth` DCR payload). `redirect` is the client redirect URI
/// (not used interactively today; registered so token flows work later).
pub fn dcr_registration_body(server_name: &str, redirect_uri: &str, grants: &[&str]) -> Value {
    json!({
        "client_name": format!("fxrs-{server_name}"),
        "client_uri": "https://github.com/1deat0r/fx-rust",
        "redirect_uris": [redirect_uri],
        "grant_types": grants,
        "token_endpoint_auth_method": "none",
        "scope": "mcp",
    })
}

/// Parse a DCR registration response into the client id (or an error text).
pub fn parse_dcr_response(body: &Value) -> Result<String, String> {
    if let Some(e) = body.get("error").and_then(|v| v.as_str()) {
        return Err(e.to_string());
    }
    body.get("client_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "DCR response missing client_id".to_string())
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mrtr_envelope_parses_roots_and_sampling() {
        let result = json!({
            "content": [{"type":"text","text":"ok"}],
            "_meta": {
                "mrtr": {
                    "inputRequired": [
                        {"key":"r1","method":"roots/list","params":{}},
                        {"key":"m1","method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"hi"}}]}}
                    ]
                }
            }
        });
        let env = parse_mrtr(&result).expect("envelope");
        assert_eq!(env.input_required.len(), 2);
        let req = parse_mrtr_request(&env.input_required[0], MrtrLimits::default()).unwrap();
        assert_eq!(req.kind, MrtrRequestKind::RootsList);
        let req2 = parse_mrtr_request(&env.input_required[1], MrtrLimits::default()).unwrap();
        assert_eq!(req2.kind, MrtrRequestKind::SamplingCreateMessage);
    }

    #[test]
    fn mrtr_responses_answer_roots_and_decline_sampling() {
        let env = parse_mrtr(&json!({
            "_meta": { "mrtr": { "inputRequired": [
                {"key":"r1","method":"roots/list","params":{}},
                {"key":"m1","method":"sampling/createMessage","params":{}}
            ]}}
        }))
        .unwrap();
        let no_answer = |_req: &MrtrRequest| -> Option<String> { None };
        let mut answer = no_answer;
        let responses = build_mrtr_responses(
            &env.input_required,
            &["file:///ws".into()],
            MrtrLimits::default(),
            &mut answer,
        );
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["key"], "r1");
        assert_eq!(responses[0]["result"]["roots"][0]["uri"], "file:///ws");
        assert_eq!(responses[1]["key"], "m1");
        assert!(responses[1]["result"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("declined"));
    }

    #[test]
    fn mrtr_envelope_rejects_oversized() {
        let big = json!({
            "_meta": { "mrtr": { "responses": [ {"key": "x", "method": "roots/list"} ] } }
        });
        // Force serialized body over the json bound with a tiny limit.
        let limits = MrtrLimits {
            max_json_bytes: 8,
            ..Default::default()
        };
        assert!(parse_mrtr_limited(&big, limits).is_none());
    }

    #[test]
    fn mrtr_with_responses_folds_into_meta() {
        let mut request = json!({"name":"t","arguments":{}});
        with_responses(&mut request, &[json!({"key":"k","result":{}})]);
        assert_eq!(request["_meta"]["mrtr"]["responses"][0]["key"], "k");
    }

    #[test]
    fn elicitation_form_parses_and_renders() {
        let body = json!({
            "mode": "form",
            "title": "Deploy target",
            "instruction": "Where should this go?",
            "schema": {
                "type": "object",
                "properties": {
                    "env": {"type": "string", "enum": ["staging","prod"]},
                    "region": {"type": "string"}
                }
            }
        });
        let e = ElicitationRequest::parse(&body).unwrap();
        assert_eq!(e.mode, "form");
        assert_eq!(e.field_names, vec!["env", "region"]);
        assert_eq!(e.options, vec!["staging", "prod"]);
        let text = e.render();
        assert!(text.contains("Deploy target"));
        assert!(text.contains("staging"));
    }

    #[test]
    fn elicitation_url_requires_url() {
        let body = json!({"mode":"url","title":"Authorize"});
        assert!(ElicitationRequest::parse(&body).is_none());
        let ok = json!({"mode":"url","title":"Authorize","url":"https://id.example/authorize"});
        assert!(ElicitationRequest::parse(&ok).is_some());
    }

    #[test]
    fn subscriptions_map_notifications() {
        assert_eq!(
            notification_for(SubscriptionFilters {
                tools_list_changed: true,
                ..Default::default()
            }),
            Some("notifications/tools/list_changed")
        );
        assert!(matches_subscription(
            "notifications/tools/list_changed",
            SubscriptionFilters {
                tools_list_changed: true,
                ..Default::default()
            }
        ));
        assert!(!matches_subscription(
            "notifications/resources/list_changed",
            SubscriptionFilters {
                tools_list_changed: true,
                ..Default::default()
            }
        ));
        assert!(notification_for(SubscriptionFilters::default()).is_none());
    }

    #[test]
    fn dcr_body_and_parse() {
        let body = dcr_registration_body(
            "remote",
            "http://localhost:3000/cb",
            &["authorization_code"],
        );
        assert_eq!(body["client_name"], "fxrs-remote");
        let resp = json!({ "client_id": "abc123" });
        assert_eq!(parse_dcr_response(&resp).unwrap(), "abc123");
        let err = json!({ "error": "invalid_redirect_uri" });
        assert!(parse_dcr_response(&err).is_err());
    }
}
