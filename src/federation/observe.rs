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

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;

    #[test]
    fn observe_session_error_display_variants() {
        let connect = ObserveSessionError::Connect(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such socket",
        ));
        assert!(connect.to_string().contains("connect failed"), "{connect}");

        let framing = ObserveSessionError::Framing(FramingError::UnexpectedEof);
        assert!(framing.to_string().contains("framing error"), "{framing}");

        let rejected = ObserveSessionError::Rejected("bad".to_string());
        assert!(
            rejected.to_string().contains("server rejected"),
            "{rejected}"
        );

        let mismatch = ObserveSessionError::ProtocolMismatch {
            remote: 5,
            local: 20,
        };
        let mismatch = mismatch.to_string();
        assert!(mismatch.contains("protocol mismatch"), "{mismatch}");
        assert!(mismatch.contains('5'), "{mismatch}");
        assert!(mismatch.contains("20"), "{mismatch}");

        let unexpected = ObserveSessionError::UnexpectedMessage("wat".to_string());
        assert!(
            unexpected.to_string().contains("unexpected message"),
            "{unexpected}"
        );
    }

    /// Start a mock server that speaks the observe protocol:
    /// 1. Reads Hello, validates version
    /// 2. Sends Welcome
    /// 3. Reads ObserveTerminal
    /// 4. Sends N frames, then closes
    fn start_mock_observe_server(
        socket_path: &std::path::Path,
        frames: Vec<FrameData>,
    ) -> std::thread::JoinHandle<()> {
        use crate::protocol::{read_message, write_message};
        use interprocess::local_socket::traits::Listener as _;

        let socket_path = socket_path.to_path_buf();
        std::thread::spawn(move || {
            let listener =
                crate::ipc::bind_local_listener(&socket_path).expect("mock server: bind listener");
            let mut stream = listener.accept().expect("mock server: accept");

            // Read Hello
            let hello: ClientMessage = read_message(&mut stream, crate::protocol::MAX_FRAME_SIZE)
                .expect("mock server: read Hello");
            match hello {
                ClientMessage::Hello { version, .. } => {
                    assert_eq!(
                        version, PROTOCOL_VERSION,
                        "mock server: protocol version mismatch"
                    );
                }
                other => panic!("mock server: expected Hello, got {other:?}"),
            }

            // Send Welcome
            write_message(
                &mut stream,
                &ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: crate::protocol::RenderEncoding::SemanticFrame,
                    error: None,
                },
            )
            .expect("mock server: send Welcome");

            // Read ObserveTerminal
            let observe: ClientMessage = read_message(&mut stream, crate::protocol::MAX_FRAME_SIZE)
                .expect("mock server: read ObserveTerminal");
            match observe {
                ClientMessage::ObserveTerminal { target } => {
                    assert_eq!(target, "term_1", "mock server: unexpected target");
                }
                other => panic!("mock server: expected ObserveTerminal, got {other:?}"),
            }

            // Send frames
            for frame in frames {
                write_message(&mut stream, &ServerMessage::Frame(frame))
                    .expect("mock server: send Frame");
            }

            // Connection closes when stream is dropped
        })
    }

    #[test]
    fn observer_receives_frames_from_mock_server() {
        let dir = std::env::temp_dir().join(format!("herdr-observe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let socket_path = dir.join("mock-observe.sock");

        // Build a frame with known content.
        let area = ratatui::layout::Rect::new(0, 0, 3, 2);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));
        buffer.cell_mut((0, 0)).unwrap().set_symbol("H");
        buffer.cell_mut((1, 0)).unwrap().set_symbol("I");
        let frame = FrameData::from_ratatui_buffer(&buffer, None);

        // Start mock server that sends one frame.
        let server = start_mock_observe_server(&socket_path, vec![frame]);

        // Give the server time to start listening.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Create a tokio channel to receive frames.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ForeignFrame>(16);

        // Run the observer session directly (blocking).
        let terminal_id = TerminalId::from_string("term_1");
        let namespaced_id = TerminalId::from_string("fed~nTEST~term_1");
        let result = run_observer_session(&socket_path, &terminal_id, &namespaced_id, 80, 24, tx);

        // Verify the session completed without error.
        result.expect("observer session must succeed");

        // Verify we received the frame.
        let received = rx.try_recv().expect("must receive one frame");
        assert_eq!(received.terminal_id, namespaced_id);
        assert_eq!(received.frame.width, 3);
        assert_eq!(received.frame.height, 2);

        // Verify the frame content survived the roundtrip.
        let restored = received
            .frame
            .to_ratatui_buffer()
            .expect("frame converts to buffer");
        assert_eq!(restored.cell((0, 0)).unwrap().symbol(), "H");
        assert_eq!(restored.cell((1, 0)).unwrap().symbol(), "I");

        // Clean up.
        server.join().expect("mock server thread must not panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observer_handles_server_rejection() {
        use crate::protocol::write_message;

        let dir = std::env::temp_dir().join(format!("herdr-observe-reject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let socket_path = dir.join("mock-reject.sock");

        let socket_path_clone = socket_path.clone();
        let server = std::thread::spawn(move || {
            let listener =
                crate::ipc::bind_local_listener(&socket_path_clone).expect("bind listener");
            let mut stream = listener.accept().expect("accept");

            // Read Hello
            let _hello: ClientMessage =
                crate::protocol::read_message(&mut stream, crate::protocol::MAX_FRAME_SIZE)
                    .expect("read Hello");

            // Send rejection
            write_message(
                &mut stream,
                &ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: crate::protocol::RenderEncoding::SemanticFrame,
                    error: Some("test rejection".to_string()),
                },
            )
            .expect("send rejection");
        });

        std::thread::sleep(std::time::Duration::from_millis(50));

        let (tx, _rx) = tokio::sync::mpsc::channel::<ForeignFrame>(16);
        let terminal_id = TerminalId::from_string("term_1");
        let namespaced_id = TerminalId::from_string("fed~nTEST~term_1");
        let result = run_observer_session(&socket_path, &terminal_id, &namespaced_id, 80, 24, tx);

        match result {
            Err(ObserveSessionError::Rejected(msg)) => {
                assert!(msg.contains("test rejection"), "rejection message: {msg}");
            }
            other => panic!("expected Rejected error, got {other:?}"),
        }

        server.join().expect("server thread must not panic");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
