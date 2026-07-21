//! Core value types for a federated origin.

use std::fmt;
use std::path::PathBuf;

/// Durable, transport-independent identity for a federated origin.
///
/// Backed by the Tailscale `StableNodeID`, which is stable across reboots,
/// sleep, IP reassignment and MagicDNS renames. This is the only key the
/// registry and id-namespacing use — never the hostname, IP or alias.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct OriginKey(String);

impl OriginKey {
    pub fn new(stable_node_id: impl Into<String>) -> Self {
        Self(stable_node_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OriginKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the local hub reaches a federated origin's JSON API socket.
///
/// Intentionally decoupled from [`OriginKey`]: discovery decides *who* an origin
/// is (its `OriginKey`), and a separate policy decides *how* to reach it. Ingest
/// and merge code must depend only on `ConnectionTarget`, never on the concrete
/// transport, so a future direct/mTLS transport slots in without churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    /// A local unix socket that already speaks the herdr JSON API. In the star
    /// topology this is typically an `ssh -L`-forwarded socket, but federation
    /// only ever sees the local path — the forwarding lives *outside* herdr,
    /// which is what makes self-federation a faithful test harness (two local
    /// `--session` sockets exercise byte-identical ingest).
    LocalSocket(PathBuf),
}

/// A remote herdr server whose agent sessions the local hub aggregates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// Durable identity. Keys the registry and namespaces foreign ids.
    pub key: OriginKey,
    /// Human display label (MagicDNS name or a static alias). Untrusted for
    /// anything but display.
    pub label: String,
    /// How to reach this origin's JSON API socket.
    pub target: ConnectionTarget,
}

impl Origin {
    pub fn new(key: OriginKey, label: impl Into<String>, target: ConnectionTarget) -> Self {
        Self {
            key,
            label: label.into(),
            target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_key_is_stable_string() {
        let key = OriginKey::new("nABC123CNTRL");
        assert_eq!(key.as_str(), "nABC123CNTRL");
        assert_eq!(key.to_string(), "nABC123CNTRL");
        assert_eq!(key, OriginKey::new(String::from("nABC123CNTRL")));
    }

    #[test]
    fn origin_carries_label_and_target_separately() {
        let origin = Origin::new(
            OriginKey::new("n1"),
            "trilliums-mini",
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/mini1.sock")),
        );
        assert_eq!(origin.key, OriginKey::new("n1"));
        assert_eq!(origin.label, "trilliums-mini");
        assert_eq!(
            origin.target,
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/mini1.sock"))
        );
    }
}
