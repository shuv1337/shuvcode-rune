//! Finding, downloading, starting and connecting to the engine over its Unix socket. The UI
//! thread only hands the work to a thread: the socket, the install's folder and the spawn are
//! touched there.

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::PoisonError;

use crate::client::BrowserHost;
use crate::installer::{self, EngineStatus, Fault};
#[cfg(target_os = "macos")]
use crate::mach_pipe::MachReceiver;
use crate::machine::{ConnectOrigin, EngineEffect, EngineEvent};
use crate::prefetch::Mode;
use crate::profile;
use crate::transport::{Link, UnixStream, spawn_and_attach, wire_stream};

#[cfg(target_os = "macos")]
static NEXT_RECEIVER_TAG: AtomicU64 = AtomicU64::new(1);

impl BrowserHost {
    /// Nothing is looked for until a page needs the engine (`EngineStatus::Unchecked`), unless
    /// this build has no engine source, trust rule or binary override at all (`Fault::NotPublished`).
    pub(crate) fn initial_status() -> EngineStatus {
        if Self::engine_configured() {
            EngineStatus::Unchecked
        } else {
            EngineStatus::Failed(Fault::NotPublished.into())
        }
    }

    /// Once a launch is over (the app drew its first frame and takes input): unless an engine
    /// this build accepts is installed, the process's one download job fetches, checks and
    /// installs it now, quietly, on its own background-priority thread, so the first page finds
    /// it there (a fresh install, or an app update that wants a newer engine revision). It reads
    /// the engine's folder first and asks the source nothing when the engine is there. Nothing
    /// shows and no engine process starts; a page that opens meanwhile joins the same download
    /// and shows its progress. A source with no engine for this build is `NotPublished` before
    /// the first link is clicked, and is read again every hour.
    pub fn fetch_in_background(&self) {
        if self.status() != EngineStatus::Unchecked || !crate::source::configured() {
            return;
        }
        rune::marks::mark("browser.engine.probe");
        self.prefetch.start(Mode::Launch, &self.shared, self.message_waker());
        rune::marks::mark("browser.engine.probe.spawned");
    }

    /// The network changed: a quiet download that waits for it tries again now.
    pub fn network_changed(&self) {
        self.prefetch.network_changed();
    }

