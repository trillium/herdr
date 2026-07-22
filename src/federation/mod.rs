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
//! the N0 origin types and id-namespacing but still does not touch the runtime —
//! splicing rows into `AppState` and the `federation.*` JSON API methods are
//! N1b+. The module-level allows below remain only for the still-unwired N0
//! surface (discovery, the registry, and the foreign-id predicates/parsers);
//! they shrink as later phases consume it.
#![allow(dead_code, unused_imports)]

mod discovery;
mod ingest;
mod namespace;
mod origin;
mod registry;

pub use discovery::{
    discover, forwarded_socket_path, DiscoveryConfig, StaticOrigin, TailscaleStatus,
};
pub use ingest::{foreign_rows, ForeignRows, IngestError, RemoteSnapshot};
pub use namespace::{
    is_foreign, namespace_public_id, namespace_terminal_id, parse_foreign_terminal_id,
};
pub use origin::{ConnectionTarget, InvalidOriginKey, Origin, OriginKey};
pub use registry::{FederationRegistry, ReconcileDelta};
