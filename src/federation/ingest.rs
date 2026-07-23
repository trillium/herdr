//! Pure snapshot -> foreign-rows mapper (federation phase **N1a**).
//!
//! Reads a remote herdr server's `session.snapshot` JSON and turns it into
//! origin-namespaced, runtime-free value rows the local hub can later splice
//! into `AppState` (that splice is N1b — this slice does not touch the runtime,
//! sockets, PTYs, or async).
//!
//! ## Ingest source
//!
//! The API returns `{ "result": { "snapshot": { .. } } }`. The snapshot carries
//! `protocol`, `version`, and parallel arrays: `workspaces`, `tabs`, `panes`,
//! and `agents`. Every field beyond the handful we read is ignored, and every
//! id/field we read defaults on absence, so remote protocol/schema drift is
//! non-fatal rather than a parse failure.
//!
//! `panes[]` and `agents[]` both describe panes: an agent-bearing pane appears
//! in *both* (once as a `PaneInfo`, once as the richer `AgentInfo`). We union
//! them, deduplicated by public pane id, so an agent pane yields exactly one
//! terminal row. Real full snapshots and trimmed agent-only captures both map
//! correctly.
//!
//! ## Untrusted strings
//!
//! `cwd`, workspace/tab `label`, and pane `name` come from a remote machine and
//! are treated as opaque display strings: no path canonicalization, no git
//! execution against them, no shell interpretation.
//!
//! ## What is and is not preserved
//!
//! Every remote id is namespaced by the [`Origin`]'s durable key so it can never
//! alias a local id: terminal ids via [`namespace_terminal_id`], workspace ids
//! via [`namespace_public_id`] (herdr derives tab/pane public ids from the
//! workspace id, so they inherit the `fed~<key>~` prefix). `Tab` and `PaneState`
//! carry no string id of their own — their public numbers are re-derived
//! locally — so the *structure and counts* of the remote tree are preserved, but
//! the remote's exact tab/pane numbers are not load-bearing here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ratatui::layout::Direction;
use serde::Deserialize;
use tokio::sync::{mpsc, Notify};

use crate::detect::{parse_agent_label, AgentState};
use crate::events::AppEvent;
use crate::layout::{PaneId, TileLayout};
use crate::pane::PaneState;
use crate::terminal::{TerminalId, TerminalState};
use crate::workspace::{Tab, Workspace};

use super::namespace::{namespace_public_id, namespace_terminal_id};
use super::origin::{Origin, OriginKey};

/// Capacity for the dead event channel handed to foreign tabs. Foreign panes
/// are read-only metadata and never emit `AppEvent`s, so the receiver is
/// dropped immediately (mirroring `Workspace::test_new`); the capacity only has
/// to be non-zero.
const FOREIGN_EVENT_CHANNEL_CAP: usize = 1;

/// Failure decoding a remote `session.snapshot` API response.
#[derive(Debug)]
pub enum IngestError {
    /// The body did not parse as JSON, or `result.snapshot` did not match the
    /// (deliberately permissive) [`RemoteSnapshot`] shape.
    Deserialize(serde_json::Error),
    /// The body parsed as JSON but had no `result.snapshot` object.
    MissingSnapshot,
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Deserialize(err) => write!(f, "failed to decode remote snapshot: {err}"),
            IngestError::MissingSnapshot => {
                f.write_str("remote API response had no result.snapshot object")
            }
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IngestError::Deserialize(err) => Some(err),
            IngestError::MissingSnapshot => None,
        }
    }
}

impl From<serde_json::Error> for IngestError {
    fn from(err: serde_json::Error) -> Self {
        IngestError::Deserialize(err)
    }
}

