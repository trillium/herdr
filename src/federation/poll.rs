//! Federation phase **N1c** — synchronous per-origin snapshot fetch + map.
//!
//! [`collect_foreign_rows`] fetches every origin's `session.snapshot` over its
//! JSON API socket, maps each through the N1a [`foreign_rows`] mapper, and
//! concatenates the results into a single [`ForeignRows`] value. That combined
//! value is what one poll tick hands to `AppState::apply_foreign_rows`, which
//! replaces the whole foreign projection atomically (never accumulates).
//!
//! ## Self-healing
//!
//! Each origin is fetched and mapped independently. An origin whose socket is
//! down, whose response is not a decodable snapshot, or that times out is logged
//! at `warn` and skipped for that tick — it never panics and never aborts the
//! collection, so one unreachable box cannot blank out the reachable fleet.
//!
//! ## Blocking
//!
//! [`crate::api::client::ApiClient`] is synchronous. Callers on the async
//! runtime MUST invoke [`collect_foreign_rows`] inside
//! `tokio::task::spawn_blocking` so the blocking socket I/O never stalls the
//! reactor.

use std::fmt;
use std::time::Duration;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget as ApiConnectionTarget};
use crate::api::schema::{EmptyParams, Method, Request};

use super::ingest::{foreign_rows, ForeignRows, IngestError, RemoteSnapshot};
use super::origin::{ConnectionTarget, Origin};

/// Failure fetching or decoding a single origin's snapshot. Never surfaced to a
/// caller — [`collect_foreign_rows`] logs it and skips the origin — but carried
/// as a typed value so the skip reason is precise in the log.
#[derive(Debug)]
enum PollError {
    /// The API request failed (socket down, timeout, transport error).
    Api(ApiClientError),
    /// The response was not a decodable `result.snapshot`.
    Ingest(IngestError),
    /// The API response value could not be re-serialized for the ingest mapper.
    Serialize(serde_json::Error),
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PollError::Api(err) => write!(f, "snapshot request failed: {err}"),
            PollError::Ingest(err) => write!(f, "snapshot decode failed: {err}"),
            PollError::Serialize(err) => write!(f, "snapshot re-serialize failed: {err}"),
        }
    }
}

impl std::error::Error for PollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PollError::Api(err) => Some(err),
            PollError::Ingest(err) => Some(err),
            PollError::Serialize(err) => Some(err),
        }
    }
}

/// Fetch + map every origin's snapshot, returning the combined foreign rows.
///
/// Synchronous and blocking (see the module docs): run inside
/// `tokio::task::spawn_blocking` from the async runtime. `timeout` bounds each
/// origin's request independently, so a single hung origin cannot stall the
/// whole tick beyond its own timeout. Unreachable or undecodable origins are
/// warned and skipped.
pub fn collect_foreign_rows(origins: &[Origin], timeout: Duration) -> ForeignRows {
    collect_with_fetcher(origins, timeout, fetch_snapshot_body)
}

/// Upper bound on simultaneously-inflight origin fetches. Federation fans out one
/// scoped thread per origin within a chunk of this size, so a large fleet cannot
/// spawn unbounded threads while still collapsing worst-case tick latency from
/// N serial per-origin timeouts down to roughly one timeout per bounded batch.
const MAX_POLL_CONCURRENCY: usize = 8;

/// The mapping/aggregation core, parameterized over the snapshot fetcher so the
/// pure mapping is unit-testable without real socket I/O. Production uses
/// [`fetch_snapshot_body`]; tests inject a fixture-backed fetcher.
///
/// Origins are fetched with bounded concurrency (chunks of
/// [`MAX_POLL_CONCURRENCY`], one scoped thread each), so a hung origin no longer
/// serializes the timeout of every origin behind it. Results are joined in origin
/// order, so the combined projection is deterministic regardless of which origins
/// respond first; the per-origin warn-and-skip on failure is preserved.
fn collect_with_fetcher<F>(origins: &[Origin], timeout: Duration, fetch: F) -> ForeignRows
where
    F: Fn(&Origin, Duration) -> Result<String, PollError> + Sync,
{
    let mut combined = ForeignRows::empty();
    for chunk in origins.chunks(MAX_POLL_CONCURRENCY) {
        // One scoped thread per origin in the chunk; join in origin order. Scoped
        // threads borrow `fetch` and each `origin` directly, so no cloning or
        // 'static bound is needed. `join` only errs if the thread panicked.
        let results: Vec<std::thread::Result<Result<ForeignRows, PollError>>> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|origin| {
                        let fetch = &fetch;
                        scope.spawn(move || {
                            fetch(origin, timeout).and_then(|body| map_snapshot_body(origin, &body))
                        })
                    })
                    .collect();
                handles.into_iter().map(|handle| handle.join()).collect()
            });

        for (origin, result) in chunk.iter().zip(results) {
            match result {
                Ok(Ok(rows)) => {
                    combined.workspaces.extend(rows.workspaces);
                    combined.terminals.extend(rows.terminals);
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        origin = %origin.key,
                        label = %origin.label,
                        error = %err,
                        "federation poll: skipping origin for this tick"
                    );
                }
                Err(_panic) => {
                    tracing::warn!(
                        origin = %origin.key,
                        label = %origin.label,
                        error = "origin fetch thread panicked",
                        "federation poll: skipping origin for this tick"
                    );
                }
            }
        }
    }
    combined
}

