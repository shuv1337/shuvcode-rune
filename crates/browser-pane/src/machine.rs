//! Explicit sidecar connection lifecycle state machine for [`crate::client::BrowserHost`].

use std::collections::HashMap;
#[cfg(feature = "engine")]
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::installer::EngineStatus;
use crate::protocol::HostMsg;

/// Tabs a connect sent itself, by id: the address and size (width, height in points, scale)
/// each opened at.
#[cfg(feature = "engine")]
pub(crate) type EarlyOpensMap = HashMap<u64, (String, (f32, f32, f32))>;

/// Stage a connected engine was in before it stopped answering (`LiveStage::Hung`).
#[cfg(feature = "engine")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HungBefore {
    Connecting,
    Ready,
}

/// Whether a live socket connection reused a pre-existing socket or spawned a fresh engine.
#[cfg(feature = "engine")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectOrigin {
    Reused,
    Spawned,
}

/// Lifecycle of a live socket connection to the sidecar engine.
#[cfg(feature = "engine")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveStage {
    /// Socket attached (`Hello` queued), waiting for `EngineMsg::Ready`.
    Connecting,
    /// `EngineMsg::Ready` received.
    Ready,
    /// No answer within `CONNECT_DEADLINE` / `ANSWER_DEADLINE`; a late message restores `before`.
    Hung { before: HungBefore },
}

/// Phase of the engine sidecar connection protected by [`crate::client::BrowserHost::shared`].
///
/// Replaces separate `status`, `outbox`, `engine` (PID), `early`, and `conn_gen` primitives so
/// a disconnected host cannot carry a live outbox or unconsumed early opens, can retain a child
/// PID only in `HungDetached` (until the process exits or Retry kills it), and a stale connection
/// generation cannot overwrite a newer one.
pub(crate) enum EnginePhase {
    /// No live outbox: absent, downloading, spawning before attach, lost/failed, shut down, or a
    /// mock preview tab (`EngineStatus::Ready` without a sidecar).
    Disconnected(EngineStatus),
    /// A pre-existing socket closed before answering `Ready` (the old engine was shutting down).
    /// Reports `EngineStatus::Connecting` so the UI never flickers to `Failed`; the next `poll()`
    /// spawns one fresh engine without reusing that socket.
    #[cfg(feature = "engine")]
    RetryFresh,
    /// Disconnected via `Shutdown` or `Unprepare` while hung, with our spawned child process still
    /// potentially alive until its watcher reports exit (`ProcessExited`) or Retry kills it (`EndHung`).
    #[cfg(feature = "engine")]
    HungDetached {
        /// Connection generation that spawned `pid`, so a stale watcher from an earlier engine
        /// cannot clear a newer engine's PID even if the OS recycles it.
        spawn_gen: u64,
        pid: u32,
        /// The socket path this detached engine was listening on (may be a Windows fallback path).
        sock: PathBuf,
    },
    /// Connected to a sidecar over its Unix socket.
    #[cfg(feature = "engine")]
    Connected {
        stage: LiveStage,
        origin: ConnectOrigin,
        /// The socket path this engine is listening on (may be a Windows fallback path).
        sock: PathBuf,
        outbox: Sender<HostMsg>,
        /// PID of the child this app spawned (`None` when attached to a pre-existing socket).
        pid: Option<u32>,
        /// Tabs `wire_stream` sent behind `Hello`, drained once by `flush_pending_opens`.
        early: EarlyOpensMap,
    },
    /// Synthetic outbox installed by `BrowserHost::capture_sent`, keeping arbitrary status.
    Captured {
        status: EngineStatus,
        outbox: Sender<HostMsg>,
    },
}

/// Shared connection generation and phase guarded by one mutex across UI and sidecar threads.
pub(crate) struct SharedEngine {
    pub(crate) generation: u64,
    pub(crate) phase: EnginePhase,
    /// Whether the host currently holds at least one backing engine tab.
    #[cfg(feature = "engine")]
    pub(crate) has_tabs: bool,
    /// True while a ready engine has sat idle with zero tabs and has not yet confirmed a newly
    /// opened tab. A clean `EMPTY_GRACE` exit while idle — even racing with a brand-new `OpenTab`
    /// — returns to `Installed` instead of `Fault::Stopped`.
    #[cfg(feature = "engine")]
    pub(crate) idle_ready: bool,
}

