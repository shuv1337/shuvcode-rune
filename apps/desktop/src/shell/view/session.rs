mod host;

use rune_kit::ScrollBar;

use super::*;

#[allow(unused_imports)]
pub(crate) use session_view::timeline::{
    BUBBLE_MAX, EAGER_ROWS, Place, ROW_BUILDS, RowContent, RowSpec, divider_runs, interrupted,
    joins, markdown_style, prompts_join, readable, row_keys, timeline, timeline_reset_row_builds,
    timeline_row_builds,
};

pub(crate) fn markdown_tinted(p: &Pal, src: &str, fade: f32, streaming: bool, color: Color, width: f32) -> El {
    session_view::style::markdown_tinted::<App>(p, src, fade, streaming, color, width)
}

// ---------------------------------------------------------------------------------------------
// Session

pub(crate) fn session_page(s: &App, p: &Pal, id: &str) -> El {
    // A session its server can't give: what Desktop's route shows instead (`session_state`).
    if let Some(panel) = crate::session_state::panel(s, p, id) {
        return col().key("session").grow(1.0).w_full().child(panel);
    }
    // A narrow window hides the header under its view switcher (`screen.tsx`'s `hideHeader`).
    if s.narrow() {
        return crate::narrow::session::page(s, p, id, || session_body(s, p, id, col().key("conversation").grow(1.0).w_full().min_h(0.0), false));
    }
    session_body(s, p, id, col().key("session").grow(1.0).w_full(), true)
}

/// The session page: the timeline with its header stuck over it (`with_header`), and what sits
/// under it.
///
/// The timeline, its header and the pills over the composer are one region: a keystroke in the
/// composer builds none of them. Everything they show is read inside it; what isn't `Tracked`
/// is its inputs (the palette, the chrome, the column's width).
pub(super) fn session_body(s: &App, p: &Pal, id: &str, page: El, with_header: bool) -> El {
    let info = s.session(id);
    // The column's width is a thread-local the composer reads too: set every build, even
    // when the region is reused or held.
    let content_w = session_view::TimelineHost::content_width(s);
    session_view::style::set_content_w(content_w);
    let displayed = s.displayed_session(id);
    let timeline = match displayed {
        Some(shown) => {
            let child = s.session(id).is_some_and(|i| i.parent.is_some());
            // Pills sit outside `"timeline-area"` so job updates and activity ticks never dirty
            // the timeline region while `has_pills` stays the same (`ours.md` §3.2).
            // Nor does a session whose folder is gone (`region.tsx`: `above` hides while `missing()`).
            let pill_list = if child || crate::location_missing::replaces_composer(s) { Vec::new() } else { pills(s, p, id) };
            let has_pills = !pill_list.is_empty();
            // A long transcript's projection fills in above its newest rows on the frame after its first.
            let inputs = (
                (p.theme, p.generation, p.dark),
                opencode_i18n::locale().id(),
                format!("{:?}", super::chrome::chrome()),
                content_w.to_bits(),
                with_header,
                session_view::style::hover_none(),
                session_view::style::permanent_cpu_canvas(),
                crate::plugin_host::surfaces_version(s),
                s.family_transcript_version(shown),
                (
                    crate::speech::listening(s),
                    session_view::timeline::projection_wanted(),
                    session_view::scroller::device_scale().to_bits(),
                    s.kernel.slots.untracked(&crate::files::FILES).reply_version(shown),
                    s.kernel.slots.untracked(&crate::subagents::SHELLS).follow.version(),
                    has_pills,
                    session_view::TimelineHost::more_above(s, shown),
                    s.opening.as_ref().filter(|o| o.id == shown).and_then(|o| o.error()).map(str::to_owned),
                ),
            );
            let lane = session_view::jump::lane(has_pills);
            let top = if with_header { session_view::style::HEADER_H } else { 0.0 };
            let scroll_fade = rune::region_with("timeline-fade", (id, shown, inputs), || {
                let t = rune::Tracked::untracked(&s.transcripts).get(shown).filter(|_| !s.loading_session(id));
                // The header scrolls with the timeline and sticks over its top (`virtualizer.tsx:1175`).
                // A session its server hasn't listed yet (a launch offline) is named by what its tab saved.
                let saved = s.session(shown).is_none().then(|| s.tabs.iter().find(|t| t.id == shown && t.kind == crate::tabs::TabKind::Session && !t.title.is_empty())).flatten().map(|t| {
                    opencode_kit::model::SessionInfo { id: t.id.clone(), title: t.title.clone(), directory: t.directory.clone(), ..Default::default() }
                });
                let head = with_header.then(|| header(s, p, s.session(shown).or(saved.as_ref()), t));
                let scroll = session_view::timeline::timeline(s, p, shown, t, lane, head);
                session_view::jump::timeline_scroll(s, p, shown, scroll, has_pills)
            });
            // Previous and Next message read the prompt in view from where it lies (`message_nav`).
            // Scope scrollbar hover to the timeline area above `composer_dock`, matching
            // `jump::over` (`jump.rs:308`) and official `screen.tsx:446-455` + `session-composer-region.tsx:52-60`
            // (where the composer dock is a `z-[70]` sibling above `viewRoot`, not inside it).
            session_view::jump::over_scroll(s, p, shown, scroll_fade, pill_list, top)
                .scroll_bar_hover(&format!("scroll-{shown}"))
                .on_layout_tx(|s: &mut App, r: [f32; 4], tx: &mut Tx| {
                s.kernel.slots.get_mut(&crate::message_nav::MESSAGE_NAV).view = Some(r);
                if let Some((id, row, place_y)) = s.layout.narrow.reading.borrow_mut().take()
                    && s.narrow() && s.active_session_id() == Some(id.as_str()) && !s.timeline_fx.ends.due(&id)
                {
                    // Official preserves `scrollTop` across the wide-to-narrow crossing while
                    // dropping the 48 pt wide header offset (`virtualizer.tsx:155,1206`: 64 -> 16).
                    tx.scroll_key_to(row, place_y - session_view::style::HEADER_H);
                    return;
                }
                tx.live_only();
            })
        }
        None => {
            let head = with_header.then(|| {
                let t = rune::Tracked::untracked(&s.transcripts).get(id).filter(|_| !s.loading_session(id));
                let saved = s.session(id).is_none().then(|| s.tabs.iter().find(|t| t.id == id && t.kind == crate::tabs::TabKind::Session && !t.title.is_empty())).flatten().map(|t| {
                    opencode_kit::model::SessionInfo { id: t.id.clone(), title: t.title.clone(), directory: t.directory.clone(), ..Default::default() }
                });
                header(s, p, s.session(id).or(saved.as_ref()), t)
            });
            col().key("timeline-area").grow(1.0).w_full().children(head)
        }
    };
    let mut page = page
        .on_layout_tx(|s: &mut App, r: [f32; 4], tx: &mut Tx| {
            s.kernel.slots.get_mut(&crate::message_nav::MESSAGE_NAV).page = Some(r);
            tx.live_only();
        })
        // Only held content belongs to the tab we left. In particular a completed size
        // refusal contains real, selectable output and a working Copy preview action.
        .child(if s.opening.as_ref().is_some_and(|o| !o.loading()) { timeline.inert() } else { timeline })
        .child(composer_dock(s, p));
    page = page.children(s.diag.debug.then(|| status_bar(s, p, info)));
    page
}

