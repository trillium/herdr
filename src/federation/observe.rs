//! Federation phase **N3** — live frame streaming for foreign panes.
//!
//! For each foreign terminal whose remote protocol version matches the local
//! hub ([`crate::federation::LiveViewStatus::Attachable`]), the observe manager
//! spawns one [`ForeignObserveHandle`] that:
//!
//! 1. Connects to the remote server's *client* socket (derived from the
//!    origin's API socket by the caller via
//!    [`crate::server::socket_paths::derive_client_socket_from_api_socket`]).
//! 2. Performs the thin-client handshake
//!    ([`crate::protocol::ClientMessage::Hello`]).
//! 3. Sends [`crate::protocol::ClientMessage::ObserveTerminal`] targeting the
//!    raw (non-namespaced) terminal id on the remote server.
//! 4. Forwards every [`crate::protocol::ServerMessage::Frame`] to the hub's
//!    run loop via `foreign_frame_tx`, which stores it in
//!    [`crate::app::AppState::foreign_frames`] for render.
//!
//! The connection is read-only (observe mode); it never sends input or resizes
//! the remote terminal. The task retries automatically after connection loss,
//! with a short delay between attempts. Dropping the returned handle stops it.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::{
    ClientKeybindings, ClientLaunchMode, ClientMessage, FrameData, FramingError, RenderEncoding,
    ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::terminal::TerminalId;

/// A frame received from a foreign server's observe stream, ready to cache and
/// render in the local hub's pane.
pub struct ForeignFrame {
    /// Namespaced terminal id (`fed~<key>~<raw>`) used as the key in
    /// [`crate::app::AppState::foreign_frames`].
    pub terminal_id: TerminalId,
    /// The rendered cell grid from the remote server.
    pub frame: FrameData,
}

/// Handle for a running foreign observer task.
///
/// Drop to abort the background task. The blocking socket-read thread may
/// linger until the remote server closes the connection, but no more frames
/// will be forwarded to the hub.
pub struct ForeignObserveHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ForeignObserveHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn a background observer for a single foreign terminal.
///
/// `client_socket_path` is the remote server's *client* socket (not the JSON
/// API socket). `raw_terminal_id` is the un-namespaced id on the remote server
/// (e.g. `"term_1"`). `namespaced_terminal_id` is the hub-local `fed~<key>~<raw>`
/// form stored in [`crate::app::AppState::foreign_frames`].
///
/// The task reconnects automatically on connection loss. Drop the handle or
/// abort the task to stop.
pub fn spawn_foreign_observer(
    client_socket_path: PathBuf,
    raw_terminal_id: TerminalId,
    namespaced_terminal_id: TerminalId,
    cols: u16,
    rows: u16,
    tx: mpsc::Sender<ForeignFrame>,
) -> ForeignObserveHandle {
    let task = tokio::spawn(async move {
        loop {
            let socket_path = client_socket_path.clone();
            let raw_id = raw_terminal_id.clone();
            let namespaced_id = namespaced_terminal_id.clone();
            let frame_tx = tx.clone();

            let result = tokio::task::spawn_blocking(move || {
                run_observer_session(&socket_path, &raw_id, &namespaced_id, cols, rows, frame_tx)
            })
            .await;

            match result {
                Ok(Ok(())) => {
                    debug!(terminal_id = %namespaced_terminal_id, "foreign observer: session ended normally");
                }
                Ok(Err(err)) => {
                    debug!(terminal_id = %namespaced_terminal_id, error = %err, "foreign observer: session error");
                }
                Err(_) => {
                    warn!(terminal_id = %namespaced_terminal_id, "foreign observer: blocking task panicked, stopping");
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

        debug!(terminal_id = %namespaced_terminal_id, "foreign observer: task exiting");
    });

    ForeignObserveHandle { task }
}

/// Connect to the remote client socket, handshake, enter observe mode, and
/// forward frames to `tx` until the connection is lost or `tx` closes.
///
/// Synchronous and blocking — run inside `tokio::task::spawn_blocking`.
fn run_observer_session(
    client_socket_path: &std::path::Path,
    raw_terminal_id: &TerminalId,
    namespaced_terminal_id: &TerminalId,
    cols: u16,
    rows: u16,
    tx: mpsc::Sender<ForeignFrame>,
) -> Result<(), ObserveSessionError> {
    use crate::protocol::{read_message, write_message};

    let mut stream = crate::ipc::connect_local_stream(client_socket_path)
        .map_err(ObserveSessionError::Connect)?;

    // Handshake: Hello
    write_message(
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
    )
    .map_err(ObserveSessionError::Framing)?;

    // Read Welcome
    let welcome: ServerMessage =
        read_message(&mut stream, MAX_FRAME_SIZE).map_err(ObserveSessionError::Framing)?;
    match welcome {
        ServerMessage::Welcome {
            error: Some(err), ..
        } => {
            return Err(ObserveSessionError::Rejected(err));
        }
        ServerMessage::Welcome { version, .. } => {
            if version != PROTOCOL_VERSION {
                return Err(ObserveSessionError::ProtocolMismatch {
                    remote: version,
                    local: PROTOCOL_VERSION,
                });
            }
            info!(
                terminal_id = %namespaced_terminal_id,
                "foreign observer: connected (protocol {version})"
            );
        }
        other => {
            return Err(ObserveSessionError::UnexpectedMessage(format!("{other:?}")));
        }
    }

    // Switch into observe mode
    write_message(
        &mut stream,
        &ClientMessage::ObserveTerminal {
            target: raw_terminal_id.as_str().to_owned(),
        },
    )
    .map_err(ObserveSessionError::Framing)?;

    // Read frames until the connection is lost or the hub channel closes.
    loop {
        if tx.is_closed() {
            break;
        }

        let msg: ServerMessage = match read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(msg) => msg,
            Err(FramingError::UnexpectedEof) => {
                debug!(
                    terminal_id = %namespaced_terminal_id,
                    "foreign observer: connection closed by remote server"
                );
                break;
            }
            Err(err) => return Err(ObserveSessionError::Framing(err)),
        };

        match msg {
            ServerMessage::Frame(frame) => {
                let ff = ForeignFrame {
                    terminal_id: namespaced_terminal_id.clone(),
                    frame,
                };
                // Non-blocking send: drop the frame if the hub is busy rather
                // than stalling the blocking read thread and building up lag.
                if tx.try_send(ff).is_err() && tx.is_closed() {
                    break;
                }
            }
            ServerMessage::ServerShutdown { reason } => {
                debug!(
                    terminal_id = %namespaced_terminal_id,
                    ?reason,
                    "foreign observer: remote server shutting down"
                );
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug)]
enum ObserveSessionError {
    Connect(std::io::Error),
    Framing(FramingError),
    Rejected(String),
    ProtocolMismatch { remote: u32, local: u32 },
    UnexpectedMessage(String),
}

impl std::fmt::Display for ObserveSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObserveSessionError::Connect(e) => write!(f, "connect failed: {e}"),
            ObserveSessionError::Framing(e) => write!(f, "framing error: {e}"),
            ObserveSessionError::Rejected(msg) => write!(f, "server rejected: {msg}"),
            ObserveSessionError::ProtocolMismatch { remote, local } => {
                write!(f, "protocol mismatch: remote={remote} local={local}")
            }
            ObserveSessionError::UnexpectedMessage(msg) => {
                write!(f, "unexpected message: {msg}")
            }
        }
    }
}
