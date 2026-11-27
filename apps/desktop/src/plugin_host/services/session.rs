//! Sessions, Models, Server (its cached reads and events; requests are in `net`), Servers and
//! Theme: what a render reads of the app's data.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use mlua::{Lua, Table, Value};
use rune::Tx;

use super::{lazy_signals, method, record, snapshot, with_app};
use crate::App;
use crate::plugin_host::cx::{self, Dep};
use crate::plugin_host::machines::{Fetched, TicketEffect, TicketEvent};
use opencode_plugin_runtime::luau::{resource_value, split_items, version_hash};
pub use opencode_plugin_runtime::luau::encode_uri;
use crate::plugin_host::vm::Vmx;

pub fn build(lua: &Lua, vmx: &Rc<Vmx>, name: &str, t: &Table) -> mlua::Result<()> {
    match name {
        "Sessions" => sessions(lua, vmx, t),
        "Models" => models(lua, vmx, t),
        "Server" => server(lua, vmx, t),
        "Servers" => servers(lua, vmx, t),
        _ => theme(lua, vmx, t),
    }
}

fn sessions(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    method(lua, t, "Get", |lua, id: String| {
        cx::read(Dep::Session(id.clone()));
        with_app("Sessions:Get", |app| session_value(lua, app, &id))
    })?;
    let window = vmx.half.kind == crate::plugin_host::plugin::Kind::Window;
    method(lua, t, "Routed", move |lua, ()| {
        cx::read(Dep::Routed);
        with_app("Sessions:Routed", |app| match app.active_session_id().filter(|_| window) {
            Some(id) => {
                cx::read(Dep::Session(id.to_owned()));
                session_value(lua, app, id)
            }
            None => Ok(Value::Nil),
        })
    })?;
    method(lua, t, "Open", |lua, ()| {
        cx::read(Dep::Sessions);
        with_app("Sessions:Open", |app| {
            let list = lua.create_table()?;
            let active = crate::servers::runtime::active(app).clone();
            for tab in app.tabs.iter().filter(|t| t.kind == crate::tabs::TabKind::Session) {
                // A tab of another server is what the window last read of it (its title and folder,
                // its mark): the window works with one server at a time.
                let value = if tab.server == active { session_value(lua, app, &tab.id)? } else { other_server_session(lua, app, tab)? };
                if value != Value::Nil {
                    list.raw_push(value)?;
                }
            }
            Ok(list)
        })
    })?;
    lazy_signals(lua, vmx, t, "Sessions", &["Changed", "RoutedChanged"])
}

/// A tracked version belongs to one cell, not to all servers or windows at that version.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp(pub u32, pub u64);

