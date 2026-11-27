//! The `wasm32` backend: the browser's own stores behind the same path-free API, as Desktop's web
//! build keeps them (`packages/app/src/runtime/persistence/{storage,drafts}.ts`).
//!
//! - `localStorage` holds `state` (`<name>:<key>`, as Desktop's web `persisted()` does), window
//!   frames, plugin overrides and trust. It's read once, synchronously, at open: the whole state
//!   cache is in memory before the first frame. State writes go through Desktop's `WriteBehind`
//!   machine (250 ms); frames, plugin and trust write at once (they're tiny and rare).
//! - `IndexedDB` (`opencode-rune-storage`, v1) holds documents, blobs, the outbox, session
//!   previews and caches. Requests before the database opens wait in order and run once it does.
//!   Unreferenced blobs are swept by Desktop's `DraftGc` machine.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;
use rune::machine::Seq;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{IdbDatabase, IdbRequest, IdbTransactionMode};

use crate::cache::{Doc, DocsIndex, OutboxCache};
use crate::machine::draft_gc::{self, BLOB_GRACE_MS, DraftGcEffect, DraftGcEvent, DraftGcState, STARTUP_COLLECT_DELAY_MS};
use crate::machine::write_behind::{self as wb, WriteBehindEffect, WriteBehindEvent, WriteBehindState};
use crate::{Cmd, FlushReason, FlushReport, OutboxRow, Storage, WindowFrame};

const DB: &str = "shuvcode-rune-storage";
const STORES: [&str; 5] = ["documents", "blobs", "outbox", "previews", "caches"];
const FRAMES: &str = "rune.window_state";
const PLUGINS: &str = "rune.plugin";
const TRUST: &str = "rune.plugin_trust";
const STATE_MS: u64 = 250;

/// What the open read.
#[derive(Clone, Debug, Default)]
pub struct BootReport {
    pub keys: usize,
    pub micros: f64,
    pub error: Option<String>,
    /// Never set on the web (the browser's storage has no file to move aside); native's twin.
    pub damaged: Option<std::path::PathBuf>,
    pub created: bool,
    pub journal_replayed: bool,
}

type StateKey = (String, String);
type Value = Option<Arc<str>>;

pub(crate) struct Web {
    ls: Option<web_sys::Storage>,
    pub(crate) idb: Idb,
    seq: Seq,
    state: WriteBehindState<StateKey, Value>,
    gc: DraftGcState,
    /// When this session last touched each blob (a blob put or referenced).
    pub(crate) touched: HashMap<String, i64>,
    pub(crate) cache: Arc<std::sync::Mutex<crate::cache::Cache>>,
}

/// The IndexedDB connection, as it opens.
pub(crate) enum Idb {
    Opening(Vec<Cmd>),
    Open(IdbDatabase),
    Failed,
}

pub(crate) struct Backend {
    web: Option<Rc<RefCell<Web>>>,
}

// SAFETY: wasm32-unknown-unknown has one thread; the Rc inside is never seen by another.
#[allow(unsafe_code)]
unsafe impl Send for Backend {}
// SAFETY: as above, one thread.
#[allow(unsafe_code)]
unsafe impl Sync for Backend {}

pub(crate) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

