//! Core value types for a federated origin.

use std::fmt;
use std::path::PathBuf;

/// Durable, transport-independent identity for a federated origin.
///
/// Backed by the Tailscale `StableNodeID`, which is stable across reboots,
/// sleep, IP reassignment and MagicDNS renames. This is the only key the
/// registry and id-namespacing use — never the hostname, IP or alias.
///
/// Construction validates the key so that every `OriginKey` in the system is
/// safe both as the first segment of a `fed~<key>~<raw>` namespaced id (no
/// `~`) and as a socket filename component (no path separators or `..`, no
/// whitespace or control characters). Deserialization goes through the same
/// validation.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct OriginKey(String);

/// Rejection of an origin key that would break id-namespacing or escape the
/// forwarded-socket directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidOriginKey {
    key: String,
    reason: &'static str,
}

impl fmt::Display for InvalidOriginKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid origin key {:?}: {}", self.key, self.reason)
    }
}

impl std::error::Error for InvalidOriginKey {}

impl OriginKey {
    pub fn new(stable_node_id: impl Into<String>) -> Result<Self, InvalidOriginKey> {
        let key = stable_node_id.into();
        let reject = |reason| {
            Err(InvalidOriginKey {
                key: key.clone(),
                reason,
            })
        };
        if key.is_empty() {
            return reject("must not be empty");
        }
        if key.contains('~') {
            return reject("must not contain the '~' namespacing separator");
        }
        if key.contains('/') || key.contains('\\') {
            return reject("must not contain path separators");
        }
        if key.contains("..") {
            return reject("must not contain '..'");
        }
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return reject("must not contain whitespace or control characters");
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OriginKey {
    type Error = InvalidOriginKey;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
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
        let key = OriginKey::new("nABC123CNTRL").unwrap();
        assert_eq!(key.as_str(), "nABC123CNTRL");
        assert_eq!(key.to_string(), "nABC123CNTRL");
        assert_eq!(key, OriginKey::new(String::from("nABC123CNTRL")).unwrap());
    }

    #[test]
    fn origin_key_accepts_static_style_keys() {
        // Non-tailnet static origins use human-chosen keys.
        assert!(OriginKey::new("static-box").is_ok());
        assert!(OriginKey::new("sess_a").is_ok());
        assert!(OriginKey::new("mini.local").is_ok());
    }

    #[test]
    fn origin_key_rejects_namespacing_separator() {
        // A key containing '~' would alias origins: `fed~my~box~term_1` parses
        // back as origin "my" with raw "box~term_1".
        assert!(OriginKey::new("my~box").is_err());
        assert!(OriginKey::new("~").is_err());
    }

    #[test]
    fn origin_key_rejects_path_and_control_characters() {
        assert!(OriginKey::new("").is_err());
        assert!(OriginKey::new("a/b").is_err());
        assert!(OriginKey::new("a\\b").is_err());
        assert!(OriginKey::new("..").is_err());
        assert!(OriginKey::new("a..b").is_err());
        assert!(OriginKey::new("../../etc/passwd").is_err());
        assert!(OriginKey::new("a b").is_err());
        assert!(OriginKey::new("a\tb").is_err());
        assert!(OriginKey::new("a\u{7}b").is_err());
    }

    #[test]
    fn origin_key_deserialization_validates() {
        assert!(serde_json::from_str::<OriginKey>("\"nABC123CNTRL\"").is_ok());
        assert!(serde_json::from_str::<OriginKey>("\"my~box\"").is_err());
        assert!(serde_json::from_str::<OriginKey>("\"../escape\"").is_err());
    }

    #[test]
    fn origin_carries_label_and_target_separately() {
        let origin = Origin::new(
            OriginKey::new("n1").unwrap(),
            "trilliums-mini",
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/mini1.sock")),
        );
        assert_eq!(origin.key, OriginKey::new("n1").unwrap());
        assert_eq!(origin.label, "trilliums-mini");
        assert_eq!(
            origin.target,
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/mini1.sock"))
        );
    }
}
