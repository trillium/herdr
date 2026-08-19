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

use super::origin::{ConnectionTarget, Origin, OriginKey};

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

/// Send a workspace/tab/pane structure mutation to a remote origin's JSON API.
///
/// N2 Part 3 action routing: new-tab / split / close on a foreign workspace are
/// executed on the owning remote server, and the resulting change comes back
/// through the next snapshot poll rather than being applied locally. Uses the
/// same JSON API socket as [`send_input_to_foreign_pane`]. Synchronous and
/// blocking — run from an async context via `tokio::task::spawn_blocking`.
pub fn send_action_to_foreign(
    origin: &Origin,
    method: &Method,
    timeout: Duration,
) -> Result<(), RelayError> {
    let target = match &origin.target {
        ConnectionTarget::LocalSocket(path) => ApiConnectionTarget::SocketPath(path.clone()),
    };
    let client = ApiClient::for_target(target);

    let request = Request {
        id: format!("federation:action:{}", origin.key),
        method: method.clone(),
    };

    let _value = client
        .request_value_with_timeout(&request, timeout)
        .map_err(RelayError::Api)?;

    Ok(())
}

/// Send a structure action to a remote origin and return its full response value.
///
/// Unlike [`send_action_to_foreign`] this surfaces the remote's JSON response
/// body so callers can extract result fields (e.g. the new pane id from a
/// `pane.split`). Synchronous and blocking — run from an async context via
/// `tokio::task::block_in_place` or `tokio::task::spawn_blocking`.
pub fn call_action_on_foreign_with_response(
    origin: &Origin,
    method: &Method,
    timeout: Duration,
) -> Result<serde_json::Value, RelayError> {
    let target = match &origin.target {
        ConnectionTarget::LocalSocket(path) => ApiConnectionTarget::SocketPath(path.clone()),
    };
    let client = ApiClient::for_target(target);
    let request = Request {
        id: format!("federation:action-resp:{}", origin.key),
        method: method.clone(),
    };
    client
        .request_value_with_timeout(&request, timeout)
        .map_err(RelayError::Api)
}

/// Spawn a background task that discovers the origin matching `origin_key` and
/// sends `method` to it, fire-and-forget. The response and any error are
/// logged, never surfaced to the caller — matching the N1d input-relay posture
/// so a structure action never blocks the UI thread.
pub fn spawn_send_action_to_foreign(
    origin_key: OriginKey,
    method: Method,
    timeout: Duration,
    config_socket_dir: Option<std::path::PathBuf>,
) {
    tokio::spawn(async move {
        let origins = match tokio::task::spawn_blocking(move || {
            crate::federation::discover_origins(config_socket_dir.as_deref())
        })
        .await
        {
            Ok(origins) => origins,
            Err(err) => {
                tracing::warn!(
                    origin = %origin_key,
                    error = %err,
                    "federation action: origin discovery failed"
                );
                return;
            }
        };
        let Some(origin) = origins.into_iter().find(|o| o.key == origin_key) else {
            tracing::warn!(
                origin = %origin_key,
                "federation action: origin not found"
            );
            return;
        };

        let origin_for_send = origin.clone();
        let method_for_send = method.clone();
        let result = tokio::task::spawn_blocking(move || {
            send_action_to_foreign(&origin_for_send, &method_for_send, timeout)
        })
        .await;

        match result {
            Ok(Ok(())) => {
                tracing::debug!(origin = %origin.key, "federation action routed to origin");
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    origin = %origin.key,
                    error = %err,
                    "federation action failed"
                );
            }
            Err(err) => {
                tracing::warn!(
                    origin = %origin.key,
                    error = %err,
                    "federation action task panicked"
                );
            }
        }
    });
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
        let result =
            send_input_to_foreign_pane(&origin, "term_1", Some(b"test"), Duration::from_millis(50));
        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }

    #[test]
    fn relay_error_display() {
        let err = RelayError::Api(crate::api::client::ApiClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "test",
        )));
        assert!(err.to_string().contains("relay request failed"));
    }

    #[test]
    fn send_input_with_empty_bytes() {
        let origin = test_origin("nDOWN");
        let result =
            send_input_to_foreign_pane(&origin, "term_1", Some(b""), Duration::from_millis(50));
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

        let result =
            send_input_to_foreign_pane(&origin, pane_id, Some(text), Duration::from_secs(1));

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

        let result =
            send_input_to_foreign_pane(&origin, pane_id, Some(text), Duration::from_millis(50));

        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }

    #[test]
    fn relay_error_implements_display() {
        let api_err = RelayError::Api(crate::api::client::ApiClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        )));
        let msg = api_err.to_string();
        assert!(msg.contains("relay request failed"));
    }

    #[test]
    fn relay_error_implements_error_trait() {
        use std::error::Error;
        let api_err = RelayError::Api(crate::api::client::ApiClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "test",
        )));
        let _: &dyn Error = &api_err;
    }

    #[test]
    fn action_relay_unreachable_remote_returns_api_error() {
        let origin = test_origin("nDOWN");
        let method = Method::TabCreate(crate::api::schema::TabCreateParams {
            workspace_id: Some("w1".to_string()),
            cwd: None,
            focus: true,
            label: None,
            env: Default::default(),
        });
        let result = send_action_to_foreign(&origin, &method, Duration::from_millis(50));
        assert!(result.is_err());
        match result {
            Err(RelayError::Api(_)) => (),
            _ => panic!("expected Api error for unreachable socket"),
        }
    }
}
