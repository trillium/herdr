//! Origin discovery: the union of `tailscale status --json` peers with a static
//! config override, keyed by durable StableNodeID.
//!
//! The subprocess call lives at the edge; the parsing and union *policy* here is
//! pure and unit-tested against fixture JSON. Reachability comes from the
//! forwarded-socket convention (or an explicit static path), because federation
//! can only ingest an origin whose JSON API socket is reachable as a local path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::origin::{ConnectionTarget, Origin, OriginKey};

/// Subset of `tailscale status --json` we consume. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct TailscaleStatus {
    #[serde(rename = "Self")]
    #[serde(default)]
    pub self_node: Option<TailscaleNode>,
    #[serde(rename = "Peer")]
    #[serde(default)]
    pub peers: HashMap<String, TailscaleNode>,
}

/// A tailnet node. `ID` is the StableNodeID — our durable origin key.
#[derive(Debug, Clone, Deserialize)]
pub struct TailscaleNode {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "HostName")]
    #[serde(default)]
    pub host_name: String,
    #[serde(rename = "DNSName")]
    #[serde(default)]
    pub dns_name: String,
    #[serde(rename = "Online")]
    #[serde(default)]
    pub online: bool,
}

impl TailscaleNode {
    /// Preferred display label: MagicDNS short name, else hostname, else id.
    fn display_label(&self) -> String {
        if !self.dns_name.is_empty() {
            // DNSName is fully-qualified with a trailing dot; take the short name.
            let trimmed = self.dns_name.trim_end_matches('.');
            if let Some((short, _)) = trimmed.split_once('.') {
                if !short.is_empty() {
                    return short.to_string();
                }
            }
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if !self.host_name.is_empty() {
            return self.host_name.clone();
        }
        self.id.clone()
    }
}

/// An explicit origin from config: reaches hosts off the tailnet, or overrides a
/// discovered peer's label/socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticOrigin {
    pub key: OriginKey,
    /// Optional display override. Falls back to the tailnet label, then the key.
    pub label: Option<String>,
    /// Local path where this origin's JSON API is reachable (typically an
    /// `ssh -L`-forwarded socket).
    pub socket_path: PathBuf,
}

/// Inputs to a discovery pass.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Base dir holding per-origin forwarded API sockets, named `<key>.sock`.
    pub forwarded_socket_dir: PathBuf,
    /// Explicit origins (non-tailnet hosts, or overrides of a discovered peer).
    pub static_origins: Vec<StaticOrigin>,
}

/// Convention for where an origin's `ssh -L`-forwarded JSON API socket lands.
///
/// [`OriginKey`] construction guarantees the key is filename-safe (no path
/// separators or `..`), so the result is always inside `dir`.
pub fn forwarded_socket_path(dir: &Path, key: &OriginKey) -> PathBuf {
    dir.join(format!("{}.sock", key.as_str()))
}