impl Storage {
    /// Opens the browser's stores: `localStorage` read into memory now, IndexedDB opening behind.
    pub fn open_web() -> (Storage, BootReport) {
        let start = js_sys::Date::now();
        let mut report = BootReport::default();
        let ls = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
        let mut cache = crate::cache::Cache { complete: true, docs_index: DocsIndex::Loading, ..Default::default() };
        let mut rows: BTreeMap<String, BTreeMap<String, Arc<str>>> = BTreeMap::new();
        if let Some(ls) = &ls {
            for i in 0..ls.length().unwrap_or(0) {
                let Some(full) = ls.key(i).ok().flatten() else { continue };
                let Some((name, key)) = full.split_once(':') else { continue };
                if !(name.starts_with("opencode.") || name.starts_with("plugin.") || name.starts_with("rune.")) {
                    continue;
                }
                let Some(value) = ls.get_item(&full).ok().flatten() else { continue };
                report.keys += 1;
                match name {
                    FRAMES => {
                        if let Some(f) = serde_json::from_str(&value).ok().as_ref().and_then(WindowFrame::from_json) {
                            cache.windows.insert(key.to_owned(), f);
                        }
                    }
                    PLUGINS => {
                        cache.plugins.insert(key.to_owned(), value == "1");
                    }
                    TRUST => {
                        cache.trust.insert(key.to_owned(), value);
                    }
                    _ => {
                        rows.entry(name.to_owned()).or_default().insert(key.to_owned(), value.into());
                    }
                }
            }
        }
        for (name, rows) in rows {
            cache.state.insert(name, crate::cache::Namespace::Loaded { rows, revision: 1 });
        }
        let cache = Arc::new(std::sync::Mutex::new(cache));
        let mut seq = Seq::default();
        let (gc, gc_effects) = draft_gc::initial(&mut seq, now_ms(), STARTUP_COLLECT_DELAY_MS);
        let web = Rc::new(RefCell::new(Web { ls, idb: Idb::Opening(Vec::new()), seq, state: WriteBehindState::Idle, gc, touched: HashMap::new(), cache: cache.clone() }));
        open_idb(&web);
        gc_run(&web, gc_effects.into_iter().collect());
        report.micros = (js_sys::Date::now() - start) * 1000.0;
        (Storage::from_parts(cache, Backend { web: Some(web) }), report)
    }
}

impl Backend {
    pub(crate) fn memory() -> Backend {
        Backend { web: None }
    }

    pub(crate) fn send(&self, cmd: Cmd) {
        match &self.web {
            Some(web) => handle(web, cmd),
            None => offline(cmd),
        }
    }

    pub(crate) fn flush(&self, _reason: FlushReason, _deadline: std::time::Duration) -> FlushReport {
        let Some(web) = &self.web else { return FlushReport { clean: true, ..FlushReport::default() } };
        if !wb::has_pending(&web.borrow().state) {
            return FlushReport { clean: true, ..FlushReport::default() };
        }
        let rows = state_event(web, WriteBehindEvent::FlushRequested);
        // IndexedDB writes were issued as they came; the browser finishes them on its own.
        FlushReport { clean: false, rows, ..FlushReport::default() }
    }

    pub(crate) fn first_frame(&self, done: Option<crate::Loaded>) {
        if let Some(done) = done {
            done();
        }
    }

    pub(crate) fn close(&self) -> FlushReport {
        self.flush(FlushReason::Quit, std::time::Duration::ZERO)
    }
}

fn offline(cmd: Cmd) {
    match cmd {
        Cmd::PluginRecord { done: Some(done), .. } => done(Ok(())),
        Cmd::Outbox { op, done } => crate::outbox::offline(op, done),
        Cmd::LoadState { done: Some(done), .. } | Cmd::LoadDocs { done: Some(done), .. } | Cmd::LoadOutbox { done: Some(done) } => done(),
        Cmd::GetBlob { done, .. } => done(None),
        Cmd::GetPreview { done, .. } => done(None),
        Cmd::GetCache { done, .. } => done(None),
        Cmd::CommitSoon => {}
        _ => {}
    }
}

fn ls_set(web: &Rc<RefCell<Web>>, key: &str, value: Option<&str>) -> Result<(), String> {
    let w = web.borrow();
    let Some(ls) = &w.ls else { return Ok(()) };
    match value {
        Some(v) => ls.set_item(key, v),
        None => ls.remove_item(key),
    }
    .map_err(|e| format!("{e:?}"))
}

