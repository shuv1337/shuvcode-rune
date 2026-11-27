//! Shell kernel assembly: wires extracted feature crates into `opencode_kernel::Kernel<App>`.
//!
//! New Rust features register here via `Feature::register` instead of adding fields to `App` or
//! editing central dispatch chains in `lib.rs`, `tick.rs`, `shell/input/keys.rs`, or `shell/view/mod.rs`.

use opencode_kernel::{
    AppEnv, AppFeedback, AppOverlays, EscapeLayer, Kernel, KernelBuilder, OverlayEntry,
    OverlayEvent, OverlayKind, OverlayRegistration,
};

use crate::{App, Route};

impl AppEnv for App {
    fn clock(&self) -> f64 {
        *self.clock
    }

    fn now_ms(&self) -> i64 {
        *self.now_ms
    }

    fn zoom(&self) -> f32 {
        *self.zoom
    }

    fn spellcheck(&self) -> bool {
        self.settings.spellcheck
    }

    fn reduced_motion(&self) -> bool {
        *self.appearance_state.reduced_motion
    }

    fn is_live(&self) -> bool {
        self.live()
    }

    fn persists(&self) -> bool {
        App::persists(self)
    }

    fn is_headless(&self) -> bool {
        crate::headless()
    }

    fn waker(&self) -> Option<rune::Waker> {
        (*self.waker).clone()
    }

