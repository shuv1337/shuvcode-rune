//! Desktop host adapter for `opencode-feature-settings` (`features/settings`).

pub use opencode_feature_settings::*;

mod feature;
mod updater_fallback;
mod stories;

pub use feature::SettingsFeature;

use std::sync::atomic::{AtomicU32, Ordering};

use opencode_kit::integration::{Integration, McpServer};
use opencode_kit::model::{Project, SessionInfo};
use opencode_kit::servers::ServerKey;
use opencode_servers::ServerConn;
use opencode_servers::health::Health;
use opencode_servers::plugin_server::{PluginStage, PluginStatus};
use opencode_kit::wire_error::WireError;
use rune::{El, Tx};
use session_view::palette::Pal;
use session_view::pickers::Model;

use crate::App;
use crate::jobs::Apply;
use crate::pickers::PickerHost as _;

/// Configures the extracted settings crate with desktop's build metadata.
pub fn init() {
    opencode_feature_settings::configure(SettingsConfig {
        build_channel: crate::updater::build_channel(),
        build_version: crate::updater::build_version(),
    });
}

fn plugin_state_name(state: opencode_plugin_spec::ServerState) -> &'static str {
    ["stopped", "starting", "auth", "ready", "failed", "incompatible"]
        .get(state as usize)
        .copied()
        .unwrap_or("stopped")
}

fn make_server_entry(
    key: ServerKey,
    name: String,
    kind: ServerKind,
    connection: Option<ServerConn>,
    source: Option<Option<PluginStage>>,
) -> ServerEntry {
    let health = crate::servers::health::get(&key);
    let availability = opencode_servers::settings::availability(opencode_servers::settings::Input {
        connection: connection.as_ref(),
        source,
        health: health.as_ref(),
        signed_out: false,
    });
    // A plugin's server has its connection only while it's ready.
    let connection = connection.filter(|c| !matches!(c, ServerConn::Plugin { status: PluginStatus::Stage(_), .. }));
    let name = if name.is_empty() { key.as_str().to_owned() } else { name };
    ServerEntry { key, name, kind, connection, health, availability }
}

static ABOUT_OTHERS: AtomicU32 = AtomicU32::new(0);
static ABOUT_ASKED: std::sync::Once = std::sync::Once::new();

/// The contributors past the 16 credited, as GitHub's contributor list's last page says.
fn about_other_contributors(s: &App) -> u32 {
    if !crate::headless() {
        ABOUT_ASKED.call_once(|| {
            let waker = (*s.waker).clone();
            rune::task::spawn("about-contributors", move || {
                let agent: ureq::Agent = net_trust::agent_config().build().into();
                let link = agent
                    .get("https://api.github.com/repos/anomalyco/opencode/contributors?anon=1&per_page=1")
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .call()
                    .ok()
                    .and_then(|r| r.headers().get("link").and_then(|v| v.to_str().ok()).map(str::to_owned));
                let writers = pages::about::WRITERS_COUNT;
                let total = link.as_deref().and_then(last_page).filter(|n| *n > writers);
                ABOUT_OTHERS.store(total.map_or(pages::about::FALLBACK_OTHERS, |n| n - writers), Ordering::Relaxed);
                if let Some(w) = waker {
                    w();
                }
            })
            .ok();
        });
    }
    match ABOUT_OTHERS.load(Ordering::Relaxed) {
        0 => pages::about::FALLBACK_OTHERS,
        n => n,
    }
}

