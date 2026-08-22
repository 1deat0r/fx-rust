//! Subagent relationship helpers — the observable surface of upstream
//! `relationship_index.zig` (parent/child lookup + subtree traversal) built
//! as a derived view over the control store. The paged on-disk index is a
//! storage-engineering concern; this module provides the same queries the
//! manager uses, with cycle protection.
//!
//! * [`parent_of`] — the immediate parent of a child (from its record).
//! * [`children_of`] — all direct children of a parent id.
//! * [`roots`] — children with no parent.
//! * [`subtree`] — a child plus every descendant, breadth-first with a
//!   visited set so a corrupt/cyclic store cannot hang the CLI.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::subagent_control::{SubagentRecord, SubagentStore};

/// Immediate parent of a child id.
pub fn parent_of(records: &[SubagentRecord], child_id: &str) -> Option<String> {
    records
        .iter()
        .find(|r| r.child_id == child_id)
        .and_then(|r| r.parent_id.clone())
}

/// Direct children of a parent id (empty when none).
pub fn children_of(records: &[SubagentRecord], parent_id: &str) -> Vec<String> {
    let mut out: Vec<String> = records
        .iter()
        .filter(|r| r.parent_id.as_deref() == Some(parent_id))
        .map(|r| r.child_id.clone())
        .collect();
    out.sort();
    out
}

/// Child ids that have no recorded parent (the store's roots).
pub fn roots(records: &[SubagentRecord]) -> Vec<String> {
    let mut out: Vec<String> = records
        .iter()
        .filter(|r| r.parent_id.is_none())
        .map(|r| r.child_id.clone())
        .collect();
    out.sort();
    out
}

/// A child plus all descendants, breadth-first. Cycles and repeated ids are
/// impossible in a well-formed store; the visited set guards against corrupt
/// records so traversal always terminates.
pub fn subtree(records: &[SubagentRecord], child_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(child_id.to_string());
    while let Some(next) = queue.pop_front() {
        if !visited.insert(next.clone()) {
            continue;
        }
        out.push(next.clone());
        for child in children_of(records, &next) {
            if !visited.contains(&child) {
                queue.push_back(child);
            }
        }
    }
    out
}

/// The full relationship map: child id -> direct children.
pub fn relationship_map(records: &[SubagentRecord]) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in records {
        map.insert(
            record.child_id.clone(),
            children_of(records, &record.child_id),
        );
    }
    map
}

/// Build the record list from the store (convenience for CLI).
pub fn load_records(store: &SubagentStore) -> Vec<SubagentRecord> {
    store.list()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, parent: Option<&str>) -> SubagentRecord {
        SubagentRecord {
            child_id: id.into(),
            parent_id: parent.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn relationship_lookups_over_a_small_tree() {
        let records = vec![
            rec("a", None),
            rec("b", Some("a")),
            rec("c", Some("a")),
            rec("d", Some("b")),
        ];
        assert_eq!(parent_of(&records, "b").as_deref(), Some("a"));
        assert_eq!(parent_of(&records, "a"), None);
        assert_eq!(
            children_of(&records, "a"),
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(children_of(&records, "missing"), Vec::<String>::new());
        assert_eq!(roots(&records), vec!["a".to_string()]);
        assert_eq!(subtree(&records, "a"), vec!["a", "b", "c", "d"]);
        assert_eq!(subtree(&records, "b"), vec!["b", "d"]);
    }

    #[test]
    fn cyclic_store_terminates() {
        // A corrupt store: a <-> b cycle.
        let records = vec![
            SubagentRecord {
                child_id: "a".into(),
                parent_id: Some("b".into()),
                ..Default::default()
            },
            SubagentRecord {
                child_id: "b".into(),
                parent_id: Some("a".into()),
                ..Default::default()
            },
        ];
        let tree = subtree(&records, "a");
        assert_eq!(tree.len(), 2);
        assert!(tree.contains(&"b".to_string()));
    }
}