/// Version-tolerant read view of a remote `session.snapshot`.
///
/// Constructed via [`RemoteSnapshot::from_api_response`]. Every field defaults
/// on absence and unknown fields are ignored, so a remote running a newer or
/// older protocol still deserializes.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RemoteSnapshot {
    /// Remote wire protocol version. Advisory only in N1a.
    #[serde(default)]
    pub protocol: u32,
    /// Remote herdr version string. Advisory only in N1a.
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    workspaces: Vec<RemoteWorkspace>,
    #[serde(default)]
    tabs: Vec<RemoteTab>,
    #[serde(default)]
    panes: Vec<RemotePane>,
    #[serde(default)]
    agents: Vec<RemotePane>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RemoteWorkspace {
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    active_tab_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RemoteTab {
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: String,
}

/// A remote pane row. Deserialized from both `panes[]` (which carries `label`)
/// and `agents[]` (which carries `name` plus authoritative agent fields); the
/// two are merged by [`RemotePane::overlay`].
#[derive(Debug, Clone, Default, Deserialize)]
struct RemotePane {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    terminal_id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    cwd: Option<String>,
    /// Pane display name from `panes[]`.
    #[serde(default)]
    label: Option<String>,
    /// Agent display name from `agents[]`.
    #[serde(default)]
    name: Option<String>,
}

impl RemotePane {
    /// Display name for this pane: the agent `name` if present, else the pane
    /// `label`. Trimmed; empty strings are treated as absent.
    fn display_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.label.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Fold a later row describing the same pane into this one. Called with the
    /// `agents[]` entry overlaying the seeded `panes[]` entry: agent-authoritative
    /// fields win; display/identity fields only fill gaps.
    fn overlay(&mut self, other: &RemotePane) {
        if other.agent.is_some() {
            self.agent = other.agent.clone();
        }
        if !other.agent_status.is_empty() {
            self.agent_status = other.agent_status.clone();
        }
        if other.name.is_some() {
            self.name = other.name.clone();
        }
        if self.label.is_none() {
            self.label = other.label.clone();
        }
        if self.cwd.is_none() {
            self.cwd = other.cwd.clone();
        }
        if self.terminal_id.is_empty() {
            self.terminal_id = other.terminal_id.clone();
        }
        if self.tab_id.is_empty() {
            self.tab_id = other.tab_id.clone();
        }
        if self.workspace_id.is_empty() {
            self.workspace_id = other.workspace_id.clone();
        }
    }
}

impl RemoteSnapshot {
    /// Reach `result.snapshot` in a raw API response body and decode it.
    ///
    /// Returns [`IngestError::MissingSnapshot`] when the JSON parses but lacks a
    /// `result.snapshot` object, and [`IngestError::Deserialize`] when the body
    /// is not JSON or the snapshot object cannot be read.
    pub fn from_api_response(body: &str) -> Result<Self, IngestError> {
        let root: serde_json::Value = serde_json::from_str(body)?;
        let snapshot = root
            .get("result")
            .and_then(|result| result.get("snapshot"))
            .ok_or(IngestError::MissingSnapshot)?;
        let parsed = serde_json::from_value(snapshot.clone())?;
        Ok(parsed)
    }
}

/// Origin-namespaced, runtime-free rows derived from a remote snapshot.
///
/// `terminals` is one entry per remote agent/pane; the [`TerminalId`] key equals
/// `terminal.id` and is already namespaced. `workspaces` reconstructs the remote
/// workspace -> tab -> pane tree with every pane's [`PaneState`] attached to its
/// namespaced terminal id. Every workspace has at least one tab and every tab at
/// least one pane, so the structures uphold `Workspace`/`Tab` invariants.
pub struct ForeignRows {
    pub workspaces: Vec<Workspace>,
    pub terminals: Vec<(TerminalId, TerminalState)>,
}

impl ForeignRows {
    /// The empty projection: no foreign workspaces or terminals. Splicing this
    /// clears every previously-injected foreign row from an `AppState`, which is
    /// how the disabled federation flag guarantees no remote state lingers.
    pub fn empty() -> Self {
        Self {
            workspaces: Vec::new(),
            terminals: Vec::new(),
        }
    }
}

/// Map an [`AgentState`] from a remote `agent_status` string. Unknown values
/// (including future variants) degrade to [`AgentState::Unknown`].
fn agent_state_from_status(status: &str) -> AgentState {
    match status {
        "working" => AgentState::Working,
        "idle" => AgentState::Idle,
        "blocked" => AgentState::Blocked,
        // "done" is idle-and-finished on the wire; there is no distinct
        // TerminalState variant, so it collapses to Idle.
        "done" => AgentState::Idle,
        _ => AgentState::Unknown,
    }
}

/// Pure mapper: remote snapshot -> origin-namespaced foreign rows.
pub fn foreign_rows(origin: &Origin, snap: &RemoteSnapshot) -> ForeignRows {
    let panes = merge_pane_universe(snap);

    // Reconstruct the workspace -> tab tree in first-seen order while building
    // one terminal row per surviving pane.
    let mut terminals: Vec<(TerminalId, TerminalState)> = Vec::new();
    let mut ws_accum: Vec<WorkspaceAccum> = Vec::new();
    let mut ws_index: HashMap<String, usize> = HashMap::new();

    for pane in &panes {
        if pane.workspace_id.is_empty() || pane.tab_id.is_empty() {
            tracing::warn!(
                pane_id = %pane.pane_id,
                terminal_id = %pane.terminal_id,
                "federation ingest: skipping remote pane with no workspace/tab id"
            );
            continue;
        }
        let Some((terminal_id, terminal)) = build_terminal(&origin.key, pane) else {
            continue;
        };
        terminals.push((terminal_id.clone(), terminal));

        let ws_slot = *ws_index
            .entry(pane.workspace_id.clone())
            .or_insert_with(|| {
                ws_accum.push(WorkspaceAccum::new(
                    pane.workspace_id.clone(),
                    pane.cwd.clone(),
                ));
                ws_accum.len() - 1
            });
        ws_accum[ws_slot].push_pane(&pane.tab_id, terminal_id);
    }

    // A single dead channel/notify shared across every foreign tab, cloned per
    // tab exactly as the live app shares its app-wide handles.
    let (events, _events_rx) = mpsc::channel::<AppEvent>(FOREIGN_EVENT_CHANNEL_CAP);
    let render_notify = Arc::new(Notify::new());
    let render_dirty = Arc::new(AtomicBool::new(false));

    let workspaces = ws_accum
        .into_iter()
        .map(|accum| accum.into_workspace(origin, snap, &events, &render_notify, &render_dirty))
        .collect();

    ForeignRows {
        workspaces,
        terminals,
    }
}

/// Deduplicated pane universe: `panes[]` seeded first, then `agents[]` overlaid,
/// keyed by public pane id, preserving first-seen order.
fn merge_pane_universe(snap: &RemoteSnapshot) -> Vec<RemotePane> {
    let mut rows: Vec<RemotePane> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for pane in snap.panes.iter().chain(snap.agents.iter()) {
        if pane.pane_id.is_empty() {
            // No stable key to dedup on; keep it and let terminal_id validation
            // decide whether it survives.
            rows.push(pane.clone());
            continue;
        }
        match index.get(&pane.pane_id) {
            Some(&slot) => rows[slot].overlay(pane),
            None => {
                index.insert(pane.pane_id.clone(), rows.len());
                rows.push(pane.clone());
            }
        }
    }
    rows
}

/// Build one namespaced [`TerminalState`] from a remote pane, or `None` (with a
/// warning) when the pane has no terminal id to key on.
fn build_terminal(
    origin_key: &OriginKey,
    pane: &RemotePane,
) -> Option<(TerminalId, TerminalState)> {
    let raw = pane.terminal_id.trim();
    if raw.is_empty() {
        tracing::warn!(
            pane_id = %pane.pane_id,
            workspace_id = %pane.workspace_id,
            "federation ingest: skipping remote pane with missing terminal_id"
        );
        return None;
    }

    let namespaced = namespace_terminal_id(origin_key, &TerminalId::from_string(raw));
    // cwd is an untrusted remote display string: stored verbatim, never resolved.
    let cwd = PathBuf::from(pane.cwd.as_deref().unwrap_or_default());
    let mut terminal = TerminalState::new(namespaced.clone(), cwd);
    terminal.state = agent_state_from_status(&pane.agent_status);
    if let Some(agent) = pane.agent.as_deref() {
        // Unrecognized agent labels leave detected_agent None but still yield a
        // valid foreign terminal.
        terminal.detected_agent = parse_agent_label(agent);
    }
    if let Some(name) = pane.display_name() {
        let name = name.to_string();
        terminal.manual_label = Some(name.clone());
        terminal.agent_name = Some(name);
    }
    Some((namespaced, terminal))
}

/// Accumulates one workspace's tabs (first-seen order) before materializing a
/// [`Workspace`].
struct WorkspaceAccum {
    id: String,
    identity_cwd: Option<String>,
    tabs: Vec<TabAccum>,
    tab_index: HashMap<String, usize>,
}

struct TabAccum {
    id: String,
    terminals: Vec<TerminalId>,
}

impl WorkspaceAccum {
    fn new(id: String, identity_cwd: Option<String>) -> Self {
        Self {
            id,
            identity_cwd,
            tabs: Vec::new(),
            tab_index: HashMap::new(),
        }
    }

    fn push_pane(&mut self, tab_id: &str, terminal_id: TerminalId) {
        let slot = *self.tab_index.entry(tab_id.to_string()).or_insert_with(|| {
            self.tabs.push(TabAccum {
                id: tab_id.to_string(),
                terminals: Vec::new(),
            });
            self.tabs.len() - 1
        });
        self.tabs[slot].terminals.push(terminal_id);
    }

    fn into_workspace(
        self,
        origin: &Origin,
        snap: &RemoteSnapshot,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<AtomicBool>,
    ) -> Workspace {
        let remote = snap
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == self.id);

        let active_tab = remote
            .map(|workspace| workspace.active_tab_id.as_str())
            .filter(|id| !id.is_empty())
            .and_then(|active_id| self.tabs.iter().position(|tab| tab.id == active_id))
            .unwrap_or(0);

        let label = workspace_label(origin, remote, &self.id);
        let identity_cwd = PathBuf::from(self.identity_cwd.unwrap_or_default());

        let mut tabs = Vec::with_capacity(self.tabs.len());
        let mut public_pane_numbers: HashMap<PaneId, usize> = HashMap::new();
        let mut next_public_pane_number = 1usize;
        for (idx, tab_accum) in self.tabs.into_iter().enumerate() {
            let tab = build_tab(
                idx + 1,
                &tab_accum.terminals,
                events.clone(),
                render_notify.clone(),
                render_dirty.clone(),
                &mut public_pane_numbers,
                &mut next_public_pane_number,
            );
            tabs.push(tab);
        }
        let next_public_tab_number = tabs.len() + 1;

        Workspace {
            id: namespace_public_id(&origin.key, &self.id),
            custom_name: Some(label),
            identity_cwd,
            // Never run git against untrusted remote paths.
            cached_git_branch: None,
            cached_git_ahead_behind: None,
            cached_git_space: None,
            worktree_space: None,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number,
            next_public_tab_number,
            tabs,
            active_tab,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        }
    }
}