fn handle(web: &Rc<RefCell<Web>>, cmd: Cmd) {
    match cmd {
        Cmd::State { name, key, value } => {
            state_event(web, WriteBehindEvent::Set { key: (name, key), value });
        }
        Cmd::ClearState { name } => {
            let n = name.clone();
            state_event(web, WriteBehindEvent::Drop(Box::new(move |k: &StateKey, _: &Value| k.0 == n)));
            let prefix = format!("{name}:");
            let keys: Vec<String> = {
                let w = web.borrow();
                let Some(ls) = &w.ls else { return };
                (0..ls.length().unwrap_or(0)).filter_map(|i| ls.key(i).ok().flatten()).filter(|k| k.starts_with(&prefix)).collect()
            };
            for k in keys {
                ls_set(web, &k, None).ok();
            }
        }
        Cmd::Window { id, frame } => {
            ls_set(web, &format!("{FRAMES}:{id}"), frame.map(|f| f.to_json().to_string()).as_deref()).ok();
        }
        Cmd::PluginRecord { id, record, done } => {
            let r = ls_set(web, &format!("{PLUGINS}:{id}"), record.enabled.map(|on| if on { "1" } else { "0" })).and_then(|()| ls_set(web, &format!("{TRUST}:{id}"), record.trusted.as_deref()));
            if let Some(done) = done {
                done(r.map_err(crate::StorageError));
            }
        }
        // Every namespace was read at open.
        Cmd::LoadState { names, done } => {
            drop(names);
            if let Some(done) = done {
                done();
            }
        }
        other => idb(web, other),
    }
}

/// Steps the state buffer; a batch is written to `localStorage` at once. Returns rows written.
fn state_event(web: &Rc<RefCell<Web>>, event: WriteBehindEvent<StateKey, Value>) -> usize {
    let effects: Vec<_> = {
        let mut w = web.borrow_mut();
        let Web { seq, state, .. } = &mut *w;
        wb::step(seq, state, event, STATE_MS).into_iter().collect()
    };
    let mut written = 0;
    for effect in effects {
        match effect {
            WriteBehindEffect::ArmTimer { attempt, delay_ms } => {
                let again = web.clone();
                later(delay_ms, move || {
                    state_event(&again, WriteBehindEvent::TimerFired { attempt });
                });
            }
            WriteBehindEffect::WriteBatch { attempt, batch } => {
                let result = write_state(web, &batch);
                written += batch.len();
                let event = match result {
                    Ok(()) => WriteBehindEvent::WriteSucceeded { attempt },
                    Err(error) => WriteBehindEvent::WriteFailed { attempt, error },
                };
                written += state_event(web, event);
            }
            WriteBehindEffect::ReportError(e) => web_sys::console::warn_1(&format!("opencode-storage: state write failed, retrying: {e}").into()),
            WriteBehindEffect::CancelTimer | WriteBehindEffect::RejectClosed => {}
        }
    }
    written
}

fn write_state(web: &Rc<RefCell<Web>>, batch: &IndexMap<StateKey, Value>) -> Result<(), String> {
    for ((name, key), value) in batch {
        ls_set(web, &format!("{name}:{key}"), value.as_deref())?;
    }
    Ok(())
}

/// Runs `f` after `ms` on the page's event loop. A stale timer is fenced by its machine's attempt.
fn later(ms: u64, f: impl FnOnce() + 'static) {
    let Some(window) = web_sys::window() else { return };
    let callback = Closure::once_into_js(f);
    window.set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), i32::try_from(ms).unwrap_or(i32::MAX)).ok();
}

pub(crate) fn on_success(req: &IdbRequest, f: impl FnOnce(JsValue) + 'static) {
    let target = req.clone();
    let callback = Closure::once_into_js(move || f(target.result().unwrap_or(JsValue::UNDEFINED)));
    req.set_onsuccess(Some(callback.unchecked_ref()));
}