pub(crate) enum EngineEvent {
    MockStatus(EngineStatus),
    Capture(Sender<HostMsg>),
    Ready,
    Shutdown,
    #[cfg(feature = "engine")]
    BeginConnect,
    #[cfg(feature = "engine")]
    EndHung,
    #[cfg(feature = "engine")]
    Unprepare,
    #[cfg(feature = "engine")]
    Attach { generation: u64, origin: ConnectOrigin, sock: PathBuf, outbox: Sender<HostMsg>, early: EarlyOpensMap },
    #[cfg(feature = "engine")]
    ProcessSpawned { generation: u64, pid: u32 },
    #[cfg(feature = "engine")]
    ProcessExited { generation: u64, pid: u32 },
    #[cfg(feature = "engine")]
    LinkStatus { generation: u64, status: EngineStatus },
    #[cfg(feature = "engine")]
    InstallStatus(EngineStatus),
    /// The start-up index read found no engine published for this platform.
    #[cfg(feature = "engine")]
    ProbeNotPublished,
    /// A later index read found an engine for this platform.
    #[cfg(feature = "engine")]
    ProbePublished,
    /// A quiet download installed the engine.
    #[cfg(feature = "engine")]
    Fetched,
    #[cfg(feature = "engine")]
    Hung { generation: u64 },
    #[cfg(feature = "engine")]
    Recovered { generation: u64 },
    #[cfg(feature = "engine")]
    Lost { generation: u64, fault: crate::installer::Fault },
    #[cfg(feature = "engine")]
    SetHasTabs(bool),
    #[cfg(feature = "engine")]
    TabConfirmed,
    #[cfg(feature = "engine")]
    ClearStaleFault,
}

pub(crate) enum EngineEffect {
    None,
    ReadyApplied,
    Shutdown {
        outbox: Option<Sender<HostMsg>>,
        #[cfg(feature = "engine")]
        sock: Option<PathBuf>,
    },
    #[cfg(feature = "engine")]
    Generation(u64),
    #[cfg(feature = "engine")]
    EndHung { kill_pid: Option<u32>, sock: Option<PathBuf>, outbox: Option<Sender<HostMsg>> },
    #[cfg(feature = "engine")]
    Rejected,
    #[cfg(feature = "engine")]
    Wake,
    #[cfg(feature = "engine")]
    LostFirst,
    /// Ready engine exited while idle (zero tabs, or unconfirmed race right at `EMPTY_GRACE`).
    #[cfg(feature = "engine")]
    IdleLost,
    /// A pre-existing socket closed before `Ready`; the host should spawn a fresh engine once.
    #[cfg(feature = "engine")]
    RetryFresh,
}

impl SharedEngine {
    pub(crate) fn new(initial: EngineStatus) -> Self {
        Self {
            generation: 0,
            phase: EnginePhase::Disconnected(initial),
            #[cfg(feature = "engine")]
            has_tabs: false,
            #[cfg(feature = "engine")]
            idle_ready: false,
        }
    }

    pub(crate) fn status(&self) -> EngineStatus {
        match &self.phase {
            EnginePhase::Disconnected(s) => s.clone(),
            #[cfg(feature = "engine")]
            EnginePhase::RetryFresh => EngineStatus::Connecting,
            #[cfg(feature = "engine")]
            EnginePhase::HungDetached { .. } => EngineStatus::Failed(crate::installer::Fault::Hung.into()),
            #[cfg(feature = "engine")]
            EnginePhase::Connected { stage, .. } => match stage {
                LiveStage::Connecting => EngineStatus::Connecting,
                LiveStage::Ready => EngineStatus::Ready,
                LiveStage::Hung { .. } => EngineStatus::Failed(crate::installer::Fault::Hung.into()),
            },
            EnginePhase::Captured { status, .. } => status.clone(),
        }
    }

    pub(crate) fn is_connected(&self) -> bool {
        match &self.phase {
            EnginePhase::Disconnected(_) => false,
            #[cfg(feature = "engine")]
            EnginePhase::RetryFresh | EnginePhase::HungDetached { .. } => false,
            #[cfg(feature = "engine")]
            EnginePhase::Connected { .. } => true,
            EnginePhase::Captured { .. } => true,
        }
    }

