//! ACP message/event writers — faithful port of upstream `src/acp/types.zig`
//! event shapes (agent_message_chunk, tool_call, session_update wrapper,
//! initialize response, prompt response).

use serde_json::{json, Value};

#[allow(non_upper_case_globals)]
pub const protocol_version: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxOutputTokens,
    MaxModelTurns,
    Refused,
    Cancelled,
}

impl StopReason {
    pub fn json_string(self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxOutputTokens => "max_output_tokens",
            StopReason::MaxModelTurns => "max_model_turns",
            StopReason::Refused => "refused",
            StopReason::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

impl ToolCallKind {
    pub fn json_string(self) -> &'static str {
        match self {
            ToolCallKind::Read => "read",
            ToolCallKind::Edit => "edit",
            ToolCallKind::Delete => "delete",
            ToolCallKind::Move => "move",
            ToolCallKind::Search => "search",
            ToolCallKind::Execute => "execute",
            ToolCallKind::Think => "think",
            ToolCallKind::Fetch => "fetch",
            ToolCallKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolCallStatus {
    pub fn json_string(self) -> &'static str {
        match self {
            ToolCallStatus::Pending => "pending",
            ToolCallStatus::InProgress => "in_progress",
            ToolCallStatus::Completed => "completed",
            ToolCallStatus::Failed => "failed",
        }
    }
}

pub fn session_update(session_id: &str, update: Value) -> Value {
    json!({ "sessionId": session_id, "update": update })
}

pub fn agent_message_chunk(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    })
}

pub fn user_message_chunk(text: &str) -> Value {
    json!({
        "sessionUpdate": "user_message_chunk",
        "content": { "type": "text", "text": text }
    })
}

pub fn tool_call(
    tool_call_id: &str,
    title: &str,
    kind: ToolCallKind,
    status: ToolCallStatus,
) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": tool_call_id,
        "title": title,
        "kind": kind.json_string(),
        "status": status.json_string()
    })
}

pub fn tool_call_update(
    tool_call_id: &str,
    status: ToolCallStatus,
    content_text: Option<String>,
) -> Value {
    let mut v = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": status.json_string()
    });
    if let Some(text) = content_text {
        v["content"] = json!([{ "type": "content", "content": { "type": "text", "text": text } }]);
    }
    v
}

pub fn available_commands_update(commands_json: Value) -> Value {
    json!({
        "sessionUpdate": "available_commands_update",
        "availableCommands": commands_json
    })
}

pub fn initialize_response(app_version: &str) -> Value {
    json!({
        "protocolVersion": protocol_version,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
            "mcpCapabilities": { "http": true, "sse": true },
            "sessionCapabilities": { "list": {}, "resume": {}, "close": {} }
        },
        "agentInfo": { "name": "fx", "title": "fx", "version": app_version },
        "authMethods": []
    })
}

pub fn prompt_response(reason: StopReason) -> Value {
    json!({ "stopReason": reason.json_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_shapes_match_upstream() {
        assert_eq!(
            agent_message_chunk("Hello world"),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello world"}})
        );
        let tc = tool_call(
            "call_001",
            "Reading file",
            ToolCallKind::Read,
            ToolCallStatus::Pending,
        );
        assert_eq!(tc["toolCallId"], "call_001");
        assert_eq!(tc["kind"], "read");
        let upd = tool_call_update("call_002", ToolCallStatus::Completed, Some("done".into()));
        assert_eq!(upd["sessionUpdate"], "tool_call_update");
        assert_eq!(upd["status"], "completed");
    }

    #[test]
    fn initialize_shape() {
        let init = initialize_response("0.1.0");
        assert_eq!(init["protocolVersion"], 1);
        assert_eq!(init["agentInfo"]["name"], "fx");
        assert_eq!(init["agentCapabilities"]["loadSession"], true);
        assert_eq!(init["agentCapabilities"]["mcpCapabilities"]["http"], true);
    }

    #[test]
    fn enums_json_values() {
        assert_eq!(StopReason::EndTurn.json_string(), "end_turn");
        assert_eq!(ToolCallKind::Execute.json_string(), "execute");
        assert_eq!(ToolCallStatus::InProgress.json_string(), "in_progress");
    }
}