/// Build a single foreign [`Tab`] whose panes attach to `terminals` in order.
///
/// Allocates one local [`PaneId`] per terminal (the root plus one split each),
/// registers each pane's public number into `public_pane_numbers`, and advances
/// `next_public_pane_number`. `terminals` is guaranteed non-empty by the caller.
fn build_tab(
    number: usize,
    terminals: &[TerminalId],
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
    public_pane_numbers: &mut HashMap<PaneId, usize>,
    next_public_pane_number: &mut usize,
) -> Tab {
    let (mut layout, root_pane) = TileLayout::new();
    let mut pane_ids = vec![root_pane];
    for _ in 1..terminals.len() {
        pane_ids.push(layout.split_focused(Direction::Horizontal));
    }

    let mut panes = HashMap::with_capacity(terminals.len());
    for (pane_id, terminal_id) in pane_ids.iter().zip(terminals.iter()) {
        panes.insert(*pane_id, PaneState::new(terminal_id.clone()));
        public_pane_numbers.insert(*pane_id, *next_public_pane_number);
        *next_public_pane_number += 1;
    }

    Tab {
        custom_name: None,
        number,
        root_pane,
        layout,
        panes,
        #[cfg(test)]
        runtimes: HashMap::new(),
        zoomed: false,
        events,
        render_notify,
        render_dirty,
    }
}