    /// Starts (or retries) the engine: connects to this app's running one, starts the installed
    /// one, or downloads it first (a download already running goes on at full speed instead).
    /// Queued tabs open once it's ready (`poll`).
    pub fn start_install(&mut self) {
        // One that stopped answering is ended: its pages open again in a new one.
        let end_hung = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::EndHung);
        if let EngineEffect::EndHung { kill_pid, sock, outbox } = end_hung {
            if let Some(pid) = kill_pid {
                crate::transport::kill_pid(pid);
            }
            if let Some(tx) = outbox {
                tx.send(crate::protocol::HostMsg::Shutdown).ok();
            }
            if let Some(sock) = sock.or_else(|| self.engine_socket().ok()) {
                std::fs::remove_file(sock).ok();
            }
        }
        if self.is_connected() || self.status() == EngineStatus::Connecting {
            return;
        }
        self.requeue_tabs();
        self.connect(Start::ForPage);
    }

    /// A page is likely within `hold` (the pointer on a launcher for one, an agent's turn): the
    /// installed engine's process starts now, so the page doesn't wait for it, and it stops again
    /// once `hold` passes with no tab opened. Nothing starts while a tab is open, an engine runs
    /// or starts, or none is installed, and nothing downloads. One sleeping thread per call
    /// keeps the hold; the next call's hold replaces it.
    pub fn prepare(&mut self, hold: std::time::Duration) {
        use std::sync::atomic::Ordering;
        if !self.tabs.is_empty() || !matches!(self.status(), EngineStatus::Unchecked | EngineStatus::Installed | EngineStatus::Ready | EngineStatus::Connecting) {
            return;
        }
        rune::marks::mark("browser.engine.prepare");
        let call = self.prepared.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.is_connected() && self.status() != EngineStatus::Connecting {
            self.connect(Start::Only);
        }
        if self.keep_engine {
            return;
        }
        let (opened, prepared, shared) = (self.opened.clone(), self.prepared.clone(), self.shared.clone());
        let sock = self.engine_socket().ok();
        let tabs = opened.load(Ordering::Relaxed);
        std::thread::Builder::new()
            .name("opencode-browser-hold".into())
            .spawn(move || {
                std::thread::sleep(hold);
                if prepared.load(Ordering::Relaxed) == call && opened.load(Ordering::Relaxed) == tabs {
                    // Hold `shared` across `remove_file` so a concurrent `open_tab` cannot observe
                    // the disconnected state and connect to the shutting-down engine's socket.
                    let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    if let EngineEffect::Shutdown { outbox: Some(tx), sock: active_sock } = shared.step(EngineEvent::Unprepare) {
                        if let Some(sock) = active_sock.or(sock) {
                            std::fs::remove_file(sock).ok();
                        }
                        rune::marks::mark("browser.engine.unprepared");
                        tx.send(crate::protocol::HostMsg::Shutdown).ok();
                    }
                }
            })
            .ok();
    }

    /// The first page of the run is open on a running engine: a newer release the source offers
    /// downloads beside it, quietly, for its next start. Once a run; before that page, a launch
    /// that found its engine installed asked the source nothing.
    pub(crate) fn check_for_update_once(&mut self) {
        let has_tabs = !self.tabs.is_empty();
        if !self.update_check.is_pending() || !has_tabs {
            return;
        }
        if !self.update_check.step(has_tabs, self.status() == EngineStatus::Ready) {
            return;
        }
        self.prefetch.start(Mode::Update, &self.shared, self.waker.clone());
    }

    fn ensure_mach_receiver(&mut self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            if self.mach.is_none() {
                let tag = NEXT_RECEIVER_TAG.fetch_add(1, Ordering::Relaxed);
                self.mach = MachReceiver::start(self.surfaces.clone(), tag).ok();
            }
            self.mach.as_ref().map(|m| m.service_name.clone())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    pub(crate) fn ensure_connected(&mut self) {
        if self.is_connected() {
            return;
        }
        match self.status() {
            EngineStatus::Connecting | EngineStatus::Failed(_) => {}
            // A tab waits for the download running in the background: let it finish, at full speed.
            EngineStatus::Downloading { .. } | EngineStatus::Verifying | EngineStatus::Paused(_) => self.prefetch.start(Mode::Urgent, &self.shared, self.message_waker()),
            _ => self.connect(Start::ForPage),
        }
    }

    pub(crate) fn engine_socket(&self) -> Result<std::path::PathBuf, &'static str> {
        self.socket_override.as_deref().map_or_else(installer::try_socket_path, installer::custom_socket)
    }

    /// Spawns a fresh engine without attempting to reuse a pre-existing socket (used once after a
    /// pre-existing socket closed before answering `Ready`).
    pub(crate) fn connect_fresh(&mut self) {
        self.connect(Start::FreshForPage);
    }

    /// Hands the connect to a thread: this app's running engine (a UI hot reload), else the
    /// installed one, started, else a download that a page waits for.
    fn connect(&mut self, start: Start) {
        self.sync_has_tabs();
        let EngineEffect::Generation(generation) = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::BeginConnect) else {
            return;
        };
        let mach_service = self.ensure_mach_receiver();
        let sock = match self.engine_socket() {
            Ok(path) => path,
            Err(err) => {
                self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::LinkStatus {
                    generation,
                    status: EngineStatus::Failed(Fault::Dirs(err).into()),
                });
                return;
            }
        };
        let profile = profile::for_spawn(self.profile_dir.as_deref());
        self.record_launched_extensions();
        let extension_args = self.startup_extension_args();
        let link = Link {
            generation,
            shared: self.shared.clone(),
            inbox_tx: self.inbox_tx.clone(),
            surfaces: self.surfaces.clone(),
            waker: self.message_waker.clone().or_else(|| self.waker.clone()),
            sock: sock.clone(),
            profile: profile.clone(),
            stderr: None,
            opens: self.pending_open_msgs(),
            extension_args,
            profile_permissions: self.site_permissions.list().to_vec(),
            preview_chunk_budget: self.preview_chunk_budget.clone(),
            #[cfg(target_os = "linux")]
            dmabuf_pools: self.dmabuf_pools.clone(),
        };
        let prefetch = self.prefetch.clone();
        let spawned = std::thread::Builder::new().name("opencode-browser-connect".into()).spawn(move || {
            if !link.current() {
                return;
            }
            if start != Start::FreshForPage
                && let Ok(stream) = UnixStream::connect(&sock)
            {
                crate::engine_log::log(
                    link.profile.as_deref(),
                    &format!("gen={generation} attach-existing sock={}", sock.display()),
                );
                let _ = wire_stream(stream, mach_service, &link, ConnectOrigin::Reused);
                return;
            }
            let Some(profile) = profile else {
                return link.set(EngineStatus::Failed(Fault::Dirs(installer::NO_DIRS).into()));
            };
            let Some(engine) = installer::installed() else {
                if start == Start::Only {
                    return link.set(EngineStatus::NotInstalled);
                }
                // The launch's quiet download, when it runs, is the one this page joins.
                return prefetch.start(Mode::Urgent, &link.shared, link.waker.clone());
            };
            rune::marks::mark("browser.engine.spawn");
            spawn_and_attach(&engine, &sock, &profile, mach_service, &link);
            // Earlier versions this one replaces, now nothing starts them (for a page only: an
            // engine started ahead of one leaves the folder alone).
            if matches!(start, Start::ForPage | Start::FreshForPage)
                && let Some(version) = engine.version
                && let Some(root) = installer::try_engines_root()
            {
                installer::remove_old(&root, version);
            }
        });
        if spawned.is_err() {
            self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::LinkStatus {
                generation,
                status: EngineStatus::Failed(Fault::Launch("no thread to start it on".to_owned()).into()),
            });
        }
    }
}

/// Why the engine starts: a page waits for it (downloading it first if need be), or one is
/// likely soon (`prepare`: the installed engine or nothing).
#[derive(Clone, Copy, PartialEq)]
enum Start {
    ForPage,
    FreshForPage,
    Only,
}
