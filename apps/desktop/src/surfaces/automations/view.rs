//! Automations screen: header, searchable status-grouped list, detail inspector, and delete
//! confirmation modal (packages/app/src/automations/{route,view}.tsx, opencode#66393).

use opencode_i18n::{plural, t};
use opencode_kit::model::{AutomationAction, AutomationOutcome, AutomationStatus, AutomationTarget, AutomationTrigger};
use opencode_ui::palette::{Pal, kit};
use opencode_ui::widgets::{BODY, button};
use rune::{Align, Color, Direction, El, TextStyle, Tx, col, el, row, spacer, text};
use rune_kit::{ButtonVariant, ControlSize};
use session_view::focus::FocusExt as _;
use serde_json::Value;

use crate::App;

use super::format::{format_date, relative_time, status_label};
use super::source::{self, Entry, MutatePhase, Operation};
use super::AUTOMATIONS;

const HEADER_TITLE: TextStyle = TextStyle::new(14.0, 530.0).lh(20.0).tracking(-0.04);
const GROUP_LABEL: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0);
const ROW_TITLE: TextStyle = TextStyle::new(13.0, 530.0).lh(20.0).tracking(-0.04);
const ROW_NOTE: TextStyle = TextStyle::new(12.0, 440.0).lh(20.0);
const ROW_SUB: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0);
const DETAIL_TITLE: TextStyle = TextStyle::new(16.0, 600.0).lh(24.0).tracking(-0.16);
const DETAIL_SUB: TextStyle = TextStyle::new(13.0, 440.0).lh(20.0);
const META_TEXT: TextStyle = TextStyle::new(12.0, 440.0).lh(20.0);
const CODE_TEXT: TextStyle = TextStyle::new(12.0, 440.0).lh(20.0).mono();
const SEARCH_TEXT: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0);

const STATUSES: [AutomationStatus; 3] = [
    AutomationStatus::Active,
    AutomationStatus::Paused,
    AutomationStatus::Done,
];

