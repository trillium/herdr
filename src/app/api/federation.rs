//! Federation API handlers — fleet status, diagnostics, and health reporting.

use crate::api::schema::{FederationStatusResponse, ResponseResult};
use crate::app::App;

use super::responses::encode_success;

impl App {
    /// Handle federation.status API request.
    ///
    /// Returns the current federation fleet status: which origins are
    /// reachable, latency, error messages, and poll timestamps.
    ///
    /// TODO: Integrate FederationStatusTracker into AppState so we can
    /// return actual runtime status instead of placeholder data.
    pub(super) fn handle_federation_status(&mut self, id: String) -> String {
        // Check if federation is enabled
        let federation_enabled = self.state.federation_enabled;

        let status = if federation_enabled {
            // Once FederationStatusTracker is integrated into AppState,
            // fetch actual status from self.federation_status_tracker
            // For now, return empty response
            FederationStatusResponse::no_origins()
        } else {
            FederationStatusResponse::disabled()
        };

        encode_success(
            id,
            ResponseResult::FederationStatus { status: Box::new(status) },
        )
    }
}
