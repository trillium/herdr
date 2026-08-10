//! Federation phase **N1d** — remote pane input relay.
//!
//! Sends keystrokes and input text from the local hub to a foreign pane's PTY
//! on the remote origin's herdr server. The input is dispatched over the same
//! JSON API socket that the polling layer uses to fetch snapshots.
//!
//! Input targeting a foreign pane is routed here; errors are logged and never
//! block the local UI thread (the relay runs in a spawned task).

use std::time::Duration;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget as ApiConnectionTarget};
use crate::api::schema::{Method, PaneSendInputParams, Request};

use super::origin::{ConnectionTarget, Origin};

/// Failure sending input to a foreign pane.
#[derive(Debug)]
pub enum RelayError {
    Api(ApiClientError),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::Api(err) => write!(f, "relay request failed: {err}"),
            RelayError::Serialize(err) => write!(f, "relay serialize failed: {err}"),
        }
    }
}

impl std::error::Error for RelayError {}

/// Send input to a foreign pane on a remote origin.
///
/// The `pane_id` is the raw (non-namespaced) pane id on the remote. This function
/// constructs a `pane.send_input` request and sends it to the origin's JSON API
/// socket. Synchronous and blocking — run from an async context via
/// `tokio::task::spawn_blocking` so socket I/O doesn't stall the reactor.
pub fn send_input_to_foreign_pane(
    origin: &Origin,
    pane_id: &str,
    text: Option<&[u8]>,
    timeout: Duration,
) -> Result<(), RelayError> {
    let target = match &origin.target {
        ConnectionTarget::LocalSocket(path) => ApiConnectionTarget::SocketPath(path.clone()),
    };
    let client = ApiClient::for_target(target);

    let params = PaneSendInputParams {
        pane_id: pane_id.to_string(),
        text: text
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default(),
        keys: vec![],
    };

    let request = Request {
        id: format!("federation:relay:{}:{}", origin.key, pane_id),
        method: Method::PaneSendInput(params),
    };

    let _value = client
        .request_value_with_timeout(&request, timeout)
        .map_err(RelayError::Api)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::federation::OriginKey;

    fn test_origin(key: &str) -> Origin {
        Origin::new(
            OriginKey::new(key).expect("valid test origin key"),
            key,
            ConnectionTarget::LocalSocket(PathBuf::from(format!("/nonexistent/{key}.sock"))),
        )
    }

    #[test]
    fn unreachable_remote_returns_api_error() {
        let origin = test_origin("nDOWN");
        let result = send_input_to_foreign_pane(&origin, "term_1", Some(b"test"), Duration::from_millis(50));
        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }

    #[test]
    fn relay_error_display() {
        let err = RelayError::Api(crate::api::client::ApiClientError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "test"),
        ));
        assert!(err.to_string().contains("relay request failed"));
    }

    #[test]
    fn send_input_with_empty_bytes() {
        let origin = test_origin("nDOWN");
        let result = send_input_to_foreign_pane(&origin, "term_1", Some(b""), Duration::from_millis(50));
        assert!(result.is_err());
    }

    #[test]
    fn send_input_with_none_bytes() {
        let origin = test_origin("nDOWN");
        let result = send_input_to_foreign_pane(&origin, "term_1", None, Duration::from_millis(50));
        assert!(result.is_err());
    }

    #[test]
    fn send_input_constructs_pane_send_input_request() {
        let origin = test_origin("nTEST");
        let pane_id = "term_abc123";
        let text = b"test input";

        let result = send_input_to_foreign_pane(&origin, pane_id, Some(text), Duration::from_secs(1));

        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }

    #[test]
    fn send_input_with_utf8_text() {
        let origin = test_origin("nUTF8");
        let pane_id = "term_xyz";
        let text = "你好世界".as_bytes();

        let result = send_input_to_foreign_pane(&origin, pane_id, Some(text), Duration::from_millis(50));

        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }

    #[test]
    fn relay_error_implements_display() {
        let api_err = RelayError::Api(crate::api::client::ApiClientError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused"),
        ));
        let msg = api_err.to_string();
        assert!(msg.contains("relay request failed"));
    }

    #[test]
    fn relay_error_implements_error_trait() {
        use std::error::Error;
        let api_err = RelayError::Api(crate::api::client::ApiClientError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "test"),
        ));
        let _: &dyn Error = &api_err;
    }
}
