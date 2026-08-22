//! Durable subagent approval registry + persistence — faithful port of the
//! observable surface of upstream `core/subagent/approval_registry.zig` +
//! `approval_persistence.zig` + the approval model in `communication.zig`.
//!
//! Approvals are appended to the child's communication ledger (bounded at
//! [`MAX_APPROVALS`]) as durable records, and each registration appends an
//! `Approval` delivery targeted at the root so the parent turn can observe
//! pending requests. Resolution commits the decision to the durable record
//! (status, resolved timestamp, resolved revision); `decide_approval_response`
//! is the pure exact-once decision model (upstream `decideApprovalResponse`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::subagent_communication::Ledger;
use crate::subagent_control::{DeliveryPayload, MAX_APPROVALS};

/// Approval request kind (upstream `communication.ApprovalKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Tool,
    Relationship,
}

/// Durable approval lifecycle (upstream `communication.ApprovalStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    AllowedOnce,
    AllowedAlways,
    Denied,
    Cancelled,
    Stale,
    Consumed,
}

impl ApprovalStatus {
    pub fn is_settled(self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }
    pub fn is_allowed(self) -> bool {
        matches!(
            self,
            ApprovalStatus::AllowedOnce | ApprovalStatus::AllowedAlways
        )
    }
}

/// The tool decision expressed by the person (upstream
/// `types.ToolPermissionDecision`, restricted to the resolution surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Always,
    Deny,
}

impl ApprovalDecision {
    pub fn is_denied(self) -> bool {
        matches!(self, ApprovalDecision::Deny)
    }
}

/// Relationship-specific approval payload (upstream
/// `communication.RelationshipApprovalInput`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipApprovalInput {
    pub action: crate::subagent_domain::RelationshipAction,
    pub prospective_parent_id: String,
    pub operation_id: String,
}

/// Durable projection of one canonical prepared request (upstream
/// `communication.Approval`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Approval {
    pub id: String,
    pub kind: ApprovalKind,
    pub child_id: String,
    pub root_id: String,
    pub work_id: Option<String>,
    pub relationship: Option<RelationshipApprovalInput>,
    #[serde(with = "hex_fingerprint")]
    pub prepared_fingerprint: [u8; 32],
    #[serde(with = "hex_fingerprint")]
    pub identity_fingerprint: [u8; 32],
    pub label: String,
    pub explanation: Option<String>,
    pub command: Option<String>,
    pub grants: Vec<(String, String)>,
    pub status: ApprovalStatus,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
    pub resolved_revision: Option<u64>,
}

impl Default for Approval {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: ApprovalKind::Tool,
            child_id: String::new(),
            root_id: String::new(),
            work_id: None,
            relationship: None,
            prepared_fingerprint: [0u8; 32],
            identity_fingerprint: [0u8; 32],
            label: String::new(),
            explanation: None,
            command: None,
            grants: Vec::new(),
            status: ApprovalStatus::Pending,
            created_at_ms: 0,
            resolved_at_ms: None,
            resolved_revision: None,
        }
    }
}

/// Canonical registration input (upstream `communication.ApprovalInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalInput {
    pub id: String,
    pub kind: ApprovalKind,
    pub child_id: String,
    pub root_id: String,
    pub work_id: Option<String>,
    pub relationship: Option<RelationshipApprovalInput>,
    pub prepared_fingerprint: [u8; 32],
    pub label: String,
    pub explanation: Option<String>,
    pub command: Option<String>,
    pub grants: Vec<(String, String)>,
    pub created_at_ms: i64,
}

/// One durable response (upstream `communication.ApprovalResponse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub child_id: String,
    pub decision: ApprovalDecision,
    pub feedback: Option<String>,
    pub timestamp_ms: i64,
}

/// Attachment/lifecycle evidence used by the pure decision model (upstream
/// `communication.ApprovalContext`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalContext {
    pub attached: bool,
    pub child_cancelled: bool,
    pub child_closed: bool,
}

