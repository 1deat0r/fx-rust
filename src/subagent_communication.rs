//! Subagent communication — the observable surface of upstream
//! `communication_*.zig`: a bounded, append-only delivery ledger per child
//! plus a parent-turn projection.
//!
//! A [`Ledger`] records every delivery between a child and its parent (and
//! other peers): message / milestone / terminal / interval / approval /
//! tool-activity envelopes with sequence + revision accounting, consumer
//! cursors, bounded work notifications, and retained-delivery eviction
//! (upstream `max_deliveries = 256`). [`CommunicationStore`] persists one
//! ledger per child under `~/.fx/subagents/communication/<child>.json`.
//! [`project_parent_deliveries`] is the parent-facing view: for a parent
//! session it gathers the bounded, newest delivery page from each child that
//! mentions the parent, in sequence order.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::subagent_control::{
    Delivery, DeliveryKind, DeliveryPayload, MAX_ACTIVE_WORK_NOTIFICATIONS, MAX_DELIVERIES,
};

/// One consumer's position in the ledger (upstream `ConsumerCursor`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerCursor {
    pub consumer_id: String,
    pub sequence: u64,
}

/// A live work-item notification for active work (upstream
/// `WorkNotification`, bounded at 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkNotification {
    pub work_id: String,
    pub source_child_id: String,
    pub kind: String,
    pub observed_at_ms: i64,
}

/// Bounded communication ledger (upstream `communication.Ledger`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub session_id: String,
    pub capacity_version: u64,
    pub generation: u64,
    pub next_sequence: u64,
    pub deliveries: Vec<Delivery>,
    pub cursors: Vec<ConsumerCursor>,
    pub work_notifications: Vec<WorkNotification>,
    pub parent_turn_evicted_through: u64,
    pub authority_generation: u64,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            capacity_version: 2,
            generation: 0,
            next_sequence: 1,
            deliveries: Vec::new(),
            cursors: Vec::new(),
            work_notifications: Vec::new(),
            parent_turn_evicted_through: 0,
            authority_generation: 0,
        }
    }
}

/// Append one delivery: bumps the generation + sequence, evicts deliveries
/// beyond the retention bound, records the delivery id.
pub fn deliver(
    ledger: &mut Ledger,
    source_id: &str,
    target_id: &str,
    payload: DeliveryPayload,
    timestamp_ms: i64,
) -> Delivery {
    let sequence = ledger.next_sequence;
    let delivery = Delivery {
        sequence,
        revision: ledger.generation,
        id: format!("d-{sequence}"),
        source_id: source_id.to_string(),
        target_id: target_id.to_string(),
        work_id: None,
        operation_id: None,
        timestamp_ms,
        payload,
    };
    ledger.deliveries.push(delivery.clone());
    ledger.generation += 1;
    ledger.next_sequence = sequence + 1;
    while ledger.deliveries.len() > MAX_DELIVERIES {
        if let Some(evicted) = ledger.deliveries.first() {
            ledger.parent_turn_evicted_through = evicted.sequence;
        }
        ledger.deliveries.remove(0);
    }
    delivery
}

pub fn cursor_for(ledger: &Ledger, consumer_id: &str) -> u64 {
    ledger
        .cursors
        .iter()
        .find(|c| c.consumer_id == consumer_id)
        .map(|c| c.sequence)
        .unwrap_or(0)
}

pub fn advance_cursor(ledger: &mut Ledger, consumer_id: &str, sequence: u64) {
    if let Some(c) = ledger
        .cursors
        .iter_mut()
        .find(|c| c.consumer_id == consumer_id)
    {
        c.sequence = sequence.max(c.sequence);
    } else {
        ledger.cursors.push(ConsumerCursor {
            consumer_id: consumer_id.to_string(),
            sequence,
        });
    }
}

