//! Subagent authority — faithful port of the admission layer of upstream
//! `core/subagent/authority.zig` + `domain.captureAdmission`.
//!
//! A child subagent never inherits the parent's live permission state
//! blindly: when it is admitted, the manager captures an immutable
//! [`AdmissionSnapshot`] of the authority values the child will run under
//! (parent + source identity, model, permission mode, allowed tool names,
//! permission rules, per-session grants, integration names, authority
//! generation). [`capture_admission`] validates every value with the same
//! constraints as upstream (ids, bounded text, item caps) and returns the
//! snapshot.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::permissions::PermissionMode;
use crate::subagent_domain::{
    validate_id, MAX_ADMISSION_ITEMS, MAX_ADMISSION_ITEM_BYTES, MAX_MODEL_BYTES,
};

/// Immutable authority + routing values captured for one admitted child
/// turn (upstream `domain.AdmissionSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdmissionSnapshot {
    pub parent_id: String,
    pub source_id: String,
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    pub tool_names: Vec<String>,
    pub rules: Vec<(String, String)>,
    pub grants: Vec<(String, String)>,
    pub integration_names: Vec<String>,
    pub authority_generation: u64,
}

impl Default for AdmissionSnapshot {
    fn default() -> Self {
        Self {
            parent_id: String::new(),
            source_id: String::new(),
            model: String::new(),
            provider: "gateway".into(),
            effort: None,
            permission_mode: PermissionMode::Auto,
            tool_names: Vec::new(),
            rules: Vec::new(),
            grants: Vec::new(),
            integration_names: Vec::new(),
            authority_generation: 0,
        }
    }
}

/// Admission input (upstream `domain.AdmissionInput`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdmissionInput {
    pub parent_id: String,
    pub source_id: String,
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    pub tool_names: Vec<String>,
    pub rules: Vec<(String, String)>,
    pub grants: Vec<(String, String)>,
    pub integration_names: Vec<String>,
    pub authority_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    InvalidModel,
    TooManyAdmissionItems,
    InvalidAdmissionItem,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for AdmissionError {}

fn validate_id_or(input: &str, err: AdmissionError) -> Result<(), AdmissionError> {
    validate_id(input).map_err(|_| err)
}

fn validate_admission_text(value: &str) -> Result<(), AdmissionError> {
    if value.is_empty() || value.len() > MAX_ADMISSION_ITEM_BYTES {
        return Err(AdmissionError::InvalidAdmissionItem);
    }
    Ok(())
}

/// Validates and owns an immutable child-turn admission snapshot
/// (upstream `domain.captureAdmission`).
pub fn capture_admission(input: &AdmissionInput) -> Result<AdmissionSnapshot, AdmissionError> {
    validate_id_or(&input.parent_id, AdmissionError::InvalidAdmissionItem)?;
    validate_id_or(&input.source_id, AdmissionError::InvalidAdmissionItem)?;
    if input.model.is_empty() || input.model.len() > MAX_MODEL_BYTES {
        return Err(AdmissionError::InvalidModel);
    }
    if input.tool_names.len() > MAX_ADMISSION_ITEMS
        || input.rules.len() > MAX_ADMISSION_ITEMS
        || input.grants.len() > MAX_ADMISSION_ITEMS
        || input.integration_names.len() > MAX_ADMISSION_ITEMS
    {
        return Err(AdmissionError::TooManyAdmissionItems);
    }
    for name in &input.tool_names {
        validate_admission_text(name)?;
    }
    for name in &input.integration_names {
        validate_admission_text(name)?;
    }
    for (key, value) in &input.rules {
        validate_admission_text(key)?;
        validate_admission_text(value)?;
    }
    for (tool, target) in &input.grants {
        validate_admission_text(tool)?;
        validate_admission_text(target)?;
    }
    Ok(AdmissionSnapshot {
        parent_id: input.parent_id.clone(),
        source_id: input.source_id.clone(),
        model: input.model.clone(),
        provider: if input.provider.is_empty() {
            "gateway".into()
        } else {
            input.provider.clone()
        },
        effort: input.effort.clone(),
        permission_mode: input.permission_mode,
        tool_names: input.tool_names.clone(),
        rules: input.rules.clone(),
        grants: input.grants.clone(),
        integration_names: input.integration_names.clone(),
        authority_generation: input.authority_generation,
    })
}

