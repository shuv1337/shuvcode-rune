//! Linux: the engine's dmabuf frames, into each tab's surface. The engine copies every GPU frame
//! into a few dmabufs per tab, sends their descriptors once (`EngineMsg::DmabufPool`), then names
//! the buffer each frame is in (`EngineMsg::DmabufFrame`). Rune imports them on the app's Vulkan
//! device (`rune::native_surface::linux`); the app tells the engine once its GPU has read a frame
//! (`HostMsg::FrameRead`), and the engine writes that buffer again only after. When the app can't
//! show them, it says so (`HostMsg::DmabufFailed`) and the engine reads its frames back to BGRA.
//!
//! Everything goes to the connection the frame came on, never to a later engine's.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use rune::native_surface::linux::{DRM_FORMAT_ARGB8888, Dmabuf, DmabufBuffer, DmabufPlane, import_target};

use crate::frame::TabSurface;
use crate::protocol::HostMsg;

/// Whether this app imports the engine's dmabufs: it does until an import fails.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Import {
    Wanted,
    Failed,
}

enum ImportEvent {
    Failed,
}

static IMPORT: Mutex<Import> = Mutex::new(Import::Wanted);

/// True when the import just failed, so the engine is told once.
fn step(state: &mut Import, event: ImportEvent) -> bool {
    match (*state, event) {
        (Import::Wanted, ImportEvent::Failed) => {
            *state = Import::Failed;
            true
        }
        (Import::Failed, ImportEvent::Failed) => false,
    }
}

/// The render node an engine started now copies its frames on, when this app imports dmabufs:
/// the GPU Rune draws the window on.
pub(crate) fn device() -> Option<String> {
    if *IMPORT.lock().unwrap_or_else(PoisonError::into_inner) == Import::Failed {
        return None;
    }
    let target = import_target()?;
    Some(target.render_node.unwrap_or_else(|| "/dev/dri/renderD128".into()))
}

/// Tells the engine on `tx` that its dmabufs can't be shown, once per app.
fn fail(tx: &Sender<HostMsg>, reason: String) {
    if step(&mut IMPORT.lock().unwrap_or_else(PoisonError::into_inner), ImportEvent::Failed) {
        eprintln!("browser: can't show the engine's dmabufs ({reason}); asking for bgra");
    }
    tx.send(HostMsg::DmabufFailed { reason }).ok();
}

/// How the engine's frames have been arriving, so the change is logged once.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Frames {
    None,
    Dmabuf,
    Bgra,
}

impl Frames {
    pub(crate) fn step(&mut self, now: Frames) {
        if *self != now {
            eprintln!("browser: page frames over {}", if now == Frames::Dmabuf { "dmabuf" } else { "bgra" });
            *self = now;
        }
    }
}

/// A buffer set as the engine describes it.
pub(crate) struct PoolInfo {
    pub tab_id: u64,
    pub pool: u64,
    pub buffers: u32,
    pub size: [u32; 2],
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// One connection's buffer sets, by tab id, and its own sender.
pub(crate) struct Pools {
    tabs: HashMap<u64, (u64, Vec<Arc<DmabufBuffer>>)>,
    /// Last intentionally dropped pool id per tab (on hide/close), so an already-in-flight
    /// `DmabufFrame` for that pool replies `FrameRead` without falsely tripping `DmabufFailed`.
    forgotten: HashMap<u64, u64>,
    tx: Sender<HostMsg>,
}

impl Pools {
    pub(crate) fn new(tx: Sender<HostMsg>) -> Self {
        Self { tabs: HashMap::new(), forgotten: HashMap::new(), tx }
    }

    pub(crate) fn add(&mut self, info: PoolInfo, fds: Vec<OwnedFd>) {
        self.tabs.remove(&info.tab_id);
        self.forgotten.remove(&info.tab_id);
        if fds.len() != info.buffers as usize {
            return fail(&self.tx, format!("pool {} came with {} of {} descriptors", info.pool, fds.len(), info.buffers));
        }
        let buffers = fds.into_iter().map(|fd| {
            let plane = DmabufPlane { fd, offset: info.offset, stride: info.stride, modifier: info.modifier };
            DmabufBuffer::new(plane, info.size, DRM_FORMAT_ARGB8888).map(Arc::new)
        });
        match buffers.collect::<Result<Vec<_>, _>>() {
            Ok(buffers) => {
                self.tabs.insert(info.tab_id, (info.pool, buffers));
            }
            Err(reason) => fail(&self.tx, reason),
        }
    }

    /// Shows frame `frame_id`, in buffer `slot` of `pool`, on `surface`.
    pub(crate) fn show(&self, surface: Option<&TabSurface>, tab_id: u64, frame_id: u64, pool: u64, slot: u32) {
        let tx = self.tx.clone();
        // A tab the app already closed reads nothing: its buffer is free at once.
        let Some(surface) = surface else {
            tx.send(HostMsg::FrameRead { tab_id, pool, frame_id }).ok();
            return;
        };
        let Some(buffer) = self.tabs.get(&tab_id).filter(|(id, _)| *id == pool).and_then(|(_, b)| b.get(slot as usize)) else {
            if self.forgotten.get(&tab_id) == Some(&pool) {
                tx.send(HostMsg::FrameRead { tab_id, pool, frame_id }).ok();
                return;
            }
            return fail(&tx, format!("frame {frame_id} names buffer {slot} of pool {pool}, which the app doesn't have"));
        };
        let frame = Dmabuf::new(buffer.clone()).on_read(move |read| match read {
            Ok(()) => {
                tx.send(HostMsg::FrameRead { tab_id, pool, frame_id }).ok();
            }
            Err(reason) => fail(&tx, reason),
        });
        if let Err(reason) = surface.push_dmabuf(frame, frame_id) {
            fail(&self.tx, reason);
        }
    }

    /// The tab's buffers go while it hides (or switches to BGRA), keeping `forgotten` so an
    /// already-in-flight `DmabufFrame` for that pool replies `FrameRead` cleanly.
    pub(crate) fn forget(&mut self, tab_id: u64) {
        if let Some((pool_id, _)) = self.tabs.remove(&tab_id) {
            self.forgotten.insert(tab_id, pool_id);
        }
    }

    /// The tab closed permanently: drop both live buffers and any `forgotten` tombstone.
    pub(crate) fn close(&mut self, tab_id: u64) {
        self.tabs.remove(&tab_id);
        self.forgotten.remove(&tab_id);
    }
}
