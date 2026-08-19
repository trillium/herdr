//! Federation phase **N3** — writable input path for foreign panes.
//!
//! For each foreign terminal whose remote protocol version matches the local
//! hub ([`crate::federation::LiveViewStatus::Attachable`]), the control manager
//! spawns one [`ForeignControlHandle`] that:
//!
//! 1. Connects to the remote server's *client* socket (derived from the
//!    origin's API socket by the caller via
//!    [`crate::server::socket_paths::derive_client_socket_from_api_socket`]).
//! 2. Performs the thin-client handshake
//!    ([`crate::protocol::ClientMessage::Hello`]).
//! 3. Sends [`crate::protocol::ClientMessage::ControlTerminal`] targeting the
//!    raw (non-namespaced) terminal id on the remote server, with `takeover`
//!    set to `true`.
//! 4. Forwards every input byte received from the hub's input routing layer
//!    as [`crate::protocol::ClientMessage::Input`] messages.
//!
//! The connection is write-only (control mode); it never reads frames from the
//! remote. Frame streaming is handled by the parallel observe connection. The
//! task retries automatically after connection loss, with a short delay between
//! attempts. Dropping the returned handle stops it.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::{
    ClientKeybindings, ClientLaunchMode, ClientMessage, FramingError, RenderEncoding,
    ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::terminal::TerminalId;

/// Command sent to a foreign controller over the control channel.
#[derive(Debug)]
pub enum ControlCommand {
    /// Raw PTY input bytes to forward to the remote terminal.
    Input(Vec<u8>),
    /// Inform the remote server of the new viewport dimensions for this
    /// terminal so it can issue TIOCSWINSZ to the PTY.
    Resize { cols: u16, rows: u16 },
}

/// Handle for a running foreign controller task.
///
/// Drop to abort the background task. The blocking socket-write thread may
/// linger until the remote server closes the connection, but no more input
/// will be forwarded to the remote.
pub struct ForeignControlHandle {
    /// Sender for control commands to forward to the remote terminal.
    pub tx: mpsc::Sender<ControlCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ForeignControlHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Channel capacity for the control sender. Kept small — input is
/// fire-and-forget; a full channel means we drop keystrokes rather than
/// build up lag.
const CONTROL_CHANNEL_CAP: usize = 64;

/// Spawn a background controller for a single foreign terminal.
///
/// `client_socket_path` is the remote server's *client* socket (not the JSON
/// API socket). `raw_terminal_id` is the un-namespaced id on the remote server
/// (e.g. `"term_1"`). `namespaced_terminal_id` is the hub-local `fed~<key>~<raw>`
/// form used as the key in the control senders map.
///
/// The task reconnects automatically on connection loss. Drop the handle or
/// abort the task to stop.
pub fn spawn_foreign_controller(
    client_socket_path: PathBuf,
    raw_terminal_id: TerminalId,
    namespaced_terminal_id: TerminalId,
    cols: u16,
    rows: u16,
) -> ForeignControlHandle {
    let (tx, rx) = mpsc::channel::<ControlCommand>(CONTROL_CHANNEL_CAP);
    let tx_for_handle = tx.clone();
    let task = tokio::spawn(async move {
        let mut rx = rx;
        loop {
            let socket_path = client_socket_path.clone();
            let raw_id = raw_terminal_id.clone();
            let namespaced_id = namespaced_terminal_id.clone();

            let result = tokio::task::spawn_blocking(move || {
                run_controller_session(&socket_path, &raw_id, &namespaced_id, cols, rows, rx)
            })
            .await;

            match result {
                Ok(Ok(returned_rx)) => {
                    debug!(
                        terminal_id = %namespaced_terminal_id,
                        "foreign controller: session ended normally"
                    );
                    rx = returned_rx;
                }
                Ok(Err((err, returned_rx))) => {
                    debug!(
                        terminal_id = %namespaced_terminal_id,
                        error = %err,
                        "federation controller: session error"
                    );
                    rx = returned_rx;
                }
                Err(_) => {
                    warn!(
                        terminal_id = %namespaced_terminal_id,
                        "foreign controller: blocking task panicked, stopping"
                    );
                    break;
                }
            }

            if tx.is_closed() {
                break;
            }

            // Brief delay before reconnect so we do not tight-loop on an
            // unreachable remote.
            tokio::time::sleep(Duration::from_secs(2)).await;

            if tx.is_closed() {
                break;
            }
        }

        debug!(
            terminal_id = %namespaced_terminal_id,
            "foreign controller: task exiting"
        );
    });

    ForeignControlHandle {
        tx: tx_for_handle,
        task,
    }
}

/// Connect to the remote client socket, handshake, enter control mode, and
/// forward input bytes from `rx` until the connection is lost or `rx` closes.
///
/// Returns `Ok(rx)` on clean shutdown so the receiver can be reused across
/// reconnects. Returns `Err((err, rx))` on failure.
///
/// Synchronous and blocking — run inside `tokio::task::spawn_blocking`.
#[allow(clippy::type_complexity)]
fn run_controller_session(
    client_socket_path: &std::path::Path,
    raw_terminal_id: &TerminalId,
    namespaced_terminal_id: &TerminalId,
    cols: u16,
    rows: u16,
    rx: mpsc::Receiver<ControlCommand>,
) -> Result<mpsc::Receiver<ControlCommand>, (ControlSessionError, mpsc::Receiver<ControlCommand>)> {
    use crate::protocol::{read_message, write_message};

    let mut stream = match crate::ipc::connect_local_stream(client_socket_path) {
        Ok(s) => s,
        Err(e) => return Err((ControlSessionError::Connect(e), rx)),
    };

    // Handshake: Hello
    if let Err(e) = write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::TerminalAttach,
        },
    ) {
        return Err((ControlSessionError::Framing(e), rx));
    }

    // Read Welcome
    let welcome: ServerMessage = match read_message(&mut stream, MAX_FRAME_SIZE) {
        Ok(msg) => msg,
        Err(e) => return Err((ControlSessionError::Framing(e), rx)),
    };
    match welcome {
        ServerMessage::Welcome {
            error: Some(err), ..
        } => {
            return Err((ControlSessionError::Rejected(err), rx));
        }
        ServerMessage::Welcome { version, .. } => {
            if version != PROTOCOL_VERSION {
                return Err((
                    ControlSessionError::ProtocolMismatch {
                        remote: version,
                        local: PROTOCOL_VERSION,
                    },
                    rx,
                ));
            }
            info!(
                terminal_id = %namespaced_terminal_id,
                "foreign controller: connected (protocol {version})"
            );
        }
        other => {
            return Err((
                ControlSessionError::UnexpectedMessage(format!("{other:?}")),
                rx,
            ));
        }
    }

    // Switch into control mode
    if let Err(e) = write_message(
        &mut stream,
        &ClientMessage::ControlTerminal {
            target: raw_terminal_id.as_str().to_owned(),
            takeover: true,
        },
    ) {
        return Err((ControlSessionError::Framing(e), rx));
    }

    // Forward commands until the connection is lost or the channel closes.
    let mut rx = rx;
    while let Some(cmd) = rx.blocking_recv() {
        let msg = match cmd {
            ControlCommand::Input(data) => ClientMessage::Input { data },
            ControlCommand::Resize { cols, rows } => ClientMessage::Resize {
                cols,
                rows,
                cell_width_px: 0,
                cell_height_px: 0,
            },
        };
        if write_message(&mut stream, &msg).is_err() {
            debug!(
                terminal_id = %namespaced_terminal_id,
                "foreign controller: write failed, connection lost"
            );
            break;
        }
    }

    Ok(rx)
}