/// Pure exact-once response decision (upstream `decideApprovalResponse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecisionOutcome {
    AcceptOnce,
    AcceptAlways,
    Deny,
    Reject { reason: RejectReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Stale,
    WrongChild,
    Resolved,
    Detached,
    Cancelled,
    Closed,
    Invalid,
}

/// Decide the response outcome without committing (upstream
/// `communication.decideApprovalResponse`).
pub fn decide_approval_response(
    approval: &Approval,
    response: &ApprovalResponse,
    context: &ApprovalContext,
) -> ApprovalDecisionOutcome {
    if approval.id != response.request_id {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::Stale,
        };
    }
    if approval.child_id != response.child_id {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::WrongChild,
        };
    }
    if approval.status != ApprovalStatus::Pending {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::Resolved,
        };
    }
    if !context.attached {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::Detached,
        };
    }
    if context.child_cancelled {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::Cancelled,
        };
    }
    if context.child_closed {
        return ApprovalDecisionOutcome::Reject {
            reason: RejectReason::Closed,
        };
    }
    match response.decision {
        ApprovalDecision::Once => ApprovalDecisionOutcome::AcceptOnce,
        ApprovalDecision::Always => {
            if approval.grants.is_empty() {
                ApprovalDecisionOutcome::Reject {
                    reason: RejectReason::Invalid,
                }
            } else {
                ApprovalDecisionOutcome::AcceptAlways
            }
        }
        ApprovalDecision::Deny => ApprovalDecisionOutcome::Deny,
    }
}

mod hex_fingerprint {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(fp: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_hex(fp))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = decode_hex(&s).map_err(serde::de::Error::custom)?;
        let mut out = [0u8; 32];
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("fingerprint must be 32 bytes"));
        }
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    fn encode_hex(bytes: &[u8; 32]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for b in bytes {
            out.push(DIGITS[(b >> 4) as usize] as char);
            out.push(DIGITS[(b & 0x0f) as usize] as char);
        }
        out
    }

    fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() != 64 {
            return Err("fingerprint must be 64 hex chars".into());
        }
        let mut out = Vec::with_capacity(32);
        for pair in chars.chunks(2) {
            let hi = pair[0].to_digit(16).ok_or("bad hex")?;
            let lo = pair[1].to_digit(16).ok_or("bad hex")?;
            out.push(((hi << 4) | lo) as u8);
        }
        Ok(out)
    }
}

/// Canonical immutable request identity — SHA-256 over the same canonical
/// fields upstream hashes (upstream `approvalIdentityFingerprint`).
pub fn approval_identity_fingerprint(input: &ApprovalInput) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"fx.subagent.approval-identity.v2\x00");
    hash_string(
        &mut h,
        match input.kind {
            ApprovalKind::Tool => "tool",
            ApprovalKind::Relationship => "relationship",
        },
    );
    hash_string(&mut h, &input.id);
    hash_string(&mut h, &input.child_id);
    hash_string(&mut h, &input.root_id);
    match &input.work_id {
        Some(w) => hash_string(&mut h, w),
        None => h.update(b"\x00"),
    };
    if let Some(rel) = &input.relationship {
        h.update(b"relationship\x00");
        hash_string(
            &mut h,
            match rel.action {
                crate::subagent_domain::RelationshipAction::Attach => "attach",
                crate::subagent_domain::RelationshipAction::Detach => "detach",
                crate::subagent_domain::RelationshipAction::Reparent => "reparent",
            },
        );
        hash_string(&mut h, &rel.prospective_parent_id);
        hash_string(&mut h, &rel.operation_id);
    } else {
        h.update(b"no-relationship\x00");
    }
    h.update(input.prepared_fingerprint);
    hash_string(&mut h, &input.label);
    match &input.explanation {
        Some(e) => hash_string(&mut h, e),
        None => h.update(b"\x00"),
    };
    if let Some(cmd) = &input.command {
        h.update(b"command-projection\x00");
        hash_string(&mut h, cmd);
    }
    // Grants are hashed as a sorted set so equivalent scopes replay.
    let mut grants: Vec<&(String, String)> = input.grants.iter().collect();
    grants.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    for (tool, target) in grants {
        hash_string(&mut h, tool);
        hash_string(&mut h, target);
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&h.finalize());
    digest
}