    fn active_session_id(&self) -> Option<&str> {
        match &*self.route {
            Route::Session { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    fn panel_root<'a>(&'a self, sid: &'a str) -> &'a str {
        App::panel_root(self, sid)
    }
}

impl AppFeedback for App {
    fn toast_say(&mut self, title: String, description: String) {
        crate::status_toast::say(self, title, description);
    }

    fn toast_succeed(&mut self, title: String, description: String) {
        crate::status_toast::succeed(self, title, description);
    }

    fn toast_fail(&mut self, title: String, description: String) {
        crate::status_toast::fail(self, title, description);
    }
}

impl AppOverlays for App {
    fn on_overlay_opened(&mut self, id: &'static str, return_focus: Option<String>) {
        let mut entry = OverlayEntry::new(id, OverlayKind::Dialog);
        if let Some(focus) = return_focus {
            entry = entry.with_return_focus(focus);
        }
        let _ = self.kernel.step_overlay(OverlayEvent::Push(entry));
    }

    fn on_overlay_closed(&mut self, id: &'static str) {
        let _ = self.kernel.step_overlay(OverlayEvent::Close(id));
    }
}

/// Builds a window's kernel with all decoupled features and core surfaces registered.
pub fn build() -> Kernel<App> {
    let mut b = KernelBuilder::new();
    b.add_stages(crate::tick::stage::ALL);
    b.add_overlay_layers(crate::overlays::layer::ALL);
    b.add_overlay_key_bands(crate::overlays::key_band::ALL);
    b.add_escape_bands(crate::overlays::escape_band::ALL);
    register_core_systems(&mut b);
    b.with_feature(&crate::surfaces::automations::AutomationsFeature);
    b.with_feature(&crate::surfaces::egg::EggFeature);
    #[cfg(feature = "office")]
    b.with_feature(&crate::surfaces::office::OfficeFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::surfaces::updater::UpdaterFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::surfaces::browser::BrowserFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::surfaces::browser::BrowserRpcFeature);
    b.with_feature(&crate::line_comments::AnnotationsFeature);
    b.with_feature(&crate::attachments::ComposerInputFeature);
    b.with_feature(&crate::connect::ConnectFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::dictation::DictationFeature);
    b.with_feature(&crate::files::FilesFeature);
    b.with_feature(&crate::find::FindFeature);
    b.with_feature(&crate::free_limit::FreeLimitFeature);
    b.with_feature(&crate::location_missing::LocationMissingFeature);
    b.with_feature(&crate::message_nav::HistoryFeature);
    b.with_feature(&crate::queue::InboxFeature);
    b.with_feature(&crate::mcp::McpFeature);
    b.with_feature(&crate::mentions::MentionsFeature);
    b.with_feature(&crate::narrow::NarrowFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::notifications::NotificationsFeature);
    #[cfg(not(target_arch = "wasm32"))]
    b.with_feature(&crate::open_in::OpenInFeature);
    b.with_feature(&crate::panels::PanelsFeature);
    b.with_feature(&crate::projects::ProjectsFeature);
    b.with_feature(&crate::restore::RestoreFeature);
    b.with_feature(&crate::review::ReviewFeature);
    b.with_feature(&crate::session_menu::SessionMenuFeature);
    b.with_feature(&crate::settings::SettingsFeature);
    b.with_feature(&crate::subagents::SubagentsFeature);
    b.with_feature(&crate::switcher::SwitcherFeature);
    b.with_feature(&crate::surfaces::terminal::TerminalSurfaceFeature);
    b.with_feature(&crate::todos::TimelineExtrasFeature);
    b.with_feature(&crate::tips::TipsFeature);
    b.with_feature(&crate::new_worktree::WorkspaceToolsFeature);
    b.with_feature(&crate::tabs::groups::TabGroupsFeature);
    b.with_feature(&crate::tabs::sections::SidebarSectionsFeature);
    b.with_feature(&crate::plugin_host::PluginHostFeature);
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.pickers")
            .with_render(crate::overlays::layer::SURFACE_POPOVER, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                session_view::pickers::view::layer(s, p)
            })
            .above("narrow.sheet"),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.diagram")
            .with_render(crate::overlays::layer::MEDIA_LIGHTBOX, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::diagram::view(s, p, cx)
            })
            .above("overlay.attachments.viewer")
            .with_key(crate::diagram::key),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.tool_details")
            .with_render(crate::overlays::layer::MEDIA_LIGHTBOX, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                session_view::tools::details_overlay(s, p, cx)
            })
            .above("overlay.diagram"),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.tab_preview")
            .with_render(crate::overlays::layer::CHROME_POPUP, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::view::tab_preview::view(s, p).into_iter().collect()
            }),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.popup")
            .with_render(crate::overlays::layer::CHROME_POPUP, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::popup::view(s, p)
            })
            .above("overlay.tab_preview")
            // Visually below `plugin.popover` (PLUGIN_TOP), runs first in `key_band::POPUP` so an
            // open context/dropdown menu takes Escape and navigation keys before every other overlay.
            .with_key(crate::popup::key),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.status_toast")
            .with_render(crate::overlays::layer::TOAST, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::status_toast::view(s, p).into_iter().collect()
            }),
    );
    // `KeySequenceHint` (`key-sequence.tsx`): the chord in progress, at the window's foot.
    b.register_overlay(
        OverlayRegistration::<App>::new("overlay.key_sequence")
            .with_render(crate::overlays::layer::TOAST, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::key_sequence::view(s, p).into_iter().collect()
            })
            .above("overlay.status_toast"),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new(crate::view::move_recovery::OVERLAY_ID)
            .with_render(crate::overlays::layer::ELEVATED_DIALOG, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::view::move_recovery::view(s, p, cx)
            })
            .with_key(crate::view::move_recovery::key)
            // Visually above `connect`/`mcp` in ELEVATED_DIALOG, kept after `mcp` in MODAL keys
            // to preserve existing key order (mutually exclusive in practice; swallows all keys while open).
            .after_key(crate::mcp::OVERLAY_ID)
            .with_dismiss(crate::view::move_recovery::keep),
    );
    b.register_overlay(
        OverlayRegistration::<App>::new(crate::servers::dialog::OVERLAY_ID)
            .with_render(crate::overlays::layer::ELEVATED_DIALOG, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::servers::dialog::view(s, p, cx)
            })
            .above("plugin.dialog")
            .with_dismiss(crate::servers::dialog::close),
    );
    b.register_escape_layer(
        EscapeLayer::<App>::new(
            "escape.tool_step",
            crate::overlays::escape_band::FALLBACK,
            |s| s.tool_view_open(),
            |s, _, tx| {
                s.dismiss_tool_step(tx);
            },
        )
        .after("escape.toast"),
    );
    b.register_escape_layer(EscapeLayer::new(
        "escape.toast",
        crate::overlays::escape_band::FALLBACK,
        |s| s.toast.current().is_some_and(|t| t.persistent),
        |s, ev, tx| {
            crate::status_toast::key(s, ev, tx);
        },
    ));
    b.register_escape_layer(EscapeLayer::new(
        "escape.opening",
        crate::overlays::escape_band::SESSION_ACTION,
        |s| s.opening.is_some(),
        |s, _, tx| {
            s.cancel_open(tx);
        },
    ));
    b.register_escape_layer(EscapeLayer::new(
        "escape.peek",
        crate::overlays::escape_band::CHROME,
        |s| !*s.sidebar_open && s.layout.peek.shown(),
        |s, _, tx| {
            s.queue_peek(crate::layout_state::PeekEvent::Escape, tx);
        },
    ));
    crate::commands::register_shell_commands(&mut b);
    b.register_slot(&crate::app::VIEW_RUNS)
        .register_slot(&crate::app::OUTPUT_ERRORS)
        .register_slot(&crate::app::CODE_WRAP)
        .register_slot(&crate::app::MATH)
        .register_slot(&crate::session_catalog::CATALOG)
        .register_slot(&crate::session::meta::flags::FLAGS)
        .register_slot(&crate::shell::view::sidebar::recent::HOVERED_HEADER)
        .register_slot(&crate::events::TAIL_CHANGE)
        .register_slot(&crate::events::STREAM_GAPS)
        .register_slot(&crate::speech::SPEECH)
        .register_slot(&crate::crash_notice::CRASH_NOTICE)
        .register_slot(&crate::key_sequence::SEQUENCE)
        .register_slot(&crate::servers::dialog::DIALOG)
        .register_slot(&crate::shell::memory_reclaim::SWEEP);
    let kernel = b.build();
    crate::shortcuts::validate(&kernel.commands);
    kernel
}