fn open_idb(web: &Rc<RefCell<Web>>) {
    let Some(factory) = web_sys::window().and_then(|w| w.indexed_db().ok().flatten()) else {
        web.borrow_mut().idb = Idb::Failed;
        return;
    };
    let Ok(req) = factory.open_with_u32(DB, 1) else {
        web.borrow_mut().idb = Idb::Failed;
        return;
    };
    let upgrade = req.clone();
    let on_upgrade = Closure::once_into_js(move || {
        let Ok(db) = upgrade.result().and_then(|r| r.dyn_into::<IdbDatabase>()) else { return };
        for store in STORES {
            if !db.object_store_names().contains(store) {
                db.create_object_store(store).ok();
            }
        }
    });
    req.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));
    let failed = web.clone();
    let on_error = Closure::once_into_js(move || {
        let queued = std::mem::replace(&mut failed.borrow_mut().idb, Idb::Failed);
        if let Idb::Opening(cmds) = queued {
            cmds.into_iter().for_each(offline);
        }
    });
    req.set_onerror(Some(on_error.unchecked_ref()));
    let opened = web.clone();
    on_success(&req, move |result| {
        let Ok(db) = result.dyn_into::<IdbDatabase>() else { return };
        let queued = std::mem::replace(&mut opened.borrow_mut().idb, Idb::Open(db));
        index_documents(&opened);
        if let Idb::Opening(cmds) = queued {
            for cmd in cmds {
                idb(&opened, cmd);
            }
        }
    });
}

/// The document keys, so a lookup of one that isn't stored answers at once.
fn index_documents(web: &Rc<RefCell<Web>>) {
    let Some(store) = store(web, "documents", IdbTransactionMode::Readonly) else { return };
    let Ok(req) = store.get_all_keys() else { return };
    let cache = web.borrow().cache.clone();
    on_success(&req, move |keys| {
        let mut c = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for k in js_sys::Array::from(&keys).iter().filter_map(|k| k.as_string()) {
            c.docs.entry(k).or_insert(Doc::Stored);
        }
        c.docs_index = DocsIndex::Ready;
    });
}

fn store(web: &Rc<RefCell<Web>>, name: &str, mode: IdbTransactionMode) -> Option<web_sys::IdbObjectStore> {
    let w = web.borrow();
    let Idb::Open(db) = &w.idb else { return None };
    db.transaction_with_str_and_mode(name, mode).ok()?.object_store(name).ok()
}

fn put(web: &Rc<RefCell<Web>>, name: &str, key: &str, value: Option<&JsValue>, done: Option<crate::Done>) {
    let req = store(web, name, IdbTransactionMode::Readwrite).and_then(|s| match value {
        Some(v) => s.put_with_key(v, &JsValue::from_str(key)).ok(),
        None => s.delete(&JsValue::from_str(key)).ok(),
    });
    match (req, done) {
        (Some(req), Some(done)) => {
            let failed = std::rc::Rc::new(RefCell::new(None::<crate::Done>));
            *failed.borrow_mut() = Some(done);
            let ok = failed.clone();
            on_success(&req, move |_| {
                if let Some(done) = ok.borrow_mut().take() {
                    done(Ok(()));
                }
            });
            let store_name = name.to_owned();
            let on_error = Closure::once_into_js(move || {
                let name = store_name;
                if let Some(done) = failed.borrow_mut().take() {
                    done(Err(crate::StorageError(format!("IndexedDB {name} write failed"))));
                }
            });
            req.set_onerror(Some(on_error.unchecked_ref()));
        }
        (None, Some(done)) => done(Err(crate::StorageError("IndexedDB is unavailable".into()))),
        _ => {}
    }
}

fn get(web: &Rc<RefCell<Web>>, name: &str, key: &str, f: impl FnOnce(JsValue) + 'static) {
    match store(web, name, IdbTransactionMode::Readonly).and_then(|s| s.get(&JsValue::from_str(key)).ok()) {
        Some(req) => on_success(&req, f),
        None => f(JsValue::UNDEFINED),
    }
}

pub(crate) fn bytes(v: &JsValue) -> Option<Vec<u8>> {
    v.dyn_ref::<js_sys::Uint8Array>().map(js_sys::Uint8Array::to_vec)
}