pub fn page(s: &App, p: &Pal, panel_sz: [f32; 2]) -> El {
    let all = source::entries(s);
    let filtered = source::filtered_entries(s, &all);
    let selected = source::selected_entry(s, &filtered);
    let target = source::draft_target(s, selected.as_ref());
    let wide = panel_sz[0] == 0.0 || panel_sz[0] >= 860.0;
    let show_search = panel_sz[0] == 0.0 || panel_sz[0] >= 640.0;

    let body = if all.is_empty() {
        empty_state(s, p, target.is_some())
    } else {
        split_body(s, p, &all, &filtered, selected.as_ref(), wide)
    };

    col()
        .key("automations-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(header(s, p, target.is_some(), show_search))
        .child(body)
}

fn header(s: &App, p: &Pal, has_draft: bool, show_search: bool) -> El {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    let k = kit(p);
    let mut right = row().key("automations-header-actions").gap(8.0).center_y();
    if show_search {
        let search = session_view::style::field(&st.query, SEARCH_TEXT)
            .key("automations-search")
            .label_static(t!("automations.search"))
            .single_line()
            .placeholder(t!("automations.search"), p.faint)
            .w(200.0)
            .h(28.0)
            .px(10.0)
            .py(6.0)
            .radius(6.0)
            .fill(p.layer1)
            .color(p.text)
            .caret(p.text, p.selection)
            .selection_style(p.field_selection)
            .focus_ring(1.0, 0.0, p.q.border_strong)
            .on_edit(|s: &mut App| &mut s.kernel.slots.get_mut(&AUTOMATIONS).query);
        right = right.child(search);
    }
    if has_draft {
        let new_btn = button(t!("automations.new"), ButtonVariant::Contrast)
            .control_size(ControlSize::Regular)
            .on_press_tx(|s: &mut App, tx: &mut Tx| super::open_new_automation(s, "/automation ", tx))
            .build(k)
            .key("automations-new");
        right = right.child(new_btn);
    }

    col()
        .key("automations-header")
        .w_full()
        .child(
            row()
                .w_full()
                .h(47.0)
                .px(10.0)
                .gap(8.0)
                .center_y()
                .window_drag()
                .child(crate::view::sidebar::collapsed_slot(s))
                .child(text(t!("automations.title")).key("automations-title").font(HEADER_TITLE).color(p.text).truncate())
                .child(spacer())
                .child(right),
        )
        .child(el().w_full().h(1.0).fill(p.border).a11y_hidden())
}

fn empty_state(s: &App, p: &Pal, has_draft: bool) -> El {
    let available = super::available(s);
    let loading = source::is_loading(s);
    let failed = source::is_failed(s);
    let heading = if !available {
        t!("automations.off")
    } else if failed {
        t!("automations.failed")
    } else if loading {
        t!("automations.loading")
    } else {
        t!("automations.empty.title")
    };

    let mut inner = col()
        .key("automations-empty")
        .max_w(480.0)
        .px(24.0)
        .gap(12.0)
        .items_center()
        .child(text(heading).font(HEADER_TITLE).color(p.text).align(Align::Center).wrap());

    if has_draft && !loading && !failed {
        let example = t!("automations.example");
        let ex_btn = button(example, ButtonVariant::Neutral)
            .control_size(ControlSize::Regular)
            .on_press_tx(move |s: &mut App, tx: &mut Tx| {
                super::open_new_automation(s, &format!("/automation {example}"), tx);
            })
            .build(kit(p))
            .key("automations-example");
        inner = inner
            .child(text(t!("automations.empty.description")).font(BODY).color(p.muted).align(Align::Center).wrap())
            .child(ex_btn);
    }

    col().w_full().grow(1.0).shrink().min_h(0.0).center().child(inner)
}

fn split_body(s: &App, p: &Pal, all: &[Entry], filtered: &[Entry], selected: Option<&Entry>, wide: bool) -> El {
    let multi_srv = source::multi_server(s);
    let multi_proj = source::multi_project(all);
    let list_col = list_pane(s, p, filtered, selected, multi_srv, multi_proj);
    let detail_col = detail_pane(s, p, selected, multi_srv, multi_proj);

    if wide {
        row()
            .key("automations-split")
            .w_full()
            .grow(1.0)
            .shrink()
            .min_h(0.0)
            .child(list_col.w(330.0).h_full())
            .child(el().w(1.0).h_full().fill(p.border).a11y_hidden())
            .child(detail_col.grow(1.0).shrink().min_w(0.0).h_full())
    } else {
        col()
            .key("automations-split")
            .w_full()
            .grow(1.0)
            .shrink()
            .min_h(0.0)
            .child(list_col.w_full().max_h(260.0))
            .child(el().w_full().h(1.0).fill(p.border).a11y_hidden())
            .child(detail_col.w_full().grow(1.0).shrink().min_h(0.0))
    }
}

fn list_pane(
    s: &App,
    p: &Pal,
    filtered: &[Entry],
    selected: Option<&Entry>,
    multi_srv: bool,
    multi_proj: bool,
) -> El {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    let has_query = !st.query.text.trim().is_empty();
    let mut content = col().key("automations-list").w_full().padding(8.0).gap(2.0);

    if filtered.is_empty() {
        content = content.child(
            row()
                .w_full()
                .px(12.0)
                .py(32.0)
                .justify_center()
                .child(text(t!("automations.empty.filtered")).font(BODY).color(p.faint)),
        );
    } else {
        for status in STATUSES {
            let group_items: Vec<&Entry> = filtered.iter().filter(|e| e.row.status == status).collect();
            if group_items.is_empty() {
                continue;
            }
            let is_done = status == AutomationStatus::Done;
            let open = !is_done || st.show_done || has_query;
            let mut section = col().key(format!("auto-group-{}", status.as_str())).w_full().gap(2.0).pb(12.0);
            let label = match status {
                AutomationStatus::Active => t!("automations.status.active"),
                AutomationStatus::Paused => t!("automations.status.paused"),
                AutomationStatus::Done => t!("automations.status.done"),
            };
            let count = text(group_items.len().to_string())
                .font(GROUP_LABEL.features("\"tnum\""))
                .color(p.faint);
            if is_done {
                let toggle_word = if open { t!("automations.group.hide") } else { t!("automations.group.show") };
                let btn = row()
                    .key("auto-group-done-toggle")
                    .role("button")
                    .label(format!("{label} {}", group_items.len()))
                    .expanded(open)
                    .w_full()
                    .h(28.0)
                    .px(10.0)
                    .gap(6.0)
                    .center_y()
                    .radius(6.0)
                    .focus_inset(p)
                    .hover_group()
                    .on_press_tx(|s: &mut App, tx: &mut Tx| {
                        tx.instant();
                        let st = s.kernel.slots.get_mut(&AUTOMATIONS);
                        st.show_done = !st.show_done;
                    })
                    .child(text(label).font(GROUP_LABEL).color(p.faint).group_color(p.muted))
                    .child(count)
                    .child(spacer())
                    .child(text(toggle_word).font(GROUP_LABEL).color(p.faint).group_color(p.muted));
                section = section.child(btn);
            } else {
                section = section.child(
                    row()
                        .w_full()
                        .h(28.0)
                        .px(10.0)
                        .gap(6.0)
                        .center_y()
                        .child(text(label).font(GROUP_LABEL).color(p.faint))
                        .child(count),
                );
            }
            if open {
                for entry in group_items {
                    let is_sel = selected.is_some_and(|cur| cur.key == entry.key);
                    section = section.child(list_row(s, p, entry, is_sel, multi_srv, multi_proj));
                }
            }
            content = content.child(section);
        }
    }

    let viewport = col().w_full().grow(1.0).shrink().min_h(0.0);
    session_view::scroller::scroller("automations-list-scroll").build::<App>(p, viewport, content)
}

fn list_row(s: &App, p: &Pal, entry: &Entry, selected: bool, multi_srv: bool, multi_proj: bool) -> El {
    let row_data = &entry.row;
    let note = if row_data.status == AutomationStatus::Paused && row_data.last_outcome == Some(AutomationOutcome::Failed) {
        Some(t!("automations.note.failed").to_owned())
    } else if row_data.status == AutomationStatus::Done && row_data.last_outcome == Some(AutomationOutcome::Expired) {
        Some(t!("automations.status.expired").to_owned())
    } else if row_data.status == AutomationStatus::Active {
        row_data.next_due_at.map(|at| relative_time(at, *s.now_ms))
    } else {
        None
    };

    let mut sub_parts = context_parts(entry, multi_srv, multi_proj);
    sub_parts.push(entry.trigger.clone());
    let sub = sub_parts.join(" · ");

    let select_key = entry.key.clone();
    let top = row()
        .w_full()
        .min_w(0.0)
        .gap(12.0)
        .center_y()
        .child(text(row_data.name.clone()).font(ROW_TITLE).color(p.text).grow(1.0).shrink().min_w(0.0).truncate().dir(Direction::Auto))
        .children(note.map(|n| text(n).font(ROW_NOTE).color(p.muted)));

    col()
        .key(format!("auto-row-{}", row_data.id))
        .role("button")
        .label(row_data.name.clone())
        .focus_selected(selected)
        .w_full()
        .px(10.0)
        .py(8.0)
        .gap(2.0)
        .radius(7.0)
        .focus_inset(p)
        .fill(if selected { p.layer2 } else { Color::TRANSPARENT })
        .hover(if selected { Color::TRANSPARENT } else { p.hover })
        .on_press_tx(move |s: &mut App, tx: &mut Tx| {
            tx.instant();
            s.kernel.slots.get_mut(&AUTOMATIONS).selected = Some(select_key.clone());
        })
        .child(top)
        .child(text(sub).font(ROW_SUB).color(p.faint).w_full().truncate())
}

fn detail_pane(s: &App, p: &Pal, selected: Option<&Entry>, multi_srv: bool, multi_proj: bool) -> El {
    let content = match selected {
        Some(entry) => inspector(s, p, entry, multi_srv, multi_proj),
        None => el(),
    };
    let viewport = col().w_full().grow(1.0).shrink().min_h(0.0);
    session_view::scroller::scroller("automations-detail-scroll").build::<App>(p, viewport, content)
}

fn inspector(s: &App, p: &Pal, entry: &Entry, multi_srv: bool, multi_proj: bool) -> El {
    let row_data = &entry.row;
    let busy = !matches!(s.kernel.slots.get(&AUTOMATIONS).mutate, MutatePhase::Idle);
    let k = kit(p);

    let mut sub_parts = vec![status_label(row_data).to_owned()];
    sub_parts.extend(context_parts(entry, multi_srv, multi_proj));

    let mut action_btns = row().key("auto-detail-actions").gap(4.0).center_y();
    if row_data.status != AutomationStatus::Done {
        let pause_label = if row_data.status == AutomationStatus::Paused {
            t!("automations.action.resume")
        } else {
            t!("automations.action.pause")
        };
        let op = if row_data.status == AutomationStatus::Paused { Operation::Resume } else { Operation::Pause };
        let e_clone = entry.clone();
        let toggle_btn = button(pause_label, ButtonVariant::Neutral)
            .control_size(ControlSize::Regular)
            .disabled(busy)
            .on_press_tx(move |s: &mut App, tx: &mut Tx| source::mutate(s, &e_clone, op, tx))
            .build(k)
            .key("auto-pause-resume");
        action_btns = action_btns.child(toggle_btn);
    }
    let del_key = entry.key.clone();
    let del_srv = entry.server.clone();
    let del_id = row_data.id.clone();
    let del_name = row_data.name.clone();
    let del_btn = button(t!("automations.delete.button"), ButtonVariant::GhostMuted)
        .control_size(ControlSize::Regular)
        .disabled(busy)
        .on_press_tx(move |s: &mut App, tx: &mut Tx| {
            tx.instant();
            super::ask_delete(s, del_key.clone(), del_srv.clone(), del_id.clone(), del_name.clone());
        })
        .build(k)
        .key("auto-delete");
    action_btns = action_btns.child(del_btn);

    let hdr = row()
        .key("auto-detail-header")
        .w_full()
        .gap(12.0)
        .items_start()
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(2.0)
                .child(text(row_data.name.clone()).key("auto-detail-name").font(DETAIL_TITLE).color(p.text).wrap().dir(Direction::Auto))
                .child(text(sub_parts.join(" · ")).key("auto-detail-status").font(DETAIL_SUB).color(p.muted)),
        )
        .child(action_btns);

    let when_val = when_block(s, p, entry);
    let do_val = do_block(p, entry);
    let last_run_val = last_run_block(s, p, entry);

    let primary = col()
        .key("auto-detail-primary")
        .w_full()
        .gap(20.0)
        .child(dl_row(p, t!("automations.when"), when_val))
        .child(dl_row(p, t!("automations.do"), do_val))
        .child(dl_row(p, t!("automations.lastRun"), last_run_val));

    let mut meta = col()
        .key("auto-detail-meta")
        .w_full()
        .gap(8.0)
        .child(el().w_full().h(1.0).mb(12.0).fill(p.border).a11y_hidden())
        .child(dl_row(
            p,
            t!("automations.folder"),
            text(row_data.directory.clone()).key("auto-detail-folder").font(CODE_TEXT).color(p.muted).wrap_anywhere(),
        ));
    if let Some(sid) = &row_data.session_id {
        let label = entry.creator.clone().unwrap_or_else(|| sid.clone());
        meta = meta.child(dl_row(
            p,
            t!("automations.creator"),
            session_link(p, "auto-creator-link", entry.server.clone(), sid.clone(), label),
        ));
    }
    meta = meta.child(dl_row(
        p,
        t!("automations.runsOn"),
        text(entry.server_label.clone()).key("auto-detail-server").font(META_TEXT).color(p.muted).wrap(),
    ));

    col()
        .w_full()
        .items_center()
        .child(
            col()
                .key("auto-detail-article")
                .role("article")
                .w_full()
                .max_w(720.0)
                .px(28.0)
                .py(24.0)
                .gap(28.0)
                .child(hdr)
                .child(primary)
                .child(meta),
        )
}