fn hash_string(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

/// Result of registering an approval (upstream `RegisterApprovalResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterApprovalResult {
    Registered,
    Replay,
}

/// Errors from registration (upstream `communication.MutationError` subset).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("invalid approval: {0}")]
    InvalidApproval(String),
    #[error("request conflict (identity mismatch)")]
    RequestConflict,
    #[error("approval capacity exceeded")]
    CapacityExceeded,
    #[error("approval not found")]
    NotFound,
    #[error("approval already resolved")]
    AlreadyResolved,
}

/// Register one approval on a ledger: validates canonical fields, appends the
/// durable record (bounded), and delivers an `Approval` envelope to the root.
/// A second registration with the same id is a replay when the identity
/// fingerprint matches, a conflict otherwise (upstream `registerApproval`).
pub fn register_approval(
    ledger: &mut Ledger,
    input: ApprovalInput,
) -> Result<RegisterApprovalResult, ApprovalError> {
    validate_operation_id(&input.id)?;
    validate_id(&input.child_id)?;
    validate_id(&input.root_id)?;
    if let Some(w) = &input.work_id {
        validate_operation_id(w)?;
    }
    validate_content(&input.label)?;
    if let Some(e) = &input.explanation {
        validate_content(e)?;
    }
    if input.grants.len() > crate::subagent_domain::MAX_ADMISSION_ITEMS {
        return Err(ApprovalError::InvalidApproval("grants too many".into()));
    }
    if let Some(existing) = ledger.approvals.iter().find(|a| a.id == input.id) {
        let fp = approval_identity_fingerprint(&input);
        if existing.identity_fingerprint != fp {
            return Err(ApprovalError::RequestConflict);
        }
        return Ok(RegisterApprovalResult::Replay);
    }
    if ledger.approvals.len() >= MAX_APPROVALS {
        return Err(ApprovalError::CapacityExceeded);
    }
    let identity_fingerprint = approval_identity_fingerprint(&input);
    let approval = Approval {
        id: input.id.clone(),
        kind: input.kind,
        child_id: input.child_id.clone(),
        root_id: input.root_id.clone(),
        work_id: input.work_id.clone(),
        relationship: input.relationship.clone(),
        prepared_fingerprint: input.prepared_fingerprint,
        identity_fingerprint,
        label: input.label.clone(),
        explanation: input.explanation.clone(),
        command: input.command.clone(),
        grants: input.grants.clone(),
        status: ApprovalStatus::Pending,
        created_at_ms: input.created_at_ms,
        resolved_at_ms: None,
        resolved_revision: None,
    };
    // Deliver an Approval envelope to the root (visible to the parent turn
    // projector only as a delivery kind; explicit listing uses `approvals`).
    let delivery_timestamp = crate::subagent_control::now_ms();
    crate::subagent_communication::deliver(
        ledger,
        &input.child_id,
        &input.root_id,
        DeliveryPayload::Approval(input.label.clone()),
        delivery_timestamp,
    );
    // Keep the delivery correlated with the approval.
    if let Some(d) = ledger.deliveries.last_mut() {
        d.operation_id = Some(approval.id.clone());
    }
    ledger.approvals.push(approval);
    Ok(RegisterApprovalResult::Registered)
}

/// Durable resolution: decide + commit the status transition on the approval
/// record. Mirrors `approval_persistence.commitResponse` + the pure decision
/// model — no worker waiter exists in the CLI surface, so resolution is the
/// durable state change itself.
pub fn resolve_approval(
    ledger: &mut Ledger,
    response: &ApprovalResponse,
    context: &ApprovalContext,
    resolved_revision: u64,
) -> Result<ApprovalDecisionOutcome, ApprovalError> {
    let approval = ledger
        .approvals
        .iter_mut()
        .find(|a| a.id == response.request_id)
        .ok_or(ApprovalError::NotFound)?;
    let outcome = decide_approval_response(approval, response, context);
    match &outcome {
        ApprovalDecisionOutcome::AcceptOnce => {
            commit_status(
                approval,
                ApprovalStatus::AllowedOnce,
                response.timestamp_ms,
                resolved_revision,
            );
        }
        ApprovalDecisionOutcome::AcceptAlways => {
            commit_status(
                approval,
                ApprovalStatus::AllowedAlways,
                response.timestamp_ms,
                resolved_revision,
            );
        }
        ApprovalDecisionOutcome::Deny => {
            commit_status(
                approval,
                ApprovalStatus::Denied,
                response.timestamp_ms,
                resolved_revision,
            );
        }
        ApprovalDecisionOutcome::Reject { .. } => {}
    }
    Ok(outcome)
}

