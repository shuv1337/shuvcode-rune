//! What `ui.browserSurface` draws: a page's frames, and until the engine runs, the engine's
//! state in their place (its first-use download, the download's progress, why it failed, or
//! that this build has none).

use browser_pane::{Distrust, ENGINE_DOWNLOAD_MB, EngineStatus, Fault, Net};
use rune::{Anim, El, Spring, Tx, col, el, icon, row, text};
use rune_theme_opencode::icons as ic;
use session_view::palette::Elevate;

use crate::App;
use crate::odometer::odometer_end;
use crate::palette::Pal;
use crate::view::{UI_MED, UI_SMALL};

const INSTANT: Anim = Anim::INSTANT;

/// Neutral button opening `tab_id`'s requested web URL in the OS default browser when one exists
/// and passes `external_url_allowed_for_tab`.
fn open_external_btn(s: &App, p: &Pal, tab_id: u64) -> Option<El> {
    s.browser_tab_external_url(tab_id)?;
    let label = opencode_i18n::t!("plugin.browser.external.open");
    Some(
        el().key("browser-open-external-btn")
            .role("button")
            .label_static(label)
            .h(30.0)
            .px(14.0)
            .radius(6.0)
            .center()
            .fill(p.q.neutral)
            .box_shadow(Pal::ring_out(p.q.neutral_ring))
            .hover(p.hover)
            .hover_motion(INSTANT, INSTANT)
            .on_press_tx(move |s: &mut App, tx: &mut Tx| s.open_browser_tab_externally(tab_id, tx))
            .child(text(label).font(UI_MED).color(p.text)),
    )
}

/// The address "Open in system browser" opens, in text: the views without an engine hide the
/// address bar, and the button must not open something the person can't see.
fn requested_url(s: &App, p: &Pal, tab_id: u64) -> Option<El> {
    let url = s.browser_tab_external_url(tab_id)?;
    Some(text(url).key("browser-requested-url").font(UI_SMALL).color(p.text).wrap().align(rune::Align::Center).max_w(360.0))
}

