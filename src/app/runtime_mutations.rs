use crate::api::schema::{
    EmptyParams, LayoutSetSplitRatioParams, Method, PaneFocusDirectionParams, PaneInputSetParams,
    PaneRenameParams, PaneResizeParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneZoomParams, TabCreateParams, TabMoveParams, TabRenameParams, TabTarget,
    WorkspaceCreateParams, WorkspaceMoveBlockParams, WorkspaceMoveParams, WorkspaceRenameParams,
    WorkspaceTarget, WorktreeCreateParams, WorktreeOpenParams, WorktreeRemoveParams,
};

use super::App;

/// Timeout for a fire-and-forget federation structure action (new tab / split /
/// close) sent to a remote origin. Matches the input-relay posture: bounded,
/// logged, never blocking the UI thread.
const FOREIGN_ACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

impl App {
    pub(crate) fn dispatch_runtime_mutation(&mut self, id: &'static str, method: Method) -> String {
        self.dispatch_api_request(id, method)
    }

    /// Resolve a federated (foreign) workspace target from an explicit public
    /// workspace id or, when `workspace_id` is `None`, the active workspace.
    /// Returns the owning origin key and the raw (non-namespaced) remote
    /// workspace id. `None` when federation is disabled or the target is local.
    fn foreign_workspace_target(
        &self,
        workspace_id: Option<&str>,
    ) -> Option<(crate::federation::OriginKey, String)> {
        if !self.state.federation_enabled {
            return None;
        }
        match workspace_id {
            Some(id) => {
                let (key, raw) = crate::federation::parse_foreign_workspace_id(id)?;
                Some((key, raw.to_string()))
            }
            None => {
                let ws = self.state.workspaces.get(self.state.active?)?;
                let (key, raw) = crate::federation::parse_foreign_workspace_id(&ws.id)?;
                Some((key, raw.to_string()))
            }
        }
    }

