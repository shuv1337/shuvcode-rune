//! Crate-root domain compatibility index for `opencode-desktop`.
//!
//! Physical code lives in seven progressive-disclosure domain directories (`shell`, `session`,
//! `workspace`, `surfaces`, `dialogs`, `stories`, `plugin_host`). Re-exporting them here keeps
//! internal `crate::<module>::*` paths concise while `lib.rs` stays focused on crate bootstrap.

#[allow(unused_imports)]
pub(crate) use crate::shell::{
    appearance,
    appearance::{background, knobs, language, text_rendering},
    chrome::{compositor, forms, glyph_masks, icons, scrollbar, status_toast, wheel, widgets},
    commands,
    commands::type_to_compose,
    diag::{diagnostics, motion_log},
    input::{escape, key_sequence, keymap, keys, shortcuts},
    menus::{add_menu, app_menu, menubar, popover, popup},
    narrow,
    nav::{history, layout_state, pinch, route},
    overlays,
    tabs,
    tabs::{actions as tab_actions, badges as tab_badges, hints as tab_hints, recently_closed},
    view,
};

#[allow(unused_imports)]
pub(crate) use crate::session::{
    composer::{
        add_selection, attachments, dictation, dropzone, free_limit, line_comments, location_missing, mentions, pickers,
        prompt_history, quote, quote::cards as quote_cards, slash, suggestions, undo,
    },
    events,
    events::{incoming, stream::pace as stream_pace},
    inbox::{
        model as inbox_model, moves as archive_moves, order as inbox_order, queue, send, send_failed, stop, submit,
    },
    menu as session_menu,
    menu::view as session_menu_view,
    meta::{
        catalog as session_catalog, context_window, cost as session_cost, open as session_open,
        state as session_state, status as session_status, title_change, turn_diffs, viewed as session_viewed,
    },
    pages as session_pages,
    timeline::{
        agent_rows, auto_accept, diagram, find, fork, genui, message_nav, permission, question,
        rewind, shell_fold, short_group, subagents, todos, tool_open,
    },
};

#[allow(unused_imports)]
pub(crate) use crate::workspace::{
    lifecycle::{
        idle, jobs, launch, new_session, new_worktree, page_sign_in, reconnect, restore, rune_mcp, start_location,
        served, storage, tips,
    },
    projects,
    search::{fulltext, home_search, project_switcher, recent_page, switcher, transcript_index},
    servers,
    system::{
        crash_notice, deep_links, dirs, errors, export_logs, jump_list, notifications, open_in,
        sounds, speech,
    },
    windows,
};
#[cfg(windows)]
pub(crate) use crate::workspace::system::install;
#[cfg(bundle_assets)]
pub(crate) use crate::workspace::system::bundle;

#[allow(unused_imports)]
pub(crate) use crate::surfaces::{browser, egg, files, glitch, review, terminal, updater};
#[cfg(feature = "office")]
pub(crate) use crate::surfaces::office;

#[allow(unused_imports)]
pub(crate) use crate::dialogs::{connect, mcp, model_picker, model_prefs, settings};

#[allow(unused_imports)]
pub(crate) use crate::stories::{demo, demo_script, fixture_ids, open as open_story, scenes};