/// Core shell and subsystem lifecycle polls registered into declared [`crate::tick::stage`]
/// barriers with explicit `after` dependencies so execution order never depends on feature
/// registration order.
fn register_core_systems(b: &mut KernelBuilder<App>) {
    use crate::tick::stage;

    b.register_slot(&crate::tab_hints::TAB_HINTS);
    b.register_slot(&crate::tabs::pins::PINS);
    b.register_slot(&crate::tabs::auto_groups::AUTO_GROUPS);
    b.register_slot(&crate::session_viewed::VIEWING);
    b.register_slot(&crate::tabs::navigation_switch::NAV_SWITCH);
    b.register_slot(&crate::view::sidebar::recent_view::RECENT_VIEW);
    b.register_slot(&crate::view::home::tip::PROVIDER_TIP);
    b.add_visibility("status_toast.visibility", |s, shown, tx| {
        crate::status_toast::visibility(s, shown, tx);
    });
    b.add_visibility("tab_hints.visibility", |s, shown, tx| {
        if !shown {
            crate::tab_hints::window_left(s, tx);
        }
    });

    // ── Phase 1: Ingest ──────────────────────────────────────────────────────────────────────────
    // Another window changed the session data this one shows.
    b.schedule(stage::INGEST_HYDRATE, "windows.peer.behind", |s, _, _| {
        crate::windows::peer::behind(s)
    });
    b.schedule(stage::INGEST_HYDRATE, "reconnect.audit", |s, _, _| {
        crate::reconnect::audit(s);
        false
    });
    b.schedule(stage::INGEST_HYDRATE, "popup.poll", |s, _, tx| {
        crate::popup::poll(s, tx);
        false
    });

    // The servers the window isn't checked out on: their launches, replies and streams.
    b.schedule(stage::INGEST_SERVERS, "servers.runtime", |s, _, tx| {
        crate::servers::runtime::poll(s, tx)
    });
    // Known: `servers.runtime::sync` creates controllers before `servers.health` calls `runtime::reported`.
    b.schedule(stage::INGEST_SERVERS, "servers.health", |s, _, tx| {
        crate::servers::health::poll(s, tx)
    })
    .after("servers.runtime");
    // Precaution: `launch.poll` mutates `source`, `tabs`, `route`, `sessions`, `projects`, and `transcripts`.
    b.schedule(stage::INGEST_SERVERS, "launch.poll", |s, _, tx| {
        crate::launch::poll(s, tx)
    })
    .after(if cfg!(target_arch = "wasm32") { "project_switcher.poll" } else { "dictation.poll" })
    .per_server(|s, _, tx| {
        crate::launch::poll(s, tx);
        false
    });
    // Known: `launch.trace` observes `rune::marks` emitted by `launch.poll`.
    b.schedule(stage::INGEST_SERVERS, "launch.trace", |_, _, tx| {
        crate::launch::trace::poll(tx);
        false
    })
    .after("launch.poll");

    // A project's image the icon worker found (or lost): the tiles that read the icons draw again.
    b.schedule(stage::INGEST_LOCAL, "appearance.icons", |s, _, _| {
        if rune::Tracked::untracked(&s.appearance_state.icons).take_found() {
            rune::Tracked::touch(&s.appearance_state.icons);
            true
        } else {
            false
        }
    });
    b.schedule(stage::INGEST_LOCAL, "glitch.poll", |s, _, tx| {
        crate::glitch::poll(s, tx)
    });
    // Precaution: `egg.poll` reads `reduced_motion`, which `appearance.poll` updates.
    b.schedule(stage::INGEST_LOCAL, "appearance.poll", |s, _, _| {
        crate::appearance::poll(s)
    })
    .after("egg.poll");
    b.schedule(stage::INGEST_LOCAL, "text_rendering.poll", |s, _, tx| {
        crate::text_rendering::poll(s, tx);
        false
    });
    // Precaution: `updater.poll` reads `I18N` via `t!("common.requestFailed")` before locale changes.
    let language_poll = b.schedule(stage::INGEST_LOCAL, "language.poll", |s, _, tx| {
        crate::language::poll(s, tx)
    });
    #[cfg(not(target_arch = "wasm32"))]
    language_poll.after("updater.poll");
    #[cfg(target_arch = "wasm32")]
    let _ = language_poll;

    // Precaution: `notifications.poll` can change `route` or answer `pending`/`requests` read by `speech.poll`.
    let speech_poll = b.schedule(stage::INGEST_DISPATCH, "speech.poll", |s, _, tx| {
        crate::speech::poll(s, tx)
    });
    #[cfg(not(target_arch = "wasm32"))]
    speech_poll.after("notifications.poll");
    #[cfg(target_arch = "wasm32")]
    let _ = speech_poll;
    // Known: requires `launch.poll` (`INGEST_SERVERS`) for `Source::Live { writes: Some(_) }`;
    // precaution: mutates `settings_open`, `route`, `tabs`, `projects`, and `composer.focused`.
    b.register_overlay(
        OverlayRegistration::<App>::new(crate::deep_links::DEEP_LINK_OVERLAY_ID)
            .with_render(crate::overlays::layer::ACTION_DIALOG, |s, cx| {
                let p = crate::palette::pal(s.dark(cx));
                crate::deep_links::dialog(s, p, cx)
            })
            .above(crate::session_menu::RENAME_OVERLAY_ID)
            .with_key(|s, ev, tx| crate::deep_links::key(s, ev, tx))
            .with_dismiss(|s, tx| crate::deep_links::decline_current(s, tx)),
    );
    b.schedule(stage::INGEST_DISPATCH, "deep_links.drain", |s, _, tx| {
        crate::deep_links::drain(s, tx)
    })
    .after("panels.sync_composer");

    b.schedule(stage::INGEST_SESSION_PRE, "turn_errors.poll", |s, _, tx| {
        crate::turn_errors::poll(s, tx)
    });
    b.schedule(stage::INGEST_SESSION_PRE, "flags.poll", |s, _, _| {
        crate::session::meta::flags::poll(s)
    });

    b.schedule(stage::INGEST_PAGES, "session_pages.poll", |s, _, tx| {
        s.poll_pages(tx)
    })
    .per_server(|s, _, tx| {
        s.poll_pages(tx);
        false
    });
    // Precaution: both `session_pages.poll` and `app.output_errors` push toasts via `App::toast`.
    b.schedule(stage::INGEST_PAGES, "app.output_errors", |s, _, _| {
        let mut changed = false;
        while let Some(why) = s
            .kernel
            .slots
            .untracked(&crate::app::OUTPUT_ERRORS)
            .0
            .as_ref()
            .and_then(|(_, rx)| rx.try_recv().ok())
        {
            crate::status_toast::fail(s, opencode_i18n::t!("common.requestFailed"), why);
            changed = true;
        }
        changed
    })
    .after("session_pages.poll");
    // Known: `session_pages.poll` updates `pages.first_read_done` and calls `context_window_landed`.
    b.schedule(stage::INGEST_PAGES, "context_window.poll", |s, _, _| {
        s.poll_context_window()
    })
    .after("session_pages.poll");
    // Precaution: `jobs.poll` executes arbitrary `Apply` closures that can touch any `App` state.
    b.schedule(stage::INGEST_PAGES, "jobs.poll", |s, _, tx| s.poll_jobs(tx))
        .after_all(if cfg!(target_arch = "wasm32") {
            &["context_window.poll", "inbox.outbox"]
        } else {
            &["context_window.poll", "inbox.outbox", "open_in.poll"]
        })
        .per_server(|s, _, tx| {
            s.poll_server_jobs(tx);
            false
        });

    b.schedule(stage::INGEST_STREAM, "tick.stream", crate::tick::poll_stream).per_server(
        |s, now, tx| {
            crate::incoming::poll(s, tx);
            crate::stop::poll(s, now, tx);
            false
        },
    );
    // Known: `tick.submit` commits staged reverts and mutates `composer`/`inboxes`/`transcripts`/`route`.
    b.schedule(stage::INGEST_STREAM, "tick.submit", |s, _, tx| {
        crate::tick::poll_submit(s, tx)
    })
    .after("history.jump");
    // Known: reads the idle marks `tick.stream` brings and the `route` `tick.submit` may change.
    b.schedule(stage::INGEST_STREAM, "session_viewed.poll", |s, _, tx| {
        crate::session_viewed::poll(s, tx)
    })
    .after_all(&["tick.stream", "tick.submit"])
    .per_server(|s, _, tx| {
        crate::session_viewed::poll_retries(s, tx);
        false
    });

    // ── Phase 2: Derive ──────────────────────────────────────────────────────────────────────────
    b.schedule(stage::DERIVE_TIMELINE, "working.sync", |s, _, _| {
        crate::working::sync(s)
    });
    b.schedule(stage::DERIVE_TIMELINE, "timeline.dock_morph", |s, _, tx| {
        let asked = s.take_asked_dock_face();
        if s.dock_face() != asked {
            s.morph(tx);
        }
        false
    });
    // Known: `subagents.shells::followed` reads `expanded` before `disclosure.poll` drops closed folds.
    b.schedule(stage::DERIVE_TIMELINE, "disclosure.poll", crate::disclosure::poll)
        .after("subagents.shells");
    // Known: `projection.rs` reads `expanded` updated by `disclosure.poll`.
    b.schedule(stage::DERIVE_TIMELINE, "timeline.projection", |s, _, tx| {
        session_view::timeline::projection_poll(s, tx)
    })
    .after("disclosure.poll");
    b.schedule(stage::DERIVE_TIMELINE, "line_reveal.poll", |s, now, tx| {
        crate::line_reveal::poll(s, now, tx)
    });
    b.schedule(stage::DERIVE_TIMELINE, "copied.poll", crate::copied::poll);
    b.schedule(stage::DERIVE_TIMELINE, "diagram.poll", crate::diagram::poll);
    b.schedule(stage::DERIVE_TIMELINE, "timeline.images", |s, now, tx| {
        session_view::images::poll(s, now, tx)
    });
    // An equation finished rendering: this window's rows that drew it build again.
    b.schedule(stage::DERIVE_TIMELINE, "timeline.math", |s, _, _| session_view::poll_math(s));
    b.schedule(stage::DERIVE_TIMELINE, "model_picker.hover", |s, now, tx| {
        crate::model_picker::hover::poll(s, now, tx)
    });
    b.schedule(stage::DERIVE_TIMELINE, "background.poll", |s, _, tx| {
        crate::background::poll(s, tx)
    });

    // ── Phase 3: React ───────────────────────────────────────────────────────────────────────────
    // Precaution: `browser.poll` fires `Browser.Picked` into plugins; run after review/files/terminal.
    #[cfg(not(target_arch = "wasm32"))]
    b.schedule(stage::REACT_PANELS, "browser.poll", |s, _, tx| {
        crate::plugin_host::native::browser::poll(s, tx)
    })
    .after_all(&["review.poll", "files.poll", "terminal.poll"]);
    // Known: `plugin_watched_stamp` hashes `review::REVIEW` and `workspace_panes.browser` and `browser.poll`
    // completes CDP promises / fires `Browser.Picked` before plugin VMs poll.
    #[cfg(not(target_arch = "wasm32"))]
    b.schedule(stage::REACT_PANELS, "plugin_host.poll", |s, _, tx| s.poll_plugins(tx))
        .after("browser.poll");
    #[cfg(target_arch = "wasm32")]
    b.schedule(stage::REACT_PANELS, "plugin_host.poll", |s, _, tx| s.poll_plugins(tx))
        .after_all(&["review.poll", "files.poll", "terminal.poll"]);
    // Precaution: `plugin_host.poll` and `workspace_tools.rune_mcp` (`tools::execute`) can mutate
    // `source`/session state before turn diffs start.
    b.schedule(stage::REACT_PANELS, "turn_diffs.poll", |s, _, _| {
        s.poll_turn_diffs()
    })
    .after("workspace_tools.rune_mcp");

    b.schedule(stage::REACT_STATUS, "reconnect.poll", |s, _, tx| {
        crate::reconnect::poll(s, tx)
    });
    // Known: `reconnect.poll` calls `session_state::reconnected` mutating `services.session_states`.
    b.schedule(stage::REACT_STATUS, "session_state.poll", |s, _, tx| {
        crate::session_state::poll(s, tx)
    })
    .after("reconnect.poll")
    .per_server(|s, _, tx| {
        crate::session_state::poll(s, tx);
        false
    });
    // Known: `tips.poll` can push a toast polled here, and closing an undo-delete toast calls
    // `undo::toast_closed` -> `send_delete` before `REACT_SHELL`; precaution: `session_state.poll`
    // reads `sessions` before an undo-delete toast commits.
    b.schedule(stage::REACT_STATUS, "status_toast.poll", |s, now, tx| {
        crate::status_toast::poll(s, now, tx)
    })
    .after_all(&["session_state.poll", "tips.poll"]);

    b.schedule(stage::REACT_SHELL, "layout_state.poll", |s, now, tx| {
        crate::layout_state::poll(s, now, tx)
    });
    // Known/precaution: reads `tab_layout()` and `narrow` settled by `layout_state.poll` -> `narrow.poll`.
    b.schedule(stage::REACT_SHELL, "inbox_model.poll", |s, now, tx| {
        crate::inbox_model::poll(s, now, tx)
    })
    .after("narrow.poll")
    .per_server(crate::inbox_model::poll_branches);
    // Known: reads `narrow()` and `tab_layout()` settled by `layout_state.poll` -> `narrow.poll`.
    b.schedule(stage::REACT_SHELL, "tab_preview.poll", |s, now, tx| {
        crate::view::tab_preview::poll(s, now, tx)
    })
    .after("narrow.poll");
    b.schedule(stage::REACT_SHELL, "tab_hints.poll", |s, now, tx| {
        crate::tab_hints::poll(s, now, tx)
    });
    b.schedule(stage::REACT_SHELL, "auto_tab_groups.poll", |s, _, _| {
        crate::tabs::auto_groups::poll(s)
    });
    // Known: reads `tab_layout()` and `settings_open()` settled by `layout_state.poll` -> `narrow.poll`.
    b.schedule(stage::REACT_SHELL, "session_status.poll", |s, now, tx| {
        crate::session_status::poll(s, now, tx)
    })
    .after("narrow.poll")
    .per_server(|s, now, tx| {
        crate::session_status::poll(s, now, tx);
        false
    });

    // ── Phase 4: Persist ─────────────────────────────────────────────────────────────────────────
    // Known: `restore.persist` runs `hydrate_restore` mutating `tabs`/`route`/`window` before
    // window, taskbar, and jump-list persistence.
    b.schedule(stage::PERSIST_FLUSH, "windows.persist", |s, _, _| {
        if !s.persists() {
            return false;
        }
        crate::windows::reopen::poll(s);
        crate::windows::reopen::poll_asked(s);
        s.kernel.slots.get_mut(&crate::prompt_history::PROMPTS).hydrate((*s.waker).clone());
        s.poll_window();
        s.poll_taskbar();
        crate::jump_list::poll(s);
        // A window with its own id saves its frame itself (`windows`), as AppKit's one autosave
        // name can't tell windows apart.
        #[cfg(not(target_arch = "wasm32"))]
        if !*s.appearance_state.framed && s.window.id.is_empty() {
            *s.appearance_state.framed = rune_mac::remember_frame("OpenCodeRuneMain");
        }
        false
    })
    .after("restore.persist");
}
