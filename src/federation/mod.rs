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
//! This module is phase **N0** (origin plumbing): pure, runtime-free value
//! types, the origin registry, the discovery parser, and id-namespacing. None
//! of it is wired into the server runtime yet — phase N1 splices origin-tagged
//! rows into `AppState` and adds the `federation.*` JSON API methods. The
//! module-level allows below cover that gap and are removed in N1.
// N0 scaffolding: types/re-exports are exercised by unit tests but not yet
// consumed by the runtime; the allows are removed when N1 wires this in.
#![allow(dead_code, unused_imports)]

mod discovery;
mod namespace;
mod origin;
mod registry;

pub use discovery::{
    discover, forwarded_socket_path, DiscoveryConfig, StaticOrigin, TailscaleStatus,
};
pub use namespace::{is_foreign, namespace_terminal_id, parse_foreign_terminal_id};
pub use origin::{ConnectionTarget, Origin, OriginKey};
pub use registry::{FederationRegistry, ReconcileDelta};
