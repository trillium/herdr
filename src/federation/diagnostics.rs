//! Federation diagnostics — runtime checks and status reporting for troubleshooting.
//!
//! Provides utilities for diagnosing federation issues: checking socket connectivity,
//! validating remote server responses, and reporting the health of each origin.

use std::time::Duration;

use super::origin::{ConnectionTarget, Origin, OriginKey};
use crate::api::client::ApiClient;
use crate::api::schema::{EmptyParams, Method, Request};

/// Diagnostic result for a single origin.
#[derive(Debug, Clone)]
pub struct OriginDiagnostic {
    pub key: OriginKey,
    pub label: String,
    pub socket_path: String,
    pub reachable: bool,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
}

impl OriginDiagnostic {
    fn new(origin: &Origin) -> Self {
        let socket_path = match &origin.target {
            ConnectionTarget::LocalSocket(path) => path.to_string_lossy().to_string(),
        };
        Self {
            key: origin.key.clone(),
            label: origin.label.clone(),
            socket_path,
            reachable: false,
            error: None,
            latency_ms: None,
        }
    }
}

/// Run diagnostics on a single origin: check socket reachability and API health.
pub fn diagnose_origin(origin: &Origin, timeout: Duration) -> OriginDiagnostic {
    let mut result = OriginDiagnostic::new(origin);

    let start = std::time::Instant::now();

    // Try to create a client and request the snapshot.
    let client = match &origin.target {
        ConnectionTarget::LocalSocket(path) => ApiClient::for_target(
            crate::api::client::ConnectionTarget::SocketPath(path.clone()),
        ),
    };
    let request = Request {
        id: format!("diagnostic:snapshot:{}", origin.key),
        method: Method::SessionSnapshot(EmptyParams::default()),
    };

    match client.request_value_with_timeout(&request, timeout) {
        Ok(_value) => {
            result.reachable = true;
            result.latency_ms = Some(start.elapsed().as_millis() as u64);
        }
        Err(err) => {
            result.reachable = false;
            result.error = Some(err.to_string());
            result.latency_ms = Some(start.elapsed().as_millis() as u64);
        }
    }

    result
}

/// Run diagnostics on all origins and return a summary.
pub fn diagnose_fleet(origins: &[Origin], timeout: Duration) -> DiagnosticSummary {
    let mut diagnostics = Vec::new();
    for origin in origins {
        diagnostics.push(diagnose_origin(origin, timeout));
    }

    let reachable_count = diagnostics.iter().filter(|d| d.reachable).count();
    let unreachable_count = diagnostics.len() - reachable_count;

    DiagnosticSummary {
        total_origins: diagnostics.len(),
        reachable: reachable_count,
        unreachable: unreachable_count,
        origins: diagnostics,
    }
}

/// Summary of fleet health diagnostics.
#[derive(Debug, Clone)]
pub struct DiagnosticSummary {
    pub total_origins: usize,
    pub reachable: usize,
    pub unreachable: usize,
    pub origins: Vec<OriginDiagnostic>,
}

impl DiagnosticSummary {
    /// Format the diagnostic summary as a human-readable string.
    pub fn format_report(&self) -> String {
        let mut report = format!(
            "Federation Diagnostic Report\n\
             ==========================\n\
             Total origins: {}\n\
             Reachable: {}\n\
             Unreachable: {}\n\
             \n",
            self.total_origins, self.reachable, self.unreachable
        );

        report.push_str("Origin Details:\n");
        report.push_str("===============\n");

        for diag in &self.origins {
            report.push_str(&format!("\n[{}] {}\n", diag.key, diag.label));
            report.push_str(&format!("  Socket: {}\n", diag.socket_path));

            if let Some(latency) = diag.latency_ms {
                report.push_str(&format!("  Latency: {}ms\n", latency));
            }

            if diag.reachable {
                report.push_str("  Status: ✓ REACHABLE\n");
            } else {
                report.push_str("  Status: ✗ UNREACHABLE\n");
                if let Some(err) = &diag.error {
                    report.push_str(&format!("  Error: {}\n", err));
                }
            }
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn diagnostic_summary_counts_correctly() {
        let origins = vec![
            Origin::new(
                OriginKey::new("origin1").unwrap(),
                "local1",
                ConnectionTarget::LocalSocket(PathBuf::from("/tmp/test1.sock")),
            ),
            Origin::new(
                OriginKey::new("origin2").unwrap(),
                "local2",
                ConnectionTarget::LocalSocket(PathBuf::from("/tmp/test2.sock")),
            ),
        ];

        // Note: This will fail to connect to the sockets, so all origins will be unreachable.
        let summary = diagnose_fleet(&origins, Duration::from_millis(50));

        assert_eq!(summary.total_origins, 2);
        assert_eq!(summary.unreachable, 2);
        assert_eq!(summary.reachable, 0);
    }

    #[test]
    fn diagnostic_report_formats_correctly() {
        let mut diagnostic = OriginDiagnostic::new(&Origin::new(
            OriginKey::new("test-origin").unwrap(),
            "test-label",
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/test.sock")),
        ));

        diagnostic.reachable = true;
        diagnostic.latency_ms = Some(42);

        let summary = DiagnosticSummary {
            total_origins: 1,
            reachable: 1,
            unreachable: 0,
            origins: vec![diagnostic],
        };

        let report = summary.format_report();

        assert!(report.contains("Federation Diagnostic Report"));
        assert!(report.contains("Total origins: 1"));
        assert!(report.contains("Reachable: 1"));
        assert!(report.contains("test-origin"));
        assert!(report.contains("REACHABLE"));
        assert!(report.contains("42ms"));
    }
}
