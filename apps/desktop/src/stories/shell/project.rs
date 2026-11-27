//! Storybook stories for opening and switching projects (`Projects/Open project`,
//! `Projects/Project switcher`).

use crate::pickers::PickerHost as _;
use rune::{El, Play, Tx, col, row};

use crate::palette::Pal;
use crate::pickers::Kind;
use crate::stories::{Story, Trigger, replay, stage};
use crate::{App, Route, view};

pub const OPEN_VARIANTS: &[(&str, usize)] = &[("Draft picker", 0), ("Opened session", 1)];

pub const OPEN_KEYS: &[Trigger] = &[
    replay(open_story),
    Trigger {
        key: 'o',
        label: "Open orbit folder",
        run: |s, tx| s.open_project_dir("/Users/demo/code/orbit", true, tx),
    },
    Trigger {
        key: 'p',
        label: "Toggle draft picker",
        run: |s, tx| {
            s.open_draft_tab();
            s.open_picker(Kind::Project);
            tx.instant();
        },
    },
];

pub const SWITCHER_VARIANTS: &[(&str, usize)] =
    &[("Recent projects", 0), ("Worktrees", 1), ("Home filter (compact)", 2), ("Home projects (wide)", 3)];

pub const SWITCHER_KEYS: &[Trigger] = &[
    replay(switcher_story),
    Trigger { key: 's', label: "Toggle ⇧⌘O switcher", run: |s, tx| s.toggle_project_switcher(tx) },
    Trigger {
        key: 'w',
        label: "Drill into lumen worktrees",
        run: |s, tx| {
            if !s.kernel.slots.get(&crate::project_switcher::PROJECT_SWITCHER).is_open() {
                s.open_project_switcher();
            }
            if let Some(id) = s.projects.iter().find(|p| p.name == "lumen").map(|p| p.id.clone()) {
                s.drill_project_worktrees(&id, tx);
            }
        },
    },
];

fn open_story(s: &mut App, _: &mut Play<App>) {
    s.finish_stream();
    s.close_project_switcher(false);
    if s.fixture_run.variant == 1 {
        s.open_project_dir("/Users/demo/code/orbit", true, &mut Tx::default());
    } else {
        s.open_draft_tab();
        s.open_picker(Kind::Project);
    }
}

fn switcher_story(s: &mut App, _: &mut Play<App>) {
    s.finish_stream();
    s.close_picker(false);
    s.close_project_switcher(false);
    s.project_state.home_menu_open = false;
    s.clear_home_project(&mut Tx::default());
    match *s.fixture_run.variant {
        1 => {
            s.open_project_switcher();
            s.kernel.slots.get_mut(&crate::project_switcher::PROJECT_SWITCHER).search.focused = false;
            if let Some(id) = s.projects.iter().find(|p| p.name == "lumen").map(|p| p.id.clone()) {
                s.drill_project_worktrees(&id, &mut Tx::default());
                s.kernel.slots.get_mut(&crate::project_switcher::PROJECT_SWITCHER).search.focused = false;
            }
        }
        2 => {
            *s.route = Route::Home;
            if let Some(id) = s.projects.iter().find(|p| p.name == "harbor").map(|p| p.id.clone()) {
                s.toggle_home_project(&id, &mut Tx::default());
            }
            s.project_state.home_menu_open = true;
        }
        3 => {
            *s.route = Route::Home;
            if let Some(id) = s.projects.iter().find(|p| p.name == "harbor").map(|p| p.id.clone()) {
                s.toggle_home_project(&id, &mut Tx::default());
            }
        }
        _ => {
            if let Some(id) = s.projects.iter().find(|p| p.name == "kiln").map(|p| p.id.clone()) {
                s.hide_project(&id, &mut Tx::default());
            }
            s.open_project_switcher();
            s.kernel.slots.get_mut(&crate::project_switcher::PROJECT_SWITCHER).search.focused = false;
        }
    }
}

fn story_view(s: &App, p: &Pal) -> El {
    let panel_w = if s.route == Route::Home && s.fixture_run.variant == 3 { 900.0 } else { 720.0 };
    let page = match &*s.route {
        Route::Draft(_) => view::draft_page(s, p, panel_w),
        Route::Session { id, .. } => view::session_page(s, p, id),
        Route::Home => view::home(s, p, [panel_w, 0.0], [0.0, 0.0]),
        Route::Automations => crate::automations::view::page(s, p, [panel_w, 0.0]),
    };
    row()
        .key("story-open-project")
        .w_full()
        .max_w(1180.0)
        .h_full()
        .fill(p.bg_deep)
        .radius(10.0)
        .clip_soft(0.0)
        .child(view::sidebar(s, p))
        .child(
            col()
                .key("story-panel")
                .grow(1.0)
                .shrink()
                .ml(8.0)
                .my(8.0)
                .mr(8.0)
                .radius(10.0)
                .fill(p.bg_base)
                .box_shadow(Pal::ring_out(p.ring))
                .clip_soft(0.0)
                .child(page),
        )
}

rune_register::register!(crate::stories::STORY_LIST: Story = [
    Story { path: "Projects/Open project", scene: |s| stage(s, story_view), knobs: &[], keys: OPEN_KEYS, variants: OPEN_VARIANTS },
    Story { path: "Projects/Project switcher", scene: |s| stage(s, story_view), knobs: &[], keys: SWITCHER_KEYS, variants: SWITCHER_VARIANTS },
]);
