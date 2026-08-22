//! Subagent operation ids + result envelopes — faithful port of upstream
//! `core/subagent/tool_result.zig`.
//!
//! * [`operation_id`] binds an untrusted invocation id to an fx-owned
//!   operation id (`call_<sha256>` when the raw id is not a valid operation
//!   id, otherwise the id itself).
//! * [`bound_operation_id`] maps an invocation id to a durable,
//!   manager-issued identity `fxop:2:<m|h>:<epoch>:<sha256>` carrying the
//!   issuance source + epoch (only the digest is retained).
//! * [`parse_bound_operation_id`] / [`bound_operation_matches_invocation`]
//!   validate and compare those identities.
//! * [`Outcome`] / [`outcome_json`] / [`failure_json`] produce the
//!   structured ok/operation_id/child_id/status/error_code/retryable/
//!   requested/cursor envelope the subagent tool returns.

use serde::{Deserialize, Serialize};

use crate::subagent_domain::{
    BoundOperationIdentity, OperationIdentityAuthority, OperationIdentitySource,
};

pub const MAX_ERROR_CODE_BYTES: usize = 64;

/// `call_<sha256hex>` for an invalid raw invocation id, else the id itself.
pub fn operation_id(invocation_id: &str) -> String {
    if crate::subagent_domain::validate_operation_id(invocation_id).is_ok() {
        return invocation_id.to_string();
    }
    let digest = sha256_hex(invocation_id.as_bytes());
    format!("call_{digest}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn operation_source_tag(source: OperationIdentitySource) -> &'static str {
    match source {
        OperationIdentitySource::Model => "m",
        OperationIdentitySource::Human => "h",
    }
}

/// Binds an untrusted invocation identifier to fx-owned issuance metadata
/// (`fxop:2:<m|h>:<epoch>:<sha256hex>`).
pub fn bound_operation_id(
    invocation_id: &str,
    source: OperationIdentitySource,
    epoch: u64,
) -> String {
    let digest = sha256_hex(invocation_id.as_bytes());
    format!("fxop:2:{}:{epoch}:{digest}", operation_source_tag(source))
}

/// Parse a bound operation id (upstream `parseBoundOperationId`).
pub fn parse_bound_operation_id(value: &str) -> Option<BoundOperationIdentity> {
    let mut parts = value.split(':');
    if parts.next()? != "fxop" {
        return None;
    }
    let second = parts.next()?;
    let manager_issued = second == "2";
    let source_raw = if manager_issued {
        parts.next()?
    } else {
        second
    };
    let epoch_raw = parts.next()?;
    let digest = parts.next()?;
    if parts.next().is_some() || digest.len() != 64 {
        return None;
    }
    if epoch_raw.is_empty() || (epoch_raw.len() > 1 && epoch_raw.starts_with('0')) {
        return None;
    }
    // Decode the digest hex to 32 bytes and re-encode lowercase; upstream
    // compares the canonical re-hedged hex to the digest string.
    let decoded: Vec<u8> = (0..digest.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digest[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    if decoded.len() != 32 {
        return None;
    }
    let canonical: String = decoded.iter().map(|b| format!("{b:02x}")).collect();
    if canonical != digest.to_ascii_lowercase() {
        return None;
    }
    let source = match source_raw {
        "m" => OperationIdentitySource::Model,
        "h" => OperationIdentitySource::Human,
        _ => return None,
    };
    let epoch: u64 = epoch_raw.parse().ok()?;
    Some(BoundOperationIdentity {
        source,
        epoch,
        authority: if manager_issued {
            OperationIdentityAuthority::Manager
        } else {
            OperationIdentityAuthority::ProcessLocal
        },
    })
}

/// Whether `operation_id` matches an invocation id under the given source.
pub fn bound_operation_matches_invocation(
    operation_id_str: &str,
    invocation_id: &str,
    source: OperationIdentitySource,
) -> bool {
    let Some(identity) = parse_bound_operation_id(operation_id_str) else {
        return false;
    };
    if identity.authority != OperationIdentityAuthority::Manager || identity.source != source {
        return false;
    }
    let Some(digest_start) = operation_id_str.rfind(':') else {
        return false;
    };
    let expected = sha256_hex(invocation_id.as_bytes());
    operation_id_str[digest_start + 1..] == expected
}

/// Structured subagent outcome (upstream `Outcome`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Outcome {
    pub ok: bool,
    pub operation_id: String,
    pub child_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub requested_json: String,
    pub cursor: Option<String>,
}

pub fn outcome_json(outcome: &Outcome) -> serde_json::Value {
    serde_json::json!({
        "ok": outcome.ok,
        "operation_id": outcome.operation_id,
        "child_id": outcome.child_id,
        "status": outcome.status,
        "error_code": outcome.error_code,
        "retryable": outcome.retryable,
        "requested": serde_json::from_str::<serde_json::Value>(&outcome.requested_json).unwrap_or(serde_json::Value::Null),
        "cursor": outcome.cursor,
    })
}

/// Structured failure envelope (upstream `failureAlloc`).
pub fn failure_json(
    invocation_id: &str,
    child_id: Option<&str>,
    status: &str,
    error_code: &str,
    retryable: bool,
    cursor: Option<&str>,
) -> serde_json::Value {
    let op_id = operation_id(invocation_id);
    outcome_json(&Outcome {
        ok: false,
        operation_id: op_id,
        child_id: child_id.map(|s| s.to_string()),
        status: status.to_string(),
        error_code: Some(error_code[..error_code.len().min(MAX_ERROR_CODE_BYTES)].to_string()),
        retryable,
        requested_json: "null".into(),
        cursor: cursor.map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_valid_ids_are_used_verbatim() {
        assert_eq!(operation_id("op-123"), "op-123");
        assert!(operation_id("not a valid id").starts_with("call_"));
        assert_eq!(operation_id("not a valid id").len(), 5 + 64);
    }

    #[test]
    fn bound_ids_roundtrip_and_carry_source_epoch_authority() {
        let bound = bound_operation_id("inv-1", OperationIdentitySource::Model, 7);
        assert!(bound.starts_with("fxop:2:m:7:"));
        let parsed = parse_bound_operation_id(&bound).unwrap();
        assert_eq!(parsed.source, OperationIdentitySource::Model);
        assert_eq!(parsed.epoch, 7);
        assert_eq!(parsed.authority, OperationIdentityAuthority::Manager);
        assert!(bound_operation_matches_invocation(
            &bound,
            "inv-1",
            OperationIdentitySource::Model
        ));
        assert!(!bound_operation_matches_invocation(
            &bound,
            "inv-2",
            OperationIdentitySource::Model
        ));
        assert!(!bound_operation_matches_invocation(
            &bound,
            "inv-1",
            OperationIdentitySource::Human
        ));
    }

    #[test]
    fn malformed_bound_ids_are_rejected() {
        assert!(parse_bound_operation_id("fxop:1:m:7:deadbeef").is_none());
        assert!(parse_bound_operation_id("nope").is_none());
        assert!(parse_bound_operation_id("fxop:2:x:7:abc").is_none());
        assert!(parse_bound_operation_id("fxop:2:m:007:abc").is_none());
        assert!(parse_bound_operation_id("fxop:2:m:7:abc").is_none());
    }

    #[test]
    fn failure_envelope_shapes_ok() {
        let v = failure_json("inv", Some("sub-1"), "failed", "boom", true, None);
        assert_eq!(v["ok"], false);
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error_code"], "boom");
        assert_eq!(v["retryable"], true);
        assert_eq!(v["child_id"], "sub-1");
        assert_eq!(v["requested"], serde_json::Value::Null);
    }
}
