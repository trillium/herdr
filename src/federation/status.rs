//! Federation origin status tracking — observes connectivity, reachability, and errors.
//!
//! Tracks which origins are currently reachable, when they last succeeded/failed,
//! and what errors occurred. Consumed by CLI commands and health checks.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use super::origin::OriginKey;

/// Status of a single federated origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginStatus {
    /// Never polled yet.
    Unknown,
    /// Last poll succeeded and returned valid data.
    Reachable,
    /// Last poll failed; origin is unreachable or returned invalid data.
    Unreachable { error: String },
}

impl std::fmt::Display for OriginStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::Reachable => write!(f, "reachable"),
            Self::Unreachable { error } => write!(f, "unreachable: {}", error),
        }
    }
}

/// Detailed status for a single origin, including timestamps and error history.
#[derive(Debug, Clone)]
pub struct OriginStatusDetail {
    pub status: OriginStatus,
    /// Timestamp of the last poll attempt (unix seconds).
    pub last_poll_at: Option<u64>,
    /// Timestamp of the last successful poll (unix seconds).
    pub last_success_at: Option<u64>,
    /// Number of consecutive failed polls.
    pub failure_count: u32,
}

impl OriginStatusDetail {
    fn new() -> Self {
        Self {
            status: OriginStatus::Unknown,
            last_poll_at: None,
            last_success_at: None,
            failure_count: 0,
        }
    }

    fn mark_success(&mut self) {
        let now = now_unix_seconds();
        self.status = OriginStatus::Reachable;
        self.last_poll_at = Some(now);
        self.last_success_at = Some(now);
        self.failure_count = 0;
    }

    fn mark_failure(&mut self, error: impl Into<String>) {
        let now = now_unix_seconds();
        let error_str = error.into();
        self.status = OriginStatus::Unreachable {
            error: error_str.clone(),
        };
        self.last_poll_at = Some(now);
        self.failure_count += 1;
    }
}

/// Tracks the status of all federated origins.
pub struct FederationStatusTracker {
    origins: HashMap<OriginKey, OriginStatusDetail>,
}

impl FederationStatusTracker {
    pub fn new() -> Self {
        Self {
            origins: HashMap::new(),
        }
    }

    /// Mark an origin as successfully polled.
    pub fn mark_success(&mut self, key: &OriginKey) {
        self.origins
            .entry(key.clone())
            .or_insert_with(OriginStatusDetail::new)
            .mark_success();
    }

    /// Mark an origin as failed to poll.
    pub fn mark_failure(&mut self, key: &OriginKey, error: impl Into<String>) {
        self.origins
            .entry(key.clone())
            .or_insert_with(OriginStatusDetail::new)
            .mark_failure(error);
    }

    /// Get the status of a single origin.
    pub fn get_status(&self, key: &OriginKey) -> OriginStatus {
        self.origins
            .get(key)
            .map(|d| d.status.clone())
            .unwrap_or(OriginStatus::Unknown)
    }

    /// Get detailed status for a single origin.
    pub fn get_detail(&self, key: &OriginKey) -> OriginStatusDetail {
        self.origins
            .get(key)
            .cloned()
            .unwrap_or_else(OriginStatusDetail::new)
    }

    /// Get all origin statuses.
    pub fn all_statuses(&self) -> Vec<(OriginKey, OriginStatus)> {
        self.origins
            .iter()
            .map(|(key, detail)| (key.clone(), detail.status.clone()))
            .collect()
    }

    /// Count reachable origins.
    pub fn reachable_count(&self) -> usize {
        self.origins
            .values()
            .filter(|d| d.status == OriginStatus::Reachable)
            .count()
    }

    /// Count unreachable origins.
    pub fn unreachable_count(&self) -> usize {
        self.origins
            .values()
            .filter(|d| matches!(d.status, OriginStatus::Unreachable { .. }))
            .count()
    }
}

impl Default for FederationStatusTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the current unix timestamp in seconds.
fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tracker_starts_empty() {
        let tracker = FederationStatusTracker::new();
        assert_eq!(tracker.reachable_count(), 0);
        assert_eq!(tracker.unreachable_count(), 0);
    }

    #[test]
    fn status_tracker_marks_success() {
        let mut tracker = FederationStatusTracker::new();
        let key = OriginKey::new("test-origin").unwrap();
        tracker.mark_success(&key);
        assert_eq!(tracker.get_status(&key), OriginStatus::Reachable);
        assert_eq!(tracker.reachable_count(), 1);
        assert_eq!(tracker.unreachable_count(), 0);
    }

    #[test]
    fn status_tracker_marks_failure() {
        let mut tracker = FederationStatusTracker::new();
        let key = OriginKey::new("test-origin").unwrap();
        tracker.mark_failure(&key, "connection timeout");
        let status = tracker.get_status(&key);
        assert!(matches!(status, OriginStatus::Unreachable { .. }));
        assert_eq!(tracker.reachable_count(), 0);
        assert_eq!(tracker.unreachable_count(), 1);
    }

    #[test]
    fn status_tracker_counts_multiple_origins() {
        let mut tracker = FederationStatusTracker::new();
        let key1 = OriginKey::new("origin1").unwrap();
        let key2 = OriginKey::new("origin2").unwrap();
        let key3 = OriginKey::new("origin3").unwrap();

        tracker.mark_success(&key1);
        tracker.mark_success(&key2);
        tracker.mark_failure(&key3, "socket error");

        assert_eq!(tracker.reachable_count(), 2);
        assert_eq!(tracker.unreachable_count(), 1);
    }

    #[test]
    fn status_recovery_from_failure() {
        let mut tracker = FederationStatusTracker::new();
        let key = OriginKey::new("test-origin").unwrap();

        tracker.mark_failure(&key, "timeout");
        assert_eq!(tracker.get_status(&key), OriginStatus::Unreachable { error: "timeout".to_string() });

        // Recovery should reset failure count
        tracker.mark_success(&key);
        let detail = tracker.get_detail(&key);
        assert_eq!(detail.status, OriginStatus::Reachable);
        assert_eq!(detail.failure_count, 0);
    }
}
