//! Federation phase **N3** — per-pane `ObserveTerminal` live frame streaming.
//!
//! N2 Part 2 ([`super::live_view`]) computes whether a foreign pane's origin
//! speaks this hub's wire protocol. This module acts on that verdict: for every
//! foreign pane whose origin is [`super::LiveViewStatus::Attachable`], it opens
//! a read-only strict-wire connection to that origin's client socket, switches
//! it into `ObserveTerminal` mode, and streams the remote's structured
//! [`FrameData`] back to the hub's run loop. The hub then blits those cells into
//! the pane rect (RENDER-FROM-GRID: raw remote ANSI is never piped to stdout).
//!
//! ## Shape
//!
//! One long-lived **manager** task owns a map of **worker** connections keyed by
//! namespaced terminal id. The run loop republishes the full desired
//! [`ObserveSpec`] set whenever the projection changes (each snapshot poll
//! tick); the manager reconciles that set against its workers — spawning
//! connections for newly-attachable panes, retiring connections whose pane
//! disappeared, whose target/geometry changed, or whose socket already dropped.
//! A dropped connection is therefore re-established by the next poll-driven
//! refresh rather than by an inner retry loop.
//!
//! ## Blocking
//!
//! [`crate::ipc::connect_local_stream`] and the wire framing helpers are
//! synchronous, so each worker runs on `tokio::task::spawn_blocking`. A blocking
//! task cannot be aborted, so shutdown is cooperative: workers observe an
//! [`AtomicBool`] between frames and use a short receive timeout so an idle
//! origin cannot pin a worker past the flag. The timeout is applied only while
//! waiting for the *first* byte of a length prefix — the rest of a frame is read
//! with the timeout cleared, so a slow origin can never tear a frame.
//!
//! ## Failure posture
//!
//! Every failure here is logged and contained. A refused socket, a protocol
//! rejection, a torn connection, or a malformed frame retires that one pane's
//! worker; it never propagates to the UI, never blocks the run loop, and never
//! affects another origin's panes.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use tokio::sync::{mpsc, Notify};

use crate::ipc::LocalStream;
use crate::protocol::{
    self, ClientKeybindings, ClientLaunchMode, ClientMessage, FrameData, FramingError,
    RenderEncoding, ServerMessage, MAX_FRAME_SIZE, MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::terminal::TerminalId;

use super::origin::{ConnectionTarget, Origin, OriginKey};

/// Frame geometry requested for a foreign pane that has not been laid out
/// locally yet (a pane in a background workspace, or the first tick after it
/// appears). The hub clips or pads the streamed frame to the real pane rect, and
/// the next spec refresh re-opens the connection at the true size.
pub const DEFAULT_OBSERVE_COLS: u16 = 120;
/// Row counterpart of [`DEFAULT_OBSERVE_COLS`].
pub const DEFAULT_OBSERVE_ROWS: u16 = 40;

/// How long a worker blocks waiting for the first byte of a frame before
/// re-checking its stop flag. Bounds shutdown latency for an idle origin.
const OBSERVE_IDLE_POLL: Duration = Duration::from_millis(250);

/// Bound on the origin's `Welcome` reply so an unresponsive remote cannot pin a
/// blocking thread through the handshake.
const OBSERVE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on simultaneous live-view connections across the whole fleet.
/// Each one makes the origin render that pane continuously, so this caps the
/// remote-side cost a single hub can impose.
const MAX_OBSERVE_CONNECTIONS: usize = 16;

/// One foreign pane the hub wants a live view of.
///
/// Equality is the reconcile key: any change to the remote target or the
/// requested geometry retires the existing connection and opens a new one at the
/// new size, because the origin fixes an observe client's render area at
/// handshake time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveSpec {
    /// Origin-namespaced local terminal id (`fed~<key>~<raw>`); the address the
    /// streamed frame is applied to in `AppState`.
    pub terminal_id: TerminalId,
    /// Owning origin's durable key, resolved to a socket by the manager.
    pub origin: OriginKey,
    /// The remote's own (non-namespaced) terminal id, sent as the
    /// `ObserveTerminal` target.
    pub target: String,
    /// Frame width requested from the origin.
    pub cols: u16,
    /// Frame height requested from the origin.
    pub rows: u16,
}