/// Page `tab_id`'s surface, `scale` device pixels a point, or the engine's state while it can't
/// show pages; `None` when the engine is ready and the page isn't open.
pub fn surface(s: &App, p: &Pal, tab_id: u64, scale: f32) -> Option<El> {
    let ext_btn = open_external_btn(s, p, tab_id);
    Some(match s.workspace_panes.browser.host.status() {
        EngineStatus::Unsupported => unsupported(p, requested_url(s, p, tab_id), ext_btn),
        // The background download, when the pane opens on it, shows here and goes on.
        EngineStatus::Downloading { downloaded_bytes, total_bytes } => {
            let frac = total_bytes.filter(|&t| t > 0).map_or(0.25, |t| (downloaded_bytes as f32 / t as f32).clamp(0.03, 1.0));
            progress(p, opencode_i18n::t!("rune.browser.engine.downloading"), Some((downloaded_bytes, total_bytes)), frac, ext_btn)
        }
        EngineStatus::Verifying => progress(p, opencode_i18n::t!("rune.browser.engine.verifying"), None, 1.0, ext_btn),
        EngineStatus::Paused(reason) => progress(p, reason.describe(), None, 0.25, ext_btn),
        EngineStatus::NotInstalled => {
            let dl_btn = el()
                .key("browser-download-btn")
                .role("button")
                .label_static(opencode_i18n::t!("rune.browser.engine.download"))
                .h(30.0)
                .px(14.0)
                .radius(6.0)
                .center()
                .fill(p.q.submit)
                .box_shadow(Pal::ring_out(p.q.submit_ring))
                .hover_motion(INSTANT, INSTANT)
                .on_press(|s: &mut App| s.start_browser_install())
                .child(text(opencode_i18n::t!("plugin.file.view.download")).font(UI_MED).color(p.q.submit_text));
            col()
                .key("browser-download-prompt")
                .grow(1.0)
                .w_full()
                .center()
                .padding(28.0)
                .gap(14.0)
                .child(icon(&ic::GLOBE, 22.0).color(p.icon_muted))
                .child(text(opencode_i18n::t!("rune.browser.engine.needsDownload", size = ENGINE_DOWNLOAD_MB)).font(UI_MED).color(p.text).truncate_fade())
                .child(row().gap(8.0).center_y().child(dl_btn).children(ext_btn))
        }
        EngineStatus::Failed(failure) => {
            let retry_btn = failure.fault.retryable().then(|| {
                el().key("browser-retry-btn")
                    .role("button")
                    .label_static(opencode_i18n::t!("rune.browser.engine.retryDownload"))
                    .h(30.0)
                    .px(14.0)
                    .radius(6.0)
                    .center()
                    .fill(p.q.neutral)
                    .box_shadow(Pal::ring_out(p.q.neutral_ring))
                    .hover(p.hover)
                    .hover_motion(INSTANT, INSTANT)
                    .on_press(|s: &mut App| s.start_browser_install())
                    .child(text(opencode_i18n::t!("rune.home.noServer.retryShort")).font(UI_MED).color(p.text))
            });
            let actions = (retry_btn.is_some() || ext_btn.is_some())
                .then(|| row().gap(8.0).center_y().children(retry_btn).children(ext_btn));
            let url = (failure.fault == Fault::NotPublished).then(|| requested_url(s, p, tab_id)).flatten();
            col()
                .key("browser-download-failed")
                .grow(1.0)
                .w_full()
                .center()
                .padding(28.0)
                .gap(14.0)
                .child(icon(&ic::WARNING, 20.0).color(p.icon_muted))
                .child(text(fault_text(&failure.fault)).font(UI_SMALL).color(p.muted).wrap().align(rune::Align::Center).max_w(320.0))
                .children(url)
                .children(actions)
        }
        EngineStatus::Unchecked | EngineStatus::Installed | EngineStatus::Connecting | EngineStatus::Ready => {
            let tab = s.workspace_panes.browser.host.tab(tab_id)?;
            super::poll::note_surface_drawn(tab.id);
            let bars = super::prompts::view_bars(s, p, tab.id);
            let modal = super::prompts::view_modal(s, p, tab.id);
            let ext_popup = s
                .workspace_panes
                .browser
                .host
                .popup_surface_el(
                    "browser-ext-popup",
                    scale / *s.zoom,
                    *s.zoom,
                    |app: &mut App| &mut app.workspace_panes.browser.host,
                )
                .map(|surface| {
                    let card = el()
                        .key("browser-ext-popup-card")
                        .pin(Some(4.0), Some(8.0), None, None)
                        .radius(8.0)
                        .fill(p.bg_base)
                        .border(1.0, p.border)
                        .elevate(p.overlay)
                        .clip_rounded(8.0)
                        .on_press(|_: &mut App| {})
                        .child(surface);
                    el().key("browser-ext-popup-backdrop")
                        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                        .on_press(|app: &mut App| {
                            app.workspace_panes.browser.host.close_extension_popup();
                        })
                        .child(card)
                });
            let comment_overlay = super::comment::view(s, p, tab.id);
            // `embeds.View`: the page on the pane's base background, with top prompt/popup bars
            // above the page and any comment/note overlay, page modal, or extension popup overlaid.
            let page_stack = col()
                .key("browser-page-stack")
                .grow(1.0)
                .min_h(0.0)
                .w_full()
                .on_layout_tx(|app: &mut App, [_, _, w, h], tx| {
                    app.workspace_panes.browser.overlay.set_pane_size(w, h);
                    app.workspace_panes.browser.host.set_pane_size(w, h);
                    if matches!(app.workspace_panes.browser.overlay.state, super::comment::Overlay::Idle)
                        && app.workspace_panes.browser.host.popup_tab_id().is_none()
                    {
                        tx.live_only();
                    }
                })
                .child(s.workspace_panes.browser.host.surface_el("browser-surface", tab.id, scale / *s.zoom, *s.zoom, |app: &mut App| &mut app.workspace_panes.browser.host, App::focus_browser_page))
                .children(ext_popup)
                .children(comment_overlay)
                .children(modal);
            col()
                .key("browser-viewport")
                .grow(1.0)
                .min_h(0.0)
                .w_full()
                .fill(p.bg_base)
                .children(bars)
                .child(page_stack)
        }
    })
}