fn last_page(link: &str) -> Option<u32> {
    let last = link.split(',').find(|l| l.contains("rel=\"last\""))?;
    let at = last.find("page=")? + 5;
    last[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// `--about-trace <file>` (proof runs): records wordmark letters on each tick while moving.
fn trace_about_wordmark(now: f64, letters: &str) {
    use std::io::Write;
    thread_local! {
        static ON: bool = crate::arg("--about-trace").is_some();
        static OUT: std::cell::RefCell<Option<std::fs::File>> =
            std::cell::RefCell::new(crate::arg("--about-trace").and_then(|p| std::fs::File::create(p).ok()));
    }
    if !ON.with(|o| *o) {
        return;
    }
    OUT.with_borrow_mut(|f| {
        if let Some(f) = f {
            writeln!(f, "{{\"t\":{now:.6},\"letters\":\"{letters}\"}}").ok();
        }
    });
}

pub(crate) static SETTINGS_SCREEN: opencode_kernel::Slot<SettingsScreen> = opencode_kernel::Slot::new();
pub(crate) static SETTINGS_PAGES: opencode_kernel::Slot<pages::State> = opencode_kernel::Slot::new();

impl SettingsHost for App {
    fn settings(&self) -> &Settings {
        &self.settings
    }

    fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    fn settings_open(&self) -> bool {
        App::settings_open(self)
    }

    fn server_directory(&self) -> Option<&str> {
        crate::start_location::server_directory(self)
    }

    fn set_settings_open(&mut self, open: bool) {
        App::set_settings_open(self, open);
    }

    fn settings_pages(&self) -> &pages::State {
        self.kernel.slots.get(&SETTINGS_PAGES)
    }

    fn settings_pages_mut(&mut self) -> &mut pages::State {
        self.kernel.slots.get_mut(&SETTINGS_PAGES)
    }

    fn can_write(&self) -> bool {
        App::can_write(self)
    }

    fn set_zoom_level(&mut self, level: f32) {
        App::set_zoom_level(self, level);
    }

    fn tab_layout(&self) -> TabLayout {
        App::tab_layout(self)
    }

    fn set_tab_layout(&mut self, layout: TabLayout, tx: &mut Tx) {
        App::set_tab_layout(self, layout, tx);
    }

    fn sync_tab_layout(&mut self, tx: &mut Tx) {
        App::sync_tab_layout(self, tx);
    }

    fn narrow(&self) -> bool {
        App::narrow(self)
    }

    fn narrow_sm(&self) -> bool {
        self.layout.narrow.sm()
    }

    fn live_server_base(&self) -> Option<String> {
        match &*self.source {
            crate::Source::Live { server, .. } => Some(server.base.clone()),
            crate::Source::Fixture => None,
        }
    }

    fn set_language(&mut self, id: &str) {
        crate::language::set(self, id);
    }

    fn set_auto_accept(&mut self, on: bool) {
        App::set_auto_accept(self, on);
    }

    fn set_project_auto_accept(&mut self, project_id: &str, value: Option<bool>) {
        App::set_project_auto_accept(self, project_id, value);
    }

    fn set_pinch_zoom(&mut self, on: bool, tx: &mut Tx) {
        App::set_pinch_zoom(self, on, tx);
    }

    fn set_theme(&mut self, id: &str, tx: &mut Tx) {
        App::set_theme(self, id, tx);
    }

    fn set_ui_font(&mut self, index: usize, custom_name: &str, tx: &mut Tx) {
        self.set_ui_font_named(index, custom_name, tx);
    }

    fn set_code_font(&mut self, index: usize, custom_name: &str, tx: &mut Tx) {
        self.set_code_font_named(index, custom_name, tx);
    }

    fn set_terminal_font(&mut self, index: usize, custom_name: &str, tx: &mut Tx) {
        App::set_terminal_font(self, index, custom_name, tx);
    }

    fn set_reduce_motion(&mut self, mode: ReduceMotion) {
        App::set_reduce_motion(self, mode);
    }

    fn set_text_renderer(&mut self, renderer: TextRenderer) {
        App::set_text_renderer(self, renderer);
    }

    fn text_rendering_fallback_reason(&self) -> Option<&'static str> {
        match *self.appearance_state.text_rendering {
            crate::text_rendering::Rendering::FellBack { reason, .. } if !reason.is_empty() => Some(reason),
            _ => None,
        }
    }

    fn play_sound(&mut self, id: &str) {
        crate::sounds::play(id);
    }

    fn developer_section(&self, p: &Pal) -> Option<El> {
        Some(crate::glitch::settings_section(self, p))
    }

    fn about_info(&self) -> AboutInfo {
        AboutInfo {
            version: crate::updater::display_version(self),
            other_contributors: about_other_contributors(self),
        }
    }

    fn on_wordmark_tick(&mut self, now: f64, letters: &str) {
        trace_about_wordmark(now, letters);
    }

    fn is_copied(&self, key: &str) -> bool {
        session_view::copied::is_copied(self, key)
    }

    fn mark_copied(&mut self, key: &str, tx: &mut Tx) {
        session_view::copied::mark_copied(self, key, tx);
    }

    fn sync_catalogs(&mut self, dir: Option<&str>) {
        App::sync_catalogs(self, dir.map(str::to_owned));
    }

    fn integrations(&self, dir: Option<&str>) -> Option<&[Integration]> {
        App::integrations(self, dir)
    }

    fn provider_rows(&self, dir: Option<&str>) -> Vec<ProviderRow> {
        self.provider_infos(dir)
            .unwrap_or_default()
            .iter()
            .map(|pi| ProviderRow { id: pi.id.clone(), name: pi.name.clone(), integration: pi.integration.clone() })
            .collect()
    }

    fn open_connect(&mut self, provider: Option<&str>, from_settings: bool, tx: &mut Tx) {
        App::open_connect(self, provider, from_settings, tx);
    }

    fn models(&self) -> &[Model] {
        &self.composer_ext.pickers.models
    }

    fn model_visible(&self, model: &Model) -> bool {
        self.composer_ext.pickers.visible(model)
    }

    fn set_model_visibility(&mut self, key: &str, visible: bool) {
        self.composer_ext.pickers.prefs.set_visibility([key], visible);
    }

    fn sync_mcp(&mut self, dir: &str) {
        App::sync_mcp(self, dir);
    }

    fn mcp_servers(&self, dir: &str) -> Option<&[McpServer]> {
        App::mcp_servers(self, dir)
    }

    fn mcp_row_trailing(&self, p: &Pal, dir: &str, server: &McpServer) -> El {
        crate::mcp::view::trailing(self, p, dir, server)
    }

    fn wake_settings_plugins(&mut self) {
        crate::plugin_host::dormant::wake_settings(self);
    }

    fn plugin_pages(&self) -> Vec<PluginPageMeta> {
        let pairing = crate::plugin_host::settings_page_key(self, "pairing");
        crate::plugin_host::settings_page_ids(self)
            .into_iter()
            .map(|(id, in_about_group)| {
                let title = crate::plugin_host::settings_page_title(self, &id).unwrap_or_else(|| id.clone());
                let icon = crate::plugin_host::settings_page_icon(self, &id).unwrap_or(&rune_theme_opencode::icons::EXTENSIONS);
                let in_server_group = !in_about_group && id == pairing;
                PluginPageMeta { id, title, icon, in_about_group, in_server_group }
            })
            .collect()
    }

    fn plugin_page_view(&self, p: &Pal, id: &str) -> Option<(String, String, El, Option<El>)> {
        let (title, description, body) = crate::plugin_host::settings_page(self, p, id)?;
        let actions = crate::plugin_host::settings_page_actions(self, p, id);
        Some((title, description, body, actions))
    }

    fn plugin_sections_for_page(&self, p: &Pal, page_id: &str) -> Vec<El> {
        let mut sections = crate::plugin_host::settings_pages(self, p, page_id);
        if page_id == "general" {
            updater_fallback::maybe_push(self, p, &mut sections);
        }
        sections
    }

    fn plugin_extensions_section(&self, p: &Pal) -> Option<El> {
        crate::plugin_host::settings_section(self, p)
    }

    fn plugin_search_entries(&self) -> Vec<PluginSearchItem> {
        crate::plugin_host::settings_search(self)
            .into_iter()
            .map(|e| PluginSearchItem {
                owner: e.owner,
                page: e.page,
                id: e.id,
                title: e.title,
                description: e.description,
                keywords: e.keywords,
            })
            .collect()
    }

    fn resolve_plugin_page(&self, page_id: &str) -> (String, Option<String>) {
        (
            crate::plugin_host::settings_page_key(self, page_id),
            crate::plugin_host::settings_page_title(self, page_id),
        )
    }

    fn commands_catalog(&self) -> Vec<CommandItem> {
        // Desktop registers the speed command only for a model that has speeds
        // (`composer/commands.tsx`): no row for it while the server offers none.
        let speeds = self.composer_ext.pickers.models.iter().any(|m| m.speed.is_some());
        self.catalog()
            .into_iter()
            .filter(|c| speeds || c.id != session_view::pickers::speed::CYCLE_COMMAND)
            .map(|c| CommandItem {
                id: c.id,
                title: c.title.to_owned(),
                section: c.section,
                chord: c.chord,
                has_keys: c.keys.is_some(),
                follows: c.follows,
            })
            .collect()
    }

    fn has_command(&self, id: &str) -> bool {
        self.commands().iter().any(|c| c.id == id)
    }

    fn run_command(&mut self, id: &str, tx: &mut Tx) {
        App::run_command(self, id, tx);
    }

    fn projects(&self) -> &[Project] {
        &self.projects
    }

    fn used_projects(&self) -> Vec<&Project> {
        App::used_projects(self)
    }

    fn other_projects(&self) -> Vec<&Project> {
        App::other_projects(self)
    }

    fn project_at(&self, dir: &str) -> Option<&Project> {
        App::project_at(self, dir)
    }

    fn apply_project(&mut self, updated: Project) {
        App::apply_project(self, updated);
    }

    fn touch_project(&mut self, id: &str) {
        App::touch_project(self, id);
    }

    fn project_has_unread(&self, project: &Project) -> bool {
        App::project_has_unread(self, project)
    }

    fn clear_project_notifications(&mut self, project: &Project, tx: &mut Tx) {
        App::clear_project_notifications(self, project, tx);
    }

    fn open_project_dialog(&mut self, tx: &mut Tx) {
        App::open_project_dialog(self, tx);
    }

    fn project_badge(&self, p: &Pal, project: Option<&Project>, label: &str) -> El {
        crate::icons::badge(self, p, project, label)
    }

    fn project_avatar_image(&self, p: &Pal, project: &Project, size: f32) -> El {
        self.appearance_state.icons.view(&crate::icons::palette(p), project, size)
    }

    fn reveal_label(&self) -> &'static str {
        crate::view::location_card::reveal_label()
    }

    fn reveal_path(&self, dir: &str) -> Result<(), String> {
        crate::view::location_card::reveal_target(self, dir).and_then(|t| crate::view::location_card::reveal(&t))
    }

    fn list_worktrees(&mut self, project_id: &str) {
        App::list_worktrees(self, project_id);
    }

    /// Settings > Worktrees lists the worktrees OpenCode made, not every checkout Git knows
    /// (`managedWorkspaceDirectories`, `app/src/workspaces/paths.ts:26-31`).
    fn project_worktrees(&self, project_id: &str, _directory: &str) -> Vec<String> {
        App::managed_worktrees(self, project_id)
    }

    fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    fn active_session_directory(&self) -> Option<String> {
        self.active_session_id().and_then(|id| self.session(id).map(|i| i.directory.clone()))
    }

    fn servers_inventory(&self) -> Vec<ServerEntry> {
        let contributed = crate::plugin_host::contributed_servers(self);
        let saved = crate::servers::list();
        let own_address = crate::servers::runtime::own_server().map(|s| s.base).unwrap_or_default();
        let mut out: Vec<ServerEntry> = crate::servers::registry::list(self)
            .into_iter()
            .filter_map(|e| {
                let key = e.key();
                let name = crate::servers::registry::label(&e.conn);
                let kind = match &e.conn {
                    ServerConn::Sidecar { .. } => ServerKind::Own { address: own_address.clone() },
                    ServerConn::Http { .. } => {
                        let x = saved
                            .iter()
                            .find(|x| ServerConn::http_server(&x.url).map(|c| c.key()) == Some(key.clone()))?;
                        ServerKind::Http(HttpServer { url: x.url.clone(), password: x.password.clone(), name: x.name.clone() })
                    }
                    ServerConn::Plugin { .. } => {
                        let c = contributed.iter().find(|c| c.key == key.as_str())?;
                        ServerKind::Plugin(PluginServerInfo {
                            key: c.key.clone(),
                            name: c.rec.name.clone(),
                            label: c.rec.label.clone(),
                            listed: c.rec.listed,
                            state_name: plugin_state_name(c.rec.state),
                        })
                    }
                };
                // A sign-in on its way shows as starting (`selectPluginStatus`).
                let conn = match e.conn {
                    ServerConn::Plugin { key: k, plugin, managed, display_name, label, .. }
                        if crate::plugin_host::server_authenticating(k.as_str()) =>
                    {
                        ServerConn::Plugin { key: k, plugin, status: PluginStatus::Stage(PluginStage::Starting), managed, display_name, label }
                    }
                    conn => conn,
                };
                Some(make_server_entry(key, name, kind, Some(conn), None))
            })
            .collect();
        for c in contributed.iter().filter(|c| !c.by_address() && !c.rec.listed) {
            let key = ServerKey::new(&c.key);
            let stage = (c.rec.state != opencode_plugin_spec::ServerState::Ready).then(|| crate::plugin_host::servers_stage(c.rec.state));
            let info = PluginServerInfo {
                key: c.key.clone(),
                name: c.rec.name.clone(),
                label: c.rec.label.clone(),
                listed: c.rec.listed,
                state_name: plugin_state_name(c.rec.state),
            };
            out.push(make_server_entry(key, c.rec.name.clone(), ServerKind::Plugin(info), None, Some(stage)));
        }
        out
    }

    fn server_health(&self, key: &ServerKey) -> Option<Health> {
        crate::servers::health::get(key)
    }

    fn desired_server_key(&self) -> ServerKey {
        crate::servers::runtime::desired(self)
    }

    fn active_server_key(&self) -> ServerKey {
        crate::servers::runtime::active(self).clone()
    }

    fn is_server_listed(&self, key: &ServerKey) -> bool {
        crate::servers::runtime::listed().iter().any(|l| l.key == *key)
    }

    fn switch_server(&mut self, key: &ServerKey) {
        crate::servers::runtime::switch(self, key);
    }

    fn retry_server_health(&mut self, tx: &mut Tx) -> Option<u64> {
        crate::servers::health::retry(self, tx)
    }

    fn server_health_retry_resolved(&self, waiter: u64) -> bool {
        crate::servers::health::resolved(waiter)
    }

    fn open_server_dialog(&mut self, edit_url: Option<&str>, tx: &mut Tx) {
        crate::servers::dialog::open(self, edit_url, tx);
    }

    fn remove_http_server(&mut self, url: &str, tx: &mut Tx) {
        crate::servers::remove_offering(self, url);
        crate::servers::dialog::sync(self, tx);
    }

    fn server_is_hidden(&self, key: &ServerKey) -> bool {
        crate::projects::home::hidden().contains(key) && !crate::servers::registry::visible(self).iter().any(|e| e.key() == *key)
    }

    fn server_can_hide(&self, key: &ServerKey) -> bool {
        let visible = crate::servers::registry::visible(self);
        visible.len() > 1 && crate::servers::registry::list(self).iter().any(|e| e.key() == *key && e.conn.builtin())
    }

    fn set_server_hidden(&mut self, key: &ServerKey, hidden: bool) {
        if hidden && crate::servers::registry::visible(self).iter().all(|e| e.key() == *key) {
            return;
        }
        crate::projects::home::set_hidden(key, hidden);
    }

    fn server_add_menu_items(&self) -> Vec<(String, String)> {
        crate::plugin_host::server_menu_items(self, "server.add")
            .into_iter()
            .map(|(k, t)| (k.to_owned(), t))
            .collect()
    }

    fn run_server_add_menu_item(&mut self, key: &str, tx: &mut Tx) {
        crate::plugin_host::run_menu_item(self, key, "", tx);
    }

    fn plugin_server_row(&self, p: &Pal, key: &str) -> Option<El> {
        let c = crate::plugin_host::contributed_servers(self).into_iter().find(|c| c.key == key)?;
        crate::plugin_host::server_row(self, p, &c)
    }

    fn can_page_sign_out(&self) -> bool {
        crate::page_sign_in::can_sign_out()
    }

    fn is_page_server(&self, url: &str) -> bool {
        crate::page_sign_in::is_page_server(url)
    }

    fn page_sign_out(&mut self) {
        crate::page_sign_in::sign_out();
    }

    fn before_settings_open(&mut self) {
        self.close_picker(false);
        self.close_switcher(false);
        self.composer.focused = false;
    }

    fn try_navigate_back_from_settings(&mut self, tx: &mut Tx) -> bool {
        self.project_navigation() && self.navigate(false, tx)
    }

    fn restore_focus_after_settings_close(&mut self, tx: &mut Tx) {
        if self.has_composer() && crate::auto_focus_composer(tx) {
            self.composer.focused = true;
        } else {
            tx.blur();
        }
    }

    fn on_toggle_settings_opened(&mut self, tx: &mut Tx) {
        if matches!(*self.route, crate::Route::Session { .. }) {
            tx.focus("settings", tx.focus_visible());
        }
    }

    fn open_external(&mut self, url: &str, tx: &mut Tx) {
        let trimmed = url.trim();
        let is_http = trimmed.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
            || trimmed.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"));
        if !browser_policy::is_safe_os_external_url(trimmed) {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if is_http && !self.external_url_allowed_for_session(self.active_session_id(), trimmed) {
            return;
        }
        let _ = is_http;
        crate::open_in::open_external(tx, trimmed);
    }

    fn toast_failed_wire(&mut self, title: String, err: &WireError) {
        crate::status_toast::failed_wire(self, &title, err);
    }

    fn spawn_job(&mut self, job: SettingsJob) {
        match job {
            SettingsJob::ActiveServer(request) => {
                self.job(move |srv| {
                    let reply = execute_request(srv, request);
                    Box::new(move |s: &mut App, tx: &mut Tx| apply_reply(s, reply, tx)) as Apply
                });
            }
            SettingsJob::ForServer { server, request } => {
                crate::servers::runtime::with_server(self, &server, |s| {
                    s.job(move |srv| {
                        let reply = execute_request(srv, request);
                        Box::new(move |s: &mut App, tx: &mut Tx| apply_reply(s, reply, tx)) as Apply
                    });
                });
            }
        }
    }
}