/// The pills over the composer: the subagent capsule, then the todo pill beside it.
fn pills(s: &App, p: &Pal, id: &str) -> Vec<El> {
    // Background activity's Hidden takes the running work's capsule away (`activity-chips.tsx`).
    let capsule = (s.settings.background_activity != crate::settings::BackgroundActivity::Off).then(|| crate::subagents::capsule(s, p, id)).flatten();
    let gap = if capsule.is_some() { crate::jump::GAP } else { 0.0 };
    // The timeline's edge rises over their row first, so a pill fades in only once nothing is
    // under it, and goes at once so the edge can come back down (t1129).
    let enter = |pill: El| pill.transition(rune::Transition::asymmetric(rune::Transition::opacity(), rune::Transition::none())).enter_delay(PILL_WAIT);
    let own: Vec<El> = capsule.into_iter().chain(crate::todos::pill(s, p, id).map(|t| t.ml(gap))).collect();
    // Plugins' pills follow the app's (`SessionPluginPills`), 8 apart.
    let gap = if own.is_empty() { 0.0 } else { 8.0 };
    let plugins = crate::plugin_host::pill_views(s, p, id).into_iter().enumerate().map(|(k, pill)| pill.ml(if k == 0 { gap } else { 8.0 }));
    own.into_iter().chain(plugins).map(enter).collect()
}

/// How long the timeline's edge takes to clear the pills' row (the page's spring).
const PILL_WAIT: f32 = 0.3;

/// The storybook's stages (`stories.rs`): the open session's timeline and what sits under it,
/// its header, and its composer with the subagent capsule over it.
pub(crate) fn story_session(s: &App, p: &Pal) -> El {
    let Route::Session { id, .. } = &*s.route else { return el() };
    session_body(s, p, id, col().key("session").grow(1.0).w_full().min_h(0.0), false)
}

pub(crate) fn story_header(s: &App, p: &Pal) -> El {
    let Route::Session { id, .. } = &*s.route else { return el() };
    header(s, p, s.session(id), s.transcripts.get(id))
}

pub(crate) fn story_composer(s: &App, p: &Pal) -> El {
    let Route::Session { id, .. } = &*s.route else { return composer_dock(s, p) };
    let pills = if s.session(id).is_some_and(|i| i.parent.is_some()) { Vec::new() } else { pills(s, p, id) };
    col().key("session")
        .w_full()
        .children((!pills.is_empty()).then(|| col().key("activity").w_full().center().px(DOCK_SIDE).pb(8.0)
            .child(row().key("activity-lane").w_full().max_w(session_view::style::dock_w()).children(pills))))
        .child(composer_dock(s, p))
}