#[derive(Debug)]
enum ControlSessionError {
    Connect(std::io::Error),
    Framing(FramingError),
    Rejected(String),
    ProtocolMismatch { remote: u32, local: u32 },
    UnexpectedMessage(String),
}

impl std::fmt::Display for ControlSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlSessionError::Connect(e) => write!(f, "connect failed: {e}"),
            ControlSessionError::Framing(e) => write!(f, "framing error: {e}"),
            ControlSessionError::Rejected(msg) => write!(f, "server rejected: {msg}"),
            ControlSessionError::ProtocolMismatch { remote, local } => {
                write!(f, "protocol mismatch: remote={remote} local={local}")
            }
            ControlSessionError::UnexpectedMessage(msg) => {
                write!(f, "unexpected message: {msg}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_session_error_display_variants() {
        let connect = ControlSessionError::Connect(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such socket",
        ));
        assert!(connect.to_string().contains("connect failed"), "{connect}");

        let framing = ControlSessionError::Framing(FramingError::UnexpectedEof);
        assert!(framing.to_string().contains("framing error"), "{framing}");

        let rejected = ControlSessionError::Rejected("bad".to_string());
        assert!(
            rejected.to_string().contains("server rejected"),
            "{rejected}"
        );

        let mismatch = ControlSessionError::ProtocolMismatch {
            remote: 5,
            local: 20,
        };
        let mismatch = mismatch.to_string();
        assert!(mismatch.contains("protocol mismatch"), "{mismatch}");
        assert!(mismatch.contains('5'), "{mismatch}");
        assert!(mismatch.contains("20"), "{mismatch}");

        let unexpected = ControlSessionError::UnexpectedMessage("wat".to_string());
        assert!(
            unexpected.to_string().contains("unexpected message"),
            "{unexpected}"
        );
    }
}
