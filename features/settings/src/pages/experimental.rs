//! Settings › Experimental (`settings/experimental/experimental.tsx`): Organize vertical sidebar,
//! New question UI and Smooth timeline; then the host's Developer section (Rewind).

use opencode_i18n::t;
use opencode_ui::popup::PopupHost;
use rune::{El, Tx, col};
use opencode_ui::palette::Pal;

use super::{header, page, select};
use crate::host::SettingsHost;
use crate::{Choice as _, Organize, change_settings, list, setting, switch};

fn toggle_new_question_ui(s: &mut dyn SettingsHost) {
    change_settings(s, |x| x.new_question_ui = !x.new_question_ui);
}

fn toggle_tab_groups(s: &mut dyn SettingsHost) {
    change_settings(s, |x| x.tab_groups = Some(!x.tab_groups()));
}

fn toggle_automations(s: &mut dyn SettingsHost) {
    change_settings(s, |x| x.automations = Some(!x.automations()));
}

fn toggle_sidebar_sections(s: &mut dyn SettingsHost) {
    change_settings(s, |x| x.sidebar_sections = Some(!x.sidebar_sections()));
}

fn toggle_smooth_timeline(s: &mut dyn SettingsHost) {
    change_settings(s, |x| x.smooth_timeline = !x.smooth_timeline);
}

fn pick_organize(s: &mut dyn SettingsHost, id: &str, tx: &mut Tx) {
    change_settings(s, |x| x.sidebar_organize = Organize::parse(Some(id)));
    s.sync_tab_layout(tx);
}

pub fn view<H: SettingsHost + PopupHost + 'static>(s: &H, p: &Pal) -> El {
    let st = s.settings();
    let rows = vec![
        (
            "settings-tab-groups",
            setting(
                p,
                "settings-tab-groups",
                t!("settings.experimental.row.tabGroups.title"),
                t!("settings.experimental.row.tabGroups.description"),
                switch(p, "tab-groups-switch", st.tab_groups()).on_press(|s: &mut H| toggle_tab_groups(s)),
            ),
        ),
        (
            "settings-sidebar-sections",
            setting(
                p,
                "settings-sidebar-sections",
                t!("rune.settings.experimental.row.sidebarSections.title"),
                t!("rune.settings.experimental.row.sidebarSections.description"),
                switch(p, "sidebar-sections-switch", st.sidebar_sections()).on_press(|s: &mut H| toggle_sidebar_sections(s)),
            ),
        ),
        (
            "settings-sidebar-organize",
            setting(
                p,
                "settings-sidebar-organize",
                t!("settings.experimental.row.sidebarOrganize.title"),
                t!("settings.experimental.row.sidebarOrganize.description"),
                organize(s, p),
            ),
        ),
        (
            "settings-automations",
            setting(
                p,
                "settings-automations",
                t!("settings.experimental.row.automations.title"),
                t!("settings.experimental.row.automations.description"),
                switch(p, "automations-switch", st.automations()).on_press(|s: &mut H| toggle_automations(s)),
            ),
        ),
        (
            "settings-new-question-ui",
            setting(
                p,
                "settings-new-question-ui",
                t!("settings.experimental.row.newQuestionUI.title"),
                t!("settings.experimental.row.newQuestionUI.description"),
                switch(p, "new-question-ui-switch", st.new_question_ui).on_press(|s: &mut H| toggle_new_question_ui(s)),
            ),
        ),
        (
            "settings-smooth-timeline",
            setting(
                p,
                "settings-smooth-timeline",
                t!("rune.settings.experimental.row.smoothTimeline.title"),
                t!("rune.settings.experimental.row.smoothTimeline.description"),
                switch(p, "smooth-timeline-switch", st.smooth_timeline).on_press(|s: &mut H| toggle_smooth_timeline(s)),
            ),
        ),
    ];
    let developer = s.developer_section(p).map(|section| super::revealable(s, p, "record-glitches", 0.0, section));
    let body = col()
        .w_full()
        .gap(32.0)
        .child(list(p, "experimental-list", super::rows(s, p, 20.0, rows)))
        .children(developer);
    page(p, "settings-experimental-page", header(p, t!("settings.tab.experimental"), t!("settings.experimental.description")), body)
}

/// Organize vertical sidebar's select (`experimental.tsx:32-45`): the choices this build offers.
fn organize<H: SettingsHost + PopupHost + 'static>(s: &H, p: &Pal) -> El {
    let options = Organize::options().iter().map(|o| (o.name().to_owned(), o.label().to_owned())).collect();
    let current = s.settings().sidebar_organize.effective().name();
    select(s, p, "settings-sidebar-organize-select", t!("settings.experimental.row.sidebarOrganize.title"), options, current, pick_organize)
}
