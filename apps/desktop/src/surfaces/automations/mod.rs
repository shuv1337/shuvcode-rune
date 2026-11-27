//! Experimental Automations screen (packages/app/src/automations/, opencode#66393 + #66635).
//!
//! Shows where automations run (server, folder, project) and which session they wake, with
//! pause/resume/delete controls, session links, and New automation draft creation.

#![forbid(unsafe_code)]

mod delete_dialog;
pub mod format;
pub mod source;
mod story;
pub mod view;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use opencode_kernel::{CommandEntry, Feature, KernelBuilder, OverlayRegistration, Slot};
use opencode_kit::servers::ServerKey;
use rune::{Icon, TextEdit, Tx};

use crate::{App, Route};

use source::{BgResult, MutatePhase, Operation, ServerState};

pub const DELETE_OVERLAY_ID: &str = "automations.delete";

/// clock icon from packages/ui/src/icons/catalog.ts (opencode#66393).
pub static CLOCK: Icon = Icon {
    name: "clock",
    view: 16.0,
    body: r#"<path d="M14.5 8C14.5 11.5899 11.5899 14.5 8 14.5C4.41015 14.5 1.5 11.5899 1.5 8C1.5 4.41015 4.41015 1.5 8 1.5C11.5899 1.5 14.5 4.41015 14.5 8Z" stroke="currentColor"/><path d="M8 4.5V8L10.5 9.75" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeleteModal {
    #[default]
    Closed,
    Confirm {
        key: String,
        server: ServerKey,
        id: String,
        name: String,
    },
}

pub struct State {
    pub servers: BTreeMap<ServerKey, ServerState>,
    pub fixture_mode: bool,
    pub query: TextEdit,
    pub selected: Option<String>,
    pub show_done: bool,
    pub mutate: MutatePhase,
    pub delete_modal: DeleteModal,
    pub results: Arc<Mutex<Vec<BgResult>>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            fixture_mode: false,
            query: TextEdit::default(),
            selected: None,
            show_done: false,
            mutate: MutatePhase::Idle,
            delete_modal: DeleteModal::Closed,
            results: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

pub(crate) static AUTOMATIONS: Slot<State> = Slot::new();

/// Whether the experimental Automations feature is enabled (source.ts:available).
pub fn available(s: &App) -> bool {
    s.settings.automations()
}

/// Active automation count shown on the sidebar Automations action (source.ts:active).
pub fn active_count(s: &App) -> usize {
    if !available(s) {
        return 0;
    }
    s.kernel
        .slots
        .get(&AUTOMATIONS)
        .servers
        .values()
        .flat_map(|srv| srv.rows.iter())
        .filter(|r| r.status == opencode_kit::model::AutomationStatus::Active)
        .count()
}

/// Navigates to /automations or back to the prior tab / Home when already there
/// (shell/tabs/tabs.tsx:872-892).
pub fn toggle(s: &mut App, tx: &mut Tx) {
    s.close_settings();
    if *s.route == Route::Automations {
        if let Some(uid) = s.gestures.home_return_tab.take()
            && s.tabs.iter().any(|tab| tab.uid() == uid)
        {
            s.select_tab(&uid, tx);
            return;
        }
        s.go_home(tx);
        return;
    }
    if let Some(cur) = s.current_tab() {
        *s.gestures.home_return_tab = Some(cur);
    }
    *s.route = Route::Automations;
    tx.instant();
}

/// Opens /automations directly (used by Home's utility nav, home/route.tsx:44).
pub fn open(s: &mut App, tx: &mut Tx) {
    s.close_settings();
    if let Some(cur) = s.current_tab() {
        *s.gestures.home_return_tab = Some(cur);
    }
    *s.route = Route::Automations;
    tx.instant();
}

/// Opens the session referenced by an automation (model.ts:openSession).
pub fn open_session(s: &mut App, server: &ServerKey, session_id: &str, tx: &mut Tx) {
    if *server != *crate::servers::runtime::active(s) {
        crate::servers::runtime::switch(s, server);
    }
    s.open(session_id, tx);
}

/// Opens a new draft tab placed in the automation target's server & directory, pre-filled with
/// prompt (model.ts:openNewAutomation, new-session/workspace/controller.ts:16-30).
pub fn open_new_automation(s: &mut App, prompt: &str, tx: &mut Tx) {
    let all = source::entries(s);
    let filtered = source::filtered_entries(s, &all);
    let selected = source::selected_entry(s, &filtered);
    let Some((server, directory)) = source::draft_target(s, selected.as_ref()) else { return };
    if server != *crate::servers::runtime::active(s) {
        crate::servers::runtime::switch(s, &server);
    }
    s.new_session_in(&directory, tx);
    crate::new_worktree::forget_new_worktree(s);
    s.composer.text = prompt.to_owned();
    s.composer.anchor = s.composer.text.len();
    s.composer.focus = s.composer.text.len();
    s.composer.focused = true;
    tx.focus("prompt", false);
}

pub fn ask_delete(s: &mut App, key: String, server: ServerKey, id: String, name: String) {
    s.kernel.slots.get_mut(&AUTOMATIONS).delete_modal = DeleteModal::Confirm { key, server, id, name };
}

pub fn cancel_delete(s: &mut App, tx: &mut Tx) -> bool {
    let open = !matches!(s.kernel.slots.get(&AUTOMATIONS).delete_modal, DeleteModal::Closed);
    if open {
        s.kernel.slots.get_mut(&AUTOMATIONS).delete_modal = DeleteModal::Closed;
        tx.instant();
    }
    open
}

pub fn confirm_delete(s: &mut App, key: &str, server: &ServerKey, id: &str, tx: &mut Tx) {
    let entry = source::entries(s).into_iter().find(|e| e.key == key && e.server == *server && e.row.id == id);
    s.kernel.slots.get_mut(&AUTOMATIONS).delete_modal = DeleteModal::Closed;
    if let Some(entry) = entry {
        source::mutate(s, &entry, Operation::Delete, tx);
    }
}

pub struct AutomationsFeature;

impl Feature<App> for AutomationsFeature {
    fn id(&self) -> &'static str {
        "feature.automations"
    }

    fn register(&self, builder: &mut KernelBuilder<App>) {
        builder.register_slot(&AUTOMATIONS);
        builder.add_command(
            CommandEntry::<App>::from_key(
                "automations.open",
                opencode_i18n::key!("automations.title"),
                crate::commands::order::AUTOMATIONS,
                toggle,
            )
            .section("view")
            .when(available),
        );
        builder.register_overlay(
            OverlayRegistration::<App>::new(DELETE_OVERLAY_ID)
                .with_render(crate::overlays::layer::ACTION_DIALOG, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    delete_dialog::view(s, p, cx)
                })
                .with_dismiss(|s, tx| { cancel_delete(s, tx); }),
        );
        builder.schedule(crate::tick::stage::REACT_SHELL, "automations.poll", |s, now, tx| {
            source::poll(s, now, tx)
        });
    }
}
