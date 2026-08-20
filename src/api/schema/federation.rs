//! Federation API schema — status, diagnostics, and health reporting.

use serde::{Deserialize, Serialize};

/// Status of a single federated origin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OriginStatusKind {
    /// Origin has never been polled.
    Unknown,
    /// Origin is reachable and responding.
    Reachable,
    /// Origin is unreachable or returned an error.
    Unreachable,
}

/// Detailed status for a single origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OriginStatusResponse {
    /// Stable origin key (Tailscale node ID or static name).
    pub key: String,
    /// Display label for the origin.
    pub label: String,
    /// Current status of the origin.
    pub status: OriginStatusKind,
    /// Error message if unreachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Latency of the last poll in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Number of consecutive failed polls.
    pub failure_count: u32,
    /// Unix timestamp of the last poll attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<u64>,
    /// Unix timestamp of the last successful poll.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<u64>,
}

/// Federation fleet status summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FederationStatusResponse {
    /// Whether federation is enabled.
    pub enabled: bool,
    /// Total number of known origins.
    pub total_origins: usize,
    /// Number of currently reachable origins.
    pub reachable: usize,
    /// Number of currently unreachable origins.
    pub unreachable: usize,
    /// Number of origins never polled.
    pub unknown: usize,
    /// Detailed status for each origin.
    pub origins: Vec<OriginStatusResponse>,
}

impl FederationStatusResponse {
    /// Create an empty response indicating federation is disabled.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            total_origins: 0,
            reachable: 0,
            unreachable: 0,
            unknown: 0,
            origins: Vec::new(),
        }
    }

    /// Create a response for an enabled but discovery-less fleet.
    pub fn no_origins() -> Self {
        Self {
            enabled: true,
            total_origins: 0,
            reachable: 0,
            unreachable: 0,
            unknown: 0,
            origins: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn federation_status_response_disabled() {
        let status = FederationStatusResponse::disabled();
        assert!(!status.enabled);
        assert_eq!(status.total_origins, 0);
        assert!(status.origins.is_empty());
    }

    #[test]
    fn federation_status_response_no_origins() {
        let status = FederationStatusResponse::no_origins();
        assert!(status.enabled);
        assert_eq!(status.total_origins, 0);
        assert!(status.origins.is_empty());
    }

    #[test]
    fn origin_status_serializes_correctly() {
        let origin_status = OriginStatusResponse {
            key: "test-origin".to_string(),
            label: "test machine".to_string(),
            status: OriginStatusKind::Reachable,
            error: None,
            latency_ms: Some(42),
            failure_count: 0,
            last_poll_at: Some(1692518400),
            last_success_at: Some(1692518400),
        };

        let json = serde_json::to_string(&origin_status).expect("valid JSON");
        assert!(json.contains("\"key\":\"test-origin\""));
        assert!(json.contains("\"status\":\"reachable\""));
        assert!(json.contains("\"latency_ms\":42"));
    }

    #[test]
    fn origin_status_omits_none_fields() {
        let origin_status = OriginStatusResponse {
            key: "test".to_string(),
            label: "test".to_string(),
            status: OriginStatusKind::Unknown,
            error: None,
            latency_ms: None,
            failure_count: 0,
            last_poll_at: None,
            last_success_at: None,
        };

        let json = serde_json::to_string(&origin_status).expect("valid JSON");
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"latency_ms\""));
    }
}