/// Origin-labeled workspace display name. Reads as `"{origin}/{remote label}"`
/// when the remote workspace has a label, otherwise just the origin label (or
/// the raw workspace id if the origin label is empty). Untrusted strings.
fn workspace_label(origin: &Origin, remote: Option<&RemoteWorkspace>, ws_id: &str) -> String {
    let base = if origin.label.trim().is_empty() {
        ws_id
    } else {
        origin.label.as_str()
    };
    match remote
        .map(|workspace| workspace.label.trim())
        .filter(|label| !label.is_empty())
    {
        Some(remote_label) => format!("{base}/{remote_label}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;
    use crate::federation::{is_foreign, parse_foreign_terminal_id};
    use std::collections::HashSet;
    use std::path::PathBuf;

    use crate::federation::{ConnectionTarget, OriginKey};

    fn origin(key: &str, label: &str) -> Origin {
        Origin::new(
            OriginKey::new(key).expect("valid test origin key"),
            label,
            ConnectionTarget::LocalSocket(PathBuf::from("/tmp/test.sock")),
        )
    }

    fn map(body: &str, key: &str, label: &str) -> ForeignRows {
        let snap = RemoteSnapshot::from_api_response(body).expect("snapshot should parse");
        foreign_rows(&origin(key, label), &snap)
    }

    fn terminal_states(rows: &ForeignRows) -> Vec<AgentState> {
        rows.terminals
            .iter()
            .map(|(_, terminal)| terminal.state)
            .collect()
    }

    #[test]
    fn sample_agent_only_snapshot_maps_faithfully() {
        // The real `herdr api snapshot` evidence: an agents-only capture.
        let body = include_str!("testdata/sample-session-snapshot.json");
        let rows = map(body, "n1", "mini");

        // Two agents in one workspace across two tabs.
        assert_eq!(
            rows.terminals.len(),
            2,
            "one terminal per remote agent pane"
        );
        assert_eq!(rows.workspaces.len(), 1);
        assert_eq!(rows.workspaces[0].tabs.len(), 2, "two distinct tab ids");

        // Every terminal id is namespaced to this origin and round-trips.
        for (id, terminal) in &rows.terminals {
            assert_eq!(id, &terminal.id);
            assert!(id.as_str().starts_with("fed~n1~"), "{}", id.as_str());
            assert!(is_foreign(id));
            let (parsed_origin, _) = parse_foreign_terminal_id(id).expect("foreign id parses");
            assert_eq!(parsed_origin, OriginKey::new("n1").unwrap());
        }

        // agent_status -> AgentState mapping, both variants present.
        let states = terminal_states(&rows);
        assert!(states.contains(&AgentState::Working));
        assert!(states.contains(&AgentState::Idle));

        // agent "claude" -> detected agent; cwd carried through untouched.
        for (_, terminal) in &rows.terminals {
            assert_eq!(terminal.detected_agent, Some(Agent::Claude));
            assert_eq!(
                terminal.cwd,
                PathBuf::from("/Users/trilliumsmith/code/firstmate")
            );
        }

        // Workspace has no remote label in this capture -> origin label only.
        assert_eq!(rows.workspaces[0].custom_name.as_deref(), Some("mini"));

        // Every pane in the tree points at a namespaced terminal that exists.
        let terminal_ids: HashSet<&str> =
            rows.terminals.iter().map(|(id, _)| id.as_str()).collect();
        for tab in &rows.workspaces[0].tabs {
            assert_eq!(tab.panes.len(), 1);
            for pane in tab.panes.values() {
                assert!(terminal_ids.contains(pane.attached_terminal_id.as_str()));
            }
        }
    }

    #[test]
    fn full_snapshot_preserves_labels_names_and_dedups_agent_panes() {
        // p1 appears in both panes[] and agents[]; the agent entry overlays a
        // fresher status ("working" over "blocked") and a display name.
        let body = r#"{"result":{"snapshot":{
            "protocol":16,"version":"0.7.3",
            "workspaces":[{"workspace_id":"w1","label":"repo","active_tab_id":"w1:t2"}],
            "tabs":[
                {"tab_id":"w1:t1","workspace_id":"w1","label":"main"},
                {"tab_id":"w1:t2","workspace_id":"w1","label":"side"}
            ],
            "panes":[
                {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_a","agent":"claude","agent_status":"blocked","cwd":"/x","label":"pane-label"},
                {"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1","terminal_id":"term_b","agent_status":"idle","cwd":"/y"}
            ],
            "agents":[
                {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_a","agent":"claude","agent_status":"working","name":"my-agent","cwd":"/x"}
            ]
        }}}"#;
        let rows = map(body, "n7", "mini2");

        // p1 deduped across panes[] and agents[] -> two terminals, not three.
        assert_eq!(rows.terminals.len(), 2);
        assert_eq!(rows.workspaces.len(), 1);

        let ws = &rows.workspaces[0];
        assert_eq!(ws.tabs.len(), 2);
        // Origin-labeled workspace name and namespaced id.
        assert_eq!(ws.custom_name.as_deref(), Some("mini2/repo"));
        assert_eq!(ws.id, "fed~n7~w1");
        // active_tab_id "w1:t2" resolves to the second tab.
        assert_eq!(ws.active_tab, 1);

        // The overlaid agent status and name win on the shared pane.
        let term_a = rows
            .terminals
            .iter()
            .find(|(id, _)| id.as_str() == "fed~n7~term_a")
            .map(|(_, terminal)| terminal)
            .expect("term_a present");
        assert_eq!(term_a.state, AgentState::Working);
        assert_eq!(term_a.detected_agent, Some(Agent::Claude));
        assert_eq!(term_a.agent_name.as_deref(), Some("my-agent"));
        assert_eq!(term_a.manual_label.as_deref(), Some("my-agent"));

        // The blocked/idle-only second pane maps its status straight through.
        let term_b = rows
            .terminals
            .iter()
            .find(|(id, _)| id.as_str() == "fed~n7~term_b")
            .map(|(_, terminal)| terminal)
            .expect("term_b present");
        assert_eq!(term_b.state, AgentState::Idle);
        assert_eq!(term_b.detected_agent, None);
    }

    #[test]
    fn tolerates_unknown_fields_and_protocol_drift() {
        let body = r#"{"result":{"snapshot":{
            "protocol":999,"version":"9.9.9","future_top_level":{"nested":[1,2,3]},
            "agents":[
                {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_z","agent_status":"surprising_new_status","brand_new_field":true}
            ]
        }}}"#;
        let snap = RemoteSnapshot::from_api_response(body).expect("drifted snapshot parses");
        assert_eq!(snap.protocol, 999);
        assert_eq!(snap.version, "9.9.9");

        let rows = foreign_rows(&origin("n1", "mini"), &snap);
        assert_eq!(rows.terminals.len(), 1);
        // Unknown status degrades rather than failing.
        assert_eq!(rows.terminals[0].1.state, AgentState::Unknown);
    }

    #[test]
    fn missing_snapshot_is_distinct_error() {
        assert!(matches!(
            RemoteSnapshot::from_api_response(r#"{"result":{}}"#),
            Err(IngestError::MissingSnapshot)
        ));
        assert!(matches!(
            RemoteSnapshot::from_api_response(r#"{"unexpected":1}"#),
            Err(IngestError::MissingSnapshot)
        ));
        assert!(matches!(
            RemoteSnapshot::from_api_response("not json at all"),
            Err(IngestError::Deserialize(_))
        ));
    }

    #[test]
    fn pane_missing_terminal_id_is_skipped_not_panicked() {
        let body = r#"{"result":{"snapshot":{"agents":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_ok","agent_status":"working"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","workspace_id":"w1","agent_status":"idle"}
        ]}}}"#;
        let rows = map(body, "n1", "mini");

        assert_eq!(rows.terminals.len(), 1, "pane without terminal_id skipped");
        assert_eq!(rows.terminals[0].0.as_str(), "fed~n1~term_ok");
        assert_eq!(rows.workspaces.len(), 1);
        assert_eq!(rows.workspaces[0].tabs.len(), 1);
        assert_eq!(rows.workspaces[0].tabs[0].panes.len(), 1);
    }

    #[test]
    fn distinct_origins_produce_disjoint_namespaced_ids() {
        let body = r#"{"result":{"snapshot":{"agents":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_1","agent_status":"idle"}
        ]}}}"#;
        let snap = RemoteSnapshot::from_api_response(body).unwrap();

        let a = foreign_rows(&origin("nA", "box-a"), &snap);
        let b = foreign_rows(&origin("nB", "box-b"), &snap);

        let ids_a: HashSet<&str> = a.terminals.iter().map(|(id, _)| id.as_str()).collect();
        let ids_b: HashSet<&str> = b.terminals.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids_a.is_disjoint(&ids_b),
            "same raw id under two origins must not collide: {ids_a:?} vs {ids_b:?}"
        );
        assert_eq!(a.workspaces[0].id, "fed~nA~w1");
        assert_eq!(b.workspaces[0].id, "fed~nB~w1");
    }

    #[test]
    fn multi_pane_tab_builds_one_pane_per_terminal() {
        // Two agent panes in the same tab must yield a two-pane tab with two
        // distinct local pane ids and stable public numbering.
        let body = r#"{"result":{"snapshot":{"agents":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_1","agent_status":"working"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","workspace_id":"w1","terminal_id":"term_2","agent_status":"idle"}
        ]}}}"#;
        let rows = map(body, "n1", "mini");

        assert_eq!(rows.terminals.len(), 2);
        let ws = &rows.workspaces[0];
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].panes.len(), 2);
        assert_eq!(ws.public_pane_numbers.len(), 2);
        let numbers: HashSet<usize> = ws.public_pane_numbers.values().copied().collect();
        assert_eq!(numbers, HashSet::from([1, 2]));
        assert_eq!(ws.next_public_pane_number, 3);
        assert_eq!(ws.next_public_tab_number, 2);
    }

    #[test]
    fn empty_snapshot_yields_no_rows() {
        let rows = map(r#"{"result":{"snapshot":{}}}"#, "n1", "mini");
        assert!(rows.terminals.is_empty());
        assert!(rows.workspaces.is_empty());
    }
}
