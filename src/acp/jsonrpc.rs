//! ACP JSON-RPC framing (upstream `src/acp/jsonrpc.zig`): newline-delimited
//! JSON-RPC 2.0 requests/responses/notifications with the upstream error
//! code table. Kept dependency-free (serde handles encoding; this module
//! owns the wire contract + validation).

use serde_json::{json, Value};

pub struct ErrorCode;
impl ErrorCode {
    pub const REQUEST_FRAME_TOO_LARGE: i64 = -32000;
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestId {
    Integer(i64),
    String(String),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: Option<RequestId>,
    pub method: Option<String>,
    pub params: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl Message {
    pub fn is_response(&self) -> bool {
        self.method.is_none()
            && self.id.is_some()
            && (self.result.is_some() || self.error.is_some())
    }
}

/// Parse one newline-delimited frame into a `Message`.
pub fn parse_message(line: &str) -> Result<Message, (i64, String)> {
    let parsed: Value = serde_json::from_str(line)
        .map_err(|e| (ErrorCode::PARSE_ERROR, format!("parse error: {e}")))?;
    let Some(obj) = parsed.as_object() else {
        return Err((
            ErrorCode::INVALID_REQUEST,
            "invalid request: root must be an object".into(),
        ));
    };
    // A response? ({"id":..,"result":..} / {"id":..,"error":..})
    if obj.contains_key("result") || obj.contains_key("error") {
        let id = parse_id(obj.get("id"));
        return Ok(Message {
            id,
            method: None,
            params: None,
            result: obj.get("result").cloned(),
            error: obj.get("error").cloned(),
        });
    }
    let Some(method) = obj.get("method").and_then(|v| v.as_str()) else {
        return Err((
            ErrorCode::INVALID_REQUEST,
            "invalid request: missing method".into(),
        ));
    };
    Ok(Message {
        id: parse_id(obj.get("id")),
        method: Some(method.to_string()),
        params: obj.get("params").cloned(),
        result: None,
        error: None,
    })
}

fn parse_id(v: Option<&Value>) -> Option<RequestId> {
    match v {
        Some(Value::Number(n)) => n.as_i64().map(RequestId::Integer),
        Some(Value::String(s)) => Some(RequestId::String(s.clone())),
        Some(Value::Null) | None => Some(RequestId::Null),
        _ => None,
    }
}

/// Render a request id back to JSON.
pub fn id_to_value(id: &Option<RequestId>) -> Value {
    match id {
        Some(RequestId::Integer(n)) => json!(n),
        Some(RequestId::String(s)) => json!(s),
        Some(RequestId::Null) | None => Value::Null,
    }
}

/// Build a success response envelope.
pub fn response(id: &Option<RequestId>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id_to_value(id), "result": result })
}

/// Build an error response envelope (code/message/data).
pub fn error(id: &Option<RequestId>, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if let Some(d) = data {
        err["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id_to_value(id), "error": err })
}

/// Build a server → client notification (no id).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_and_notification() {
        let req =
            parse_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
        assert_eq!(req.id, Some(RequestId::Integer(1)));
        assert_eq!(req.method.as_deref(), Some("initialize"));
        assert!(!req.is_response());

        let notif =
            parse_message(r#"{"jsonrpc":"2.0","method":"session/notify","params":{}}"#).unwrap();
        assert_eq!(notif.method.as_deref(), Some("session/notify"));
    }

    #[test]
    fn rejects_bad_json_and_missing_method() {
        assert_eq!(
            parse_message("not json").unwrap_err().0,
            ErrorCode::PARSE_ERROR
        );
        assert_eq!(
            parse_message(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err().0,
            ErrorCode::INVALID_REQUEST
        );
    }

    #[test]
    fn detects_responses() {
        let resp = parse_message(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert!(resp.is_response());
        assert_eq!(resp.result, Some(json!({"ok":true})));
    }

    #[test]
    fn envelopes_roundtrip() {
        let resp = response(&Some(RequestId::String("s1".into())), json!({"a": 1}));
        assert_eq!(resp["id"], "s1");
        assert!(resp["result"]["a"] == 1);
        let err = error(&None, ErrorCode::METHOD_NOT_FOUND, "no method", None);
        assert_eq!(err["error"]["code"], ErrorCode::METHOD_NOT_FOUND);
    }
}