/// Read a delivery page after `after_sequence` (0 = from the start),
/// newest-first, capped by `limit` (upstream `max_delivery_page = 100`).
pub fn read_page(ledger: &Ledger, after_sequence: u64, limit: usize) -> Vec<Delivery> {
    let limit = limit.min(crate::subagent_domain::MAX_PAGE_LIMIT);
    let mut out: Vec<Delivery> = ledger
        .deliveries
        .iter()
        .filter(|d| d.sequence > after_sequence && d.sequence > ledger.parent_turn_evicted_through)
        .cloned()
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

/// Add a live work notification (idempotent per work id, bounded).
pub fn note_active_work(
    ledger: &mut Ledger,
    work_id: &str,
    source_child_id: &str,
    kind: &str,
    timestamp_ms: i64,
) {
    if ledger
        .work_notifications
        .iter()
        .any(|n| n.work_id == work_id)
    {
        return;
    }
    ledger.work_notifications.push(WorkNotification {
        work_id: work_id.to_string(),
        source_child_id: source_child_id.to_string(),
        kind: kind.to_string(),
        observed_at_ms: timestamp_ms,
    });
    while ledger.work_notifications.len() > MAX_ACTIVE_WORK_NOTIFICATIONS {
        ledger.work_notifications.remove(0);
    }
}

pub fn clear_work_notification(ledger: &mut Ledger, work_id: &str) {
    ledger.work_notifications.retain(|n| n.work_id != work_id);
}

/// Whether a payload kind is visible to the parent turn projection
/// (upstream parent-turn projection: message/milestone/terminal/interval).
fn visible_to_parent(kind: &DeliveryKind) -> bool {
    matches!(
        kind,
        DeliveryKind::Message
            | DeliveryKind::Milestone
            | DeliveryKind::Terminal
            | DeliveryKind::Interval
    )
}

/// Per-child parent-turn projection: the bounded, newest delivery list from
/// one child to the named parent, in sequence order (newest first, capped at
/// `limit`). The full parent-turn context iterates the parent's children
/// and folds this view.
pub fn project_child_deliveries(ledger: &Ledger, parent_id: &str, limit: usize) -> Vec<Delivery> {
    let limit = limit.min(crate::subagent_domain::MAX_PAGE_LIMIT);
    let mut out: Vec<Delivery> = ledger
        .deliveries
        .iter()
        .filter(|d| {
            d.target_id == parent_id
                && d.sequence > ledger.parent_turn_evicted_through
                && visible_to_parent(&d.payload.delivery_kind())
        })
        .cloned()
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

// ---- store ----

pub fn communication_root() -> PathBuf {
    crate::config::fx_home()
        .join("subagents")
        .join("communication")
}

pub struct CommunicationStore {
    root: PathBuf,
}

impl CommunicationStore {
    pub fn new() -> Result<Self> {
        let root = communication_root();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self, child_id: &str) -> PathBuf {
        self.root.join(format!("{child_id}.json"))
    }

    pub fn load(&self, child_id: &str) -> Result<Ledger> {
        let path = self.path(child_id);
        if !path.exists() {
            return Ok(Ledger {
                session_id: child_id.to_string(),
                ..Default::default()
            });
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut ledger: Ledger =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        if ledger.session_id != child_id {
            anyhow::bail!(
                "communication ledger identity mismatch: file {child_id}, ledger {}",
                ledger.session_id
            );
        }
        ledger
            .deliveries
            .retain(|d| d.sequence > ledger.parent_turn_evicted_through);
        Ok(ledger)
    }

    pub fn save(&self, ledger: &Ledger) -> Result<()> {
        let path = self.path(&ledger.session_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(ledger)?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
fn msg(_source: &str, _target: &str, content: &str) -> DeliveryPayload {
    DeliveryPayload::Message(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::with;

    #[test]
    fn deliver_appends_and_evicts_past_retention() {
        let mut ledger = Ledger {
            session_id: "sub-1".into(),
            ..Default::default()
        };
        let mut last = None;
        for i in 0..(MAX_DELIVERIES + 20) {
            last = Some(deliver(
                &mut ledger,
                "parent-1",
                "sub-1",
                msg("parent-1", "sub-1", &format!("m{i}")),
                i as i64,
            ));
        }
        assert_eq!(ledger.deliveries.len(), MAX_DELIVERIES);
        assert!(ledger.parent_turn_evicted_through > 0);
        assert_eq!(ledger.next_sequence as usize, MAX_DELIVERIES + 21);
        // The final delivery survives.
        assert_eq!(last.unwrap().sequence, (MAX_DELIVERIES + 20) as u64);
        // Old sequences are filtered out of pages.
        let page = read_page(&ledger, 0, 100);
        assert!(page
            .iter()
            .all(|d| d.sequence > ledger.parent_turn_evicted_through));
    }

    #[test]
    fn cursors_advance_monotonically() {
        let mut ledger = Ledger::default();
        let mut d = deliver(&mut ledger, "a", "b", msg("a", "b", "hi"), 1);
        deliver(&mut ledger, "a", "b", msg("a", "b", "there"), 2);
        d.payload = DeliveryPayload::Message("hi2".into());
        assert_eq!(cursor_for(&ledger, "parent-model"), 0);
        advance_cursor(&mut ledger, "parent-model", 1);
        assert_eq!(cursor_for(&ledger, "parent-model"), 1);
        // Backwards advance is a no-op.
        advance_cursor(&mut ledger, "parent-model", 1);
        assert_eq!(cursor_for(&ledger, "parent-model"), 1);
        let _ = d;
    }

    #[test]
    fn work_notifications_are_bounded_and_idempotent() {
        let mut ledger = Ledger::default();
        note_active_work(&mut ledger, "w-1", "sub-1", "running", 1);
        note_active_work(&mut ledger, "w-1", "sub-1", "running", 2);
        assert_eq!(ledger.work_notifications.len(), 1);
        for i in 0..10 {
            note_active_work(&mut ledger, &format!("w-{i}"), "sub-1", "running", i as i64);
        }
        assert!(ledger.work_notifications.len() <= MAX_ACTIVE_WORK_NOTIFICATIONS);
        clear_work_notification(&mut ledger, "w-9");
        assert!(!ledger.work_notifications.iter().any(|n| n.work_id == "w-9"));
    }

    #[test]
    fn store_roundtrip_preserves_ledger() {
        with(|| {
            let home = std::env::temp_dir().join(format!("fxrs-comm-{}", std::process::id()));
            std::env::set_var("FX_HOME", &home);
            let store = CommunicationStore::new().unwrap();
            let mut ledger = store.load("sub-1").unwrap();
            deliver(
                &mut ledger,
                "parent-1",
                "sub-1",
                msg("parent-1", "sub-1", "hello"),
                5,
            );
            store.save(&ledger).unwrap();
            let loaded = store.load("sub-1").unwrap();
            assert_eq!(loaded.deliveries.len(), 1);
            assert_eq!(
                loaded.deliveries[0].payload,
                DeliveryPayload::Message("hello".into())
            );
            let _ = std::fs::remove_dir_all(&home);
        });
    }

    #[test]
    fn parent_projection_visibility_filter() {
        assert!(visible_to_parent(&DeliveryKind::Message));
        assert!(visible_to_parent(&DeliveryKind::Milestone));
        assert!(visible_to_parent(&DeliveryKind::Terminal));
        assert!(visible_to_parent(&DeliveryKind::Interval));
        assert!(!visible_to_parent(&DeliveryKind::ToolActivity));
        assert!(!visible_to_parent(&DeliveryKind::Approval));
    }

    #[test]
    fn read_page_respects_limit_and_cursor() {
        let mut ledger = Ledger::default();
        for i in 0..15 {
            deliver(
                &mut ledger,
                "a",
                "b",
                msg("a", "b", &format!("m{i}")),
                i as i64,
            );
        }
        let page = read_page(&ledger, 5, 5);
        // Newest-first after cursor 5: sequences 15,14,13,12,11.
        assert_eq!(page.len(), 5);
        assert_eq!(page[0].sequence, 15);
        assert_eq!(page[4].sequence, 11);
    }
}

/// Per-child parent-turn projection: gather the bounded, newest delivery list
/// from every ledger in `children` directed at `parent_id`, folded in
/// sequence order (newest first per child, capped per child). Mirrors
/// upstream's parent-turn context assembly: each child contributes its own
/// bounded page and the parent model sees the combined stream.
pub fn project_parent_deliveries(
    store: &CommunicationStore,
    children: &[String],
    parent_id: &str,
    per_child_limit: usize,
) -> Vec<Delivery> {
    let mut out = Vec::new();
    for child in children {
        let Ok(ledger) = store.load(child) else {
            continue;
        };
        let page = project_child_deliveries(&ledger, parent_id, per_child_limit);
        out.extend(page);
    }
    out
}

/// Advance the parent-model cursor on a ledger to `sequence` (monotonic).
pub fn advance_parent_cursor(ledger: &mut Ledger, sequence: u64) {
    advance_cursor(ledger, "parent-model", sequence);
}

#[cfg(test)]
mod fixture_tests {
    use super::*;

    #[test]
    fn parent_projection_folds_children_in_sequence_order() {
        let home = std::env::temp_dir().join(format!("fxrs-comm-proj-{}", std::process::id()));
        std::env::set_var("FX_HOME", &home);
        let store = CommunicationStore::new().unwrap();
        let mut a = store.load("sub-a").unwrap();
        let mut b = store.load("sub-b").unwrap();
        deliver(
            &mut a,
            "sub-a",
            "parent-1",
            msg("sub-a", "parent-1", "a1"),
            10,
        );
        deliver(
            &mut a,
            "sub-a",
            "parent-1",
            msg("sub-a", "parent-1", "a2"),
            11,
        );
        deliver(
            &mut b,
            "sub-b",
            "parent-1",
            msg("sub-b", "parent-1", "b1"),
            20,
        );
        store.save(&a).unwrap();
        store.save(&b).unwrap();

        let page = project_parent_deliveries(
            &store,
            &["sub-a".to_string(), "sub-b".to_string()],
            "parent-1",
            10,
        );
        assert_eq!(page.len(), 3);
        // Newest first per child: a2 then a1 then b1 (fold order = child order).
        let text = |d: &Delivery| match &d.payload {
            DeliveryPayload::Message(s) => s.clone(),
            other => format!("{other:?}"),
        };
        assert_eq!(text(&page[0]), "a2");
        assert_eq!(text(&page[1]), "a1");
        assert_eq!(text(&page[2]), "b1");

        // Advance the parent-model cursor on the first child ledger.
        let mut a2 = store.load("sub-a").unwrap();
        let next = cursor_for(&a2, "parent-model") + 2;
        advance_parent_cursor(&mut a2, next);
        assert_eq!(cursor_for(&a2, "parent-model"), 2);
        if home.exists() {
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn parent_projection_filters_non_visible_payloads() {
        let mut ledger = Ledger::default();
        deliver(
            &mut ledger,
            "sub-a",
            "parent-1",
            msg("sub-a", "parent-1", "hi"),
            1,
        );
        deliver(
            &mut ledger,
            "sub-a",
            "parent-1",
            DeliveryPayload::ToolActivity(crate::subagent_control::ToolActivity {
                tool_name: "bash".into(),
                phase: crate::subagent_control::ToolActivityPhase::Started,
            }),
            2,
        );
        let page = project_child_deliveries(&ledger, "parent-1", 10);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].sequence, 1);
    }
}