/// Compute the reachable origins: online tailnet peers (via the forwarded-socket
/// convention) unioned with static config origins. Static origins win on key
/// conflict — they carry an explicit socket path and label. The local `Self`
/// node is never federated to itself. Offline peers and peers whose id is not a
/// valid [`OriginKey`] are excluded.
pub fn discover(status: &TailscaleStatus, config: &DiscoveryConfig) -> Vec<Origin> {
    let mut by_key: std::collections::BTreeMap<OriginKey, Origin> =
        std::collections::BTreeMap::new();

    let self_id = status.self_node.as_ref().map(|node| node.id.clone());

    for node in status.peers.values() {
        if !node.online {
            continue;
        }
        if self_id.as_deref() == Some(node.id.as_str()) {
            continue;
        }
        let Ok(key) = OriginKey::new(node.id.clone()) else {
            continue;
        };
        let target = ConnectionTarget::LocalSocket(forwarded_socket_path(
            &config.forwarded_socket_dir,
            &key,
        ));
        by_key.insert(key.clone(), Origin::new(key, node.display_label(), target));
    }

    // Static origins override discovered peers on key conflict.
    for static_origin in &config.static_origins {
        let label = static_origin
            .label
            .clone()
            .or_else(|| by_key.get(&static_origin.key).map(|o| o.label.clone()))
            .unwrap_or_else(|| static_origin.key.to_string());
        by_key.insert(
            static_origin.key.clone(),
            Origin::new(
                static_origin.key.clone(),
                label,
                ConnectionTarget::LocalSocket(static_origin.socket_path.clone()),
            ),
        );
    }

    by_key.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "Self": { "ID": "nSELF", "HostName": "laptop", "DNSName": "laptop.tail.ts.net.", "Online": true },
        "Peer": {
            "key1": { "ID": "nMINI1", "HostName": "trilliums-mini", "DNSName": "trilliums-mini.tail.ts.net.", "Online": true },
            "key2": { "ID": "nMINI2", "HostName": "mini-two", "DNSName": "mini-two.tail.ts.net.", "Online": false }
        }
    }"#;

    fn config() -> DiscoveryConfig {
        DiscoveryConfig {
            forwarded_socket_dir: PathBuf::from("/run/herdr/fed"),
            static_origins: vec![],
        }
    }

    #[test]
    fn parses_status_and_skips_offline_and_self() {
        let status: TailscaleStatus = serde_json::from_str(FIXTURE).unwrap();
        let origins = discover(&status, &config());

        // Only the online peer (mini1); self excluded, offline mini2 excluded.
        assert_eq!(origins.len(), 1);
        let mini1 = &origins[0];
        assert_eq!(mini1.key, OriginKey::new("nMINI1").unwrap());
        assert_eq!(mini1.label, "trilliums-mini");
        assert_eq!(
            mini1.target,
            ConnectionTarget::LocalSocket(PathBuf::from("/run/herdr/fed/nMINI1.sock"))
        );
    }

    #[test]
    fn static_origin_reaches_non_tailnet_host() {
        let status = TailscaleStatus {
            self_node: None,
            peers: HashMap::new(),
        };
        let cfg = DiscoveryConfig {
            forwarded_socket_dir: PathBuf::from("/run/herdr/fed"),
            static_origins: vec![StaticOrigin {
                key: OriginKey::new("static-box").unwrap(),
                label: Some("build-box".to_string()),
                socket_path: PathBuf::from("/tmp/build.sock"),
            }],
        };
        let origins = discover(&status, &cfg);
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].label, "build-box");
        assert_eq!(
            origins[0].target,
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/build.sock"))
        );
    }

    #[test]
    fn static_origin_overrides_discovered_peer() {
        let status: TailscaleStatus = serde_json::from_str(FIXTURE).unwrap();
        let cfg = DiscoveryConfig {
            forwarded_socket_dir: PathBuf::from("/run/herdr/fed"),
            static_origins: vec![StaticOrigin {
                key: OriginKey::new("nMINI1").unwrap(),
                label: None,
                socket_path: PathBuf::from("/custom/mini1.sock"),
            }],
        };
        let origins = discover(&status, &cfg);
        assert_eq!(origins.len(), 1);
        // Label falls back to the discovered peer's; socket is the override.
        assert_eq!(origins[0].label, "trilliums-mini");
        assert_eq!(
            origins[0].target,
            ConnectionTarget::LocalSocket(PathBuf::from("/custom/mini1.sock"))
        );
    }

    #[test]
    fn self_federation_harness_uses_two_static_local_sockets() {
        // The CI harness: no tailscale, two local `--session` API sockets.
        let status = TailscaleStatus {
            self_node: None,
            peers: HashMap::new(),
        };
        let cfg = DiscoveryConfig {
            forwarded_socket_dir: PathBuf::from("/unused"),
            static_origins: vec![
                StaticOrigin {
                    key: OriginKey::new("sess-a").unwrap(),
                    label: Some("A".to_string()),
                    socket_path: PathBuf::from("/tmp/a.sock"),
                },
                StaticOrigin {
                    key: OriginKey::new("sess-b").unwrap(),
                    label: Some("B".to_string()),
                    socket_path: PathBuf::from("/tmp/b.sock"),
                },
            ],
        };
        let origins = discover(&status, &cfg);
        assert_eq!(origins.len(), 2);
    }

    #[test]
    fn forwarded_socket_path_follows_convention() {
        let path = forwarded_socket_path(
            Path::new("/run/herdr/fed"),
            &OriginKey::new("nMINI1").unwrap(),
        );
        assert_eq!(path, PathBuf::from("/run/herdr/fed/nMINI1.sock"));
    }

    #[test]
    fn peers_with_invalid_ids_are_skipped() {
        let status = TailscaleStatus {
            self_node: None,
            peers: HashMap::from([(
                "key1".to_string(),
                TailscaleNode {
                    id: "bad~id".to_string(),
                    host_name: "rogue".to_string(),
                    dns_name: String::new(),
                    online: true,
                },
            )]),
        };
        assert!(discover(&status, &config()).is_empty());
    }

    #[test]
    fn socket_paths_cannot_escape_the_socket_dir() {
        // Keys with path separators or '..' are rejected at construction, so no
        // OriginKey can ever produce a socket path outside the configured dir.
        assert!(OriginKey::new("../../etc/cron.d/evil").is_err());
        assert!(OriginKey::new("sub/dir").is_err());
        assert!(OriginKey::new("..").is_err());

        let dir = Path::new("/run/herdr/fed");
        let path = forwarded_socket_path(dir, &OriginKey::new("mini.local").unwrap());
        assert!(path.starts_with(dir));
        assert_eq!(path.components().count(), dir.components().count() + 1);
    }
}