/// Failure of a single pane's live-view connection. Logged and contained by the
/// worker; never surfaced to the UI.
#[derive(Debug)]
pub enum ObserveError {
    /// The origin's client socket could not be reached.
    Connect(io::Error),
    /// The origin refused the handshake or negotiated something unusable.
    Handshake(String),
    /// Wire framing failed (write, decode, or oversized frame).
    Protocol(FramingError),
    /// Socket I/O failed mid-stream.
    Io(io::Error),
}

impl std::fmt::Display for ObserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObserveError::Connect(err) => write!(f, "observe connect failed: {err}"),
            ObserveError::Handshake(reason) => write!(f, "observe handshake failed: {reason}"),
            ObserveError::Protocol(err) => write!(f, "observe framing failed: {err}"),
            ObserveError::Io(err) => write!(f, "observe stream failed: {err}"),
        }
    }
}

impl std::error::Error for ObserveError {}

/// A running per-pane connection.
struct ObserveWorker {
    /// The spec this worker was opened for; compared against the desired set to
    /// decide whether it can be kept.
    spec: ObserveSpec,
    /// Cooperative stop flag — the only way to end a blocking task.
    stop: Arc<AtomicBool>,
    /// Used solely to notice a connection that already ended on its own, so the
    /// next reconcile re-opens it.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ObserveWorker {
    /// Ask the connection to finish. It observes the flag within
    /// [`OBSERVE_IDLE_POLL`] (or when the origin next sends anything, on
    /// platforms without receive timeouts) and closes its socket on the way out.
    ///
    /// This lives in `Drop` rather than a consuming `stop()` on purpose: a
    /// blocking task cannot be aborted, and the manager task itself *can* be
    /// (`FederationObserveHandle::stop`). Tying the signal to drop means every
    /// path that discards a worker — reconcile, manager exit, or an abort that
    /// unwinds the map — stops the underlying connection.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Spawn the federation live-view manager.
///
/// `specs_rx` carries the full desired observe set on every change (never a
/// delta). `frame_tx` delivers `(namespaced terminal id, frame)` to the run
/// loop. `shutdown` ends the manager and every worker it owns, so a runtime
/// federation disable performs no further live-view network I/O.
pub fn spawn_observe_manager(
    mut specs_rx: mpsc::Receiver<Vec<ObserveSpec>>,
    frame_tx: mpsc::Sender<(TerminalId, FrameData)>,
    shutdown: Arc<Notify>,
    config_socket_dir: Option<PathBuf>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Discover origins once at task start, matching the poll task: the
        // subprocess runs off the reactor and the result keys socket lookups.
        let discovered = match tokio::task::spawn_blocking(move || {
            super::discover_origins(config_socket_dir.as_deref())
        })
        .await
        {
            Ok(origins) => origins,
            Err(err) => {
                tracing::warn!(%err, "federation observe: origin discovery failed; live view idle");
                Vec::new()
            }
        };
        let origins: HashMap<OriginKey, Origin> = discovered
            .into_iter()
            .map(|origin| (origin.key.clone(), origin))
            .collect();
        tracing::debug!(
            origins = origins.len(),
            "federation observe: manager started"
        );

        let mut workers: HashMap<TerminalId, ObserveWorker> = HashMap::new();
        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                maybe_specs = specs_rx.recv() => match maybe_specs {
                    Some(specs) => reconcile_workers(&mut workers, &specs, &origins, &frame_tx),
                    // The app holds a live sender for its whole lifetime, so this
                    // only fires once the app itself is gone.
                    None => break,
                },
            }
        }

        // Dropping the workers signals every connection to close.
        workers.clear();
        tracing::debug!("federation observe: manager stopped");
    })
}