fn commit_status(
    approval: &mut Approval,
    status: ApprovalStatus,
    timestamp_ms: i64,
    resolved_revision: u64,
) {
    approval.status = status;
    approval.resolved_at_ms = Some(timestamp_ms);
    approval.resolved_revision = Some(resolved_revision);
}

/// Invalidate every pending approval of `child_id` (upstream
/// `approval_persistence.invalidate` + registry `invalidateChild`): pending
/// approvals become `cancelled`/`stale`. Returns the number changed.
pub fn invalidate_child_approvals(
    ledger: &mut Ledger,
    child_id: &str,
    status: ApprovalStatus,
    timestamp_ms: i64,
    resolved_revision: u64,
) -> usize {
    if status != ApprovalStatus::Cancelled && status != ApprovalStatus::Stale {
        return 0;
    }
    let mut changed = 0;
    for approval in ledger.approvals.iter_mut() {
        if approval.child_id == child_id && approval.status == ApprovalStatus::Pending {
            approval.status = status;
            approval.resolved_at_ms = Some(timestamp_ms);
            approval.resolved_revision = Some(resolved_revision);
            changed += 1;
        }
    }
    changed
}

/// Bounded, paged route projection of pending approvals (upstream
/// `approval_registry.snapshotPendingRoutes`): the durable, newest-first
/// pending routes for a child, with page offsets and revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRoute {
    pub request_id: String,
    pub child_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRouteSnapshot {
    pub revision: u64,
    pub total: usize,
    pub offset: usize,
    pub previous_offset: Option<usize>,
    pub next_offset: Option<usize>,
    pub routes: Vec<PendingRoute>,
}

pub fn snapshot_pending_routes(
    ledger: &Ledger,
    requested_offset: usize,
    limit: usize,
) -> PendingRouteSnapshot {
    let pending: Vec<&Approval> = ledger
        .approvals
        .iter()
        .filter(|a| a.status == ApprovalStatus::Pending)
        .collect();
    let total = pending.len();
    let page_offset = pending_route_page_offset(total, requested_offset, limit);
    let count = limit.min(total.saturating_sub(page_offset));
    let routes = pending[page_offset..page_offset + count]
        .iter()
        .map(|a| PendingRoute {
            request_id: a.id.clone(),
            child_id: a.child_id.clone(),
            label: a.label.clone(),
        })
        .collect();
    PendingRouteSnapshot {
        revision: ledger.generation,
        total,
        offset: page_offset,
        previous_offset: if page_offset == 0 || limit == 0 {
            None
        } else {
            Some(page_offset - page_offset.min(limit))
        },
        next_offset: if limit > 0 && page_offset + count < total {
            Some(page_offset + count)
        } else {
            None
        },
        routes,
    }
}

fn pending_route_page_offset(total: usize, requested: usize, limit: usize) -> usize {
    if total == 0 || limit == 0 {
        return 0;
    }
    let last_page = ((total - 1) / limit) * limit;
    (requested - requested % limit).min(last_page)
}

fn validate_id(s: &str) -> Result<(), ApprovalError> {
    if s.is_empty()
        || s.len() > 255
        || s.contains('/')
        || s.contains('\\')
        || s.contains('"')
        || s.contains('\'')
        || s.contains(char::is_whitespace)
    {
        return Err(ApprovalError::InvalidApproval("invalid id".into()));
    }
    Ok(())
}

fn validate_operation_id(s: &str) -> Result<(), ApprovalError> {
    validate_id(s).map_err(|_| ApprovalError::InvalidApproval("invalid operation id".into()))
}

