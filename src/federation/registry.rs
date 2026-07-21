//! The set of federated origins the local hub aggregates.
//!
//! Star topology: this registry lives only on the local (hub) machine; origins
//! are never aware of one another. Keyed by the durable [`OriginKey`] so that
//! re-discovery with a churned IP or renamed host updates in place instead of
//! duplicating.

use std::collections::BTreeMap;

use super::origin::{Origin, OriginKey};

/// What a [`FederationRegistry::reconcile`] pass changed. Callers (N1 ingest)
/// use this to splice/drop the matching foreign sidebar rows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileDelta {
    /// Origins present after the pass that were absent before.
    pub added: Vec<OriginKey>,
    /// Origins whose target/label changed but whose key persisted.
    pub updated: Vec<OriginKey>,
    /// Origins absent after the pass that were present before (went offline).
    pub removed: Vec<OriginKey>,
}

impl ReconcileDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Registry of federated origins, ordered by durable key for deterministic
/// sidebar rendering.
#[derive(Debug, Default, Clone)]
pub struct FederationRegistry {
    origins: BTreeMap<OriginKey, Origin>,
}

impl FederationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an origin, returning the previous entry if any.
    pub fn upsert(&mut self, origin: Origin) -> Option<Origin> {
        self.origins.insert(origin.key.clone(), origin)
    }

    pub fn remove(&mut self, key: &OriginKey) -> Option<Origin> {
        self.origins.remove(key)
    }

    pub fn get(&self, key: &OriginKey) -> Option<&Origin> {
        self.origins.get(key)
    }

    pub fn contains(&self, key: &OriginKey) -> bool {
        self.origins.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.origins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    /// Origins in stable key order.
    pub fn iter(&self) -> impl Iterator<Item = &Origin> {
        self.origins.values()
    }

    pub fn keys(&self) -> impl Iterator<Item = &OriginKey> {
        self.origins.keys()
    }

    /// Replace the whole set from a fresh discovery pass, reporting what changed.
    ///
    /// Additions and updates are applied; keys no longer present are removed.
    /// Star topology means this is a full replace, not a merge — a discovery
    /// pass is authoritative about who is currently reachable.
    pub fn reconcile(&mut self, discovered: Vec<Origin>) -> ReconcileDelta {
        let mut delta = ReconcileDelta::default();

        let discovered_keys: std::collections::BTreeSet<OriginKey> =
            discovered.iter().map(|origin| origin.key.clone()).collect();

        for origin in discovered {
            match self.origins.get(&origin.key) {
                Some(existing) if *existing == origin => {}
                Some(_) => delta.updated.push(origin.key.clone()),
                None => delta.added.push(origin.key.clone()),
            }
            self.origins.insert(origin.key.clone(), origin);
        }

        let removed: Vec<OriginKey> = self
            .origins
            .keys()
            .filter(|key| !discovered_keys.contains(*key))
            .cloned()
            .collect();
        for key in &removed {
            self.origins.remove(key);
        }
        delta.removed = removed;

        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::origin::ConnectionTarget;
    use std::path::PathBuf;

    fn origin(key: &str, label: &str, sock: &str) -> Origin {
        Origin::new(
            OriginKey::new(key).unwrap(),
            label,
            ConnectionTarget::LocalSocket(PathBuf::from(sock)),
        )
    }

    #[test]
    fn upsert_keys_by_durable_id_not_label() {
        let mut reg = FederationRegistry::new();
        reg.upsert(origin("n1", "old-name", "/tmp/a.sock"));
        // Same durable key, renamed host + new forwarded socket => update in place.
        let prev = reg.upsert(origin("n1", "new-name", "/tmp/b.sock"));

        assert_eq!(prev.map(|o| o.label), Some("old-name".to_string()));
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.get(&OriginKey::new("n1").unwrap()).unwrap().label,
            "new-name"
        );
    }

    #[test]
    fn iter_is_deterministic_key_order() {
        let mut reg = FederationRegistry::new();
        reg.upsert(origin("n3", "c", "/tmp/c.sock"));
        reg.upsert(origin("n1", "a", "/tmp/a.sock"));
        reg.upsert(origin("n2", "b", "/tmp/b.sock"));

        let keys: Vec<String> = reg.keys().map(|k| k.to_string()).collect();
        assert_eq!(keys, vec!["n1", "n2", "n3"]);
    }

    #[test]
    fn reconcile_reports_added_updated_removed() {
        let mut reg = FederationRegistry::new();
        reg.upsert(origin("n1", "a", "/tmp/a.sock"));
        reg.upsert(origin("n2", "b", "/tmp/b.sock"));

        // n1 unchanged, n2 relabeled, n3 new, n... old n2 socket churn, drop none yet.
        let delta = reg.reconcile(vec![
            origin("n1", "a", "/tmp/a.sock"),
            origin("n2", "b-renamed", "/tmp/b.sock"),
            origin("n3", "c", "/tmp/c.sock"),
        ]);

        assert_eq!(delta.added, vec![OriginKey::new("n3").unwrap()]);
        assert_eq!(delta.updated, vec![OriginKey::new("n2").unwrap()]);
        assert!(delta.removed.is_empty());
        assert_eq!(reg.len(), 3);

        // Next pass drops n1 and n2 (went offline).
        let delta = reg.reconcile(vec![origin("n3", "c", "/tmp/c.sock")]);
        assert!(delta.added.is_empty());
        assert!(delta.updated.is_empty());
        assert_eq!(
            delta.removed,
            vec![OriginKey::new("n1").unwrap(), OriginKey::new("n2").unwrap()]
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn reconcile_to_empty_removes_all() {
        let mut reg = FederationRegistry::new();
        reg.upsert(origin("n1", "a", "/tmp/a.sock"));
        let delta = reg.reconcile(vec![]);
        assert_eq!(delta.removed, vec![OriginKey::new("n1").unwrap()]);
        assert!(reg.is_empty());
    }
}