/// Bring the running worker set in line with `specs`.
///
/// Retires first (so a geometry change frees its slot before the replacement is
/// counted against [`MAX_OBSERVE_CONNECTIONS`]), then opens connections in the
/// caller's order, which is deterministic — so which panes win the cap does not
/// vary run to run.
fn reconcile_workers(
    workers: &mut HashMap<TerminalId, ObserveWorker>,
    specs: &[ObserveSpec],
    origins: &HashMap<OriginKey, Origin>,
    frame_tx: &mpsc::Sender<(TerminalId, FrameData)>,
) {
    let desired: HashMap<&TerminalId, &ObserveSpec> =
        specs.iter().map(|spec| (&spec.terminal_id, spec)).collect();

    let retire: Vec<TerminalId> = workers
        .iter()
        .filter(|(terminal_id, worker)| {
            desired
                .get(*terminal_id)
                .is_none_or(|spec| **spec != worker.spec)
                || worker.task.is_finished()
        })
        .map(|(terminal_id, _)| terminal_id.clone())
        .collect();
    for terminal_id in retire {
        // Removing the worker drops it, which signals its connection to close.
        workers.remove(&terminal_id);
    }

    for spec in specs {
        if workers.contains_key(&spec.terminal_id) {
            continue;
        }
        if workers.len() >= MAX_OBSERVE_CONNECTIONS {
            tracing::warn!(
                max = MAX_OBSERVE_CONNECTIONS,
                terminal = %spec.terminal_id.as_str(),
                "federation observe: connection cap reached; pane stays on the status placeholder"
            );
            break;
        }
        let Some(origin) = origins.get(&spec.origin) else {
            tracing::debug!(
                origin = %spec.origin,
                terminal = %spec.terminal_id.as_str(),
                "federation observe: no discovered origin for pane; skipping live view"
            );
            continue;
        };
        workers.insert(
            spec.terminal_id.clone(),
            spawn_worker(origin.clone(), spec.clone(), frame_tx.clone()),
        );
    }
}

/// Open one pane's connection on the blocking pool.
fn spawn_worker(
    origin: Origin,
    spec: ObserveSpec,
    frame_tx: mpsc::Sender<(TerminalId, FrameData)>,
) -> ObserveWorker {
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = stop.clone();
    let task_spec = spec.clone();
    let task = tokio::task::spawn_blocking(move || {
        match run_observe_connection(&origin, &task_spec, &frame_tx, &task_stop) {
            Ok(()) => tracing::debug!(
                origin = %origin.key,
                terminal = %task_spec.terminal_id.as_str(),
                "federation observe: live view ended"
            ),
            Err(err) => tracing::warn!(
                origin = %origin.key,
                terminal = %task_spec.terminal_id.as_str(),
                error = %err,
                "federation observe: live view dropped"
            ),
        }
    });
    ObserveWorker { spec, stop, task }
}

/// Connect, handshake, switch to observe mode, and stream frames until the stop
/// flag is set or the origin ends the connection.
///
/// Synchronous and blocking by design (see the module docs). Returns `Ok(())`
/// for every ordinary end-of-stream — a stop request, a peer close, a server
/// shutdown notice, or a run loop that went away — and an error only for a real
/// transport or protocol failure.
fn run_observe_connection(
    origin: &Origin,
    spec: &ObserveSpec,
    frame_tx: &mpsc::Sender<(TerminalId, FrameData)>,
    stop: &AtomicBool,
) -> Result<(), ObserveError> {
    // Infallible today because a local socket is the only transport; a second
    // variant would surface here as a compile error, which is the intent.
    let ConnectionTarget::LocalSocket(path) = &origin.target;
    let mut stream = crate::ipc::connect_local_stream(path).map_err(ObserveError::Connect)?;
    stream.set_nonblocking(false).map_err(ObserveError::Io)?;

    handshake(&mut stream, spec)?;

    protocol::write_message(
        &mut stream,
        &ClientMessage::ObserveTerminal {
            target: spec.target.clone(),
        },
    )
    .map_err(ObserveError::Protocol)?;

    tracing::info!(
        origin = %origin.key,
        terminal = %spec.terminal_id.as_str(),
        target = %spec.target,
        cols = spec.cols,
        rows = spec.rows,
        "federation observe: live view attached"
    );

    let idle_timeout_active = set_idle_recv_timeout(&stream);
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(message) = read_next_message(&mut stream, stop, idle_timeout_active)? else {
            return Ok(());
        };
        match message {
            ServerMessage::Frame(frame) => {
                // Every semantic frame is a complete screen, so ordinary
                // backpressure here is safe: the run loop drains this channel in
                // its select, and a closed receiver means the app is gone.
                if frame_tx
                    .blocking_send((spec.terminal_id.clone(), frame))
                    .is_err()
                {
                    return Ok(());
                }
            }
            ServerMessage::ServerShutdown { reason } => {
                tracing::info!(
                    origin = %origin.key,
                    terminal = %spec.terminal_id.as_str(),
                    reason = reason.as_deref().unwrap_or("none"),
                    "federation observe: origin ended the live view"
                );
                return Ok(());
            }
            // An observe connection negotiates semantic frames, so terminal-ANSI
            // frames, graphics, and control traffic are not expected; ignore them
            // rather than tearing down a working stream.
            _ => {}
        }
    }
}