fn validate_content(s: &str) -> Result<(), ApprovalError> {
    if s.is_empty() || s.len() > 64 * 1024 {
        return Err(ApprovalError::InvalidApproval("invalid content".into()));
    }
    Ok(())
}

/// Load a child's ledger and find a pending approval by id (CLI convenience).
pub fn find_pending<'a>(ledger: &'a Ledger, request_id: &str) -> Option<&'a Approval> {
    ledger
        .approvals
        .iter()
        .find(|a| a.id == request_id && a.status == ApprovalStatus::Pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_communication::Ledger;

    fn input(id: &str, child: &str, root: &str) -> ApprovalInput {
        ApprovalInput {
            id: id.into(),
            kind: ApprovalKind::Tool,
            child_id: child.into(),
            root_id: root.into(),
            work_id: Some("work-1".into()),
            relationship: None,
            prepared_fingerprint: [7u8; 32],
            label: "run_command".into(),
            explanation: Some("runs a shell command".into()),
            command: Some("ls -la".into()),
            grants: vec![("run_command".into(), "*".into())],
            created_at_ms: 1,
        }
    }

    #[test]
    fn register_approval_appends_record_delivery_and_fingerprint() {
        let mut ledger = Ledger::default();
        let result = register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap();
        assert_eq!(result, RegisterApprovalResult::Registered);
        assert_eq!(ledger.approvals.len(), 1);
        assert_eq!(ledger.approvals[0].status, ApprovalStatus::Pending);
        // Delivery was appended targeted at the root.
        assert_eq!(ledger.deliveries.len(), 1);
        let d = &ledger.deliveries[0];
        assert_eq!(d.source_id, "sub-1");
        assert_eq!(d.target_id, "root-1");
        assert_eq!(d.operation_id.as_deref(), Some("app-1"));
        // Identity fingerprint is canonical (deterministic).
        let fp = approval_identity_fingerprint(&input("app-1", "sub-1", "root-1"));
        assert_eq!(ledger.approvals[0].identity_fingerprint, fp);
    }

    #[test]
    fn replay_matches_identity_conflict_rejected() {
        let mut ledger = Ledger::default();
        register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap();
        assert_eq!(
            register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap(),
            RegisterApprovalResult::Replay
        );
        let mut changed = input("app-1", "sub-1", "root-1");
        changed.label = "different".into();
        assert_eq!(
            register_approval(&mut ledger, changed).unwrap_err(),
            ApprovalError::RequestConflict
        );
    }

    #[test]
    fn capacity_is_bounded() {
        let mut ledger = Ledger::default();
        for i in 0..MAX_APPROVALS {
            register_approval(&mut ledger, input(&format!("app-{i}"), "sub-1", "root-1")).unwrap();
        }
        assert_eq!(
            register_approval(&mut ledger, input("app-over", "sub-1", "root-1")).unwrap_err(),
            ApprovalError::CapacityExceeded
        );
    }

    #[test]
    fn decision_model_matches_upstream_semantics() {
        let mut ledger = Ledger::default();
        register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap();
        let approval = ledger.approvals[0].clone();
        let ctx = ApprovalContext {
            attached: true,
            child_cancelled: false,
            child_closed: false,
        };
        let ok = ApprovalResponse {
            request_id: "app-1".into(),
            child_id: "sub-1".into(),
            decision: ApprovalDecision::Once,
            feedback: None,
            timestamp_ms: 2,
        };
        assert_eq!(
            decide_approval_response(&approval, &ok, &ctx),
            ApprovalDecisionOutcome::AcceptOnce
        );
        // Wrong child rejected.
        let wrong = ApprovalResponse {
            child_id: "other".into(),
            ..ok.clone()
        };
        assert_eq!(
            decide_approval_response(&approval, &wrong, &ctx),
            ApprovalDecisionOutcome::Reject {
                reason: RejectReason::WrongChild
            }
        );
        // Always with grants accepted; without grants rejected as invalid.
        let always = ApprovalResponse {
            decision: ApprovalDecision::Always,
            ..ok.clone()
        };
        assert_eq!(
            decide_approval_response(&approval, &always, &ctx),
            ApprovalDecisionOutcome::AcceptAlways
        );
        let mut no_grants = approval.clone();
        no_grants.grants.clear();
        assert_eq!(
            decide_approval_response(&no_grants, &always, &ctx),
            ApprovalDecisionOutcome::Reject {
                reason: RejectReason::Invalid
            }
        );
        // Detached rejected.
        let detached = ApprovalContext {
            attached: false,
            ..ctx
        };
        assert_eq!(
            decide_approval_response(&approval, &ok, &detached),
            ApprovalDecisionOutcome::Reject {
                reason: RejectReason::Detached
            }
        );
    }

    #[test]
    fn resolve_commits_status_exactly_once() {
        let mut ledger = Ledger::default();
        register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap();
        let ctx = ApprovalContext {
            attached: true,
            child_cancelled: false,
            child_closed: false,
        };
        let resp = ApprovalResponse {
            request_id: "app-1".into(),
            child_id: "sub-1".into(),
            decision: ApprovalDecision::Deny,
            feedback: Some("no".into()),
            timestamp_ms: 3,
        };
        let outcome = resolve_approval(&mut ledger, &resp, &ctx, 9).unwrap();
        assert_eq!(outcome, ApprovalDecisionOutcome::Deny);
        assert_eq!(ledger.approvals[0].status, ApprovalStatus::Denied);
        assert_eq!(ledger.approvals[0].resolved_at_ms, Some(3));
        assert_eq!(ledger.approvals[0].resolved_revision, Some(9));
        // Second resolution is a no-op reject (status no longer pending).
        let again = resolve_approval(&mut ledger, &resp, &ctx, 10).unwrap();
        assert!(matches!(
            again,
            ApprovalDecisionOutcome::Reject {
                reason: RejectReason::Resolved
            }
        ));
        assert_eq!(ledger.approvals[0].resolved_revision, Some(9));
    }

    #[test]
    fn invalidation_marks_pending_and_skips_settled() {
        let mut ledger = Ledger::default();
        register_approval(&mut ledger, input("app-1", "sub-1", "root-1")).unwrap();
        let ctx = ApprovalContext {
            attached: true,
            child_cancelled: false,
            child_closed: false,
        };
        resolve_approval(
            &mut ledger,
            &ApprovalResponse {
                request_id: "app-1".into(),
                child_id: "sub-1".into(),
                decision: ApprovalDecision::Once,
                feedback: None,
                timestamp_ms: 2,
            },
            &ctx,
            5,
        )
        .unwrap();
        register_approval(&mut ledger, input("app-2", "sub-1", "root-1")).unwrap();
        let changed =
            invalidate_child_approvals(&mut ledger, "sub-1", ApprovalStatus::Cancelled, 50, 6);
        assert_eq!(changed, 1);
        assert_eq!(ledger.approvals[0].status, ApprovalStatus::AllowedOnce);
        assert_eq!(ledger.approvals[1].status, ApprovalStatus::Cancelled);
    }

    #[test]
    fn pending_routes_page_and_offset_clamp() {
        let mut ledger = Ledger::default();
        for i in 0..10 {
            register_approval(
                &mut ledger,
                input(&format!("app-{i:02}"), "sub-1", "root-1"),
            )
            .unwrap();
        }
        let first = snapshot_pending_routes(&ledger, 0, 8);
        assert_eq!(first.total, 10);
        assert_eq!(first.offset, 0);
        assert_eq!(first.next_offset, Some(8));
        assert_eq!(first.routes.len(), 8);
        assert_eq!(first.routes[0].request_id, "app-00");

        let second = snapshot_pending_routes(&ledger, 8, 8);
        assert_eq!(second.offset, 8);
        assert_eq!(second.routes.len(), 2);
        assert_eq!(second.routes[0].request_id, "app-08");
        assert_eq!(second.next_offset, None);

        // Offset beyond the last page clamps to the last page.
        let clamped = snapshot_pending_routes(&ledger, 20, 8);
        assert_eq!(clamped.offset, 8);
        assert_eq!(clamped.routes.len(), 2);
    }
}