    #[cfg(feature = "engine")]
    pub(crate) fn needs_fresh_spawn(&self) -> bool {
        matches!(self.phase, EnginePhase::RetryFresh)
    }

    pub(crate) fn outbox(&self) -> Option<&Sender<HostMsg>> {
        match &self.phase {
            EnginePhase::Disconnected(_) => None,
            #[cfg(feature = "engine")]
            EnginePhase::RetryFresh | EnginePhase::HungDetached { .. } => None,
            #[cfg(feature = "engine")]
            EnginePhase::Connected { outbox, .. } => Some(outbox),
            EnginePhase::Captured { outbox, .. } => Some(outbox),
        }
    }

    #[cfg(feature = "engine")]
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(crate) fn take_early(&mut self) -> HashMap<u64, (String, (f32, f32, f32))> {
        match &mut self.phase {
            #[cfg(feature = "engine")]
            EnginePhase::Connected { early, .. } => std::mem::take(early),
            #[cfg(feature = "engine")]
            EnginePhase::RetryFresh | EnginePhase::HungDetached { .. } => HashMap::new(),
            EnginePhase::Disconnected(_) | EnginePhase::Captured { .. } => HashMap::new(),
        }
    }

    pub(crate) fn step(&mut self, event: EngineEvent) -> EngineEffect {
        match event {
            EngineEvent::MockStatus(next) => {
                // Accepted difference from the old separate `status`/`outbox` locks, which only
                // overwrote `status` and left `outbox` intact: turning `Connected` into
                // `Disconnected` is safe because `MockStatus` is only called by `set_mock_status`,
                // `insert_mock_tab`, and `with_socket` (stories, fixtures, and fresh hosts), never
                // on a live sidecar connection.
                self.phase = match std::mem::replace(&mut self.phase, EnginePhase::Disconnected(EngineStatus::Unsupported)) {
                    EnginePhase::Captured { outbox, .. } => EnginePhase::Captured { status: next, outbox },
                    _ => EnginePhase::Disconnected(next),
                };
                EngineEffect::None
            }
            EngineEvent::Capture(outbox) => {
                let status = self.status();
                self.phase = EnginePhase::Captured { status, outbox };
                EngineEffect::None
            }
            EngineEvent::Ready => match &mut self.phase {
                #[cfg(feature = "engine")]
                EnginePhase::Connected { stage, .. } => {
                    *stage = LiveStage::Ready;
                    self.idle_ready = !self.has_tabs;
                    EngineEffect::ReadyApplied
                }
                EnginePhase::Captured { status, .. } => {
                    *status = EngineStatus::Ready;
                    EngineEffect::ReadyApplied
                }
                #[cfg(feature = "engine")]
                EnginePhase::RetryFresh | EnginePhase::HungDetached { .. } => EngineEffect::None,
                EnginePhase::Disconnected(_) => EngineEffect::None,
            },
            EngineEvent::Shutdown => {
                #[cfg(feature = "engine")]
                let prev_gen = self.generation;
                #[cfg(feature = "engine")]
                {
                    self.idle_ready = false;
                }
                self.generation = self.generation.wrapping_add(1);
                let prev = std::mem::replace(&mut self.phase, EnginePhase::Disconnected(EngineStatus::Unsupported));
                #[cfg(feature = "engine")]
                let mut active_sock: Option<PathBuf> = None;
                let (next_phase, outbox) = match prev {
                    #[cfg(feature = "engine")]
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, sock, outbox, pid: Some(pid), .. } => {
                        active_sock = Some(sock.clone());
                        (EnginePhase::HungDetached { spawn_gen: prev_gen, pid, sock }, Some(outbox))
                    }
                    #[cfg(feature = "engine")]
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, sock, outbox, pid: None, .. } => {
                        active_sock = Some(sock);
                        (EnginePhase::Disconnected(EngineStatus::Failed(crate::installer::Fault::Hung.into())), Some(outbox))
                    }
                    #[cfg(feature = "engine")]
                    EnginePhase::Connected { sock, outbox, .. } => {
                        active_sock = Some(sock);
                        (EnginePhase::Disconnected(EngineStatus::Installed), Some(outbox))
                    }
                    #[cfg(feature = "engine")]
                    EnginePhase::RetryFresh => (EnginePhase::Disconnected(EngineStatus::Installed), None),
                    #[cfg(feature = "engine")]
                    EnginePhase::HungDetached { spawn_gen, pid, sock } => {
                        active_sock = Some(sock.clone());
                        (EnginePhase::HungDetached { spawn_gen, pid, sock }, None)
                    }
                    EnginePhase::Captured { status, outbox } => {
                        let next = match status {
                            EngineStatus::Ready | EngineStatus::Connecting => EngineStatus::Installed,
                            other => other,
                        };
                        (EnginePhase::Disconnected(next), Some(outbox))
                    }
                    EnginePhase::Disconnected(status) => {
                        let next = match status {
                            EngineStatus::Ready | EngineStatus::Connecting => EngineStatus::Installed,
                            other => other,
                        };
                        (EnginePhase::Disconnected(next), None)
                    }
                };
                self.phase = next_phase;
                EngineEffect::Shutdown {
                    outbox,
                    #[cfg(feature = "engine")]
                    sock: active_sock,
                }
            }
            #[cfg(feature = "engine")]
            EngineEvent::BeginConnect => {
                self.idle_ready = false;
                self.generation = self.generation.wrapping_add(1);
                self.phase = EnginePhase::Disconnected(EngineStatus::Connecting);
                EngineEffect::Generation(self.generation)
            }
            #[cfg(feature = "engine")]
            EngineEvent::EndHung => {
                self.idle_ready = false;
                let is_hung = matches!(
                    &self.phase,
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, .. }
                        | EnginePhase::HungDetached { .. }
                ) || matches!(&self.phase, EnginePhase::Disconnected(s) if s.is_hung())
                    || matches!(&self.phase, EnginePhase::Captured { status, .. } if status.is_hung());
                if !is_hung {
                    return EngineEffect::None;
                }
                let prev = std::mem::replace(
                    &mut self.phase,
                    EnginePhase::Disconnected(EngineStatus::Failed(crate::installer::Fault::Hung.into())),
                );
                match prev {
                    EnginePhase::Connected { sock, outbox, pid, .. } => {
                        self.generation = self.generation.wrapping_add(1);
                        let shutdown_tx = pid.is_none().then_some(outbox);
                        EngineEffect::EndHung { kill_pid: pid, sock: Some(sock), outbox: shutdown_tx }
                    }
                    EnginePhase::HungDetached { pid, sock, .. } => {
                        EngineEffect::EndHung { kill_pid: Some(pid), sock: Some(sock), outbox: None }
                    }
                    _ => EngineEffect::EndHung { kill_pid: None, sock: None, outbox: None },
                }
            }
            #[cfg(feature = "engine")]
            EngineEvent::Unprepare => {
                if !self.is_connected() {
                    return EngineEffect::None;
                }
                self.idle_ready = false;
                let prev_gen = self.generation;
                self.generation = self.generation.wrapping_add(1);
                let prev = std::mem::replace(&mut self.phase, EnginePhase::Disconnected(EngineStatus::Unsupported));
                let mut active_sock: Option<PathBuf> = None;
                let (next_phase, outbox) = match prev {
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, sock, outbox, pid: Some(pid), .. } => {
                        active_sock = Some(sock.clone());
                        (EnginePhase::HungDetached { spawn_gen: prev_gen, pid, sock }, Some(outbox))
                    }
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, sock, outbox, pid: None, .. } => {
                        active_sock = Some(sock);
                        (EnginePhase::Disconnected(EngineStatus::Failed(crate::installer::Fault::Hung.into())), Some(outbox))
                    }
                    EnginePhase::Connected { sock, outbox, .. } => {
                        active_sock = Some(sock);
                        (EnginePhase::Disconnected(EngineStatus::Installed), Some(outbox))
                    }
                    EnginePhase::Captured { status, outbox } => {
                        let next = match status {
                            EngineStatus::Ready | EngineStatus::Connecting => EngineStatus::Installed,
                            other => other,
                        };
                        (EnginePhase::Disconnected(next), Some(outbox))
                    }
                    detached @ (EnginePhase::Disconnected(_) | EnginePhase::RetryFresh | EnginePhase::HungDetached { .. }) => (detached, None),
                };
                self.phase = next_phase;
                EngineEffect::Shutdown { outbox, sock: active_sock }
            }
            #[cfg(feature = "engine")]
            EngineEvent::Attach { generation, origin, sock, outbox, early } => {
                if self.generation != generation {
                    return EngineEffect::Rejected;
                }
                self.idle_ready = false;
                self.phase = EnginePhase::Connected { stage: LiveStage::Connecting, origin, sock, outbox, pid: None, early };
                EngineEffect::Wake
            }
            #[cfg(feature = "engine")]
            EngineEvent::ProcessSpawned { generation, pid } => {
                if self.generation == generation
                    && let EnginePhase::Connected { pid: slot, .. } = &mut self.phase
                {
                    *slot = Some(pid);
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::ProcessExited { generation, pid } => {
                match &mut self.phase {
                    EnginePhase::Connected { pid: slot, .. } if self.generation == generation && *slot == Some(pid) => {
                        *slot = None;
                    }
                    EnginePhase::HungDetached { spawn_gen, pid: hung_pid, .. } if *spawn_gen == generation && *hung_pid == pid => {
                        self.phase = EnginePhase::Disconnected(EngineStatus::Failed(crate::installer::Fault::Hung.into()));
                    }
                    _ => {}
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::LinkStatus { generation, status } => {
                if self.generation != generation || !matches!(self.phase, EnginePhase::Disconnected(_)) {
                    return EngineEffect::None;
                }
                self.phase = EnginePhase::Disconnected(status);
                EngineEffect::Wake
            }
            #[cfg(feature = "engine")]
            EngineEvent::InstallStatus(status) => {
                if !matches!(self.phase, EnginePhase::Disconnected(_)) {
                    return EngineEffect::None;
                }
                self.phase = EnginePhase::Disconnected(status);
                EngineEffect::Wake
            }
            // Only before anything else looked for the engine: a page that started a connect or
            // download meanwhile keeps its own outcome.
            #[cfg(feature = "engine")]
            EngineEvent::ProbeNotPublished => {
                if !matches!(self.phase, EnginePhase::Disconnected(EngineStatus::Unchecked)) {
                    return EngineEffect::None;
                }
                self.phase = EnginePhase::Disconnected(EngineStatus::Failed(crate::installer::Fault::NotPublished.into()));
                EngineEffect::Wake
            }
            // Back to unknown, so the next page downloads the engine the index now lists.
            #[cfg(feature = "engine")]
            EngineEvent::ProbePublished => {
                if !matches!(&self.phase, EnginePhase::Disconnected(s) if s.is_not_published()) {
                    return EngineEffect::None;
                }
                self.phase = EnginePhase::Disconnected(EngineStatus::Unchecked);
                EngineEffect::Wake
            }
            // Only a host that looked before and found none (`prepare`) changes: it starts the
            // engine ahead of a page again. One that never looked finds it when a page opens.
            #[cfg(feature = "engine")]
            EngineEvent::Fetched => {
                if !matches!(self.phase, EnginePhase::Disconnected(EngineStatus::NotInstalled)) {
                    return EngineEffect::None;
                }
                self.phase = EnginePhase::Disconnected(EngineStatus::Installed);
                EngineEffect::Wake
            }
            #[cfg(feature = "engine")]
            EngineEvent::Hung { generation } => {
                if self.generation != generation {
                    return EngineEffect::None;
                }
                if let EnginePhase::Connected { stage, .. } = &mut self.phase {
                    let before = match *stage {
                        LiveStage::Connecting => HungBefore::Connecting,
                        LiveStage::Ready => HungBefore::Ready,
                        LiveStage::Hung { .. } => return EngineEffect::None,
                    };
                    *stage = LiveStage::Hung { before };
                    return EngineEffect::Wake;
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::Recovered { generation } => {
                if self.generation != generation {
                    return EngineEffect::None;
                }
                if let EnginePhase::Connected { stage, .. } = &mut self.phase
                    && let LiveStage::Hung { before } = *stage
                {
                    *stage = match before {
                        HungBefore::Connecting => LiveStage::Connecting,
                        HungBefore::Ready => LiveStage::Ready,
                    };
                    return EngineEffect::Wake;
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::Lost { generation, fault } => {
                if self.generation != generation {
                    return EngineEffect::None;
                }
                match &self.phase {
                    // Attached to a pre-existing socket whose engine was already shutting down
                    // (`EMPTY_GRACE`, `Unprepare`, or `shutdown_sidecar`) and closed before
                    // sending `Ready`. Spawn one fresh engine instead of flashing `Fault::Stopped`.
                    EnginePhase::Connected { stage: LiveStage::Connecting, origin: ConnectOrigin::Reused, .. }
                        if fault == crate::installer::Fault::Stopped =>
                    {
                        self.generation = self.generation.wrapping_add(1);
                        self.idle_ready = false;
                        self.phase = EnginePhase::RetryFresh;
                        EngineEffect::RetryFresh
                    }
                    // The engine exited after its idle minute (`EMPTY_GRACE`) with no tabs open,
                    // or right as a brand-new tab was sent before it could confirm it. Return to
                    // `Installed` so the next tab starts a fresh engine instead of showing a
                    // stale crash banner.
                    EnginePhase::Connected { stage: LiveStage::Ready, .. }
                        if !self.has_tabs
                            || (self.idle_ready && fault == crate::installer::Fault::Stopped) =>
                    {
                        self.generation = self.generation.wrapping_add(1);
                        self.idle_ready = false;
                        self.phase = EnginePhase::Disconnected(EngineStatus::Installed);
                        EngineEffect::IdleLost
                    }
                    EnginePhase::Connected { .. } | EnginePhase::Disconnected(EngineStatus::Connecting) => {
                        self.idle_ready = false;
                        self.phase = EnginePhase::Disconnected(EngineStatus::Failed(fault.into()));
                        EngineEffect::LostFirst
                    }
                    EnginePhase::Disconnected(EngineStatus::Failed(f))
                        if f.fault == crate::installer::Fault::Stopped && fault == crate::installer::Fault::OutOfMemory =>
                    {
                        self.phase = EnginePhase::Disconnected(EngineStatus::Failed(fault.into()));
                        EngineEffect::Wake
                    }
                    _ => EngineEffect::None,
                }
            }
            #[cfg(feature = "engine")]
            EngineEvent::SetHasTabs(has_tabs) => {
                self.has_tabs = has_tabs;
                if !has_tabs {
                    if matches!(self.phase, EnginePhase::Connected { stage: LiveStage::Ready, .. }) {
                        self.idle_ready = true;
                    } else if matches!(
                        &self.phase,
                        EnginePhase::Disconnected(EngineStatus::Failed(f))
                            if matches!(
                                f.fault,
                                crate::installer::Fault::Stopped
                                    | crate::installer::Fault::OutOfMemory
                                    | crate::installer::Fault::Hung
                                    | crate::installer::Fault::Exited(_)
                            )
                    ) {
                        self.phase = EnginePhase::Disconnected(EngineStatus::Installed);
                    }
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::TabConfirmed => {
                if self.has_tabs {
                    self.idle_ready = false;
                }
                EngineEffect::None
            }
            #[cfg(feature = "engine")]
            EngineEvent::ClearStaleFault => {
                if self.has_tabs {
                    return EngineEffect::None;
                }
                match &self.phase {
                    EnginePhase::Disconnected(EngineStatus::Failed(f))
                        if matches!(
                            f.fault,
                            crate::installer::Fault::Stopped
                                | crate::installer::Fault::OutOfMemory
                                | crate::installer::Fault::Hung
                                | crate::installer::Fault::Exited(_)
                        ) =>
                    {
                        self.phase = EnginePhase::Disconnected(EngineStatus::Installed);
                        EngineEffect::None
                    }
                    EnginePhase::HungDetached { .. } => {
                        let EnginePhase::HungDetached { pid, sock, .. } =
                            std::mem::replace(&mut self.phase, EnginePhase::Disconnected(EngineStatus::Installed))
                        else {
                            return EngineEffect::None;
                        };
                        EngineEffect::EndHung { kill_pid: Some(pid), sock: Some(sock), outbox: None }
                    }
                    EnginePhase::Connected { stage: LiveStage::Hung { .. }, .. } => {
                        let EnginePhase::Connected { sock, outbox, pid, .. } =
                            std::mem::replace(&mut self.phase, EnginePhase::Disconnected(EngineStatus::Installed))
                        else {
                            return EngineEffect::None;
                        };
                        self.generation = self.generation.wrapping_add(1);
                        self.idle_ready = false;
                        let shutdown_tx = pid.is_none().then_some(outbox);
                        EngineEffect::EndHung { kill_pid: pid, sock: Some(sock), outbox: shutdown_tx }
                    }
                    _ => EngineEffect::None,
                }
            }
        }
    }
}

/// Capabilities advertised by the running (or most recently run) engine's `Ready` message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EngineCaps {
    #[default]
    Unknown,
    Known {
        downloads: bool,
        policy: bool,
        prompts: bool,
        popups: bool,
        find: bool,
        context_menu: bool,
        partitions: bool,
        remote_proxy: bool,
        agent_tools: bool,
        preview: bool,
        extensions: bool,
        permission_sync: bool,
        frame_authority: bool,
        cookies: bool,
        agent_files: bool,
        passkeys: bool,
        trace_isolation: bool,
        menu_extras: bool,
        tab_lifecycle: bool,
    },
}

impl EngineCaps {
    pub(crate) fn downloads(self) -> bool {
        matches!(self, Self::Known { downloads: true, .. })
    }

    pub(crate) fn policy(self) -> bool {
        matches!(self, Self::Known { policy: true, .. })
    }

    pub(crate) fn prompts(self) -> bool {
        matches!(self, Self::Known { prompts: true, .. })
    }

    pub(crate) fn popups(self) -> bool {
        matches!(self, Self::Known { popups: true, .. })
    }

    pub(crate) fn find(self) -> bool {
        matches!(self, Self::Known { find: true, .. })
    }

    pub(crate) fn context_menu(self) -> bool {
        matches!(self, Self::Known { context_menu: true, .. })
    }

    pub(crate) fn partitions(self) -> bool {
        matches!(self, Self::Known { partitions: true, .. })
    }

    pub(crate) fn remote_proxy(self) -> bool {
        matches!(self, Self::Known { remote_proxy: true, .. })
    }

    pub(crate) fn agent_tools(self) -> bool {
        matches!(self, Self::Known { agent_tools: true, .. })
    }

    pub(crate) fn preview(self) -> bool {
        matches!(self, Self::Known { preview: true, .. })
    }

    pub(crate) fn extensions(self) -> bool {
        matches!(self, Self::Known { extensions: true, .. })
    }

    pub(crate) fn permission_sync(self) -> bool {
        matches!(self, Self::Known { permission_sync: true, .. })
    }

    pub(crate) fn frame_authority(self) -> bool {
        matches!(self, Self::Known { frame_authority: true, .. })
    }

    pub(crate) fn agent_files(self) -> bool {
        matches!(self, Self::Known { agent_files: true, .. })
    }

    #[allow(dead_code)]
    pub(crate) fn passkeys(self) -> bool {
        matches!(self, Self::Known { passkeys: true, .. })
    }

    pub(crate) fn trace_isolation(self) -> bool {
        matches!(self, Self::Known { trace_isolation: true, .. })
    }

    pub(crate) fn menu_extras(self) -> bool {
        matches!(self, Self::Known { menu_extras: true, .. })
    }

    pub(crate) fn tab_lifecycle(self) -> bool {
        matches!(self, Self::Known { tab_lifecycle: true, .. })
    }
}

/// One-shot background update check per `BrowserHost`, started once the first page reaches `Ready`.
#[cfg(feature = "engine")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UpdateCheck {
    #[default]
    Pending,
    Started,
}

#[cfg(feature = "engine")]
impl UpdateCheck {
    pub(crate) fn is_pending(self) -> bool {
        self == Self::Pending
    }

    pub(crate) fn step(&mut self, has_tabs: bool, ready: bool) -> bool {
        if *self == Self::Pending && has_tabs && ready {
            *self = Self::Started;
            true
        } else {
            false
        }
    }
}
