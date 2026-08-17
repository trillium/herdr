//! Federation phase **N2 Part 2** — live-view protocol compatibility.
//!
//! Interactive attach to a foreign pane rides the strict exact-match render
//! wire (`ObserveTerminal`/`ControlTerminal`; `PROTOCOL_VERSION` enforced at
//! `src/client_transport.rs` and `src/protocol/wire.rs`). The hub and a remote
//! must share a protocol version before a live view can attach, so this slice
//! computes that compatibility from the remote's snapshot protocol and exposes
//! a non-crash status the hub renders in place of the (otherwise blank) foreign
//! pane grid.
//!
//! The current fleet runs remotes on an older protocol than this fork's hub, so
//! a version mismatch — rendered here as an explicit, non-crash state — is the
//! common case. When versions match, the same status gates the strict-wire
//! observe connection that streams the remote's structured frame/grid data
//! through the hub's existing render path (RENDER-FROM-GRID: raw remote ANSI is
//! never piped to the hub's stdout).

use crate::protocol::PROTOCOL_VERSION;

/// Whether a remote's reported protocol can attach a live render view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveViewStatus {
    /// The remote reports the same protocol as the hub; a live view may attach.
    Attachable,
    /// The remote reports a different protocol; a live view cannot attach.
    VersionMismatch { remote: u32, local: u32 },
    /// The remote did not report a protocol (older snapshot or unreachable).
    UnknownRemoteProtocol,
}

/// Classify a remote's snapshot protocol against the hub's own.
///
/// Pure and side-effect free: the `remote_protocol` value is advisory (read from
/// the remote `session.snapshot`), and a `None` degrades to
/// [`LiveViewStatus::UnknownRemoteProtocol`] rather than an error.
pub fn live_view_status(remote_protocol: Option<u32>) -> LiveViewStatus {
    match remote_protocol {
        Some(remote) if remote == PROTOCOL_VERSION => LiveViewStatus::Attachable,
        Some(remote) => LiveViewStatus::VersionMismatch {
            remote,
            local: PROTOCOL_VERSION,
        },
        None => LiveViewStatus::UnknownRemoteProtocol,
    }
}

impl LiveViewStatus {
    /// Short human-readable status for the foreign pane's placeholder surface.
    pub fn message(self) -> String {
        match self {
            LiveViewStatus::Attachable => {
                format!("live view ready (protocol {PROTOCOL_VERSION})")
            }
            LiveViewStatus::VersionMismatch { remote, local } => {
                format!("live view unavailable: protocol mismatch (remote {remote} vs hub {local})")
            }
            LiveViewStatus::UnknownRemoteProtocol => {
                "live view unavailable: remote protocol unknown".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_protocol_is_attachable() {
        assert_eq!(
            live_view_status(Some(PROTOCOL_VERSION)),
            LiveViewStatus::Attachable
        );
    }

    #[test]
    fn mismatched_protocol_reports_both_versions() {
        let remote = PROTOCOL_VERSION - 1;
        assert_eq!(
            live_view_status(Some(remote)),
            LiveViewStatus::VersionMismatch {
                remote,
                local: PROTOCOL_VERSION
            }
        );
    }

    #[test]
    fn unknown_protocol_is_distinct_from_mismatch() {
        assert_eq!(
            live_view_status(None),
            LiveViewStatus::UnknownRemoteProtocol
        );
    }

    #[test]
    fn messages_are_non_crash_and_include_versions() {
        let mismatch = live_view_status(Some(PROTOCOL_VERSION - 1));
        let message = mismatch.message();
        assert!(message.contains("protocol mismatch"), "{message}");
        assert!(message.contains(&PROTOCOL_VERSION.to_string()), "{message}");

        assert!(live_view_status(None).message().contains("unknown"));
    }
}