/// Perform the client handshake against a remote origin.
///
/// Requests [`RenderEncoding::SemanticFrame`] so the origin streams structured
/// [`FrameData`] the hub can blit, and identifies as
/// [`ClientLaunchMode::TerminalAttach`] so the origin does not treat this
/// connection as a full app client driving its own UI.
fn handshake(stream: &mut LocalStream, spec: &ObserveSpec) -> Result<(), ObserveError> {
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        cols: spec.cols,
        rows: spec.rows,
        cell_width_px: 0,
        cell_height_px: 0,
        requested_encoding: RenderEncoding::SemanticFrame,
        keybindings: ClientKeybindings::Server,
        launch_mode: ClientLaunchMode::TerminalAttach,
    };
    protocol::write_message(stream, &hello).map_err(ObserveError::Protocol)?;

    let timeout_active = stream
        .set_recv_timeout(Some(OBSERVE_HANDSHAKE_TIMEOUT))
        .is_ok();
    let welcome: Result<ServerMessage, FramingError> =
        protocol::read_message(stream, MAX_FRAME_SIZE);
    if timeout_active {
        // Clear before streaming; the read loop installs its own idle timeout.
        let _ = stream.set_recv_timeout(None);
    }

    match welcome.map_err(ObserveError::Protocol)? {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ObserveError::Handshake(format!(
                    "origin rejected live view (remote protocol {version}): {error}"
                )));
            }
            if encoding != RenderEncoding::SemanticFrame {
                return Err(ObserveError::Handshake(format!(
                    "origin negotiated {encoding:?}, but the live view needs semantic frames"
                )));
            }
            Ok(())
        }
        other => Err(ObserveError::Handshake(format!(
            "expected Welcome, got {other:?}"
        ))),
    }
}

/// Install the idle receive timeout, reporting whether it took effect.
///
/// Platforms that do not support it still work — shutdown then waits for the
/// origin's next message instead of the poll interval — so this degrades rather
/// than failing the connection.
fn set_idle_recv_timeout(stream: &LocalStream) -> bool {
    match stream.set_recv_timeout(Some(OBSERVE_IDLE_POLL)) {
        Ok(()) => true,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "federation observe: idle receive timeout unavailable; shutdown waits on origin activity"
            );
            false
        }
    }
}

/// Read the next server message, waking periodically to honour `stop`.
///
/// Returns `Ok(None)` when the worker should end without error: the stop flag
/// was set while idle, or the origin closed the connection. The idle timeout
/// covers only the first byte of the length prefix; it is cleared for the rest
/// of the frame so a timeout can never leave a half-read message on the wire.
fn read_next_message(
    stream: &mut LocalStream,
    stop: &AtomicBool,
    idle_timeout_active: bool,
) -> Result<Option<ServerMessage>, ObserveError> {
    let first_prefix_byte = loop {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => break byte[0],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if idle_timeout_active && is_recv_timeout(&err) => continue,
            Err(err) => return Err(ObserveError::Io(err)),
        }
    };

    if idle_timeout_active {
        // Frame body must be read whole: a timeout here would tear it.
        let _ = stream.set_recv_timeout(None);
    }
    let message =
        protocol::read_message_with_prefix_byte(stream, first_prefix_byte, MAX_GRAPHICS_FRAME_SIZE);
    if idle_timeout_active {
        let _ = stream.set_recv_timeout(Some(OBSERVE_IDLE_POLL));
    }

    match message {
        Ok(message) => Ok(Some(message)),
        Err(FramingError::UnexpectedEof) => Ok(None),
        Err(err) => Err(ObserveError::Protocol(err)),
    }
}