/// What failed, why, and what to do, in the app's language.
pub fn fault_text(fault: &Fault) -> String {
    use opencode_i18n::t;
    let mb = |b: &u64| b.div_ceil(1_000_000);
    match fault {
        Fault::NotPublished => t!("rune.browser.engine.fault.notPublished").to_owned(),
        Fault::Unreachable { host, why: Net::NotFound } => t!("rune.browser.engine.fault.offline", host = host),
        Fault::Unreachable { host, why: Net::Secure } => t!("rune.browser.engine.fault.secure", host = host),
        Fault::Unreachable { host, why: Net::Proxy } => t!("rune.browser.engine.fault.proxy", host = host),
        Fault::Unreachable { host, why: Net::Refused | Net::TimedOut | Net::Other } => t!("rune.browser.engine.fault.connect", host = host),
        Fault::Blocked { host, status } => t!("rune.browser.engine.fault.blocked", host = host, status = status),
        Fault::Server { host, status } => t!("rune.browser.engine.fault.server", host = host, status = status),
        Fault::Unexpected { host } => t!("rune.browser.engine.fault.unexpected", host = host),
        Fault::Interrupted { have, total } => t!("rune.browser.engine.fault.interrupted", have = mb(have), total = mb(total)),
        Fault::Corrupt => t!("rune.browser.engine.fault.corrupt").to_owned(),
        Fault::Untrusted(Distrust::Signer { file, signer }) => t!("rune.browser.engine.fault.signer", file = file, signer = signer),
        Fault::Untrusted(Distrust::Unsigned { file }) => t!("rune.browser.engine.fault.unsigned", file = file),
        Fault::Untrusted(Distrust::Rule) => t!("rune.browser.engine.fault.untrusted").to_owned(),
        Fault::NoSpace { need, free } => t!("rune.browser.engine.fault.noSpace", need = mb(need), free = mb(free)),
        Fault::DiskFull => t!("rune.browser.engine.fault.diskFull").to_owned(),
        Fault::NoMemory => t!("rune.browser.engine.fault.noMemory").to_owned(),
        Fault::Write(error) => t!("rune.browser.engine.fault.write", error = error),
        Fault::Dirs(error) => t!("rune.browser.engine.fault.dirs", error = error),
        Fault::Busy => t!("rune.browser.engine.fault.busy").to_owned(),
        Fault::Launch(error) => t!("rune.browser.engine.fault.launch", error = error),
        Fault::Prevented(error) => t!("rune.browser.engine.fault.prevented", error = error),
        Fault::Damaged(code) => t!("rune.browser.engine.fault.damaged", code = format!("{code:#x}")),
        Fault::Exited(Some(code)) => t!("rune.browser.engine.fault.exited", code = format!("{code:#x}")),
        Fault::Exited(None) | Fault::Stopped => t!("rune.browser.engine.fault.stopped").to_owned(),
        Fault::Hung => t!("rune.browser.engine.fault.hung").to_owned(),
        Fault::OutOfMemory => t!("rune.browser.engine.fault.outOfMemory").to_owned(),
    }
}

/// The engine download's progress: a label, the megabytes so far (rolling) when known, and a bar
/// filled to `frac`.
fn progress(p: &Pal, label: &str, bytes: Option<(u64, Option<u64>)>, frac: f32, ext_btn: Option<El>) -> El {
    let bar_w = 240.0;
    let odometer = bytes.map(|(downloaded_bytes, total_bytes)| {
        let done_mb = (downloaded_bytes as f64 / 1_000_000.0).floor();
        let tot_mb = total_bytes.filter(|&t| t > 0).map(|t| (t as f64 / 1_000_000.0).round().max(1.0));
        let odo_label = match tot_mb {
            Some(tot) => format!("{done_mb:.0} / {tot:.0} MB"),
            None => format!("{done_mb:.0} MB"),
        };
        odometer_end("browser-dl-mb", odo_label, done_mb, UI_MED, p.text)
    });
    col()
        .key("browser-installing")
        .grow(1.0)
        .w_full()
        .center()
        .padding(28.0)
        .gap(10.0)
        .child(row().w(bar_w).center_y().child(text(label.to_owned()).font(UI_SMALL).color(p.muted).grow(1.0).shrink().min_w(0.0).truncate_fade()).children(odometer))
        .child(
            el().key("browser-dl-track")
                .w(bar_w)
                .h(5.0)
                .radius(2.5)
                .clip_rounded(2.5)
                .fill(p.layer3)
                .child(el().key("browser-dl-fill").w(bar_w * frac).h(5.0).radius(2.5).fill(p.icon).motion(Spring::new(0.25, 0.0))),
        )
        .children(ext_btn.map(|b| b.mt(4.0)))
}

/// This build has no browser engine for its platform: no address bar, nothing to download.
fn unsupported(p: &Pal, url: Option<El>, ext_btn: Option<El>) -> El {
    col()
        .key("browser-unsupported")
        .grow(1.0)
        .w_full()
        .center()
        .padding(28.0)
        .gap(8.0)
        .child(icon(&ic::GLOBE, 22.0).color(p.icon_muted))
        .child(text(opencode_i18n::t!("rune.browser.unsupported.title")).font(UI_MED).color(p.text).truncate_fade())
        .child(text(opencode_i18n::t!("rune.browser.unsupported.description")).font(UI_SMALL).color(p.muted).wrap().align(rune::Align::Center).max_w(320.0))
        .children(url.map(|u| u.mt(6.0)))
        .children(ext_btn.map(|b| b.mt(6.0)))
}
