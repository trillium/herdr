//! Fleet federation — aggregate remote herdr servers' agent sessions into the
//! local hub's sidebar.
//!
//! Topology is a **star**: the local machine is the sole aggregator and origins
//! never talk to each other. An [`Origin`] is a remote herdr server we ingest
//! from; its durable identity is the Tailscale StableNodeID (survives restart,
//! sleep, IP churn and host renames), kept deliberately separate from the
//! *transport* used to reach it ([`ConnectionTarget`]) so that ingest and merge
//! logic never bakes in SSH — a future direct/mTLS transport must slot in
//! without touching them.
//!
//! Phase **N1a** adds [`ingest`]: a pure, runtime-free mapper from a remote
//! `session.snapshot` to origin-namespaced [`ingest::ForeignRows`]. It consumes
//! the N0 origin types and id-namespacing without touching the runtime. Phase
//! **N1b** splices those rows into `AppState` (`set_foreign_rows` /
//! `apply_foreign_rows`), gated behind `experimental.federation` (default off)
//! and projected read-only — foreign panes carry dead channels and receive no
//! input or PTY relay. Phase **N1c** delivers the async snapshot poll loop
//! ([`poll::collect_foreign_rows`] spawned in `App::spawn_federation_poll`) that
//! fetches each remote origin's snapshot on a ~5s timer when the flag is enabled
//! at startup and projects it live into the sidebar. Phase **N1d** adds input
//! relay ([`relay::send_input_to_foreign_pane`]) so typing into a foreign pane
//! sends keystrokes to its remote PTY. The module-level allows below remain only
//! for the still-unwired N0 surface (registry); they shrink as later phases
//! consume it.
#![allow(dead_code, unused_imports)]

mod discovery;
mod ingest;
mod label;
mod live_view;
mod namespace;
mod observe;
mod origin;
mod poll;
mod registry;
pub mod relay;

pub use discovery::{
    discover, discover_origins, forwarded_socket_path, DiscoveryConfig, StaticOrigin,
    TailscaleStatus,
};
pub use ingest::{foreign_rows, ForeignRows, IngestError, RemoteSnapshot};
pub use label::{local_origin_prefix, origin_label_prefix, prefixed_workspace_label};
pub use live_view::{live_view_status, LiveViewStatus};
pub use namespace::{
    is_foreign, is_foreign_workspace_id, namespace_public_id, namespace_terminal_id,
    parse_foreign_terminal_id, parse_foreign_workspace_id,
};
pub use observe::{spawn_foreign_observer, ForeignFrame, ForeignObserveHandle};
pub use origin::{ConnectionTarget, InvalidOriginKey, Origin, OriginKey};
pub use poll::collect_foreign_rows;
pub use registry::{FederationRegistry, ReconcileDelta};

/// Message sent from the federation poll task to the run loop on the
/// `foreign_rows` channel. Carries either the initial origin discovery
/// (sent once at task start) or a tick's worth of projected foreign rows.
pub enum ForeignRowsMessage {
    /// The set of origins discovered at task startup.
    OriginsDiscovered(Vec<Origin>),
    /// A full replacement of the projected foreign rows for one poll tick.
    Rows(ForeignRows),
}