/// Resume admission — upstream `domain.resumeAdmission`. When a child is
/// resumed (run again after interruption) the manager re-derives authority
/// from the record's *live* configuration instead of trusting a stale
/// snapshot: model, permission mode and any configured tool filter are
/// authoritative. A pre-existing tool filter is preserved when the live
/// configuration carries none (the parent's restriction stays in force
/// across resume).
pub fn resume_admission(
    record: &crate::subagent_control::SubagentRecord,
    parent_id: &str,
    source_id: &str,
) -> Result<AdmissionSnapshot, AdmissionError> {
    // An unset live model means "inherit the parent's model"; capture still
    // wants a non-empty value, so fall back to the prior snapshot's model
    // and otherwise to the explicit inherit marker `default` (the executor
    // only consumes permission mode + tool names from the snapshot).
    let model = record
        .configuration
        .model
        .clone()
        .or_else(|| {
            record
                .admission
                .as_ref()
                .map(|a| a.model.clone())
                .filter(|m| !m.is_empty())
        })
        .unwrap_or_else(|| "default".into());
    let mut input = AdmissionInput {
        parent_id: parent_id.to_string(),
        source_id: source_id.to_string(),
        model,
        permission_mode: PermissionMode::parse(&record.configuration.permission_mode)
            .unwrap_or(PermissionMode::Auto),
        tool_names: Vec::new(),
        ..Default::default()
    };
    // The parent's tool restriction persists across resume unless the live
    // configuration carries a model change that invalidates it — we keep the
    // prior snapshot's filter (upstream preserves admission tool names).
    if let Some(prior) = &record.admission {
        input.tool_names = prior.tool_names.clone();
    }
    let mut snapshot = capture_admission(&input)?;
    snapshot.authority_generation = record
        .admission
        .as_ref()
        .map(|a| a.authority_generation.saturating_add(1))
        .unwrap_or(0);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> AdmissionInput {
        AdmissionInput {
            parent_id: "parent-1".into(),
            source_id: "call-1".into(),
            model: "claude-sonnet-4-6".into(),
            permission_mode: PermissionMode::Auto,
            tool_names: vec!["read_file".into(), "run_command".into()],
            ..Default::default()
        }
    }

    #[test]
    fn capture_accepts_valid_admission() {
        let snapshot = capture_admission(&input()).unwrap();
        assert_eq!(snapshot.parent_id, "parent-1");
        assert_eq!(snapshot.tool_names.len(), 2);
        assert_eq!(snapshot.provider, "gateway");
    }

    #[test]
    fn capture_rejects_invalid_identity() {
        let bad = AdmissionInput {
            parent_id: "bad/id".into(),
            ..input()
        };
        assert_eq!(
            capture_admission(&bad).unwrap_err(),
            AdmissionError::InvalidAdmissionItem
        );
    }

    #[test]
    fn capture_rejects_too_many_items() {
        let many: Vec<String> = (0..MAX_ADMISSION_ITEMS + 1)
            .map(|i| format!("t{i}"))
            .collect();
        let bad = AdmissionInput {
            tool_names: many,
            ..input()
        };
        assert_eq!(
            capture_admission(&bad).unwrap_err(),
            AdmissionError::TooManyAdmissionItems
        );
    }

    #[test]
    fn resume_recaptures_from_live_config_and_bumps_generation() {
        let mut record = crate::subagent_control::SubagentRecord::default();
        record.configuration.model = Some("claude-haiku-4-5".into());
        record.configuration.permission_mode = "yolo".into();
        record.admission = Some(capture_admission(&input()).unwrap());
        let resumed = resume_admission(&record, "parent-1", "call-9").unwrap();
        assert_eq!(resumed.model, "claude-haiku-4-5");
        assert_eq!(resumed.permission_mode, PermissionMode::Yolo);
        assert_eq!(resumed.authority_generation, 1);
        // The prior tool restriction is preserved across resume.
        let expected: Vec<String> = vec!["read_file".into(), "run_command".into()];
        assert_eq!(resumed.tool_names, expected);
    }

    #[test]
    fn resume_without_prior_admission_starts_generation_zero() {
        let mut record = crate::subagent_control::SubagentRecord::default();
        record.configuration.permission_mode = "auto".into();
        let resumed = resume_admission(&record, "parent-1", "call-1").unwrap();
        assert_eq!(resumed.authority_generation, 0);
        assert!(resumed.tool_names.is_empty());
    }

    #[test]
    fn capture_rejects_invalid_model() {
        let bad = AdmissionInput {
            model: String::new(),
            ..input()
        };
        assert_eq!(
            capture_admission(&bad).unwrap_err(),
            AdmissionError::InvalidModel
        );
    }
}
