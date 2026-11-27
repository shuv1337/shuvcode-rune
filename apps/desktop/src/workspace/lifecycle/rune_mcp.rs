//! Desktop host adapter for `opencode-feature-workspace-tools::rune_mcp`
//! (`features/workspace-tools`).

pub use opencode_feature_workspace_tools::rune_mcp::*;

use opencode_kit::client::Server;
use opencode_kit::client_mcp::McpCall;
use opencode_kit::model::{Project, SessionInfo};
use opencode_kit::servers::ServerKey;
use rune::{Tx, Waker};

use crate::{App, Route, Source};

/// Binds desktop's plugin MCP tool catalog and generation counter once per process.
pub fn init() {
    tools::configure(tools::ToolsProvider {
        catalog: crate::plugin_host::mcp_tools,
        generation: crate::plugin_host::mcp_tools_generation,
    });
}

pub(crate) static RUNE_MCP: opencode_kernel::Slot<WindowMcp> = opencode_kernel::Slot::new();

impl RuneMcpHost for App {
    fn rune_mcp(&self) -> &WindowMcp {
        self.kernel.slots.get(&RUNE_MCP)
    }

    fn rune_mcp_mut(&mut self) -> &mut WindowMcp {
        self.kernel.slots.get_mut(&RUNE_MCP)
    }

    fn is_launch_waiting(&self) -> bool {
        crate::launch::waiting(self)
    }

    fn is_stage(&self) -> bool {
        self.fixture_run.stage.is_some()
    }

    fn client_mcp_flag(&self) -> bool {
        crate::flag("--client-mcp")
    }

    fn live_server(&self) -> Option<(&Server, bool)> {
        match &*self.source {
            Source::Live { server, allow_send, .. } => Some((server, *allow_send)),
            Source::Fixture => None,
        }
    }

    fn route(&self) -> McpRoute<'_> {
        match &*self.route {
            Route::Home | Route::Automations => McpRoute::Home,
            Route::Draft(id) => McpRoute::Draft(id.as_str()),
            Route::Session { server, id } => McpRoute::Session { server, id: id.as_str() },
        }
    }

    fn draft_dir(&self) -> &str {
        App::draft_dir(self)
    }

    fn used_project_ids(&self) -> &[String] {
        &self.project_state.used
    }

    fn projects(&self) -> &[Project] {
        &self.projects
    }

    fn used_projects(&self) -> Vec<&Project> {
        App::used_projects(self)
    }

    fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    fn tab_at(&self, index: usize) -> Option<McpTab<'_>> {
        self.tabs.get(index).map(|t| McpTab {
            id: t.id.as_str(),
            directory: t.directory.as_str(),
            server: &t.server,
        })
    }

    fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    fn session(&self, id: &str) -> Option<&SessionInfo> {
        App::session(self, id)
    }

    fn sessions_version(&self) -> (u32, u64) {
        (rune::Tracked::id(&self.sessions), rune::Tracked::version(&self.sessions))
    }

    fn root_of<'a>(&'a self, id: &'a str) -> &'a str {
        App::root_of(self, id)
    }

    fn active_server(&self) -> &ServerKey {
        crate::servers::runtime::active(self)
    }

    fn browser_mcp_refusal(&self, session_id: &str) -> Option<&'static str> {
        let server_key = crate::servers::runtime::active(self);
        let same_machine = server_key.is_sidecar()
            && match &*self.source {
                Source::Live { server, .. } => server.is_local(),
                Source::Fixture => true,
            };
        if !same_machine {
            return Some(REMOTE_BROWSER_MCP_ERROR);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if crate::surfaces::browser::rpc::is_mcp_browser_suppressed(self, server_key, Some(session_id)) {
            return Some(RPC_ATTACHED_BROWSER_MCP_ERROR);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = session_id;
        None
    }

    fn dispatch_mcp_call(&mut self, session_id: &str, call: McpCall, tx: &mut Tx) -> Option<McpCall> {
        crate::plugin_host::mcp_call(self, session_id, call, tx)
    }

    fn expire_plugin_calls(&mut self, tx: &mut Tx) -> bool {
        crate::plugin_host::commands::expire_calls(self, tx)
    }
}

pub fn poll(s: &mut App, tx: &mut Tx) -> bool {
    opencode_feature_workspace_tools::rune_mcp::poll(s, tx)
}

impl App {
    pub fn setup_rune_mcp(&mut self, waker: Waker) {
        init();
        opencode_feature_workspace_tools::rune_mcp::setup_rune_mcp(self, waker);
    }

    pub fn close_rune_mcp(&mut self) {
        opencode_feature_workspace_tools::rune_mcp::close_rune_mcp(self);
    }
}