/// Whether an error is the expected "nothing arrived before the timeout".
/// Unix reports `WouldBlock`, Windows reports `TimedOut`.
fn is_recv_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec(terminal: &str, origin: &str, cols: u16, rows: u16) -> ObserveSpec {
        ObserveSpec {
            terminal_id: TerminalId::from_string(format!("fed~{origin}~{terminal}")),
            origin: OriginKey::new(origin).expect("valid test origin key"),
            target: terminal.to_string(),
            cols,
            rows,
        }
    }

    fn origin(key: &str) -> Origin {
        Origin::new(
            OriginKey::new(key).expect("valid test origin key"),
            key,
            ConnectionTarget::LocalSocket(PathBuf::from(format!("/nonexistent/{key}.sock"))),
        )
    }

    #[test]
    fn unreachable_origin_reports_connect_error() {
        let origin = origin("nDOWN");
        let spec = spec("term_1", "nDOWN", 80, 24);
        let (tx, _rx) = mpsc::channel(1);
        let stop = AtomicBool::new(false);

        let result = run_observe_connection(&origin, &spec, &tx, &stop);

        match result {
            Err(ObserveError::Connect(_)) => (),
            other => panic!("expected Connect error for a dead socket, got {other:?}"),
        }
    }

    #[test]
    fn error_display_names_the_failing_stage() {
        let connect =
            ObserveError::Connect(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        assert!(connect.to_string().contains("observe connect failed"));

        let handshake = ObserveError::Handshake("protocol mismatch".to_string());
        assert!(handshake.to_string().contains("observe handshake failed"));
        assert!(handshake.to_string().contains("protocol mismatch"));

        let stream = ObserveError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "gone"));
        assert!(stream.to_string().contains("observe stream failed"));
    }

    #[test]
    fn geometry_change_makes_specs_unequal_so_the_connection_is_reopened() {
        // The origin fixes an observe client's render area at handshake time, so
        // a resized pane must not keep its old connection.
        let before = spec("term_1", "n1", 80, 24);
        let after = spec("term_1", "n1", 120, 40);
        assert_ne!(before, after);
        assert_eq!(before, spec("term_1", "n1", 80, 24));
    }

    #[tokio::test]
    async fn reconcile_retires_workers_whose_pane_disappeared() {
        // Both origins are dead sockets, so the workers fail fast; what is under
        // test is the bookkeeping, not the transport.
        let origins: HashMap<OriginKey, Origin> = [origin("n1")]
            .into_iter()
            .map(|origin| (origin.key.clone(), origin))
            .collect();
        let (tx, _rx) = mpsc::channel(4);
        let mut workers = HashMap::new();

        let first = spec("term_1", "n1", 80, 24);
        let second = spec("term_2", "n1", 80, 24);
        reconcile_workers(&mut workers, &[first.clone(), second], &origins, &tx);
        assert_eq!(workers.len(), 2, "one worker per attachable pane");

        reconcile_workers(&mut workers, std::slice::from_ref(&first), &origins, &tx);
        assert_eq!(workers.len(), 1, "the removed pane's worker is retired");
        assert!(workers.contains_key(&first.terminal_id));

        reconcile_workers(&mut workers, &[], &origins, &tx);
        assert!(workers.is_empty(), "an empty projection retires everything");
    }

    #[tokio::test]
    async fn reconcile_skips_panes_whose_origin_was_not_discovered() {
        let origins: HashMap<OriginKey, Origin> = HashMap::new();
        let (tx, _rx) = mpsc::channel(4);
        let mut workers = HashMap::new();

        reconcile_workers(&mut workers, &[spec("term_1", "n1", 80, 24)], &origins, &tx);

        assert!(
            workers.is_empty(),
            "an undiscovered origin must not spawn a connection"
        );
    }

    #[tokio::test]
    async fn reconcile_caps_simultaneous_connections() {
        let origins: HashMap<OriginKey, Origin> = [origin("n1")]
            .into_iter()
            .map(|origin| (origin.key.clone(), origin))
            .collect();
        let (tx, _rx) = mpsc::channel(4);
        let mut workers = HashMap::new();

        let specs: Vec<ObserveSpec> = (0..MAX_OBSERVE_CONNECTIONS + 5)
            .map(|i| spec(&format!("term_{i}"), "n1", 80, 24))
            .collect();
        reconcile_workers(&mut workers, &specs, &origins, &tx);

        assert_eq!(workers.len(), MAX_OBSERVE_CONNECTIONS);
    }

    #[tokio::test]
    async fn manager_stops_on_shutdown_notify() {
        let (_specs_tx, specs_rx) = mpsc::channel(1);
        let (frame_tx, _frame_rx) = mpsc::channel(1);
        let shutdown = Arc::new(Notify::new());

        let task = spawn_observe_manager(specs_rx, frame_tx, shutdown.clone(), None);
        // Let the manager reach its select before signalling.
        tokio::task::yield_now().await;
        shutdown.notify_one();

        let stopped = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(
            stopped.is_ok(),
            "manager must stop promptly when shutdown is signalled"
        );
    }
}