/// Decode a raw API response body into origin-namespaced foreign rows.
fn map_snapshot_body(origin: &Origin, body: &str) -> Result<ForeignRows, PollError> {
    let snapshot = RemoteSnapshot::from_api_response(body).map_err(PollError::Ingest)?;
    Ok(foreign_rows(origin, &snapshot))
}

/// Production fetcher: bridge the origin's federation transport to an API client
/// target and request its `session.snapshot`, returning the raw response body.
///
/// The two `ConnectionTarget` types are deliberately distinct — federation's
/// [`ConnectionTarget`] is the origin model, the API's is the client edge — and
/// are bridged only here, at the poll boundary.
fn fetch_snapshot_body(origin: &Origin, timeout: Duration) -> Result<String, PollError> {
    let target = match &origin.target {
        ConnectionTarget::LocalSocket(path) => ApiConnectionTarget::SocketPath(path.clone()),
    };
    let client = ApiClient::for_target(target);
    let request = Request {
        id: format!("federation:snapshot:{}", origin.key),
        method: Method::SessionSnapshot(EmptyParams::default()),
    };
    let value = client
        .request_value_with_timeout(&request, timeout)
        .map_err(PollError::Api)?;
    serde_json::to_string(&value).map_err(PollError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    use crate::federation::{is_foreign_workspace_id, OriginKey};

    const SAMPLE: &str = include_str!("testdata/sample-session-snapshot.json");

    fn origin(key: &str) -> Origin {
        Origin::new(
            OriginKey::new(key).expect("valid test origin key"),
            key,
            ConnectionTarget::LocalSocket(PathBuf::from(format!("/nonexistent/{key}.sock"))),
        )
    }

    #[test]
    fn unreachable_origins_yield_empty_rows_without_panic() {
        // Real fetcher against sockets that do not exist: every origin errors and
        // is skipped, so the tick is empty rather than a panic.
        let origins = vec![origin("nDOWN1"), origin("nDOWN2")];
        let rows = collect_foreign_rows(&origins, Duration::from_millis(50));
        assert!(
            rows.workspaces.is_empty(),
            "no workspaces from dead sockets"
        );
        assert!(rows.terminals.is_empty(), "no terminals from dead sockets");
    }

    #[test]
    fn no_origins_yields_empty_rows() {
        let rows = collect_foreign_rows(&[], Duration::from_millis(50));
        assert!(rows.workspaces.is_empty());
        assert!(rows.terminals.is_empty());
    }

    #[test]
    fn reachable_origin_maps_rows_and_unreachable_is_skipped() {
        // Injected fetcher: one origin resolves to the sample snapshot, the other
        // errors. The reachable origin's rows still land; the failure is skipped.
        let reachable = origin("nUP");
        let unreachable = origin("nDOWN");
        let origins = vec![reachable, unreachable];

        let rows = collect_with_fetcher(&origins, Duration::from_millis(50), |origin, _| {
            if origin.key == OriginKey::new("nUP").unwrap() {
                Ok(SAMPLE.to_string())
            } else {
                Err(PollError::Ingest(IngestError::MissingSnapshot))
            }
        });

        // Fixture is one workspace / two terminals, all namespaced to nUP.
        assert_eq!(rows.workspaces.len(), 1, "only the reachable origin's ws");
        assert_eq!(rows.terminals.len(), 2, "only the reachable origin's terms");
        assert!(is_foreign_workspace_id(&rows.workspaces[0].id));
        for (id, _) in &rows.terminals {
            assert!(id.as_str().starts_with("fed~nUP~"), "{}", id.as_str());
        }
    }

    #[test]
    fn combined_rows_concatenate_across_origins_disjoint() {
        // Two reachable origins each returning the same fixture body: the rows
        // concatenate and, because ids are origin-namespaced, never collide.
        let origins = vec![origin("nA"), origin("nB")];

        let rows = collect_with_fetcher(&origins, Duration::from_millis(50), |_, _| {
            Ok(SAMPLE.to_string())
        });

        assert_eq!(rows.workspaces.len(), 2, "one workspace per origin");
        assert_eq!(rows.terminals.len(), 4, "two terminals per origin");

        let ws_ids: HashSet<&str> = rows.workspaces.iter().map(|ws| ws.id.as_str()).collect();
        assert_eq!(ws_ids.len(), 2, "workspace ids are disjoint across origins");
        let term_ids: HashSet<&str> = rows.terminals.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            term_ids.len(),
            4,
            "terminal ids are disjoint across origins"
        );
    }

    #[test]
    fn combined_rows_preserve_origin_order_across_chunks() {
        // Ten origins (more than MAX_POLL_CONCURRENCY, so this crosses a chunk
        // boundary) each map the sample snapshot to a single workspace namespaced
        // `fed~<key>~w1`. Concurrency must not reorder the projection: the
        // combined workspaces must come out in origin order regardless of which
        // scoped thread finishes first.
        let keys: Vec<String> = (0..10).map(|i| format!("n{i}")).collect();
        let origins: Vec<Origin> = keys.iter().map(|key| origin(key)).collect();

        let rows = collect_with_fetcher(&origins, Duration::from_millis(50), |_, _| {
            Ok(SAMPLE.to_string())
        });

        assert_eq!(
            rows.workspaces.len(),
            keys.len(),
            "one workspace per origin"
        );
        let observed: Vec<&str> = rows.workspaces.iter().map(|ws| ws.id.as_str()).collect();
        let expected: Vec<String> = keys.iter().map(|key| format!("fed~{key}~w1")).collect();
        assert_eq!(
            observed, expected,
            "combined workspaces stay in origin order across chunk boundaries"
        );
    }
}