impl Stamp {
    pub(super) fn of<T>(cell: &rune::Tracked<T>) -> Self {
        Self(rune::Tracked::id(cell), rune::Tracked::version(cell))
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct UsageInputs {
    server: opencode_kit::servers::ServerKey,
    transcripts: Stamp,
    window: Stamp,
    revert: Option<String>,
}

/// Each session's last step with token usage, and the inputs it was read from.
pub(crate) type CachedUsage = (UsageInputs, Option<usize>);

/// Session `id`'s step whose tokens fill the context window (`session_view::usage`).
fn last_usage<'a>(app: &'a App, id: &str) -> Option<(&'a opencode_kit::model::Assistant, &'a opencode_kit::model::Tokens)> {
    let info = app.session(id)?;
    let t = rune::Tracked::untracked(&app.transcripts).get(&info.id)?;
    let key = UsageInputs {
        server: crate::servers::runtime::active(app).clone(),
        transcripts: Stamp::of(&app.transcripts),
        window: Stamp::of(&app.context_windows),
        revert: info.revert.clone(),
    };
    if let Some((_, at)) = app.plugins.last_usage.borrow().get(id).filter(|(k, _)| *k == key) {
        return session_view::usage::at(t, (*at)?);
    }
    let at = session_view::usage::last_with_usage_at(t, info.revert.as_deref(), |m| app.holds(id, m));
    app.plugins.last_usage.borrow_mut().insert(id.to_owned(), (key, at));
    session_view::usage::at(t, at?)
}

/// Session `info`'s project ID as Desktop's `MountedSession.project` resolves it
/// (`mounted-session.ts:297-301`, `session/model.ts:129-137`): a listed project matching the
/// session (`project_for_session`), else a non-`"global"` raw `projectID`.
fn session_project_id<'a>(app: &'a App, info: &'a opencode_kit::model::SessionInfo) -> Option<&'a str> {
    app.project_for_session(info)
        .map(|p| p.id.as_str())
        .or_else(|| (!info.project.is_empty() && info.project != "global").then_some(info.project.as_str()))
}

/// HTTP status code of `t`'s last assistant reply failure, when it failed with one.
fn last_error_status(t: Option<&opencode_kit::model::Transcript>) -> Option<u16> {
    t?.messages
        .iter()
        .rev()
        .find_map(|m| match m {
            opencode_kit::model::Message::Assistant(a) => Some(a),
            _ => None,
        })
        .and_then(|a| a.failure.as_ref())
        .and_then(|e| e.status)
}

/// A session's version, as plugins see it: it moves when its title, cost, usage, message count
/// or busy state does, and not while a reply streams within a step.
/// `watch::SessionInputs` tracks the shared inputs. Only this session reads its transcript.
pub fn session_version(app: &App, id: &str) -> u64 {
    let Some(info) = app.session(id) else { return 0 };
    let t = rune::Tracked::untracked(&app.transcripts).get(&info.id);
    let last = last_usage(app, id);
    let cost = app.session_cost(id);
    let messages = t.map(|t| t.messages.len());
    let usage = last.map(|(_, k)| (k.input, k.output, k.reasoning, k.cache_read, k.cache_write));
    let mut h = std::hash::DefaultHasher::new();
    (&info.title, info.updated, cost.to_bits(), messages, last_error_status(t), app.subtree_running(&info.id), usage, last.map(|(a, _)| (&a.id, a.completed)), (&info.model.provider, &info.model.id, &info.model.variant, &info.agent, session_project_id(app, info), app.held(id))).hash(&mut h);
    // The Context tab lists the inbox's waiting input too.
    messages_version(app, id).hash(&mut h);
    h.finish()
}

/// What moves when session `id`'s newest messages or its inbox do, and not as Rune reads older
/// pages or the reader scrolls: its newest message and whether it finished, the inbox's waiting
/// input, and the server's last change.
fn messages_version(app: &App, id: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    app.session(id).map(|i| i.updated).hash(&mut h);
    if let Some(m) = rune::Tracked::untracked(&app.transcripts).get(id).and_then(|t| t.messages.last()) {
        m.id().hash(&mut h);
        if let opencode_kit::model::Message::Assistant(a) = m {
            a.completed.hash(&mut h);
        }
    }
    if let Some(inbox) = app.inboxes.get(id) {
        inbox.items.iter().for_each(|i| i.id.hash(&mut h));
        inbox.waiting_notices().for_each(|n| n.id.hash(&mut h));
    }
    h.finish()
}

/// Session `id` as a `Session` record, kept per VM while its version and the server hold.
fn session_value(lua: &Lua, app: &App, id: &str) -> mlua::Result<Value> {
    let mut h = std::hash::DefaultHasher::new();
    (session_version(app, id), &app.server_key()).hash(&mut h);
    snapshot(lua, &format!("session:{}:{id}", server_id(crate::servers::runtime::active(app))), h.finish(), || build_session(lua, app, id))
}

/// A server's key as plugins name it (`ServerInfo.key`): `local` for the app's own (its
/// `ServerScope`), else its key.
pub fn server_id(key: &opencode_kit::servers::ServerKey) -> String {
    key.scope().as_str().to_owned()
}

/// Tab `tab`'s session on a server the window isn't working with: its saved title and folder,
/// busy while its mark says so.
fn other_server_session(lua: &Lua, app: &App, tab: &crate::tabs::Tab) -> mlua::Result<Value> {
    let busy = app.checkout.snap(&tab.server, &tab.id) == Some(crate::TabStatus::Busy);
    let mut h = std::hash::DefaultHasher::new();
    (&tab.title, &tab.directory, busy).hash(&mut h);
    let version = h.finish();
    snapshot(lua, &format!("session:{}:{}", tab.server, tab.id), version, || {
        record(lua, 6, |s| {
            s.str(c"id", &tab.id);
            s.str(c"serverId", &server_id(&tab.server));
            s.str(c"directory", &tab.directory);
            s.str(c"title", &tab.title);
            s.bool(c"busy", busy);
            s.num(c"version", (version >> 11) as f64);
            s.num(c"cost", 0.0);
        })
        .map(Value::Table)
    })
}

fn build_session(lua: &Lua, app: &App, id: &str) -> mlua::Result<Value> {
    let Some(info) = app.session(id) else { return Ok(Value::Nil) };
    let t = rune::Tracked::untracked(&app.transcripts).get(&info.id);
    let last = last_usage(app, id);
    let busy = app.subtree_running(&info.id);
    let version = (session_version(app, id) >> 11) as f64;
    let cost = app.session_cost(id);
    let held = app.held(id);
    let s = record(lua, 25, |s| {
        s.str(c"id", &info.id);
        s.str(c"serverId", &server_id(crate::servers::runtime::active(app)));
        s.str(c"directory", &info.directory);
        s.str(c"title", &info.title);
        s.opt_str(c"parentId", info.parent.as_deref());
        s.opt_str(c"projectId", session_project_id(app, info));
        s.opt_str(c"agent", (!info.agent.is_empty()).then_some(info.agent.as_str()));
        s.opt_str(c"variant", (!info.model.variant.is_empty() && info.model.variant != "default").then_some(info.model.variant.as_str()));
        s.bool(c"busy", busy);
        s.num(c"version", version);
        s.num(c"messagesVersion", (messages_version(app, &info.id) >> 11) as f64);
        s.num(c"cost", cost);
        if let Some((a, k)) = last {
            s.record(c"usage", 5, |u| {
                u.num(c"input", k.input as f64);
                u.num(c"output", k.output as f64);
                u.num(c"reasoning", k.reasoning as f64);
                u.num(c"cacheRead", k.cache_read as f64);
                u.num(c"cacheWrite", k.cache_write as f64);
            });
            s.str(c"usageMessageId", &a.id);
            if a.created > 0 {
                s.num(c"usageCreated", a.created as f64);
            }
            if let Some(m) = a.model.as_ref() {
                s.str(c"usageProviderId", &m.provider);
                s.str(c"usageModelId", &m.id);
            }
        }
        if !info.model.id.is_empty() {
            s.str(c"providerId", &info.model.provider);
            s.str(c"modelId", &info.model.id);
        }
        match held {
            Some(crate::context_window::Held::From(start)) => s.str(c"windowStart", start),
            Some(crate::context_window::Held::Unknown) => s.str(c"windowStart", ""),
            None => {}
        }
        s.num(c"created", info.created as f64);
        s.num(c"updated", info.updated as f64);
        if let Some(t) = t {
            s.num(c"messages", t.messages.len() as f64);
        }
        if let Some(status) = last_error_status(t) {
            s.num(c"lastErrorStatus", f64::from(status));
        }
    })?;
    Ok(Value::Table(s))
}

/// What the full model and provider catalog reads now, as a number (`Models.Changed`, `Dep::Models`).
pub fn models_version(app: &App) -> u64 {
    let p = &app.composer_ext.pickers;
    let mut h = std::hash::DefaultHasher::new();
    (p.models.len(), p.limits.len(), p.paid.len(), p.providers.len()).hash(&mut h);
    for m in &p.models {
        (&m.provider, &m.id, &m.name, m.free).hash(&mut h);
    }
    if let Some(infos) = app.provider_infos(None) {
        infos.len().hash(&mut h);
        for info in infos {
            (&info.id, &info.integration).hash(&mut h);
        }
    }
    h.finish()
}

/// What a model's record reads now, as a number.
pub fn model_version(app: &App, provider: &str, model: &str) -> u64 {
    let p = &app.composer_ext.pickers;
    let key = format!("{provider}:{model}");
    let mut h = std::hash::DefaultHasher::new();
    (p.models.iter().find(|m| m.provider == provider && m.id == model).map(|m| &m.name), p.providers.get(provider), p.limits.get(&key), p.paid.contains(&key)).hash(&mut h);
    h.finish()
}

fn models(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    method(lua, t, "Get", |lua, (provider, model): (String, String)| {
        let m = with_app("Models:Get", |app| {
            let key = format!("{provider}:{model}");
            let mut h = std::hash::DefaultHasher::new();
            (model_version(app, &provider, &model), app.composer_ext.pickers.models.iter().any(|m| m.provider == provider)).hash(&mut h);
            // Each server has its own catalog.
            snapshot(lua, &format!("model:{}:{key}", server_id(crate::servers::runtime::active(app))), h.finish(), || {
                let p = &app.composer_ext.pickers;
                let listed = p.models.iter().find(|m| m.provider == provider && m.id == model);
                let provider_name = p.providers.get(&provider).map(String::as_str).or_else(|| p.models.iter().any(|m| m.provider == provider).then_some(provider.as_str()));
                record(lua, 4, |m| {
                    m.opt_str(c"providerName", provider_name);
                    m.opt_str(c"modelName", listed.map(|m| m.name.as_str()));
                    if let Some(l) = p.limits.get(&key) {
                        m.num(c"contextLimit", *l as f64);
                    }
                    m.bool(c"paid", p.paid.contains(&key));
                })
                .map(Value::Table)
            })
        })?;
        cx::read(Dep::Model(provider, model));
        Ok(m)
    })?;
    method(lua, t, "List", |lua, ()| {
        let list = with_app("Models:List", |app| {
            let ver = models_version(app);
            snapshot(lua, &format!("models:{}", server_id(crate::servers::runtime::active(app))), ver, || {
                let p = &app.composer_ext.pickers;
                let infos = app.provider_infos(None).unwrap_or_default();
                let out = lua.create_table_with_capacity(p.models.len(), 0)?;
                for m in &p.models {
                    let provider_name = p.providers.get(&m.provider).map(String::as_str);
                    let integration = infos.iter().find(|i| i.id == m.provider).and_then(|i| i.integration.as_deref());
                    let paid = p.paid.contains(&m.key);
                    out.raw_push(record(lua, 7, |e| {
                        e.str(c"providerId", &m.provider);
                        e.str(c"modelId", &m.id);
                        e.str(c"name", &m.name);
                        e.opt_str(c"providerName", provider_name);
                        e.opt_str(c"integration", integration);
                        e.bool(c"free", m.free);
                        e.bool(c"paid", paid);
                    })?)?;
                }
                Ok(Value::Table(out))
            })
        })?;
        cx::read(Dep::Models);
        Ok(list)
    })?;
    let window = vmx.half.kind == crate::plugin_host::plugin::Kind::Window;
    let v = vmx.clone();
    method(lua, t, "SetBadges", move |_, badges: Table| {
        if !window {
            return Err(mlua::Error::runtime("Models:SetBadges is only available in the window half"));
        }
        let mut map = std::collections::HashMap::new();
        for item in badges.sequence_values::<Table>() {
            let item = item?;
            let provider: String = item.raw_get("providerId")?;
            let model: String = item.raw_get("modelId")?;
            let label: String = item.raw_get("label")?;
            let critical: bool = item.raw_get::<Option<bool>>("critical")?.unwrap_or(false);
            if !provider.is_empty() && !model.is_empty() && !label.is_empty() {
                map.insert(format!("{provider}:{model}"), (label, critical));
            }
        }
        cx::ask(&v.half.id, cx::Request::SetModelBadges { vm: v.vm.get(), badges: map });
        Ok(())
    })?;
    lazy_signals(lua, vmx, t, "Models", &["Changed"])
}

fn server(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    let plugin = vmx.half.id.clone();
    let half = vmx.half.clone();
    method(lua, t, "Resource", move |lua, (path, version, items): (String, Value, Option<bool>)| {
        let items = items == Some(true);
        with_app("Server:Resource", |app| {
            // Each server's reads are cached apart: a panel in a tab from another server reads that
            // server's answer, never this one's.
            let key = resource_key(crate::servers::runtime::active(app), &path);
            cx::read(Dep::Resource(plugin.clone(), key.clone()));
            let version = version_hash(&version);
            let mut map = half.resources.borrow_mut();
            let r = map.entry(key.clone()).or_default();
            let (ticket, effects) = std::mem::take(&mut r.ticket).step(TicketEvent::Ask(version));
            r.ticket = ticket;
            for TicketEffect::Fetch(asked) in effects {
                let (plugin, path, key) = (plugin.to_string(), path.clone(), key.clone());
                app.job(move |srv| {
                    // A server still loading its plugins answers `/api/skill` with 503 for a while.
                    let answer = srv.get_text_ready(&path).map(|text| {
                        // Split off the UI thread: a long list's JSON is too much to decode in a frame.
                        let items = if items { split_items(&text) } else { None };
                        Fetched { text, items }
                    }).map_err(|e| e.to_string());
                    Box::new(move |s: &mut App, tx: &mut Tx| land(s, &plugin, &key, asked, answer, tx)) as crate::jobs::Apply
                });
            }
            resource_value(lua, r)
        })
    })?;
    let half = vmx.half.clone();
    method(lua, t, "Invalidate", move |_, prefix: String| {
        half.resources.borrow_mut().retain(|key, _| !key.split_once('\n').map_or(key.as_str(), |(_, path)| path).starts_with(&prefix));
        crate::plugin_host::bump();
        Ok(())
    })?;
    let v = vmx.clone();
    method(lua, t, "OnEvent", move |lua, prefix: String| {
        let s: Value = v.script_signal.call(())?;
        v.half.events.borrow_mut().push((v.vm.get(), prefix, s.clone()));
        let _ = lua;
        Ok(s)
    })?;
    method(lua, t, "EncodeUri", |_, text: String| Ok(encode_uri(&text)))?;
    // The window half's server is the one it works with now; the app half's, the app's own.
    let window = vmx.half.kind == crate::plugin_host::plugin::Kind::Window;
    method(lua, t, "Version", move |_, ()| {
        cx::read(Dep::Server);
        cx::read(Dep::Servers);
        with_app("Server:Version", |app| {
            let own = app.plugins.shared.facts.server_version.clone();
            let active = crate::servers::runtime::active(app);
            if !window || active.is_sidecar() {
                return Ok(own);
            }
            Ok(opencode_servers::health::version(crate::servers::health::get(active).as_ref()).map(str::to_owned))
        })
    })?;
    method(lua, t, "IsLocal", move |_, ()| {
        cx::read(Dep::Server);
        with_app("Server:IsLocal", |app| {
            if !window {
                return Ok(crate::servers::runtime::own_server().is_some_and(|s| s.is_local()));
            }
            Ok(matches!(&*app.source, crate::Source::Live { server, .. } if server.is_local()))
        })
    })?;
    method(lua, t, "IsLive", move |_, ()| {
        cx::read(Dep::Server);
        with_app("Server:IsLive", |app| {
            if !window {
                return Ok(crate::servers::runtime::own_server().is_some());
            }
            Ok(app.live())
        })
    })?;
    method(lua, t, "CanWrite", |_, ()| {
        cx::read(Dep::Server);
        with_app("Server:CanWrite", |app| Ok(app.can_write()))
    })?;
    super::async_method(lua, vmx, t, "RequestAsync", "Server.RequestAsync")?;
    lazy_signals(lua, vmx, t, "Server", &["Connected"])
}

/// A cached read's key: its server's, then its path (`(ServerKey, path)`).
fn resource_key(server: &opencode_kit::servers::ServerKey, path: &str) -> String {
    format!("{server}\n{path}")
}

fn land(app: &mut App, plugin: &str, key: &str, version: u64, answer: Result<Fetched, String>, tx: &mut Tx) {
    let Some(half) = app.plugins.half(plugin) else { return };
    let mut map = half.resources.borrow_mut();
    let Some(r) = map.get_mut(key) else { return };
    let before = r.ticket.generation();
    let (ticket, _) = std::mem::take(&mut r.ticket).step(TicketEvent::Land(version, answer));
    r.ticket = ticket;
    if r.ticket.generation() != before {
        tx.instant();
    }
}

fn servers(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    let window = vmx.half.kind == crate::plugin_host::plugin::Kind::Window;
    method(lua, t, "List", move |lua, ()| {
        cx::read(Dep::Server);
        cx::read(Dep::Servers);
        with_app("Servers:List", |app| {
            let active = crate::servers::runtime::active(app).clone();
            let own = crate::servers::runtime::own_server().map(|s| s.base);
            type ListedServer = (String, String, Option<String>, &'static str, bool, bool, Option<String>);
            // (key, name, label, state, local, current, version) of each server the app lists: its
            // own, the plugins' (ready or not) and those added by address.
            let listed: Vec<ListedServer> = crate::settings::pages::servers::inventory::list(app)
                .into_iter()
                .filter(|x| !matches!(&x.kind, crate::settings::pages::servers::inventory::Kind::Plugin(rec) if !rec.listed))
                .map(|x| {
                    use crate::settings::pages::servers::inventory::Kind;
                    let state = match &x.kind {
                        Kind::Plugin(rec) => rec.state_name,
                        _ => "ready",
                    };
                    let label = match &x.kind {
                        Kind::Plugin(rec) if !rec.label.is_empty() => Some(rec.label.clone()),
                        _ => None,
                    };
                    let local = x.key.is_sidecar() || x.connection.as_ref().is_some_and(|c| c.local(own.as_deref()));
                    let version = opencode_servers::health::version(x.health.as_ref()).map(str::to_owned);
                    (server_id(&x.key), x.name.clone(), label, state, local, window && x.key == active, version)
                })
                .collect();
            let mut h = std::hash::DefaultHasher::new();
            listed.hash(&mut h);
            snapshot(lua, "servers", h.finish(), || {
                let list = lua.create_table_with_capacity(listed.len(), 0)?;
                for (key, name, label, state, local, current, version) in &listed {
                    list.raw_push(record(lua, 7, |e| {
                        e.str(c"key", key);
                        e.str(c"name", name);
                        e.opt_str(c"label", label.as_deref());
                        e.str(c"state", state);
                        e.bool(c"isLocal", *local);
                        e.bool(c"current", *current);
                        e.opt_str(c"version", version.as_deref());
                    })?)?;
                }
                Ok(Value::Table(list))
            })
        })
    })?;
    method(lua, t, "Current", move |_, ()| {
        cx::read(Dep::Server);
        with_app("Servers:Current", |app| Ok(window.then(|| server_id(crate::servers::runtime::active(app)))))
    })?;
    method(lua, t, "Health", |lua, key: String| {
        cx::read(Dep::Servers);
        let key = if key == opencode_kit::servers::ServerScope::LOCAL { opencode_kit::servers::ServerKey::sidecar() } else { opencode_kit::servers::ServerKey::new(&key) };
        let data = crate::plugin_host::surfaces::health_data(crate::servers::health::get(&key).as_ref());
        mlua::IntoLua::into_lua(data, lua)
    })?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "Connect", move |_, key: String| {
        cx::ask(&plugin, cx::Request::RunCommand { id: format!("server:{key}"), input: None });
        Ok(())
    })?;
    lazy_signals(lua, vmx, t, "Servers", &["Changed"])
}


fn theme(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    method(lua, t, "Current", |lua, ()| {
        let dark = with_app("Theme:Current", |app| Ok(app.plugins.dark.get()))?;
        let index = crate::palette::pal(dark).theme;
        let theme = session_view::themes::all().get(index);
        let mut h = std::hash::DefaultHasher::new();
        (index, dark, theme.map(|t| (&t.id, &t.name))).hash(&mut h);
        snapshot(lua, "theme", h.finish(), || {
            record(lua, 3, |info| {
                info.opt_str(c"id", theme.map(|t| t.id.as_str()));
                info.opt_str(c"name", theme.map(|t| t.name.as_str()));
                info.str(c"scheme", if dark { "dark" } else { "light" });
            })
            .map(Value::Table)
        })
    })?;
    method(lua, t, "IsDark", |_, ()| with_app("Theme:IsDark", |app| Ok(app.plugins.dark.get())))?;
    method(lua, t, "Resolve", |_, color: String| {
        let dark = with_app("Theme:Resolve", |app| Ok(app.plugins.dark.get()))?;
        let c = if color.starts_with('#') { rune::color::hex(&color) } else { crate::plugin_host::vm::palette(&color, dark) };
        let c = c.unwrap_or(rune::Color::TRANSPARENT);
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Ok(format!("#{:02x}{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b), byte(c.a)))
    })?;
    method(lua, t, "Font", |_, family: String| Ok(if family == "mono" { "OpenMono" } else { "Inter" }))?;
    lazy_signals(lua, vmx, t, "Theme", &["Changed"])
}