fn idb(web: &Rc<RefCell<Web>>, cmd: Cmd) {
    if let Idb::Opening(queue) = &mut web.borrow_mut().idb {
        queue.push(cmd);
        return;
    }
    if matches!(web.borrow().idb, Idb::Failed) {
        offline(cmd);
        return;
    }
    let now = now_ms();
    match cmd {
        Cmd::Doc { key, value, .. } => {
            put(web, "documents", &key, value.map(|v| JsValue::from_str(&v)).as_ref(), None);
            let effects = {
                let mut w = web.borrow_mut();
                let Web { seq, gc, .. } = &mut *w;
                draft_gc::step(seq, gc, DraftGcEvent::DocumentsWritten { at_ms: now })
            };
            gc_run(web, effects.into_iter().collect());
        }
        Cmd::Blob { id, data } => {
            put(web, "blobs", &id, Some(&js_sys::Uint8Array::from(&*data).into()), None);
            web.borrow_mut().touched.insert(id, now);
            let effects = {
                let mut w = web.borrow_mut();
                let Web { seq, gc, .. } = &mut *w;
                draft_gc::step(seq, gc, DraftGcEvent::BlobUploaded { at_ms: now })
            };
            gc_run(web, effects.into_iter().collect());
        }
        Cmd::Outbox { op, done } => crate::outbox::web::handle(web, op, done),
        Cmd::Preview { server, session, value } => {
            let key = format!("{server}\n{session}");
            let v = value.map(|(at, data)| {
                let mut b = at.to_le_bytes().to_vec();
                b.extend_from_slice(&data);
                JsValue::from(js_sys::Uint8Array::from(&b[..]))
            });
            put(web, "previews", &key, v.as_ref(), None);
        }
        Cmd::Cache { key, value } => {
            let v = value.map(|(version, data)| {
                let mut b = version.to_le_bytes().to_vec();
                b.extend_from_slice(&data);
                JsValue::from(js_sys::Uint8Array::from(&b[..]))
            });
            put(web, "caches", &key, v.as_ref(), None);
        }
        Cmd::LoadDocs { keys, done } => {
            let left = Rc::new(RefCell::new((keys.len(), done)));
            if keys.is_empty()
                && let Some(done) = left.borrow_mut().1.take()
            {
                done();
            }
            for key in keys {
                let (cache, left) = (web.borrow().cache.clone(), left.clone());
                let k = key.clone();
                get(web, "documents", &key, move |v| {
                    if let Some(text) = v.as_string() {
                        let mut c = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if !matches!(c.docs.get(&k), Some(Doc::Present(_))) {
                            c.docs.insert(k, Doc::Present(text.into()));
                        }
                    }
                    let mut l = left.borrow_mut();
                    l.0 -= 1;
                    if l.0 == 0
                        && let Some(done) = l.1.take()
                    {
                        done();
                    }
                });
            }
        }
        Cmd::LoadOutbox { done } => {
            let cache = web.borrow().cache.clone();
            let req = store(web, "outbox", IdbTransactionMode::Readonly).and_then(|s| s.get_all().ok());
            let finish = move |all: JsValue| {
                let mut rows: BTreeMap<String, Vec<OutboxRow>> = BTreeMap::new();
                for v in js_sys::Array::from(&all).iter().filter_map(|v| v.as_string()) {
                    if let Some(row) = outbox_from(&v) {
                        rows.entry(row.server.clone()).or_default().push(row);
                    }
                }
                rows.values_mut().for_each(|l| l.sort_by_key(|r| r.seq));
                let mut c = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(c.outbox, OutboxCache::Unloaded) {
                    c.outbox = OutboxCache::Loaded(rows);
                }
                drop(c);
                if let Some(done) = done {
                    done();
                }
            };
            match req {
                Some(req) => on_success(&req, finish),
                None => finish(JsValue::UNDEFINED),
            }
        }
        Cmd::GetBlob { id, done } => get(web, "blobs", &id, move |v| done(bytes(&v))),
        Cmd::GetPreview { server, session, done } => get(web, "previews", &format!("{server}\n{session}"), move |v| {
            done(bytes(&v).filter(|b| b.len() >= 8).map(|b| (i64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8])), b[8..].to_vec())))
        }),
        Cmd::GetCache { key, version, done } => get(web, "caches", &key, move |v| {
            done(bytes(&v).filter(|b| b.len() >= 8 && i64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8])) == version).map(|b| b[8..].to_vec()))
        }),
        other => offline(other),
    }
}