fn when_block(s: &App, p: &Pal, entry: &Entry) -> El {
    let row_data = &entry.row;
    let mut block = col()
        .w_full()
        .gap(6.0)
        .child(text(entry.trigger.clone()).key("auto-when-trigger").font(DETAIL_SUB).color(p.text).wrap());

    match &row_data.trigger {
        AutomationTrigger::Event { args, .. } => {
            if let Some(map) = args {
                let nested = map.values().any(|v| v.is_object() || v.is_array());
                if nested {
                    let pretty = serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_default();
                    block = block.child(content_box(p, "auto-when-args", pretty, true));
                } else {
                    let inline = map
                        .iter()
                        .map(|(k, v)| format!("{k} = {v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    block = block.child(text(inline).key("auto-when-args-inline").font(CODE_TEXT).color(p.muted).wrap_anywhere());
                }
            }
        }
        _ => {
            let mut line = row_data.tz.clone();
            let mut tip: Option<String> = None;
            if row_data.status == AutomationStatus::Active
                && let Some(next) = row_data.next_due_at
            {
                let rel = relative_time(next, *s.now_ms);
                line = format!("{line} · {}", t!("automations.nextRun", time = rel));
                tip = Some(format_date(next, &row_data.tz));
            }
            let mut sub = text(line).key("auto-when-tz").font(DETAIL_SUB).color(p.muted).wrap();
            if let Some(title) = tip {
                sub = sub.help(title);
            }
            block = block.child(sub);
        }
    }
    block
}

fn do_block(p: &Pal, entry: &Entry) -> El {
    let mut block = col().w_full().gap(6.0);
    match &entry.row.action {
        AutomationAction::Prompt { prompt, to } => {
            let summary = match to {
                AutomationTarget::Session(sid) => {
                    let name = entry.recipient.clone().unwrap_or_else(|| sid.clone());
                    let tpl = t!("automations.do.prompt", session = "{{SESSION}}");
                    let (pre, post) = tpl.split_once("{{SESSION}}").unwrap_or((&tpl, ""));
                    let mut r = row().w_full().wrap().gap(0.0).center_y();
                    if !pre.is_empty() {
                        r = r.child(text(pre.to_owned()).font(DETAIL_SUB).color(p.text));
                    }
                    r = r.child(session_link(p, "auto-recipient-link", entry.server.clone(), sid.clone(), name));
                    if !post.is_empty() {
                        r = r.child(text(post.to_owned()).font(DETAIL_SUB).color(p.text));
                    }
                    r
                }
                AutomationTarget::Subagent(agent) => {
                    text(t!("automations.do.subagent", agent = agent)).font(DETAIL_SUB).color(p.text).wrap()
                }
            };
            block = block
                .child(summary)
                .child(content_box(p, "auto-do-body", prompt.clone(), false));
        }
        AutomationAction::Code { code, args } => {
            block = block
                .child(text(t!("automations.do.code")).font(DETAIL_SUB).color(p.text))
                .child(text(t!("automations.do.codeHint")).font(DETAIL_SUB).color(p.muted).wrap())
                .child(content_box(p, "auto-do-body", code.clone(), true));
            if let Some(a) = args {
                let pretty = serde_json::to_string_pretty(a).unwrap_or_else(|_| a.to_string());
                block = block.child(content_box(p, "auto-do-args", pretty, true));
            }
        }
        AutomationAction::Shell { command } => {
            block = block
                .child(text(t!("automations.do.shell")).font(DETAIL_SUB).color(p.text))
                .child(text(t!("automations.do.shellHint")).font(DETAIL_SUB).color(p.muted).wrap())
                .child(content_box(p, "auto-do-body", command.clone(), true));
        }
    }
    block
}

fn last_run_block(s: &App, p: &Pal, entry: &Entry) -> El {
    let row_data = &entry.row;
    let mut block = col().w_full().gap(6.0);
    let summary = match row_data.last_run_at {
        Some(at) => {
            let rel = relative_time(at, *s.now_ms);
            let runs = plural!("automations.runCount", row_data.run_count);
            row()
                .gap(4.0)
                .center_y()
                .child(text(rel).font(DETAIL_SUB).color(p.text).help(format_date(at, &row_data.tz)))
                .child(text(format!("· {runs}")).font(DETAIL_SUB).color(p.muted))
        }
        None => text(t!("automations.neverRun")).font(DETAIL_SUB).color(p.text),
    };
    block = block.child(summary);
    if let Some(out) = &row_data.last_output {
        block = block.child(content_box(p, "auto-last-output", out.clone(), true));
    }
    block
}

fn dl_row(p: &Pal, label: &str, value: El) -> El {
    row()
        .w_full()
        .gap(16.0)
        .items_start()
        .child(text(label).w(88.0).font(DETAIL_SUB).color(p.faint))
        .child(col().grow(1.0).shrink().min_w(0.0).child(value))
}

fn session_link(p: &Pal, key: &str, server: opencode_kit::servers::ServerKey, sid: String, label: String) -> El {
    row()
        .key(key)
        .role("button")
        .label(label.clone())
        .radius(4.0)
        .focus_inset(p)
        .hover(p.hover)
        .on_press_tx(move |s: &mut App, tx: &mut Tx| {
            super::open_session(s, &server, &sid, tx);
        })
        .child(col().child(text(label).font(ROW_TITLE).color(p.text)).child(el().w_full().h(1.0).fill(p.q.border_strong).a11y_hidden()))
}

fn content_box(p: &Pal, key: &str, body: String, code: bool) -> El {
    let style = if code { CODE_TEXT } else { DETAIL_SUB };
    let t = text(body)
        .font(style)
        .color(p.text)
        .wrap_anywhere()
        .selectable(true)
        .dir(if code { Direction::Ltr } else { Direction::Auto });
    col()
        .key(key)
        .w_full()
        .mt(4.0)
        .px(12.0)
        .py(10.0)
        .max_h(224.0)
        .radius(6.0)
        .fill(p.layer1)
        .child(t)
}

fn context_parts(entry: &Entry, multi_srv: bool, multi_proj: bool) -> Vec<String> {
    let mut parts = Vec::new();
    if multi_srv {
        parts.push(entry.server_label.clone());
    }
    if multi_proj {
        parts.push(entry.project_name.clone());
    }
    parts
}