    /// Raw remote public pane id for the active foreign workspace's focused
    /// pane, resolved through the retained `foreign_remote_pane_id` on the
    /// pane's terminal. `None` when the active workspace is local or the pane
    /// has no retained remote id.
    fn active_foreign_pane_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        let terminal_id = ws.terminal_id(pane_id)?;
        self.state
            .terminals
            .get(terminal_id)?
            .foreign_remote_pane_id
            .clone()
    }

    /// Route a structure mutation to the owning remote origin, fire-and-forget.
    fn route_foreign_structure_action(
        &self,
        origin_key: crate::federation::OriginKey,
        method: Method,
    ) {
        crate::federation::relay::spawn_send_action_to_foreign(
            origin_key,
            method,
            FOREIGN_ACTION_TIMEOUT,
            self.federation_socket_dir.clone(),
        );
    }

    pub(crate) fn dispatch_deferred_runtime_mutation(
        &mut self,
        id: &'static str,
        method: Method,
    ) -> Option<String> {
        self.dispatch_deferred_api_request(id, method)
    }

    pub(crate) fn runtime_workspace_focus(
        &mut self,
        id: &'static str,
        workspace_id: String,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorkspaceFocus(WorkspaceTarget { workspace_id }))
    }

    pub(crate) fn runtime_workspace_create(
        &mut self,
        id: &'static str,
        params: WorkspaceCreateParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorkspaceCreate(params))
    }

    pub(crate) fn runtime_workspace_rename(
        &mut self,
        id: &'static str,
        params: WorkspaceRenameParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorkspaceRename(params))
    }

    pub(crate) fn runtime_workspace_move(
        &mut self,
        id: &'static str,
        params: WorkspaceMoveParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorkspaceMove(params))
    }

    pub(crate) fn runtime_workspace_move_block(
        &mut self,
        id: &'static str,
        params: WorkspaceMoveBlockParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorkspaceMoveBlock(params))
    }

    pub(crate) fn runtime_workspace_close(
        &mut self,
        id: &'static str,
        workspace_id: String,
    ) -> String {
        // N2 Part 3: closing a foreign workspace executes on the owning remote.
        if let Some((origin_key, raw_ws)) = self.foreign_workspace_target(Some(&workspace_id)) {
            self.route_foreign_structure_action(
                origin_key,
                Method::WorkspaceClose(WorkspaceTarget {
                    workspace_id: raw_ws,
                }),
            );
            return String::new();
        }
        self.dispatch_runtime_mutation(id, Method::WorkspaceClose(WorkspaceTarget { workspace_id }))
    }

    pub(crate) fn runtime_tab_create(
        &mut self,
        id: &'static str,
        mut params: TabCreateParams,
    ) -> String {
        // N2 Part 3: creating a tab in a foreign workspace executes on the owning
        // remote, never locally. Rewrite the target to the raw remote workspace id
        // and fire the action at the origin; the new tab arrives via the next
        // snapshot poll.
        if let Some((origin_key, raw_ws)) =
            self.foreign_workspace_target(params.workspace_id.as_deref())
        {
            params.workspace_id = Some(raw_ws);
            self.route_foreign_structure_action(origin_key, Method::TabCreate(params));
            return String::new();
        }
        self.dispatch_runtime_mutation(id, Method::TabCreate(params))
    }

    pub(crate) fn runtime_tab_focus(&mut self, id: &'static str, tab_id: String) -> String {
        self.dispatch_runtime_mutation(id, Method::TabFocus(TabTarget { tab_id }))
    }

    pub(crate) fn runtime_tab_rename(
        &mut self,
        id: &'static str,
        params: TabRenameParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::TabRename(params))
    }

    pub(crate) fn runtime_tab_move(&mut self, id: &'static str, params: TabMoveParams) -> String {
        self.dispatch_runtime_mutation(id, Method::TabMove(params))
    }

    pub(crate) fn runtime_tab_close(&mut self, id: &'static str, tab_id: String) -> String {
        self.dispatch_runtime_mutation(id, Method::TabClose(TabTarget { tab_id }))
    }

    pub(crate) fn runtime_server_reload_config(&mut self, id: &'static str) -> String {
        self.dispatch_runtime_mutation(id, Method::ServerReloadConfig(EmptyParams::default()))
    }

    pub(crate) fn runtime_pane_focus(&mut self, id: &'static str, pane_id: String) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneFocus(PaneTarget { pane_id }))
    }

    pub(crate) fn runtime_pane_close(&mut self, id: &'static str, pane_id: String) -> String {
        // N2 Part 3: closing a foreign pane executes on the owning remote. Resolve
        // the remote's raw pane id from the focused foreign pane and fire the
        // action at the origin; the removal arrives via the next snapshot poll.
        // A foreign workspace is never mutated locally, even when the raw pane id
        // cannot be resolved.
        if let Some((origin_key, _raw_ws)) = self.foreign_workspace_target(None) {
            match self.active_foreign_pane_id() {
                Some(raw_pane_id) => self.route_foreign_structure_action(
                    origin_key,
                    Method::PaneClose(PaneTarget {
                        pane_id: raw_pane_id,
                    }),
                ),
                None => tracing::warn!(
                    "federation: cannot resolve remote pane id for foreign pane close; skipping"
                ),
            }
            return String::new();
        }
        self.dispatch_runtime_mutation(id, Method::PaneClose(PaneTarget { pane_id }))
    }

    pub(crate) fn runtime_pane_rename(
        &mut self,
        id: &'static str,
        params: PaneRenameParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneRename(params))
    }

    pub(crate) fn runtime_pane_input_set(
        &mut self,
        id: &'static str,
        params: PaneInputSetParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneInputSet(params))
    }

    pub(crate) fn runtime_pane_focus_direction(
        &mut self,
        id: &'static str,
        params: PaneFocusDirectionParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneFocusDirection(params))
    }

    pub(crate) fn runtime_pane_resize(
        &mut self,
        id: &'static str,
        params: PaneResizeParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneResize(params))
    }

    pub(crate) fn runtime_pane_swap(&mut self, id: &'static str, params: PaneSwapParams) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneSwap(params))
    }

    pub(crate) fn runtime_pane_split(
        &mut self,
        id: &'static str,
        mut params: PaneSplitParams,
    ) -> String {
        // N2 Part 3: splitting a foreign pane executes on the owning remote. Rewrite
        // the target to the raw remote workspace/pane ids and fire the action at the
        // origin; the new pane arrives via the next snapshot poll.
        if let Some((origin_key, raw_ws)) =
            self.foreign_workspace_target(params.workspace_id.as_deref())
        {
            params.workspace_id = Some(raw_ws);
            if params.target_pane_id.is_none() {
                params.target_pane_id = self.active_foreign_pane_id();
            }
            self.route_foreign_structure_action(origin_key, Method::PaneSplit(params));
            return String::new();
        }
        self.dispatch_runtime_mutation(id, Method::PaneSplit(params))
    }

    pub(crate) fn runtime_pane_zoom(&mut self, id: &'static str, params: PaneZoomParams) -> String {
        self.dispatch_runtime_mutation(id, Method::PaneZoom(params))
    }

    pub(crate) fn runtime_layout_set_split_ratio(
        &mut self,
        id: &'static str,
        params: LayoutSetSplitRatioParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::LayoutSetSplitRatio(params))
    }

    pub(crate) fn runtime_worktree_create_deferred(
        &mut self,
        id: &'static str,
        params: WorktreeCreateParams,
    ) -> Option<String> {
        self.dispatch_deferred_runtime_mutation(id, Method::WorktreeCreate(params))
    }

    pub(crate) fn runtime_worktree_open(
        &mut self,
        id: &'static str,
        params: WorktreeOpenParams,
    ) -> String {
        self.dispatch_runtime_mutation(id, Method::WorktreeOpen(params))
    }

    pub(crate) fn runtime_worktree_remove_deferred(
        &mut self,
        id: &'static str,
        params: WorktreeRemoveParams,
    ) -> Option<String> {
        self.dispatch_deferred_runtime_mutation(id, Method::WorktreeRemove(params))
    }
}