pub(crate) fn outbox_json(row: &OutboxRow) -> String {
    serde_json::json!({
        "server": row.server, "id": row.id, "seq": row.seq, "session": row.session, "state": row.state,
        "operation_json": row.operation_json, "draft_json": row.draft_json, "prompt_json": row.prompt_json,
        "attachments": row.attachments, "paths_json": row.paths_json, "inbox": row.inbox, "why": row.why,
        "after_check": row.after_check, "restored": row.restored, "durable": row.durable, "legacy_json": row.legacy_json,
    })
    .to_string()
}

pub(crate) fn outbox_from(text: &str) -> Option<OutboxRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    Some(OutboxRow {
        server: s("server"),
        id: s("id"),
        seq: v["seq"].as_i64().unwrap_or(0),
        session: s("session"),
        state: s("state"),
        operation_json: s("operation_json"),
        draft_json: s("draft_json"),
        prompt_json: v["prompt_json"].as_str().map(str::to_owned),
        attachments: serde_json::from_value(v["attachments"].clone()).unwrap_or_default(),
        paths_json: s("paths_json"),
        inbox: v["inbox"].as_bool().unwrap_or(false),
        why: s("why"),
        after_check: v["after_check"].as_str().map(str::to_owned),
        restored: v["restored"].as_bool().unwrap_or(false),
        durable: v["durable"].as_bool().unwrap_or(true),
        legacy_json: v["legacy_json"].as_str().map(str::to_owned),
    })
}

/// Carries out the blob sweep's effects. A sweep deletes the blobs no document or outbox row
/// mentions and this session hasn't touched within the grace period.
fn gc_run(web: &Rc<RefCell<Web>>, effects: Vec<DraftGcEffect>) {
    for effect in effects {
        match effect {
            DraftGcEffect::ArmTimer { attempt, delay_ms } => {
                let again = web.clone();
                later(u64::try_from(delay_ms).unwrap_or(0), move || {
                    let effects = {
                        let mut w = again.borrow_mut();
                        let Web { seq, gc, .. } = &mut *w;
                        draft_gc::step(seq, gc, DraftGcEvent::TimerFired { attempt, at_ms: now_ms() })
                    };
                    gc_run(&again, effects.into_iter().collect());
                });
            }
            DraftGcEffect::Collect { attempt, at_ms, before_ms } => collect(web, attempt, at_ms, before_ms),
            DraftGcEffect::ReportError(e) => web_sys::console::warn_1(&format!("opencode-storage: blob sweep failed: {e}").into()),
            DraftGcEffect::CancelTimer => {}
        }
    }
}

fn collect(web: &Rc<RefCell<Web>>, attempt: rune::machine::Attempt, at_ms: i64, before_ms: i64) {
    let reqs = (
        store(web, "documents", IdbTransactionMode::Readonly).and_then(|s| s.get_all().ok()),
        store(web, "outbox", IdbTransactionMode::Readonly).and_then(|s| s.get_all().ok()),
        store(web, "blobs", IdbTransactionMode::Readonly).and_then(|s| s.get_all_keys().ok()),
    );
    let (Some(docs), Some(outbox), Some(blobs)) = reqs else {
        let effects = {
            let mut w = web.borrow_mut();
            let Web { seq, gc, .. } = &mut *w;
            draft_gc::step(seq, gc, DraftGcEvent::CollectFailed { attempt, at_ms, error: "IndexedDB is unavailable".into() })
        };
        gc_run(web, effects.into_iter().collect());
        return;
    };
    let text = Rc::new(RefCell::new(String::new()));
    let (t1, t2) = (text.clone(), text.clone());
    on_success(&docs, move |all| js_sys::Array::from(&all).iter().filter_map(|v| v.as_string()).for_each(|s| t1.borrow_mut().push_str(&s)));
    on_success(&outbox, move |all| js_sys::Array::from(&all).iter().filter_map(|v| v.as_string()).for_each(|s| t2.borrow_mut().push_str(&s)));
    let web = web.clone();
    // IndexedDB answers requests in the order they were made, so the keys come last.
    on_success(&blobs, move |keys| {
        let referenced = text.borrow();
        let (stale, oldest): (Vec<String>, Option<i64>) = {
            let w = web.borrow();
            let mut oldest: Option<i64> = None;
            let stale = js_sys::Array::from(&keys)
                .iter()
                .filter_map(|k| k.as_string())
                .filter(|id| !referenced.contains(id.as_str()))
                .filter(|id| match w.touched.get(id) {
                    Some(t) if *t >= before_ms => {
                        oldest = Some(oldest.map_or(*t, |o: i64| o.min(*t)));
                        false
                    }
                    _ => true,
                })
                .collect();
            (stale, oldest)
        };
        for id in &stale {
            put(&web, "blobs", id, None, None);
        }
        let effects = {
            let mut w = web.borrow_mut();
            w.touched.retain(|_, t| *t >= at_ms - BLOB_GRACE_MS);
            let Web { seq, gc, .. } = &mut *w;
            draft_gc::step(seq, gc, DraftGcEvent::CollectSucceeded { attempt, at_ms, oldest_orphan_ms: oldest })
        };
        gc_run(&web, effects.into_iter().collect());
    });
}

/// The browser's stores need no configuration: [`StorageConfig::app`] is the one way to ask for them.
pub struct StorageConfig(());

impl StorageConfig {
    /// The app's storage ([`crate::AppStorage`]): in the browser, its own stores; the folders
    /// and the import mean nothing here.
    pub fn app(_app: crate::AppStorage) -> Option<StorageConfig> {
        Some(StorageConfig(()))
    }

    /// The native open's packet hook: here the packet is ready at once.
    pub fn on_packet(self, _sent: fn()) -> StorageConfig {
        self
    }

    /// The native open's wait for the single-instance claim: one tab, nothing to wait for.
    pub fn after_claim(self) -> StorageConfig {
        self
    }

    /// The native open's early cache read; the browser keeps no cache rows yet, so nothing is
    /// offered.
    pub fn early_cache(self, _key: &'static str, _version: i64, _offer: impl FnOnce(std::sync::Arc<[u8]>) + Send + 'static) -> StorageConfig {
        self
    }
}

/// The open, as the native backend stages it: here it's done at once (the state is read from
/// `localStorage` synchronously), so the boot packet and the storage are both ready.
pub struct Opening {
    boot: crate::BootPacket,
    ready: Option<(Storage, BootReport)>,
}

impl Opening {
    pub fn boot(&self) -> crate::BootPacket {
        self.boot.clone()
    }

    pub fn try_storage(&mut self) -> Option<(Storage, BootReport)> {
        self.ready.take()
    }

    pub fn proceed(&self) {}

    pub fn storage(mut self) -> (Storage, BootReport) {
        self.ready.take().unwrap_or_else(|| (Storage::memory(), BootReport { error: Some("the storage was taken already".into()), ..BootReport::default() }))
    }
}

impl Storage {
    /// [`Storage::open_web`], as an [`Opening`] whose packet is built from what it read.
    pub fn open_async(_config: StorageConfig) -> Opening {
        let (storage, report) = Storage::open_web();
        let boot = crate::boot_packet::from_cache(&storage.cache());
        Opening { boot, ready: Some((storage, report)) }
    }
}

/// Desktop's databases aren't reachable from a browser.
pub fn desktop_recent_tabs(_dirs: &[std::path::PathBuf]) -> Option<(serde_json::Value, serde_json::Value)> {
    None
}
